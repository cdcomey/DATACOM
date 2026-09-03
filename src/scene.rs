
use text::{TextDisplay};
use log::{debug, info, warn};
use std::process::{Command, Stdio};
use std::io::Write;
use std::sync::Arc;
use cgmath::{InnerSpace, Matrix4, Point3, Quaternion};
use wgpu::{Device, Queue, BindGroupLayout, util::DeviceExt};

use std::collections::HashSet;

use crate::{model, com, text, camera, camera_behavior, behaviors_and_entities, ring_buffer};
use behaviors_and_entities::{Entity, DISAMBIGUATOR, PATH_SEPARATOR};
use camera_behavior::{PositionBehavior, RotationBehavior};
use model::DrawModel;

const BYTES_PER_PIXEL: u32 = 4;
const NUM_CAPTURE_BUFFERS: usize = 3;

/// Frame rate `ffmpeg` is told to assume while frames are still arriving.
///
/// The real capture rate is only known once recording stops, but `-r` has to be fixed when the
/// process spawns — so frames are encoded against this and the container is retimed afterwards.
/// See `VideoEncoder::retime`.
const NOMINAL_CAPTURE_FPS: f64 = 60.0;

/// Scratch file the streaming encode writes to before it is retimed into `CAPTURE_OUTPUT_PATH`.
const RAW_CAPTURE_PATH: &str = "capture_raw.mp4";
const CAPTURE_OUTPUT_PATH: &str = "output.mp4";

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

/// Speed a camera falls back to when its scene declares no position behavior. The value every
/// viewport used before behaviors were declarable, so an undeclared camera moves as it always did.
const DEFAULT_CAMERA_SPEED: f32 = 8.0;
const DEFAULT_CAMERA_SENSITIVITY: f32 = 0.4;

/// Reads a fixed-length number array out of a command payload, or `None` if it is missing, the
/// wrong length, or not numeric. Every caller reports the miss and drops the command.
fn read_floats(json: &serde_json::Value, field: &str, expected: usize) -> Option<Vec<f32>> {
    let array = json[field].as_array()?;
    if array.len() != expected {
        return None;
    }
    array.iter().map(|v| v.as_f64().map(|f| f as f32)).collect()
}

fn read_point(json: &serde_json::Value, field: &str) -> Option<Point3<f32>> {
    let v = read_floats(json, field, 3)?;
    Some(Point3::new(v[0], v[1], v[2]))
}

/// Drops the registry entry a replaced stream was reading, so its buffer and the frames still in
/// it go with the behavior that wanted them.
///
/// Skipped when the incoming behavior reads the same name. The new stream has already bound to
/// that entry, and `com::write_to_registry` looks entries up with `get` — so removing it would
/// leave the new stream holding a buffer nothing can ever write to again, and the camera would
/// stall forever with nothing reporting an error.
fn release_replaced_stream(
    replaced: Option<&str>,
    installed: Option<&str>,
    registry: &ring_buffer::BufferRegistry,
) {
    let Some(source) = replaced else { return };
    if installed == Some(source) {
        return;
    }

    registry.lock().unwrap().remove(source);
    debug!("released the stream buffer for {}", source);
}

fn default_position_behavior() -> PositionBehavior {
    PositionBehavior::FreeRoam { speed: DEFAULT_CAMERA_SPEED }
}

fn default_rotation_behavior() -> RotationBehavior {
    RotationBehavior::UserControlled { sensitivity: DEFAULT_CAMERA_SENSITIVITY }
}

/// Turns one JSON camera `name` into the address the master server uses.
///
/// A camera nobody named is addressed by its index in the `viewports` array, exactly as an unnamed
/// entity is addressed by its index in `entities`. Viewports come from a single scene — the first
/// to arrive — so that index is stable for the run and predictable to whoever wrote that scene.
fn normalize_camera_name(raw: &str, index: usize) -> String {
    if raw.is_empty() {
        format!("{}{}", DISAMBIGUATOR, index)
    } else {
        raw.to_string()
    }
}

/// Renames colliding cameras until every name is distinct.
///
/// Mirrors `disambiguate_names` for entities, down to suffixing *every* member of a colliding
/// group rather than letting the first keep the bare name: a command that quietly aimed the wrong
/// viewport would be harder to spot than one that visibly aimed nothing.
fn disambiguate_camera_names(names: &mut [String]) {
    let mut totals: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for name in names.iter() {
        *totals.entry(name.clone()).or_insert(0) += 1;
    }

    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for name in names.iter_mut() {
        let total = totals[name.as_str()];
        if total < 2 {
            continue;
        }

        let occurrence = *seen.entry(name.clone()).or_insert(0);
        if occurrence == 0 {
            warn!(
                "the scene declares {} cameras named {:?}; they are addressable as {}{}0 through \
                 {}{}{}",
                total, name, name, DISAMBIGUATOR, name, DISAMBIGUATOR, total - 1,
            );
        }
        *seen.get_mut(name.as_str()).unwrap() += 1;
        *name = format!("{}{}{}", name, DISAMBIGUATOR, occurrence);
    }
}

