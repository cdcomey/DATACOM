
use text::{TextDisplay};
use log::{debug, info};
use std::process::{Command, Stdio};
use std::io::Write;
use std::sync::Arc;
use std::rc::Rc;
use cgmath::{Matrix4, Vector3};
use wgpu::{Device, Queue, BindGroupLayout, util::DeviceExt};

use crate::{model, com, text, camera, behaviors_and_entities, ring_buffer, transform_stream};
use behaviors_and_entities::Entity;
use model::DrawModel;

const BYTES_PER_PIXEL: u32 = 4;
const NUM_CAPTURE_BUFFERS: usize = 3;

/// Pixel height the glyph atlas is rasterized at.
const FONT_SIZE: f32 = 100.0;
/// Viewport-local baseline position for toasts, below the FPS counter at (30, 100).
const TOAST_X: f32 = 30.0;
const TOAST_Y: f32 = 220.0;

#[derive(PartialEq)]
enum BorderAlignment {
    Free,
    TopLeft,
    TopRight,
    BottomRight,
    BottomLeft,
    FullScreen,
}

impl BorderAlignment {
    fn from_str(s: &str) -> Self {
        match s {
            "TopLeft" => BorderAlignment::TopLeft,
            "TopRight" => BorderAlignment::TopRight,
            "BottomLeft" => BorderAlignment::BottomLeft,
            "BottomRight" => BorderAlignment::BottomRight,
            "FullScreen" => BorderAlignment::FullScreen,
            _ => BorderAlignment::Free,
        }
    }
}

/// Groups `(source, consumer)` pairs by source, keeping only the sources with several consumers.
///
/// Order is stable — sources in first-seen order, consumers in the order given — so the warning
/// text does not shuffle between runs of the same scene.
fn find_shared_sources(consumers: &[(String, String)]) -> Vec<(String, Vec<String>)> {
    let mut order: Vec<String> = Vec::new();
    let mut grouped: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();

    for (source, consumer) in consumers {
        let entry = grouped.entry(source.clone()).or_insert_with(|| {
            order.push(source.clone());
            Vec::new()
        });
        entry.push(consumer.clone());
    }

    order.into_iter()
        .filter(|source| grouped[source].len() > 1)
        .map(|source| {
            let sharers = grouped[&source].clone();
            (source, sharers)
        })
        .collect()
}

pub struct Viewport {
    pub rect: model::Rect,
    aspect_ratio: f32,
    pub camera_controller: camera::CameraController,
    projection: camera::Projection,
    camera_uniform: camera::CameraUniform,
    camera_buffer: wgpu::Buffer,
    pub camera_bind_group: wgpu::BindGroup,
    ortho_transform_matrix: cgmath::Matrix4<f32>,
    ortho_transform_buffer: wgpu::Buffer,
    pub ortho_matrix_bind_group: wgpu::BindGroup,
    alignment: BorderAlignment,
}

impl Viewport {
    fn new(
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        camera: camera::Camera,
        camera_speed: f32,
        device: &Device,
        camera_bind_group_layout: &BindGroupLayout,
        ortho_matrix_bind_group_layout: &BindGroupLayout,
        border_color: cgmath::Vector3<f32>,
        alignment: BorderAlignment,
    ) -> Self {
        let projection = camera::Projection::new(w, h, cgmath::Deg(45.0), 0.1, 100.0);
        let mut camera_uniform = camera::CameraUniform::new();
        camera_uniform.update_view_proj(&camera, &projection);
        let camera_controller = camera::CameraController::new(camera_speed, 0.4, camera);

        let camera_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Camera Buffer"),
            contents: bytemuck::cast_slice(&[camera_uniform]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
            label: Some("Camera Bind Group"),
        });

        let ortho_transform_matrix: Matrix4<f32> = cgmath::ortho(0.0, w, h, 0.0, -1.0, 1.0);
        let ortho_transform_arr: [[f32; 4]; 4] = ortho_transform_matrix.into();

