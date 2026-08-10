use std::iter;
use std::sync::Arc;

use winit::{
    event::*,
    keyboard::{KeyCode, PhysicalKey},
    window::Window,
};
use wgpu::TextureUsages;
use cgmath::{Deg, Quaternion, Rotation3};

use crate::model::ModelVertex;
use crate::scene::Scene;
use crate::model;
use crate::camera;
use crate::text::GlyphVertex;

use model::{Vertex, DrawModel};

pub struct State<'a> {
    surface: wgpu::Surface<'a>,
    offscreen_texture: wgpu::Texture,
    depth_texture_view: wgpu::TextureView,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    config: wgpu::SurfaceConfiguration,
    pub size: winit::dpi::PhysicalSize<u32>,
    render_pipeline: wgpu::RenderPipeline,
    solid_render_pipeline: wgpu::RenderPipeline,
    rect_render_pipeline: wgpu::RenderPipeline,
    lines_render_pipeline: wgpu::RenderPipeline,
    text_render_pipeline: wgpu::RenderPipeline,
    terrain_render_pipeline: wgpu::RenderPipeline,
    pub scene: Scene,
    window: &'a Window,
    pub framerate: f32,
    pub mouse_pressed: bool,
    /// Last known cursor position, in the same window space as `Viewport::rect`. Tracked
    /// unconditionally so a click can hit-test the viewport under the cursor — `MouseInput`
    /// carries no coordinates of its own.
    cursor_position: winit::dpi::PhysicalPosition<f64>,
}

impl<'a> State<'a> {
    const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