pub struct Viewport {
    pub rect: model::Rect,
    aspect_ratio: f32,
    /// What the master server addresses this viewport's camera by. Declared in scene JSON, or the
    /// viewport's index under `#` when it declares nothing — the same convention entities use for
    /// the same reason, so an operator who learns one address format can read the other.
    camera_name: String,
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
        camera_name: String,
        camera_controller: camera::CameraController,
        device: &Device,
        camera_bind_group_layout: &BindGroupLayout,
        ortho_matrix_bind_group_layout: &BindGroupLayout,
        border_color: cgmath::Vector3<f32>,
        alignment: BorderAlignment,
    ) -> Self {
        let projection = camera::Projection::new(w, h, cgmath::Deg(45.0), 0.1, 100.0);
        let mut camera_uniform = camera::CameraUniform::new();
        camera_uniform.update_view_proj(camera_controller.camera(), &projection);

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
            camera_name,
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

    /// The viewport a scene gets when it declares none: full screen, user-driven.
    ///
    /// The HDF5 loader has no scene JSON at all and the JSON loader falls back to this when the
    /// `viewports` array is missing or empty, so this is the one camera whose behaviors cannot be
    /// declared anywhere. Free roam under user control is the only default that leaves someone
    /// able to look around a scene they have just opened.
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
            format!("{}0", DISAMBIGUATOR),
            camera::CameraController::new(
                camera,
                default_position_behavior(),
                default_rotation_behavior(),
            ),
            device,
            camera_bind_group_layout,
            ortho_matrix_bind_group_layout,
            cgmath::Vector3::<f32>::new(0.0, 0.0, 0.0),
            BorderAlignment::FullScreen,
        )
    }

    fn load_from_json(
        json: &serde_json::Value,
        index: usize,
        device: &Device,
        camera_bind_group_layout: &BindGroupLayout,
        ortho_matrix_bind_group_layout: &BindGroupLayout,
        registry: &ring_buffer::BufferRegistry,
    ) -> Self {
        let camera = {
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
        };

        let camera_name = normalize_camera_name(json["camera"]["name"].as_str().unwrap_or(""), index);

        // A behavior that will not load is reported and replaced rather than fatal. A scene file
        // is hand-written and a viewport is the thing you look through: refusing to open the
        // window over a typo in one camera's speed leaves nothing on screen to read the message
        // against, and the surviving camera is user-driven, which is the state someone can
        // recover from by hand.
        let position_behavior = PositionBehavior::from_json(&json["camera"]["position_behavior"], registry)
            .unwrap_or_else(|e| {
                warn!(
                    "camera {:?}: {}; falling back to a user-driven position — declare a \
                     \"position_behavior\" on this viewport", camera_name, e,
                );
                default_position_behavior()
            });

        let rotation_behavior = RotationBehavior::from_json(&json["camera"]["rotation_behavior"], registry)
            .unwrap_or_else(|e| {
                warn!(
                    "camera {:?}: {}; falling back to user-driven aim — declare a \
                     \"rotation_behavior\" on this viewport", camera_name, e,
                );
                default_rotation_behavior()
            });

        let mut camera_controller = camera::CameraController::new(camera, position_behavior, rotation_behavior);

        // Geometry the declared position and the behavior have to agree on — an orbit's axis and
        // radius. Only reachable after both exist, and a behavior the two cannot agree on is
        // dropped the same way an unparseable one is.
        if let Err(e) = camera_controller.prepare_position_behavior() {
            warn!("camera {:?}: {}; falling back to a user-driven position", camera_name, e);
            camera_controller.set_position_behavior(default_position_behavior());
        }

        Viewport::new(
            json["x"].as_f64().unwrap() as f32,
            json["y"].as_f64().unwrap() as f32,
            json["w"].as_f64().unwrap() as f32,
            json["h"].as_f64().unwrap() as f32,
            camera_name,
            camera_controller,
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
        )
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
            // Both dimensions. Height alone leaves the rect at whatever width the scene JSON
            // declared, so growing the window past that width leaves `set_viewport` and
            // `set_scissor_rect` covering only the left part of the framebuffer — everything to
            // the right of the original width is never drawn into, which reads as a hard vertical
            // line with the scene cut off beyond it.
            self.rect.width = screen_width;
            self.rect.height = screen_height;
        }
        if self.alignment == BorderAlignment::TopRight || self.alignment == BorderAlignment::BottomRight {
            self.rect.x = screen_width - self.rect.width;
        }

        if self.alignment == BorderAlignment::BottomLeft || self.alignment == BorderAlignment::BottomRight {
            self.rect.y = screen_height - self.rect.height;
        }

        self.projection.resize(self.rect.width, self.rect.height);
        self.aspect_ratio = self.rect.width / self.rect.height;

        // Rebuilt, not just re-uploaded. This matrix maps the viewport's own `0..width, 0..height`
        // space onto NDC, so a resize that changes the rect and reuses the old matrix leaves every
        // ortho-space draw — the background, the border, the progress bar — scaled for the
        // previous size.
        self.ortho_transform_matrix = cgmath::ortho(0.0, self.rect.width, self.rect.height, 0.0, -1.0, 1.0);

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

    /// Advances this viewport's camera by one frame.
    ///
    /// Takes the scene's entities because a camera can be aimed at, or held against, any of them —
    /// resolved fresh each frame, so a camera keeps working through entities joining and leaving.
    fn update_camera(
        &mut self,
        dt: std::time::Duration,
        queue: &Queue,
        entities: &[Entity],
    ){
        self.camera_controller.update_camera(dt, entities);
        self.camera_uniform.update_view_proj(&self.camera_controller.camera(), &self.projection);

        queue.write_buffer(
            &self.camera_buffer,
            0,
            bytemuck::cast_slice(&[self.camera_uniform]),
        );
    }
}