        let ortho_transform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Ortho transform matrix buffer"),
            contents: bytemuck::cast_slice(&ortho_transform_arr),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let ortho_matrix_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Ortho Matrix Bind Group"),
            layout: &ortho_matrix_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: ortho_transform_buffer.as_entire_binding(),
            }],
        });

        let rect = model::Rect::new(x, y, w, h, border_color, device, camera_bind_group_layout);

        Viewport {
            rect,
            aspect_ratio: w / h,
            camera_controller,
            projection,
            camera_uniform,
            camera_buffer,
            camera_bind_group,
            ortho_transform_matrix,
            ortho_transform_buffer,
            ortho_matrix_bind_group,
            alignment,
        }
    }

    fn default(
        device: &Device, 
        camera_bind_group_layout: &BindGroupLayout, 
        ortho_matrix_bind_group_layout: &BindGroupLayout, 
    ) -> Self {
        use cgmath::{Deg, Quaternion, Rotation3};
        let camera_yaw = Quaternion::from_angle_z(Deg(-45.0));
        let camera_pitch = Quaternion::from_angle_x(Deg(-45.0));
        let camera_roll = Quaternion::from_angle_y(Deg(0.0));
        let camera_rotation = camera_yaw * camera_roll * camera_pitch;
        let camera = camera::Camera::new((0.0, -5.0, 5.0), camera_rotation);

        Viewport::new(
            0.0,
            0.0,
            1600.0,
            1200.0,
            camera,
            8.0,
            device,
            camera_bind_group_layout,
            ortho_matrix_bind_group_layout,
            cgmath::Vector3::<f32>::new(0.0, 0.0, 0.0),
            BorderAlignment::FullScreen,
        )
    }

    fn load_from_json(
        json: &serde_json::Value,
        device: &Device,
        camera_bind_group_layout: &BindGroupLayout,
        ortho_matrix_bind_group_layout: &BindGroupLayout,
        registry: &ring_buffer::BufferRegistry,
    ) -> Self {
        let mut viewport = Viewport::new(
            json["x"].as_f64().unwrap() as f32,
            json["y"].as_f64().unwrap() as f32,
            json["w"].as_f64().unwrap() as f32,
            json["h"].as_f64().unwrap() as f32,
            {
                let pos_val = json["camera"]["position"].as_array().unwrap();
                let pos = cgmath::Point3::new(
                    pos_val[0].as_f64().unwrap() as f32,
                    pos_val[1].as_f64().unwrap() as f32,
                    pos_val[2].as_f64().unwrap() as f32,
                );
                let rot_val = json["camera"]["rotation"].as_array().unwrap();
                let s = rot_val[0].as_f64().unwrap() as f32;
                let v = cgmath::Vector3::new(
                    rot_val[1].as_f64().unwrap() as f32,
                    rot_val[2].as_f64().unwrap() as f32,
                    rot_val[3].as_f64().unwrap() as f32,
                );
                let rot: cgmath::Quaternion<f32> = cgmath::Quaternion::<f32>::from_sv(s, v);
                camera::Camera::new(pos, rot)
            },
            json["camera"]["speed"].as_f64().unwrap_or(8.0) as f32,
            device,
            camera_bind_group_layout,
            ortho_matrix_bind_group_layout,
            {
                let color = json["border color"].as_array().unwrap();
                cgmath::Vector3::new(
                    color[0].as_f64().unwrap() as f32,
                    color[1].as_f64().unwrap() as f32,
                    color[2].as_f64().unwrap() as f32,
                )
            },
            BorderAlignment::from_str(json["alignment"].as_str().unwrap()),
        );

        // A "stream" name puts this camera under wire control for the rest of the run. Absent,
        // the camera keeps the position and rotation above and stays user-driven. The name is
        // bound in the registry exactly like an entity's, so a mismatch with what the server
        // sends fails the same silent way — the camera simply never moves.
        if let Some(name) = json["camera"]["stream"].as_str() {
            debug!("viewport camera is stream-driven from {name}");
            viewport.camera_controller.attach_stream(
                transform_stream::TransformStream::from_registry(name, registry)
            );
        }

        viewport
    }

    pub fn resize_from_window(&mut self, screen_width: f32, screen_height: f32, queue: &Queue){
        /*
        if the width increased, we need to adjust right-aligned borders
        if the height increased, we need to adjust bottom-aligned borders
        if the width decreased, we need to make sure 

        when we change the screen dims, the vps should be moved and scaled accordingly
        right-aligned vps need their x adjusted
        bottom-aligned vps need their y adjusted
        screen-sized vp needs its w and h adjusted

         */
        // println!("resize from window called");
        // println!("new screen dims: {}, {}", screen_width, screen_height);
        if self.alignment == BorderAlignment::FullScreen {
            self.rect.height = screen_height;
        }
        if self.alignment == BorderAlignment::TopRight || self.alignment == BorderAlignment::BottomRight {
            self.rect.x = screen_width - self.rect.width;
        }

        if self.alignment == BorderAlignment::BottomLeft || self.alignment == BorderAlignment::BottomRight {
            self.rect.y = screen_height - self.rect.height;
        }

        self.projection.resize(self.rect.width, self.rect.height);

        let ortho_transform_arr: [[f32; 4]; 4] = self.ortho_transform_matrix.into();
        queue.write_buffer(
            &self.ortho_transform_buffer, 
            0, 
            bytemuck::cast_slice(&ortho_transform_arr)
        );
    }

    pub fn draw_background_and_border<'a>(
        &'a self,
        device: &Device,
        render_pass: &mut wgpu::RenderPass<'a>,
        lines_render_pipeline: &'a wgpu::RenderPipeline,
        rect_render_pipeline: &'a wgpu::RenderPipeline,
    ) {
        self.rect.draw_background_and_border(
            device,
            render_pass,
            lines_render_pipeline,
            rect_render_pipeline,
            &self.ortho_matrix_bind_group,
        );
    }

    pub fn update_camera(
        &mut self, 
        dt: std::time::Duration, 
        queue: &Queue
    ){
        self.camera_controller.update_camera(dt);
        self.camera_uniform.update_view_proj(&self.camera_controller.camera(), &self.projection);
        // log::info!("{:?}", viewport.camera_uniform);
    
        queue.write_buffer(
            &self.camera_buffer,
            0,
            bytemuck::cast_slice(&[self.camera_uniform]),
        );
    }
}