    fn create_depth_texture(device: &wgpu::Device, size: winit::dpi::PhysicalSize<u32>) -> wgpu::TextureView {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Depth Texture"),
            size: wgpu::Extent3d { width: size.width, height: size.height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        }).create_view(&wgpu::TextureViewDescriptor::default())
    }

    fn create_render_pipeline(
        device: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        color_format: wgpu::TextureFormat,
        vertex_layouts: &[wgpu::VertexBufferLayout],
        shader: wgpu::ShaderModuleDescriptor,
        topology: wgpu::PrimitiveTopology,
        polygon_mode: wgpu::PolygonMode,
        cull_mode: Option<wgpu::Face>,
        depth_stencil: Option<wgpu::DepthStencilState>,
        blend: wgpu::BlendState,
    ) -> wgpu::RenderPipeline {
        let shader = device.create_shader_module(shader);

        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(&format!("{:?}", shader)),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: vertex_layouts,
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: Some(blend),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: topology,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode,
                // Setting this to anything other than Fill requires Features::NON_FILL_POLYGON_MODE
                polygon_mode: polygon_mode,
                // Requires Features::DEPTH_CLIP_CONTROL
                unclipped_depth: false,
                // Requires Features::CONSERVATIVE_RASTERIZATION
                conservative: false,
            },
            depth_stencil,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            // If the pipeline will be used with a multiview render pass, this
            // indicates how many array layers the attachments will have.
            multiview: None,
            cache: None,
        })
    }

    pub async fn new(window: &'a Window, filepath: &str, registry: crate::ring_buffer::BufferRegistry) -> State<'a> {
        let size = window.inner_size();
        // println!("window size: {} * {} = {}", size.width, size.height, size.width * size.height);

        // The instance is a handle to our GPU
        // BackendBit::PRIMARY => Vulkan + Metal + DX12 + Browser WebGPU
        log::warn!("WGPU setup");
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        // # Safety
        //
        // The surface needs to live as long as the window that created it.
        // State owns the window so this should be safe.
        let surface = instance.create_surface(window).unwrap();

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap();

        log::warn!("device and queue");

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: None,
                    required_features: wgpu::Features::POLYGON_MODE_LINE,
                    // WebGL doesn't support all of wgpu's features, so if
                    // we're building for the web we'll have to disable some.
                    required_limits: wgpu::Limits::default(),
                    memory_hints: Default::default(),
                    trace: wgpu::Trace::Off, // Trace path
                },
            )
            .await
            .unwrap();
        let device = Arc::new(device);
        let queue = Arc::new(queue);
        assert!(device.features().contains(wgpu::Features::POLYGON_MODE_LINE), "Wireframe polygon mode not supported!");

        log::warn!("Surface");
        let surface_caps = surface.get_capabilities(&adapter);
        // Shader code in this tutorial assumes an Srgb surface texture. Using a different
        // one will result all the colors comming out darker. If you want to support non
        // Srgb surfaces, you'll need to account for that when drawing to the frame.
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
            format: surface_format,
            width: size.width,
            height: size.height,
            present_mode: surface_caps.present_modes[0],
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };

        let offscreen_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Offscreen Target"),
            size: wgpu::Extent3d {
                width: size.width, 
                height: size.height, 
                depth_or_array_layers: 1
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let camera_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
                label: Some("Camera Bind Group Layout"),
            });

        let ortho_matrix_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Ortho Transformation Matrix"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }
            ]
        });

        let model_bind_group_layout = 
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
                label: Some("Model Bind Group Layout"),
        });

        let text_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // Per-TextDisplay fade alpha, rewritten each frame while a toast fades out.
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
            label: Some("Text Bind Group Layout"),
        });

        let render_pipeline_layout: wgpu::PipelineLayout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Render Pipeline Layout"),
                bind_group_layouts: &[
                    &camera_bind_group_layout,
                    &model_bind_group_layout,
                    ],
                push_constant_ranges: &[],
            });

        let text_render_pipeline_layout: wgpu::PipelineLayout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Text Render Pipeline Layout"),
                bind_group_layouts: &[
                    &ortho_matrix_bind_group_layout,
                    &text_bind_group_layout,
                    ],
                push_constant_ranges: &[],
            });
        
        let terrain_render_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Terrain Render Pipeline Layout"),
            bind_group_layouts: &[
                &camera_bind_group_layout,
            ],
            push_constant_ranges: &[],
        });

        let scene = Scene::load_scene(
            filepath,
            Arc::clone(&device),
            Arc::clone(&queue),
            &config.format,
            model_bind_group_layout,
            &text_bind_group_layout,
            camera_bind_group_layout,
            &ortho_matrix_bind_group_layout,
            size.width,
            size.height,
            registry,
        );

        let depth_3d = Some(wgpu::DepthStencilState {
            format: Self::DEPTH_FORMAT,
            depth_write_enabled: false,
            depth_compare: wgpu::CompareFunction::LessEqual,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        });
        let depth_ui = Some(wgpu::DepthStencilState {
            format: Self::DEPTH_FORMAT,
            depth_write_enabled: false,
            depth_compare: wgpu::CompareFunction::Always,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        });

        let render_pipeline = {
            let shader = wgpu::ShaderModuleDescriptor {
                label: Some("Normal Shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shaders/shader.wgsl").into()),
            };
            State::create_render_pipeline(
                &device,
                &render_pipeline_layout,
                config.format,
                &[model::ModelVertex::desc()],
                shader,
                wgpu::PrimitiveTopology::TriangleList,
                wgpu::PolygonMode::Line,
                None,
                depth_3d.clone(),
                wgpu::BlendState::REPLACE,
            )
        };

        let solid_render_pipeline = {
            let shader = wgpu::ShaderModuleDescriptor {
                label: Some("Solid Shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shaders/solid_shader.wgsl").into()),
            };
            State::create_render_pipeline(
                &device,
                &render_pipeline_layout,
                config.format,
                &[model::ModelVertex::desc()],
                shader,
                wgpu::PrimitiveTopology::TriangleList,
                wgpu::PolygonMode::Fill,
                Some(wgpu::Face::Back),
                Some(wgpu::DepthStencilState {
                    format: Self::DEPTH_FORMAT,
                    depth_write_enabled: true,
                    depth_compare: wgpu::CompareFunction::Less,
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState {
                        constant: 2,
                        slope_scale: 1.0,
                        clamp: 0.0,
                    },
                }),
                wgpu::BlendState::REPLACE,
            )
        };

        let lines_render_pipeline = {
            let shader = wgpu::ShaderModuleDescriptor {
                label: Some("Lines Shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shaders/shader.wgsl").into()),
            };
            State::create_render_pipeline(
                &device,
                &render_pipeline_layout,
                config.format,
                &[model::ModelVertex::desc()],
                shader,
                wgpu::PrimitiveTopology::LineList,
                wgpu::PolygonMode::Line,
                None,
                depth_3d.clone(),
                wgpu::BlendState::REPLACE,
            )
        };

        // this is mostly the same as the normal pipeline, but with Fill mode
        let rect_render_pipeline = {
            let shader = wgpu::ShaderModuleDescriptor {
                label: Some("Normal Shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shaders/shader.wgsl").into()),
            };
            State::create_render_pipeline(
                &device,
                &render_pipeline_layout,
                config.format,
                &[model::ModelVertex::desc()],
                shader,
                wgpu::PrimitiveTopology::TriangleList,
                wgpu::PolygonMode::Fill,
                None,
                depth_ui.clone(),
                wgpu::BlendState::REPLACE,
            )
        };

        let text_render_pipeline = {
            let shader = wgpu::ShaderModuleDescriptor {
                label: Some("Text Shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shaders/text_shader.wgsl").into()),
            };
            State::create_render_pipeline(
                &device,
                &text_render_pipeline_layout,
                config.format,
                &[GlyphVertex::desc()],
                shader,
                wgpu::PrimitiveTopology::TriangleList,
                wgpu::PolygonMode::Fill,
                None,
                depth_ui,
                wgpu::BlendState::ALPHA_BLENDING,
            )
        };

        let terrain_render_pipeline = {
            let shader = wgpu::ShaderModuleDescriptor {
                label: Some("Terrain Shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shaders/terrain_shader.wgsl").into()),
            };
            State::create_render_pipeline(
                &device,
                &terrain_render_pipeline_layout,
                config.format,
                &[ModelVertex::desc()],
                shader,
                wgpu::PrimitiveTopology::LineList,
                wgpu::PolygonMode::Line,
                None,
                depth_3d,
                wgpu::BlendState::REPLACE,
            )
        };

        let depth_texture_view = Self::create_depth_texture(&device, size);

        surface.configure(&device, &config);

        Self {
            surface,
            offscreen_texture,
            depth_texture_view,
            device,
            queue,
            config,
            size,
            render_pipeline,
            solid_render_pipeline,
            rect_render_pipeline,
            lines_render_pipeline,
            text_render_pipeline,
            terrain_render_pipeline,
            scene,
            window,
            framerate: 60.0,
            mouse_pressed: false,
            cursor_position: winit::dpi::PhysicalPosition::new(0.0, 0.0),
        }
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            for viewport in &mut self.scene.viewports {
                viewport.resize_from_window(
                    new_size.width as f32,
                    new_size.height as f32,
                    &self.queue,
                );
            }
            self.size = new_size;
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            self.surface.configure(&self.device, &self.config);
            self.depth_texture_view = Self::create_depth_texture(&self.device, new_size);
        }
    }

    pub fn device_input(&mut self, event: &DeviceEvent) -> bool {
        match event {
            DeviceEvent::MouseMotion { delta } => {
                if self.mouse_pressed {
                    self.scene.focused_camera().process_mouse(delta.0, delta.1);
                }
            }
            _ => {}
        };

        true
    }
    
    /// Tags an on-screen message with the viewport it refers to.
    ///
    /// Toasts draw in every viewport at once, so in a multi-viewport scene an untagged message
    /// about one camera reads as though it applied to all of them. A single-viewport scene has
    /// no ambiguity to resolve and is left alone, so its messages read as they did before
    /// viewports became addressable. The index is the one from the scene JSON's `viewports`
    /// array, so a message points at a line of JSON.
    fn viewport_toast(&self, index: usize, message: String) -> String {
        if self.scene.viewports.len() < 2 {
            message
        } else {
            format!("[viewport {}] {}", index, message)
        }
    }

    pub fn window_input(&mut self, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(key),
                        state,
                        repeat,
                        ..
                    },
                ..
            } => {
                // Drop OS auto-repeat. `process_keyboard` treats every Pressed event as a
                // fresh press, so a held Enter would toggle the camera mode dozens of times
                // a second. Movement is unaffected: it reads the pressed-keys set, which a
                // repeat would only redundantly re-insert.
                if *repeat {
                    return true;
                }

                // Watch the mode across the keypress rather than keying off Enter, so the message
                // stays correct wherever the switch is triggered from and never fires when
                // `switch_mode` declines to switch.
                //
                // Indexed rather than routed through `focused_camera`, so the borrow stays
                // scoped to `viewports` and `process_keyboard` can still take `&entities`.
                let focused = self.scene.focused_viewport();
                let mode_before = self.scene.viewports[focused].camera_controller.mode();
                let handled = self.scene.viewports[focused].camera_controller
                    .process_keyboard(*key, *state, &self.scene.entities);
                let mode_after = self.scene.viewports[focused].camera_controller.mode();

                let cycled = *key == KeyCode::KeyT
                    && *state == ElementState::Pressed
                    && mode_after == camera::CameraMode::OrbitPoint;

                if mode_after != mode_before {
                    let msg = self.viewport_toast(
                        focused,
                        format!("Switched camera to {}", mode_after.display_name()),
                    );
                    self.scene.show_toast(msg);
                } else if cycled {
                    // Reported on every press rather than only when the target actually moved.
                    // A scene whose entities are already exhausted by one cycle would otherwise
                    // swallow the keypress with no feedback at all, which is indistinguishable
                    // from the binding being broken. Re-showing the current target says "that
                    // worked, there is just nowhere else to go".
                    //
                    // Skipped when the mode changed: entering OrbitPoint already implies a
                    // target, and toasts share one slot, so this would erase that message.
                    let entity = self.scene.viewports[focused].camera_controller.orbit_target()
                        .and_then(|i| self.scene.entities.get(i))
                        .map_or("?".to_string(), |e| e.name().to_string());
                    let msg = self.viewport_toast(focused, format!("Orbiting {}", entity));
                    self.scene.show_toast(msg);
                }

                handled
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.scene.focused_camera().process_scroll(delta);
                true
            }
            WindowEvent::MouseInput {
                button: MouseButton::Left,
                state,
                ..
            } => {
                self.mouse_pressed = *state == ElementState::Pressed;

                // Focus follows the click, not the drag: picking on press means the whole
                // drag that follows belongs to one viewport, even if it leaves that viewport's
                // bounds. A click outside every viewport leaves focus where it is.
                if self.mouse_pressed {
                    if let Some(hit) = self.scene.viewport_at(
                        self.cursor_position.x as f32,
                        self.cursor_position.y as f32,
                    ) {
                        if self.scene.focus_viewport(hit) {
                            let mode = self.scene.viewports[hit].camera_controller.mode();
                            let msg = self.viewport_toast(
                                hit,
                                format!("Now controlling ({})", mode.display_name()),
                            );
                            self.scene.show_toast(msg);
                        }
                    }
                }

                true
            }
            WindowEvent::CursorMoved {
                position,
                ..
            } => {
                // Tracked on every move, not just while dragging: the position has to be
                // current the instant the button goes down for the hit test above to work.
                self.cursor_position = *position;

                true
            }
            _ => false,
        }
    }

    pub fn update(&mut self, dt: std::time::Duration, should_save_to_file: bool) {
        for viewport in &mut self.scene.viewports {
            viewport.update_camera(dt, &self.queue);
        }

        self.framerate = dt.as_secs_f32().recip();
        let fr_str = format!("{:.1} fps", self.framerate);
        self.scene.text_boxes[0].change_text(&self.device, fr_str);


        self.scene.update_toasts(dt);

        self.scene.run_behaviors();

        if should_save_to_file {
            self.scene.read_and_write_capture_buffers(
                &self.device,
                &self.queue,
                &self.offscreen_texture,
                self.size.width,
                self.size.height,
                dt
            );
        }
    }

    pub fn render(&mut self, should_save_to_file: bool) -> Result<(), wgpu::SurfaceError> {
        let output = self.surface.get_current_texture()?;
        let offscreen_view = self.offscreen_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let target = if should_save_to_file {
            offscreen_view
        } else {
            view
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_texture_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            for viewport in self.scene.viewports.iter() {
                render_pass.set_scissor_rect(
                    viewport.rect.x as u32,
                    viewport.rect.y as u32,
                    viewport.rect.width as u32,
                    viewport.rect.height as u32,
                );
                render_pass.set_viewport(viewport.rect.x, viewport.rect.y, viewport.rect.width, viewport.rect.height, 0.0, 1.0);

                viewport.draw_background_and_border(
                    &self.device, 
                    &mut render_pass,
                    &self.lines_render_pipeline, 
                    &self.rect_render_pipeline, 
                );
                render_pass.set_pipeline(&self.lines_render_pipeline);
                render_pass.draw_axes(&self.scene.axes, &viewport.camera_bind_group);

                self.scene.draw(
                    &mut render_pass,
                    &viewport.camera_bind_group,
                    &viewport.ortho_matrix_bind_group,
                    &self.solid_render_pipeline,
                    &self.render_pipeline,
                    &self.lines_render_pipeline,
                    &self.rect_render_pipeline,
                    &self.text_render_pipeline,
                    &self.terrain_render_pipeline,
                    &self.queue,
                );
            }
        }

        if should_save_to_file {
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.offscreen_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
            wgpu::TexelCopyTextureInfo {
                    texture: &output.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
            wgpu::Extent3d {
                    width: self.size.width,
                    height: self.size.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        self.queue.submit(iter::once(encoder.finish()));
        output.present();

        Ok(())
    }
}