// Define the scene structure
/// Reserves the namespace a scene declares, returning the one its entities are addressed under.
///
/// Mirrors the authority claim: declared rather than derived, first declarer wins, and a second
/// claim is a misconfiguration worth surfacing rather than a tie to break. What happens to the
/// loser differs — authority is simply withheld, but a scene's entities have to stay addressable,
/// so a duplicate claim yields `fleet_a#1` rather than nothing.
///
/// A scene that declares nothing claims the empty namespace and is addressed by bare entity name,
/// so single-source runs and every scene written before this existed are unaffected. A *second*
/// undeclared scene cannot also do that without its roots colliding with the first's, so it is
/// given a synthesized namespace and told to declare one.
///
/// Deriving the namespace from the connection instead would be free of declarations, but the
/// network layer cannot supply one: the assembly thread never learns its own address, and the base
/// scene reaches the renderer through a file on disk rather than through a message. Riding along
/// in the scene JSON is what survives both paths.
fn claim_namespace(claimed: &mut HashSet<String>, json: &serde_json::Value, context: &str) -> String {
    let declared = json["namespace"].as_str().unwrap_or("");
    let declared = if declared.contains(PATH_SEPARATOR) {
        let substituted = declared.replace(PATH_SEPARATOR, "_");
        warn!(
            "namespace {:?} contains '{}', which separates path segments; {} is addressed under \
             {:?} instead",
            declared, PATH_SEPARATOR, context, substituted,
        );
        substituted
    } else {
        declared.to_string()
    };

    if claimed.insert(declared.clone()) {
        return declared;
    }

    let mut suffix = 1;
    let granted = loop {
        let candidate = format!("{}{}{}", declared, DISAMBIGUATOR, suffix);
        if claimed.insert(candidate.clone()) {
            break candidate;
        }
        suffix += 1;
    };

    if declared.is_empty() {
        warn!(
            "{} declares no namespace, and entities are already addressed by bare name; its own \
             are addressed under {:?} instead — set a top-level \"namespace\" to choose one",
            context, granted,
        );
    } else {
        warn!(
            "{} claims namespace {:?}, which another stream already holds; its entities are \
             addressed under {:?} instead",
            context, declared, granted,
        );
    }

    granted
}

pub struct Scene {
    pub axes: model::Axes,
    pub entities: Vec<Entity>,
    /// Namespaces already spoken for, so a second stream claiming one is caught rather than
    /// silently sharing addresses with the first. See `claim_namespace`.
    claimed_namespaces: HashSet<String>,
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
    /// Spawned on the first frame read back, so recording costs a fixed amount of memory rather
    /// than growing with run length. `None` until then, and again once `finish_capture` has run.
    encoder: Option<VideoEncoder>,
    capture_duration: std::time::Duration,
}

/// A running `ffmpeg` process being fed frames as they come off the GPU.
///
/// Frames are handed over one at a time and never collected, which is the whole point of this
/// type. Raw BGRA is ~8 MB a frame at 1080p, so buffering a run in memory and encoding at exit
/// costs gigabytes — 15 seconds at 60 fps is about 7 GB. Streaming into `ffmpeg` instead holds
/// resident memory at the `NUM_CAPTURE_BUFFERS` staging buffers no matter how long recording runs.
struct VideoEncoder {
    process: std::process::Child,
    stdin: std::process::ChildStdin,
    frames_written: usize,
}