// Define the scene structure
pub struct Scene {
    pub axes: model::Axes,
    pub entities: Vec<Entity>,
    pub terrain: model::Terrain,
    pub text_boxes: Vec<text::TextDisplay>,
    text_resources: text::TextResources,
    toasts: Vec<text::Toast>,
    pub viewports: Vec<Viewport>,
    /// Index of the viewport that camera input is routed to. Always a valid index into
    /// `viewports`, which is never empty — the loaders push a default when the JSON has none.
    focused_viewport: usize,
    pub device: std::sync::Arc<Device>,
    pub queue: std::sync::Arc<Queue>,
    pub model_bind_group_layout: BindGroupLayout,
    pub camera_bind_group_layout: BindGroupLayout,
    elapsed_timesteps: usize,
    total_timesteps: Option<usize>,
    progress_bar: Option<model::ProgressBar>,
    data_counter: Option<usize>,
    frame_counter: usize,
    capture_buffers: Vec<wgpu::Buffer>,
    screen_recordings: Vec<Vec<u8>>,
    capture_duration: std::time::Duration,
}

impl Scene {
    pub fn new(
        entities: Vec<Entity>,
        total_timesteps: Option<usize>,
        progress_bar: Option<model::ProgressBar>,
        data_counter: Option<usize>,
        terrain: model::Terrain,
        viewports: Vec<Viewport>,
        device: std::sync::Arc<Device>,
        queue: std::sync::Arc<Queue>,
        format: &wgpu::TextureFormat,
        model_bind_group_layout: BindGroupLayout,
        camera_bind_group_layout: BindGroupLayout,
        text_bind_group_layout: &BindGroupLayout,
        screen_width: u32,
        screen_height: u32,
    ) -> Self {
        let axes = model::Axes::new(&device);
        let text_resources = text::TextResources::new(&device, &queue, format, text_bind_group_layout, FONT_SIZE);
        let text_boxes = Scene::init_text_boxes(&device, &text_resources, 60.0);
        let frame_counter: usize = 0;
        let capture_buffers = Scene::init_capture_buffers(
            &device,
            NUM_CAPTURE_BUFFERS,
            (Into::<u64>::into(BYTES_PER_PIXEL) * ((screen_width * screen_height) as u64)) as wgpu::BufferAddress
        );

        let screen_recordings = Vec::new();

        debug!("created Scene");

        Scene {
            axes,
            entities,
            terrain,
            text_boxes,
            text_resources,
            toasts: Vec::new(),
            viewports,
            focused_viewport: 0,
            device,
            queue,
            model_bind_group_layout,
            camera_bind_group_layout,
            elapsed_timesteps: 0,
            total_timesteps,
            progress_bar,
            data_counter,
            frame_counter,
            capture_buffers,
            screen_recordings,
            capture_duration: std::time::Duration::ZERO,
        }
    }

    fn init_text_boxes(
        device: &Device,
        text_resources: &text::TextResources,
        framerate: f32,
    ) -> Vec<TextDisplay> {
        let text_objects: Vec<TextDisplay> = vec![
            text_resources.make_display(
                device,
                framerate.to_string(),
                30.0,
                100.0,
                cgmath::Vector3::new(0.0, 255.0, 0.0),
            )
        ];

        text_objects
    }

    pub fn focused_viewport(&self) -> usize { self.focused_viewport }

    /// The camera controller every input path in `state.rs` addresses.
    pub fn focused_camera(&mut self) -> &mut camera::CameraController {
        &mut self.viewports[self.focused_viewport].camera_controller
    }

    /// Index of the topmost viewport containing a window-space point, if any.
    ///
    /// Viewports overlap — a `FullScreen` one typically sits under several insets — and `draw`
    /// paints them in order, so the *last* match is the one visible at that point. Searching in
    /// reverse makes the hit test agree with what the user sees.
    pub fn viewport_at(&self, x: f32, y: f32) -> Option<usize> {
        self.viewports.iter().rposition(|v| v.rect.contains(x, y))
    }

    /// Routes subsequent camera input to `index`, returning whether focus actually moved.
    ///
    /// Releases the outgoing viewport's held input, since it will never see the matching key-up.
    pub fn focus_viewport(&mut self, index: usize) -> bool {
        if index >= self.viewports.len() || index == self.focused_viewport {
            return false;
        }

        self.viewports[self.focused_viewport].camera_controller.release_all_input();
        self.focused_viewport = index;
        true
    }

    /// Shows a message that holds at full opacity for a second, then fades out quickly.
    ///
    /// Toasts are positioned in viewport-local coordinates, so one call draws the same
    /// message in every viewport. Only one is shown at a time — every toast occupies the
    /// same slot, so a new message replaces any still-fading predecessor rather than
    /// overlapping it into unreadable text.
    pub fn show_toast(&mut self, content: String) {
        self.toasts.clear();

        let display = self.text_resources.make_display(
            &self.device,
            content,
            TOAST_X,
            TOAST_Y,
            cgmath::Vector3::new(0.0, 255.0, 255.0),
        );

        self.toasts.push(text::Toast::new(
            display,
            text::Toast::DEFAULT_HOLD,
            text::Toast::DEFAULT_FADE,
        ));
    }

    /// Advances every toast's fade and drops the ones that have finished.
    pub fn update_toasts(&mut self, dt: std::time::Duration) {
        let queue = Arc::clone(&self.queue);
        self.toasts.retain_mut(|toast| toast.update(&queue, dt));
    }

    pub fn run_behaviors(&mut self) {
        for entity in &mut self.entities {
            entity.run_behaviors(self.data_counter);
        }

        if let Some(c) = self.data_counter {
            // self.data_counter = Some(c + DATA_ARR_WIDTH * AVERAGE_REFRESH_RATE);
            self.data_counter = Some(c + behaviors_and_entities::DATA_ARR_WIDTH);
            debug!("data counter is now {}", self.data_counter.unwrap());
        }

        self.elapsed_timesteps += 1;

        if let Some(pb) = &mut self.progress_bar {
            pb.current_transform = pb.get_transform_matrix(self.elapsed_timesteps);
        }
    }

    pub fn increment_frame_counter(&mut self){
        self.frame_counter += 1;
        // println!("frame {}", self.frame_counter);
    }

    // pub fn bhvr_msg_str(&mut self, json_unparsed: &str) {
    //     if json_unparsed.is_empty() {
    //         return;
    //     }
    //     // let json_parsed: Value = serde_json::from_str(json_unparsed);
    //     // self.cmd_msg(&json_parsed);

    //     let json_parsed: serde_json::Value = match serde_json::from_str(&json_unparsed) {
    //         serde_json::Result::Ok(val) => val,
    //         serde_json::Result::Err(_) => serde_json::Value::Null,
    //         // _ => {}
    //     };

    //     // debug!("Parsed JSON Packet: {}", json_parsed.to_string());

    //     if json_parsed != serde_json::Value::Null {
    //         for behavior in json_parsed.as_array().expect("").into_iter() {
    //             // debug!("Target ID: {}", cmd["targetEntityID"]);
    //             // debug!("Cmd Type: {}", cmd["commandType"]);
    //             // debug!("Data: {}", cmd["data"]);
    //             self.bhvr_msg(&behavior);
    //         }
    //         // self.bhvr_msg(&json_parsed);
    //     } else {
    //         error!("json failed to load!");
    //         error!("{}", json_unparsed);
    //     }
    // }

    // pub fn bhvr_msg(&mut self, json_parsed: &serde_json::Value) {

    //     // debug!("Target ID: {}", json_parsed["targetEntityID"]);
    //     let target_entity_id = json_parsed["targetEntityID"].as_u64().unwrap() as usize;

    //     let behavior = Behavior::load_from_json(json_parsed);

    //     self.get_entity(target_entity_id).expect("Out of bounds!").run_behavior(behavior);
    // }

    pub fn load_scene(
        filepath: &str,
        device: std::sync::Arc<Device>,
        queue: std::sync::Arc<Queue>,
        format: &wgpu::TextureFormat,
        model_bind_group_layout: BindGroupLayout,
        text_bind_group_layout: &BindGroupLayout,
        camera_bind_group_layout: BindGroupLayout,
        ortho_matrix_bind_group_layout: &BindGroupLayout,
        screen_width: u32,
        screen_height: u32,
        registry: ring_buffer::BufferRegistry,
    ) -> Self {
        if filepath.ends_with(".hdf5") {
            Scene::load_scene_from_hdf5(
                filepath,
                device,
                queue,
                format,
                model_bind_group_layout,
                text_bind_group_layout,
                camera_bind_group_layout,
                ortho_matrix_bind_group_layout,
                screen_width,
                screen_height,
            ).unwrap()
        } else {
            Scene::load_scene_from_json(
                filepath,
                device,
                queue,
                format,
                model_bind_group_layout,
                text_bind_group_layout,
                camera_bind_group_layout,
                ortho_matrix_bind_group_layout,
                screen_width,
                screen_height,
                registry,
            )
        }
    }

    fn load_scene_from_hdf5(
        filepath: &str,
        device: std::sync::Arc<Device>,
        queue: std::sync::Arc<Queue>,
        format: &wgpu::TextureFormat,
        model_bind_group_layout: BindGroupLayout,
        text_bind_group_layout: &BindGroupLayout,
        camera_bind_group_layout: BindGroupLayout,
        ortho_matrix_bind_group_layout: &BindGroupLayout,
        screen_width: u32,
        screen_height: u32,
    ) -> hdf5::Result<Scene> {
        let file = hdf5::File::open(filepath).unwrap();
        let vehicles = file.group("Vehicles").unwrap();
        let vehicles_vec = vehicles.groups().unwrap();
        let mut entity_vec = vec![];
        for vehicle in vehicles_vec.iter() {
            let name_full = vehicle.name();
            let name = name_full["/Vehicles/".len()..].to_string();
            let data = vehicle.dataset("states").unwrap();
            let entity = Entity::load_from_hdf5(name, data, &device, &model_bind_group_layout).unwrap();
            entity_vec.push(entity);
        }

        let terrain = model::Terrain::new(serde_json::Value::Null, &device);

        let viewports = vec![
            Viewport::default(
                &device,
                &camera_bind_group_layout,
                ortho_matrix_bind_group_layout,
            )
        ];

        let num_entities = entity_vec.len();
        println!("LOADED {} ENTITIES INTO SCENE", num_entities);

        let total_timesteps = Scene::find_timesteps(&entity_vec);
        let data_counter = total_timesteps.map(|_| 0 as usize);
        let progress_bar = total_timesteps.map(|n| model::ProgressBar::new(
            serde_json::Value::Null,
            &device,
            &camera_bind_group_layout,
            n,
        ));

        Ok(Scene::new(
            entity_vec,
            total_timesteps,
            progress_bar,
            data_counter,
            terrain,
            viewports,
            device,
            queue,
            format,
            model_bind_group_layout,
            camera_bind_group_layout,
            text_bind_group_layout,
            screen_width,
            screen_height,
        ))
    }