impl VideoEncoder {
    /// Starts `ffmpeg` reading raw frames from stdin into `RAW_CAPTURE_PATH`.
    fn spawn(width: u32, height: u32) -> Self {
        let size_arg = format!("{}x{}", width, height);
        let rate_arg = format!("{:.3}", NOMINAL_CAPTURE_FPS);

        let mut process = Command::new("ffmpeg")
            .args(&[
                "-f", "rawvideo", //    input is raw video pixels
                "-pix_fmt", "bgra", //  BGRA format (wgpu surface on macOS is Bgra8Unorm)
                "-s", &size_arg, //     dimensions (actual capture resolution)
                "-r", &rate_arg, //     provisional; corrected by `retime` once the real rate is known
                "-i", "pipe:0", //      read input from stdin
                "-c:v", "libx264", //   specify video codex
                "-pix_fmt", "yuv420p",
                "-preset", "fast", // fast encoding preset
                "-movflags", "faststart", // optimize for streaming
                "-y", // overwrite output file
                RAW_CAPTURE_PATH,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("ffmpeg process failed to execute");

        let stdin = process.stdin.take().expect("failed to extract stdin from ffmpeg process");

        VideoEncoder { process, stdin, frames_written: 0 }
    }

    fn write_frame(&mut self, frame: &[u8]) {
        self.stdin.write_all(frame).expect("failed to write input to ffmpeg process");
        self.frames_written += 1;
    }

    /// Closes the encode and retimes the result to the rate frames actually arrived at.
    fn finish(self, measured_fps: f64) {
        let VideoEncoder { mut process, stdin, .. } = self;

        // Dropping stdin closes the pipe, which is what tells ffmpeg to flush and exit. Without
        // this the wait below blocks forever.
        drop(stdin);

        let status = process.wait().unwrap();

        if !status.success() {
            eprintln!("ffmpeg failed with status: {:?}", status);
            return;
        }

        VideoEncoder::retime(measured_fps);
    }

    /// Rewrites `RAW_CAPTURE_PATH` into `CAPTURE_OUTPUT_PATH` at the measured capture rate.
    ///
    /// The encode ran against `NOMINAL_CAPTURE_FPS` because `-r` is fixed at spawn time, so the
    /// timestamps in the raw file are wrong by whatever the two rates differ by. `-itsscale` with
    /// `-c copy` scales them without touching the encoded frames — a file copy rather than a
    /// re-encode.
    fn retime(measured_fps: f64) {
        let scale = NOMINAL_CAPTURE_FPS / measured_fps;

        // A rename is exact and free, so only pay for the remux when the rates actually differ.
        // Also the fallback when the measured rate is unusable, where scaling would be nonsense.
        if !measured_fps.is_finite() || measured_fps <= 0.0 || (scale - 1.0).abs() < 0.01 {
            match std::fs::rename(RAW_CAPTURE_PATH, CAPTURE_OUTPUT_PATH) {
                Ok(()) => println!("video successfully converted!"),
                Err(e) => eprintln!("failed to move {} to {}: {}", RAW_CAPTURE_PATH, CAPTURE_OUTPUT_PATH, e),
            }
            return;
        }

        let scale_arg = format!("{:.6}", scale);

        let status = Command::new("ffmpeg")
            .args(&[
                "-itsscale", &scale_arg, // stretch input timestamps to the real capture rate
                "-i", RAW_CAPTURE_PATH,
                "-c", "copy", //          retime the container only; do not re-encode
                "-movflags", "faststart",
                "-y",
                CAPTURE_OUTPUT_PATH,
            ])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .expect("ffmpeg retime process failed to execute");

        if status.success() {
            println!("video successfully converted! ({:.3} fps)", measured_fps);
            let _ = std::fs::remove_file(RAW_CAPTURE_PATH);
        } else {
            // Left in place deliberately — the frames are fine, only the timing is off, and a
            // playable file at the wrong rate beats deleting the whole capture.
            eprintln!(
                "ffmpeg retime failed with status: {:?}; raw capture left at {}",
                status, RAW_CAPTURE_PATH,
            );
        }
    }
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

        debug!("created Scene");

        Scene {
            axes,
            entities,
            // Filled in by the JSON loader after construction rather than threaded through this
            // constructor's fourteen parameters. The HDF5 path claims nothing: it has no scene
            // JSON to declare a namespace in, and no server that could address its entities.
            claimed_namespaces: HashSet::new(),
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
            encoder: None,
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

    /// Advances every camera by one frame.
    ///
    /// Lives here rather than in `state.rs` because a camera needs the entity list: iterating the
    /// viewports mutably while reading entities is only a legal borrow from inside `Scene`, where
    /// the two are separate fields rather than two paths through one `&mut`.
    pub fn update_cameras(&mut self, dt: std::time::Duration, queue: &Queue) {
        let entities = &self.entities;
        for viewport in &mut self.viewports {
            viewport.update_camera(dt, queue, entities);
        }
    }

    fn find_camera(&self, name: &str) -> Option<usize> {
        self.viewports.iter().position(|v| v.camera_name == name)
    }

    /// Acts on one of the master server's camera commands.
    ///
    /// Every failure is a warning and a dropped command, never a panic or a partial application: a
    /// command arrives from another machine over an unauthenticated protocol, and a scene that
    /// aborted on a malformed one would hand any misconfigured server a way to end the session.
    /// The log line is the only channel back — the wire is one-way, so the server is never told.
    pub fn apply_camera_command(
        &mut self,
        command: com::ServerCommand,
        payload: &str,
        registry: &ring_buffer::BufferRegistry,
    ) {
        let json: serde_json::Value = match serde_json::from_str(payload) {
            Ok(json) => json,
            Err(e) => {
                warn!("ignoring {:?}: its payload is not valid JSON ({})", command, e);
                return;
            }
        };

        let Some(name) = json["camera"].as_str() else {
            warn!("ignoring {:?}: its payload names no \"camera\"", command);
            return;
        };

        let Some(index) = self.find_camera(name) else {
            warn!(
                "ignoring {:?}: no camera named {:?} — this scene has {}",
                command, name, self.camera_names().join(", "),
            );
            return;
        };

        match command {
            com::ServerCommand::UPDATE_CAMERA_POSITION_BEHAVIOR => {
                self.update_camera_position_behavior(index, &json["behavior"], registry)
            }
            com::ServerCommand::UPDATE_CAMERA_ROTATION_BEHAVIOR => {
                self.update_camera_rotation_behavior(index, &json["behavior"], registry)
            }
            com::ServerCommand::UPDATE_CAMERA_POSITION => self.update_camera_position(index, &json),
            com::ServerCommand::UPDATE_CAMERA_ROTATION => self.update_camera_rotation(index, &json),
            other => warn!("{:?} is not a camera command", other),
        }
    }

    fn update_camera_position_behavior(
        &mut self,
        index: usize,
        behavior_json: &serde_json::Value,
        registry: &ring_buffer::BufferRegistry,
    ) {
        let name = self.viewports[index].camera_name.clone();

        let mut behavior = match PositionBehavior::from_json(behavior_json, registry) {
            Ok(behavior) => behavior,
            Err(e) => {
                warn!("ignoring a position behavior for camera {:?}: {}", name, e);
                return;
            }
        };

        // An entity that does not exist yet is a dropped command rather than a behavior that
        // starts working later. A command is a statement about the scene as it stands, and one
        // that silently waited would leave the server believing a camera had moved when it had
        // not — whereas a scene-declared behavior, which cannot know what has connected yet,
        // does resolve late.
        if let Some(path) = behavior.entity_path() {
            if behaviors_and_entities::world_position(&self.entities, path).is_none() {
                warn!(
                    "ignoring a position behavior for camera {:?}: no entity at {:?}",
                    name, path,
                );
                return;
            }
        }

        let controller = &mut self.viewports[index].camera_controller;
        let snap = match behavior.prepare(controller.camera().position) {
            Ok(snap) => snap,
            Err(e) => {
                warn!("ignoring a position behavior for camera {:?}: {}", name, e);
                return;
            }
        };

        let replaced = controller.set_position_behavior(behavior);
        if let Some(position) = snap {
            controller.set_position(position);
        }
        release_replaced_stream(
            replaced.stream_source(),
            controller.position_behavior().stream_source(),
            registry,
        );

        info!("camera {:?} position behavior is now {}", name, controller.position_behavior().display_name());
        self.warn_on_shared_stream_sources();
    }

    fn update_camera_rotation_behavior(
        &mut self,
        index: usize,
        behavior_json: &serde_json::Value,
        registry: &ring_buffer::BufferRegistry,
    ) {
        let name = self.viewports[index].camera_name.clone();

        let behavior = match RotationBehavior::from_json(behavior_json, registry) {
            Ok(behavior) => behavior,
            Err(e) => {
                warn!("ignoring a rotation behavior for camera {:?}: {}", name, e);
                return;
            }
        };

        if let Some(path) = behavior.entity_path() {
            if behaviors_and_entities::world_position(&self.entities, path).is_none() {
                warn!(
                    "ignoring a rotation behavior for camera {:?}: no entity at {:?}",
                    name, path,
                );
                return;
            }
        }

        let controller = &mut self.viewports[index].camera_controller;
        let replaced = controller.set_rotation_behavior(behavior);
        release_replaced_stream(
            replaced.stream_source(),
            controller.rotation_behavior().stream_source(),
            registry,
        );

        info!("camera {:?} rotation behavior is now {}", name, controller.rotation_behavior().display_name());
        self.warn_on_shared_stream_sources();
    }

    fn update_camera_position(&mut self, index: usize, json: &serde_json::Value) {
        let name = self.viewports[index].camera_name.clone();

        let Some(position) = read_point(json, "position") else {
            warn!("ignoring a position for camera {:?}: \"position\" must be three numbers", name);
            return;
        };

        let entities = &self.entities;
        let controller = &mut self.viewports[index].camera_controller;
        controller.set_position(position);

        // A tracking camera whose entity has since gone is left holding its old offset, which will
        // pull it off the commanded position on the next frame that entity exists again. Nothing
        // better is available — there is no offset to derive without the entity — so it is
        // reported rather than papered over.
        if let Err(e) = controller.reconcile_commanded_position(entities) {
            warn!("camera {:?} was moved, but its behavior could not be reconciled: {}", name, e);
        }

        info!("camera {:?} moved to {:?}", name, position);
    }

    fn update_camera_rotation(&mut self, index: usize, json: &serde_json::Value) {
        let name = self.viewports[index].camera_name.clone();

        let Some(values) = read_floats(json, "rotation", 4) else {
            warn!("ignoring a rotation for camera {:?}: \"rotation\" must be four numbers", name);
            return;
        };

        // Normalized rather than trusted: a quaternion that is not unit-length scales every vector
        // it rotates, so a slightly-off one from the wire would quietly warp the whole view.
        let rotation = Quaternion::new(values[0], values[1], values[2], values[3]);
        if !rotation.magnitude2().is_finite() || rotation.magnitude2() < 1e-8 {
            warn!("ignoring a rotation for camera {:?}: the quaternion has no magnitude", name);
            return;
        }

        let controller = &mut self.viewports[index].camera_controller;
        controller.set_rotation(rotation.normalize());
        controller.reconcile_commanded_rotation();

        info!("camera {:?} rotated", name);
    }

    /// Every camera a command can name.
    pub fn camera_names(&self) -> Vec<String> {
        self.viewports.iter().map(|v| v.camera_name.clone()).collect()
    }

    /// Logs a warning for every camera aimed at an entity path that names nothing.
    ///
    /// Run at load and after every merge, because that is the whole window in which a legitimate
    /// unresolved target exists: viewports are built before entities and before any other stream's
    /// scene has arrived, so a camera in the first scene may name a drone from the third. One that
    /// is still unresolved holds still and reports nothing on its own, which is indistinguishable
    /// from a camera that is simply not moving.
    fn warn_on_unresolved_camera_targets(&self) {
        for viewport in &self.viewports {
            for path in viewport.camera_controller.entity_paths() {
                if behaviors_and_entities::world_position(&self.entities, path).is_none() {
                    warn!(
                        "camera {:?} is aimed at {:?}, which no entity in the scene answers to; \
                         it will hold still until one does",
                        viewport.camera_name, path,
                    );
                }
            }
        }
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
            for (index, i) in viewport_temp.iter().enumerate() {
                viewport_vec.push(Viewport::load_from_json(i, index, &device, &camera_bind_group_layout, ortho_matrix_bind_group_layout, &registry));
            }
        }
        if viewport_vec.is_empty() {
            viewport_vec.push(Viewport::default(&device, &camera_bind_group_layout, ortho_matrix_bind_group_layout));
        }
        // One pass settles camera addresses for the run: only the first scene to arrive defines
        // viewports, so no later scene can add a camera that collides with these.
        let mut camera_names: Vec<String> = viewport_vec.iter().map(|v| v.camera_name.clone()).collect();
        disambiguate_camera_names(&mut camera_names);
        for (viewport, name) in viewport_vec.iter_mut().zip(camera_names) {
            viewport.camera_name = name;
        }

        let entity_temp: Vec<_> = json["entities"]
            .as_array()
            .unwrap()
            .into_iter()
            .collect();

        let mut claimed_namespaces = HashSet::new();
        let namespace = claim_namespace(&mut claimed_namespaces, &json, "the base scene");

        let mut entity_vec = vec![];
        for (index, i) in entity_temp.iter().enumerate() {
            let mut entity = Entity::load_from_json(*i, index, &device, &model_bind_group_layout, &registry);
            entity.set_namespace(namespace.clone());
            entity_vec.push(entity);
        }
        // Roots only need to be distinct within their own namespace, and every later scene gets a
        // namespace of its own, so one pass over this scene's roots settles them for the run.
        behaviors_and_entities::disambiguate_names(&mut entity_vec, "the base scene");

        let mut scene = Scene::new(
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

        scene.claimed_namespaces = claimed_namespaces;
        scene.warn_on_shared_stream_sources();
        scene.warn_on_unresolved_camera_targets();
        scene.log_addressable_paths();
        info!("addressable cameras: {}", scene.camera_names().join(", "));
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
                let frame = Scene::read_capture_buf(
                    device,
                    &self.capture_buffers,
                    width,
                    height,
                    index,
                ).expect("Error in State::update(); failed to capture screen data in buffer");
                // Straight out to ffmpeg rather than collected — see `VideoEncoder`. Spawned here
                // rather than in `Scene::new` because this is the first point the capture
                // resolution is known, and because a run that records nothing should not leave a
                // stray process or an empty file behind.
                self.encoder
                    .get_or_insert_with(|| VideoEncoder::spawn(width, height))
                    .write_frame(&frame);
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

        for viewport in self.viewports.iter() {
            for (source, half) in viewport.camera_controller.stream_sources() {
                consumers.push((source, format!("camera '{}' {}", viewport.camera_name, half)));
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

    /// Removes every entity and the buffers feeding them, leaving the view as it was.
    ///
    /// Takes the registry so the entity drop and the buffer purge happen together: they are one
    /// operation, and splitting them lets a replacement scene bind a name whose buffer is about to
    /// be dropped. See `ring_buffer::clear_registry` for why dropping entities alone is not enough.
    ///
    /// Deliberately left intact:
    /// - `viewports` and `terrain`. `base_scene_written` is one-way, so nothing can re-establish
    ///   either — dropping a viewport would cost a stream camera that no later scene could
    ///   restore, and a session is expected to hold the viewport count it started with.
    /// - `text_boxes` is HUD chrome, not scene content; `state.rs` indexes `text_boxes[0]` for the
    ///   framerate readout and would panic if it were emptied.
    /// - the frame and timestep counters, so a clear does not look like a restart to anything
    ///   reading data by index.
    ///
    /// Cameras keep whatever they were pointed at. An orbiting one holds its focus point through
    /// an `Rc` shared with the entity, so the point survives the entity being dropped and the
    /// camera stays put rather than snapping or panicking.
    pub fn clear(&mut self, registry: &ring_buffer::BufferRegistry) {
        self.entities.clear();
        // Released with the entities they addressed. A namespace is only a claim on names, and
        // there are no names left to collide with — so a fleet that reconnects after a clear gets
        // `fleet_a` back rather than being pushed to `fleet_a#1` by its own earlier self.
        self.claimed_namespaces.clear();
        ring_buffer::clear_registry(registry);
        // Entities are dropped above, so their buffer handles go with them. Cameras are not: every
        // stream-driven one outlives the purge still holding an `Arc`, and `com::write_to_registry`
        // looks entries up with `get`, so a camera left unbound would stall forever against a
        // buffer the registry no longer indexes. Must follow the purge, and must cover every
        // viewport rather than one — a stalled camera reports nothing.
        for viewport in &mut self.viewports {
            viewport.camera_controller.rebind_streams(registry);
        }
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
                    .filter(|v| {
                        v["camera"]["position_behavior"]["type"] == "Streamed"
                            || v["camera"]["rotation_behavior"]["type"] == "Streamed"
                    })
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

        let namespace = claim_namespace(&mut self.claimed_namespaces, &json, "a merged scene");
        let first_merged = self.entities.len();
        for (index, e) in entity_array.iter().enumerate() {
            let mut entity = Entity::load_from_json(e, index, &self.device, &self.model_bind_group_layout, registry);
            entity.set_namespace(namespace.clone());
            self.entities.push(entity);
        }
        // Scoped to what this scene contributed. Roots already in the scene are under a namespace
        // this one cannot have been granted, so they cannot collide with these and renaming them
        // would break addresses a server is already using.
        behaviors_and_entities::disambiguate_names(&mut self.entities[first_merged..], "a merged scene");

        // Re-run after the merge rather than only at load: a stream camera declared by the first
        // scene collides with an entity that only arrives now.
        self.warn_on_shared_stream_sources();
        // And re-run for the opposite reason: a camera aimed at an entity from another stream's
        // scene has been holding still until exactly this moment, so a target that is *still*
        // unresolved after a merge is worth repeating.
        self.warn_on_unresolved_camera_targets();
        self.log_addressable_paths();

        entity_array.len()
    }

    /// Every path a command can name, parents before their children.
    pub fn entity_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        for entity in &self.entities {
            entity.collect_paths("", &mut paths);
        }
        paths
    }

    /// Logs what is addressable, at debug: a ten-drone fleet is a hundred-odd paths, which is too
    /// much for a run's normal output but exactly what is wanted when a command misses.
    fn log_addressable_paths(&self) {
        let paths = self.entity_paths();
        debug!("{} addressable entities: {}", paths.len(), paths.join(", "));
    }

    /// Resolves a slash-separated path — `fleet_a/Drone_02/propeller RRT` — to the entity it names.
    ///
    /// Root paths are unique, so the root whose path prefixes this one settles which subtree to
    /// search; if the rest names nothing in that subtree the answer is `None`, never another
    /// root's child. Callers are expected to say so loudly: a command that names an entity which
    /// has been cleared, was never sent, or was renamed by a collision fails no other way.
    //
    // Nothing calls this yet — the command frame has no room for a target, so carrying one is the
    // next protocol change. It ships with the naming it resolves because a name that nothing can
    // resolve is not an addressing scheme.
    #[allow(dead_code)]
    pub fn find_entity_mut(&mut self, path: &str) -> Option<&mut Entity> {
        for root in &mut self.entities {
            let prefix = root.qualified_name();
            if path == prefix {
                return Some(root);
            }
            if let Some(rest) = path.strip_prefix(&prefix).and_then(|r| r.strip_prefix(PATH_SEPARATOR)) {
                return root.find_descendant_mut(rest);
            }
        }
        None
    }

    /// Flushes the staging buffers, closes the encode, and retimes the result.
    ///
    /// Taking the encoder up front makes this idempotent, which matters because the event loops
    /// reach it by two routes — the close request and capture completion — and a run can hit both.
    /// It also keeps the tail flush below from spawning a fresh encoder on a second call.
    pub fn finish_capture(&mut self, width: u32, height: u32) {
        let Some(mut encoder) = self.encoder.take() else {
            println!("no frames were captured; nothing to encode");
            return;
        };

        let device = self.device.clone();
        Scene::read_remaining_buffers(&device, &self.capture_buffers, &mut encoder, self.frame_counter, width, height);

        let frame_count = encoder.frames_written;
        println!("{} total frames recorded", frame_count);

        // Derive the average capture framerate from wall-clock time spent recording,
        // falling back to the nominal rate if we somehow have no timing data.
        let elapsed_secs = self.capture_duration.as_secs_f64();
        let fps = if frame_count > 0 && elapsed_secs > 0.0 {
            frame_count as f64 / elapsed_secs
        } else {
            NOMINAL_CAPTURE_FPS
        };

        encoder.finish(fps);
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

    /// Maps one staging buffer and returns its frame with the row padding stripped.
    fn read_capture_buf(device: &Device, capture_buffers: &Vec<wgpu::Buffer>, width: u32, height: u32, index: usize) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let padded_bytes_per_row = ((width * BYTES_PER_PIXEL + 255) / 256) * 256;

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

        // println!("read from buffer[{}]", index);

        Ok(pixels)
    }

    /// Drains the frames still sitting in the staging buffers when recording stops.
    ///
    /// Takes the encoder rather than reaching for `self.encoder` so it cannot spawn one; by the
    /// time this runs the encoder has been taken out of the scene and is on its way to being
    /// closed.
    fn read_remaining_buffers(device: &Device, capture_buffers: &Vec<wgpu::Buffer>, encoder: &mut VideoEncoder, frame_counter: usize, width: u32, height: u32){
        for i in 0..NUM_CAPTURE_BUFFERS {
            let index = (frame_counter + i) % NUM_CAPTURE_BUFFERS;
            let frame = Scene::read_capture_buf(device, capture_buffers, width, height, index).expect("problem with reading final few buffers");
            encoder.write_frame(&frame);
        }
    }

}
#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(raw: &[(&str, &str)]) -> Vec<(String, String)> {
        raw.iter().map(|(s, c)| (s.to_string(), c.to_string())).collect()
    }

    fn claim(claimed: &mut HashSet<String>, scene: &str) -> String {
        claim_namespace(claimed, &serde_json::from_str(scene).unwrap(), "a test scene")
    }

    #[test]
    fn a_declared_namespace_is_granted_as_written() {
        let mut claimed = HashSet::new();

        assert_eq!(claim(&mut claimed, r#"{"namespace": "fleet_a"}"#), "fleet_a");
        assert_eq!(claim(&mut claimed, r#"{"namespace": "fleet_b"}"#), "fleet_b");
    }

    // First declarer wins, as with authority — but the loser's entities still have to be
    // addressable, so it is moved aside rather than left sharing addresses with the winner.
    #[test]
    fn a_second_claim_on_one_namespace_is_moved_aside() {
        let mut claimed = HashSet::new();

        assert_eq!(claim(&mut claimed, r#"{"namespace": "fleet_a"}"#), "fleet_a");
        assert_eq!(claim(&mut claimed, r#"{"namespace": "fleet_a"}"#), "fleet_a#1");
        assert_eq!(claim(&mut claimed, r#"{"namespace": "fleet_a"}"#), "fleet_a#2");
    }

    // Every scene written before namespaces existed declares nothing, and a single-source run of
    // one must keep addressing entities by bare name.
    #[test]
    fn the_first_undeclared_scene_keeps_bare_names() {
        let mut claimed = HashSet::new();

        assert_eq!(claim(&mut claimed, r#"{"entities": []}"#), "");
    }

    // Two undeclared scenes cannot both hold the bare-name space without their roots colliding,
    // so the second is given one of its own.
    #[test]
    fn a_second_undeclared_scene_is_given_a_namespace() {
        let mut claimed = HashSet::new();

        assert_eq!(claim(&mut claimed, r#"{"entities": []}"#), "");
        assert_eq!(claim(&mut claimed, r#"{"entities": []}"#), "#1");
        assert_eq!(claim(&mut claimed, r#"{"entities": []}"#), "#2");
    }

    // A separator in the namespace would split into segments the author never meant, and could
    // shadow another stream's root path.
    #[test]
    fn a_namespace_containing_the_separator_is_substituted() {
        let mut claimed = HashSet::new();

        assert_eq!(claim(&mut claimed, r#"{"namespace": "fleet/a"}"#), "fleet_a");
    }

    // Anything that is not a JSON string is not a declaration, matching how `authority` treats
    // anything that is not a JSON bool.
    #[test]
    fn a_non_string_namespace_declares_nothing() {
        for scene in [r#"{"namespace": 7}"#, r#"{"namespace": null}"#, r#"{"namespace": true}"#] {
            let mut claimed = HashSet::new();
            assert_eq!(claim(&mut claimed, scene), "", "scene {} should not declare", scene);
        }
    }

    fn strings(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    // A camera nobody named still has to be addressable, and the index used is the one whoever
    // wrote the scene can predict: its position in that scene's own `viewports` array.
    #[test]
    fn an_unnamed_camera_is_addressed_by_its_viewport_index() {
        assert_eq!(normalize_camera_name("", 0), "#0");
        assert_eq!(normalize_camera_name("", 2), "#2");
        assert_eq!(normalize_camera_name("chase", 2), "chase", "a real name ignores the index");
    }

    // The first duplicate is suffixed too. Leaving it holding the bare name would let a command
    // aim an arbitrary member of the group, which is worse than aiming nothing.
    #[test]
    fn every_colliding_camera_is_suffixed_including_the_first() {
        let mut names = strings(&["chase", "main", "chase"]);

        disambiguate_camera_names(&mut names);

        assert_eq!(names, strings(&["chase#0", "main", "chase#1"]));
    }

    #[test]
    fn distinct_camera_names_are_left_alone() {
        let mut names = strings(&["main", "chase"]);

        disambiguate_camera_names(&mut names);

        assert_eq!(names, strings(&["main", "chase"]));
    }

    // A command payload arrives from another machine and is not to be trusted into an index.
    #[test]
    fn command_payload_vectors_are_validated_before_use() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"ok": [1.0, 2.0, 3.0], "short": [1.0], "wordy": ["x", "y", "z"], "flat": 3}"#,
        ).unwrap();

        assert_eq!(read_point(&json, "ok"), Some(Point3::new(1.0, 2.0, 3.0)));
        assert_eq!(read_point(&json, "short"), None);
        assert_eq!(read_point(&json, "wordy"), None);
        assert_eq!(read_point(&json, "flat"), None);
        assert_eq!(read_point(&json, "absent"), None);
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