    fn find_timesteps(entity_vec: &Vec<Entity>) -> Option<usize> {
        for entity in entity_vec.iter() {
            let total_timesteps = entity.find_timesteps();
            if let Some(_) = total_timesteps {
                return total_timesteps;
            }
        }

        println!("could only find constant behavior");
        None
    }

    fn load_scene_from_json(
        filepath: &str,
        device: std::sync::Arc<Device>,
        queue: std::sync::Arc<Queue>,
        format: &wgpu::TextureFormat,
        model_bind_group_layout: BindGroupLayout,
        text_bind_group_layout: &BindGroupLayout,
        camera_bind_group_layout: BindGroupLayout,
        ortho_matrix_bind_group_layout: &BindGroupLayout,
        screen_width: u32,
        screen_height: u32,
        registry: ring_buffer::BufferRegistry,
    ) -> Scene {
        info!("Loading {filepath}...");
        let json_unparsed = std::fs::read_to_string(filepath).unwrap();
        Scene::load_scene_from_json_str(
            json_unparsed,
            device,
            queue,
            format,
            model_bind_group_layout,
            text_bind_group_layout,
            camera_bind_group_layout,
            ortho_matrix_bind_group_layout,
            screen_width,
            screen_height,
            registry,
        )
    }

    fn load_scene_from_json_str(
        json_unparsed: String,
        device: std::sync::Arc<Device>,
        queue: std::sync::Arc<Queue>,
        format: &wgpu::TextureFormat,
        model_bind_group_layout: BindGroupLayout,
        text_bind_group_layout: &BindGroupLayout,
        camera_bind_group_layout: BindGroupLayout,
        ortho_matrix_bind_group_layout: &BindGroupLayout,
        screen_width: u32,
        screen_height: u32,
        registry: ring_buffer::BufferRegistry,
    ) -> Scene {
        let mut json: serde_json::Value = serde_json::from_str(&json_unparsed).unwrap();
        let total_timesteps = json["total_timesteps"].as_u64();
        // total_timesteps in JSON is a frame count; scale to match data_counter which advances by DATA_ARR_WIDTH per frame
        let total_timesteps = total_timesteps.map(|e| e as usize * behaviors_and_entities::DATA_ARR_WIDTH);
        let data_counter = total_timesteps.map(|_| 0 as usize);
        let terrain = model::Terrain::new(json["terrain"].take(), &device);

        let mut viewport_vec = Vec::new();
        if let Some(viewport_temp) = json["viewports"].as_array() {
            for i in viewport_temp.iter() {
                viewport_vec.push(Viewport::load_from_json(i, &device, &camera_bind_group_layout, ortho_matrix_bind_group_layout, &registry));
            }
        }
        if viewport_vec.is_empty() {
            viewport_vec.push(Viewport::default(&device, &camera_bind_group_layout, ortho_matrix_bind_group_layout));
        }

        let entity_temp: Vec<_> = json["entities"]
            .as_array()
            .unwrap()
            .into_iter()
            .collect();
        let mut entity_vec = vec![];
        for i in entity_temp.iter() {
            entity_vec.push(Entity::load_from_json(*i, &device, &model_bind_group_layout, &registry));
        }

        let scene = Scene::new(
            entity_vec,
            total_timesteps,
            None,
            data_counter,
            terrain,
            viewport_vec,
            device,
            queue,
            format,
            model_bind_group_layout,
            camera_bind_group_layout,
            text_bind_group_layout,
            screen_width,
            screen_height,
        );

        scene.warn_on_shared_stream_sources();
        scene
    }

    // pub fn load_scene_from_network(
    //     addr: &str, 
    //     device: &Device, 
    //     queue: &Queue,
    //     format: &wgpu::TextureFormat, 
    //     model_bind_group_layout: &BindGroupLayout, 
    //     text_bind_group_layout: &BindGroupLayout, 
    //     screen_width: u32,
    //     screen_height: u32,
    // ) -> Result<Scene, Box<dyn std::error::Error>> {
    //     // Open port
    //     let listener = std::net::TcpListener::bind(addr).unwrap();
    //     let mut num_attempt = 0usize;
        
    //     // Attempt to recieve initialization packet and parse when successful.
    //     let initialization_packet = loop {
    //         match listener.accept() {
    //             Ok((stream, _)) => {
    //                 // debug!("{}", com::from_network(&stream));
    //                 break com::from_network(&stream)
    //             },
    //             _ => {
    //                 num_attempt += 1;
    //                 debug!("No packet recieved. Trying attempt {}...", num_attempt);
    //                 std::thread::sleep(std::time::Duration::from_millis(100));
    //             },
    //         }
    //     };
    //     info!("Received initialization file");
    //     let initialization_packet = String::from_utf8(initialization_packet).unwrap();
    //     // debug!("Initialization file: {}", initialization_packet);

    //     // Receive and save model files
    //     for stream in listener.incoming() {

    //         let mut local_stream = stream.unwrap();
    //         match com::from_network_with_protocol(&mut local_stream) {
    //             Ok(_) => {},
    //             Err("END") => {
    //                 debug!("Finished recieving files!");
    //             }
    //             _ => {break}
    //         }
    //     }

    //     info!("All files recieved.");

    //     //
        
    //     // Load Scene from initialization packet

    //     Ok(
    //         Scene::load_scene_from_json_str(
    //             initialization_packet, 
    //             device, 
    //             queue,
    //             format,
    //             model_bind_group_layout, 
    //             text_bind_group_layout,
    //             screen_width, 
    //             screen_height,
    //         )
    //     )
        
    // }

    pub fn draw<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        ortho_matrix_bind_group: &'a wgpu::BindGroup,
        solid_render_pipeline: &'a wgpu::RenderPipeline,
        model_render_pipeline: &'a wgpu::RenderPipeline,
        lines_render_pipeline: &'a wgpu::RenderPipeline,
        rect_render_pipeline: &'a wgpu::RenderPipeline,
        text_render_pipeline: &'a wgpu::RenderPipeline,
        terrain_render_pipeline: &'a wgpu::RenderPipeline,
        queue: &Queue,
    ){
        render_pass.set_pipeline(terrain_render_pipeline);
        render_pass.draw_terrain(&self.terrain, camera_bind_group);

        render_pass.set_pipeline(lines_render_pipeline);
        for entity in self.entities.iter() {
            entity.draw_trail(render_pass, camera_bind_group, &self.device);
        }

        render_pass.set_pipeline(solid_render_pipeline);
        for entity in self.entities.iter() {
            entity.draw(render_pass, camera_bind_group, queue);
        }

        render_pass.set_pipeline(model_render_pipeline);
        for entity in self.entities.iter() {
            entity.draw(render_pass, camera_bind_group, queue);
        }

        // render_pass.set_pipeline(text_render_pipeline);
        // for text_box in self.text_boxes.iter() {
        //     text_box.draw(ortho_matrix_bind_group, render_pass);
        // }

        if let Some(pb) = &self.progress_bar {
            pb.draw(&self.device, render_pass, lines_render_pipeline, rect_render_pipeline, ortho_matrix_bind_group);
        }

        // Drawn last so toasts sit on top of everything else in the viewport.
        render_pass.set_pipeline(text_render_pipeline);
        for toast in self.toasts.iter() {
            toast.draw(ortho_matrix_bind_group, render_pass);
        }
    }

    fn init_capture_buffers(device: &Device, num_buffers: usize, size: wgpu::BufferAddress) -> Vec<wgpu::Buffer> {
        let buffers: Vec<wgpu::Buffer> = (0..num_buffers)
            .map(|_| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Frame readback buffer"),
                    size,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                })
            })
            .collect();
        
        buffers
    }

    pub fn read_and_write_capture_buffers(&mut self, device: &Device, queue: &Queue, offscreen_texture: &wgpu::Texture, width: u32, height: u32, dt: std::time::Duration){
            self.capture_duration += dt;
            let index = self.frame_counter % NUM_CAPTURE_BUFFERS;
            // read buf n
            if self.frame_counter > NUM_CAPTURE_BUFFERS {
                let mut saved_frames = Scene::read_capture_buf(
                    device, 
                    &self.capture_buffers, 
                    width, 
                    height,
                    index,
                ).expect("Error in State::update(); failed to capture screen data in buffer");
                self.screen_recordings.append(&mut saved_frames);
            }

            // write to buf n
            Scene::write_screen_to_capture_buf(
                device,
                queue,
                offscreen_texture,
                &mut self.capture_buffers,
                width, 
                height,
                index,
            );

            self.increment_frame_counter();
    }

    pub fn capture_complete(&self) -> bool {
        self.data_counter > self.total_timesteps
    }

    /// Logs a warning for every stream source read by more than one consumer.
    ///
    /// Two consumers bound to one name share a single `SharedBuffer`, and `next_frame` *drains*
    /// what it reads — so they do not both see the stream, they take alternate frames and each
    /// runs at half speed. Nothing errors; the motion just looks wrong.
    ///
    /// This is the likely mistake for the feature that motivated stream cameras: pointing a chase
    /// camera at the same `.bin` as the drone it follows reads as the obvious thing to write, and
    /// is exactly the thing that breaks. Give each consumer its own name and have the server send
    /// the frames twice.
    pub fn warn_on_shared_stream_sources(&self) {
        let mut consumers: Vec<(String, String)> = Vec::new();

        for (i, viewport) in self.viewports.iter().enumerate() {
            if let Some(name) = viewport.camera_controller.stream_source() {
                consumers.push((name.to_string(), format!("viewport {} camera", i)));
            }
        }
        for entity in &self.entities {
            entity.collect_stream_sources(&mut consumers);
        }

        for (source, sharers) in find_shared_sources(&consumers) {
            log::warn!(
                "stream '{}' is read by {} consumers ({}) — they will take alternate frames and \
                 each run at a fraction of the intended rate; give each its own source name",
                source,
                sharers.len(),
                sharers.join(", "),
            );
        }
    }

    /// Whether every entity stream has run dry — the network path's signal that a run is over.
    ///
    /// Deliberately ignores stream-driven cameras. A camera holding its last pose between packets
    /// is a fine steady state, so counting it would let a quiet camera either hold the session
    /// open past the end of the data or close it early while entities were still moving.
    pub fn all_streams_exhausted(&self) -> bool {
        self.entities.iter().all(|e| e.all_streams_exhausted())
    }

    /// Resets the scene to a single camera and no entities.
    ///
    /// Takes the registry so the entity drop and the buffer purge happen together: they are one
    /// operation, and splitting them lets a replacement scene bind a name whose buffer is about to
    /// be dropped. See `ring_buffer::clear_registry` for why dropping entities alone is not enough.
    ///
    /// Deliberately left intact:
    /// - `terrain` and any viewport beyond the first are environment the *first* scene defined.
    ///   `base_scene_written` is one-way, so nothing can ever re-establish them — wiping terrain
    ///   would be unrecoverable for the rest of the run.
    /// - `text_boxes` is HUD chrome, not scene content; `state.rs` indexes `text_boxes[0]` for the
    ///   framerate readout and would panic if it were emptied.
    /// - the frame and timestep counters, so a clear does not look like a restart to anything
    ///   reading data by index.
    pub fn clear(&mut self, registry: &ring_buffer::BufferRegistry) {
        self.entities.clear();
        ring_buffer::clear_registry(registry);
        // keep the first viewport rather than building a fresh one: it already owns a valid camera
        // and bind groups
        self.viewports.truncate(1);
        // Entities are dropped by the line above, so their buffer handles go with them. The
        // surviving viewport's camera is the one thing that outlives the purge still holding an
        // `Arc`, so a stream-driven camera has to be re-bound explicitly or it would stall
        // forever against a buffer the registry no longer indexes. Must follow the purge.
        if let Some(viewport) = self.viewports.first_mut() {
            viewport.camera_controller.rebind_stream(registry);
        }
        // the focused viewport may have just been dropped, and `focused_viewport` must stay a
        // valid index — every input path indexes with it unchecked
        self.focused_viewport = 0;
    }

    // returns how many entities were merged in, so callers can report whether a stream that
    // connected actually contributed anything to the scene
    pub fn append_entities_from_json_str(&mut self, json_str: &str, registry: &ring_buffer::BufferRegistry) -> usize {
        let json: serde_json::Value = serde_json::from_str(json_str).unwrap();

        // Only the first scene to arrive defines viewports, so anything declared here is dropped.
        // Silently ignoring it is the constraint most likely to waste someone's time: a camera
        // stream declared on a losing scene simply never happens, with nothing to show why.
        if let Some(viewports) = json["viewports"].as_array() {
            if !viewports.is_empty() {
                let streamed = viewports.iter()
                    .filter(|v| v["camera"]["stream"].is_string())
                    .count();
                log::warn!(
                    "ignoring {} viewport definition(s) from a merged scene ({} with a camera \
                     stream) — only the first scene to arrive defines viewports; declare them in \
                     every scene if which one arrives first is not deterministic",
                    viewports.len(),
                    streamed,
                );
            }
        }

        let Some(entity_array) = json["entities"].as_array() else { return 0 };
        for e in entity_array {
            self.entities.push(Entity::load_from_json(e, &self.device, &self.model_bind_group_layout, registry));
        }

        // Re-run after the merge rather than only at load: a stream camera declared by the first
        // scene collides with an entity that only arrives now.
        self.warn_on_shared_stream_sources();

        entity_array.len()
    }

    pub fn finish_capture(&mut self, width: u32, height: u32) {
        let device = self.device.clone();
        self.read_remaining_buffers(&device, width, height);
        let frame_count = self.screen_recordings.len();
        println!("{} total frames recorded", frame_count);

        // Derive the average capture framerate from wall-clock time spent recording,
        // falling back to 60 fps if we somehow have no timing data.
        let elapsed_secs = self.capture_duration.as_secs_f64();
        let fps = if frame_count > 0 && elapsed_secs > 0.0 {
            frame_count as f64 / elapsed_secs
        } else {
            60.0
        };

        Scene::save_screen_data_to_file(&self.screen_recordings, width, height, fps);
    }

    fn write_screen_to_capture_buf(device: &Device, queue: &Queue, texture: &wgpu::Texture, capture_buffers: &mut Vec<wgpu::Buffer>, width: u32, height: u32, index: usize){
        let padded_bytes_per_row = ((width * BYTES_PER_PIXEL + 255) / 256) * 256;

        let capture_buf = &capture_buffers[index];

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("CopyTextureToBuffer Encoder"),
        });

        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &capture_buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1
            },
        );

        // println!("wrote to buffer[{}]", index);
        // println!("oldest buffer is now {}", (index+1) % NUM_CAPTURE_BUFFERS);

        queue.submit(Some(encoder.finish()));
    }

    fn read_capture_buf(device: &Device, capture_buffers: &Vec<wgpu::Buffer>, width: u32, height: u32, index: usize) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
        let padded_bytes_per_row = ((width * BYTES_PER_PIXEL + 255) / 256) * 256;
        let mut output = Vec::new();

        let buffer = &capture_buffers[index];
        let buffer_slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |v| tx.send(v).unwrap());
        device.poll(wgpu::MaintainBase::Wait)?;
        rx.recv().unwrap().unwrap();

        let data = buffer_slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((width * height * BYTES_PER_PIXEL) as usize);

        for chunk in data.chunks(padded_bytes_per_row as usize) {
            pixels.extend_from_slice(&chunk[..(width * BYTES_PER_PIXEL) as usize]);
        }

        drop(data);
        buffer.unmap();
        output.push(pixels);

        // println!("read from buffer[{}]", index);

        Ok(output)
    }

    fn read_remaining_buffers(&mut self, device: &Device, width: u32, height: u32){
        for i in 0..NUM_CAPTURE_BUFFERS {
            let index = (self.frame_counter + i) % NUM_CAPTURE_BUFFERS;
            let mut saved_frame = Scene::read_capture_buf(device, &self.capture_buffers, width, height, index).expect("problem with reading final few buffers");
            self.screen_recordings.append(&mut saved_frame);
        }
    }

    fn save_screen_data_to_file(screen_data: &Vec<Vec<u8>>, width: u32, height: u32, fps: f64){
        let size_arg = format!("{}x{}", width, height);
        let rate_arg = format!("{:.3}", fps);

        let mut ffmpeg_process = Command::new("ffmpeg")
            .args(&[
                "-f", "rawvideo", //    input is raw video pixels
                "-pix_fmt", "bgra", //  BGRA format (wgpu surface on macOS is Bgra8Unorm)
                "-s", &size_arg, //     dimensions (actual capture resolution)
                "-r", &rate_arg, //     fps (measured average capture framerate)
                "-i", "pipe:0", //      read input from stdin
                "-c:v", "libx264", //   specify video codex
                "-pix_fmt", "yuv420p",
                "-preset", "fast", // fast encoding preset
                "-movflags", "faststart", // optimize for streaming
                "-y", // overwrite output file
                "output.mp4",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("ffmpeg process failed to execute");

        let mut stdin = ffmpeg_process.stdin.take().expect("failed to extract stdin from ffmpeg process");

        for frame in screen_data {
            stdin.write_all(frame).expect("failed to write input to ffmpeg process");
        }
        drop(stdin);

        let status = ffmpeg_process.wait().unwrap();

        if status.success() {
            println!("video successfully converted!");
        } else {
            eprintln!("ffmpeg failed with status: {:?}", status)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(raw: &[(&str, &str)]) -> Vec<(String, String)> {
        raw.iter().map(|(s, c)| (s.to_string(), c.to_string())).collect()
    }

    #[test]
    fn distinct_sources_do_not_collide() {
        let consumers = pairs(&[
            ("drone1.bin", "entity 'Drone_01'"),
            ("chase_cam.bin", "viewport 0 camera"),
        ]);

        assert!(find_shared_sources(&consumers).is_empty());
    }

    // The mistake the warning exists for: aiming a chase camera at the drone's own stream. Both
    // drain the one buffer, so each sees every other frame.
    #[test]
    fn a_camera_sharing_an_entitys_source_is_reported() {
        let consumers = pairs(&[
            ("drone1.bin", "viewport 1 camera"),
            ("drone1.bin", "entity 'Drone_01'"),
        ]);

        let shared = find_shared_sources(&consumers);

        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].0, "drone1.bin");
        assert_eq!(shared[0].1, vec!["viewport 1 camera", "entity 'Drone_01'"]);
    }

    #[test]
    fn every_colliding_source_is_reported_once_with_all_sharers() {
        let consumers = pairs(&[
            ("a.bin", "entity 'A'"),
            ("b.bin", "entity 'B'"),
            ("a.bin", "viewport 0 camera"),
            ("a.bin", "viewport 2 camera"),
            ("c.bin", "entity 'C'"),
        ]);

        let shared = find_shared_sources(&consumers);

        assert_eq!(shared.len(), 1, "only a.bin is shared");
        assert_eq!(shared[0].1.len(), 3);
    }

    // Warning text that reorders between runs of the same scene is hard to diff and reads as if
    // something changed when nothing did.
    #[test]
    fn report_order_follows_first_appearance() {
        let consumers = pairs(&[
            ("z.bin", "entity 'Z1'"),
            ("a.bin", "entity 'A1'"),
            ("z.bin", "entity 'Z2'"),
            ("a.bin", "entity 'A2'"),
        ]);

        let sources: Vec<String> = find_shared_sources(&consumers)
            .into_iter().map(|(s, _)| s).collect();

        assert_eq!(sources, vec!["z.bin", "a.bin"]);
    }
}
