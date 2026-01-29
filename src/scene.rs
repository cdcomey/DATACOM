
use text::{TextDisplay};
use log::{debug, info};
use std::process::{Command, Stdio};
use std::io::Write;
use std::sync::Arc;
use cgmath::Matrix4;
use wgpu::util::DeviceExt;

use crate::{model, com, text, camera, behaviors_and_entities};
use behaviors_and_entities::Entity;
use model::DrawModel;

const BYTES_PER_PIXEL: u32 = 4;
const NUM_CAPTURE_BUFFERS: usize = 3;

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

pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    aspect_ratio: f32,
    pub camera_controller: camera::CameraController,
    projection: camera::Projection,
    camera_uniform: camera::CameraUniform,
    camera_buffer: wgpu::Buffer,
    pub camera_bind_group: wgpu::BindGroup,
    ortho_transform_matrix: cgmath::Matrix4<f32>,
    ortho_transform_buffer: wgpu::Buffer,
    pub ortho_matrix_bind_group: wgpu::BindGroup,
    vertex_buffer: wgpu::Buffer,
    color: cgmath::Vector3<f32>,
    identity_camera_bind_group: wgpu::BindGroup,
    alignment: BorderAlignment,
}

impl Viewport {
    const NUM_VERTICES: u32 = 8;
    const BORDER_BUFFER: f32 = 0.1; // the border needs to be drawn slightly inwards; if it is drawn right on the border (eg 0.0), it will be cut off
    const BACKGROUND_COLOR: cgmath::Vector3<f32> = cgmath::Vector3::<f32>::new(0.0, 0.0, 0.0);

    fn new(
        x: f32, 
        y: f32, 
        w: f32, 
        h: f32, 
        camera: camera::Camera, 
        device: &wgpu::Device, 
        camera_bind_group_layout: &wgpu::BindGroupLayout, 
        ortho_matrix_bind_group_layout: &wgpu::BindGroupLayout, 
        border_color: cgmath::Vector3<f32>, 
        alignment: BorderAlignment,
    ) -> Self {
        // println!("creating projection with aspect {}", w / h);
        let projection = camera::Projection::new(w, h, cgmath::Deg(45.0), 0.1, 100.0);
        let mut camera_uniform = camera::CameraUniform::new();
        camera_uniform.update_view_proj(&camera, &projection);
        let camera_controller = camera::CameraController::new(8.0, 0.4, camera);

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
        // for r in ortho_transform_arr {
        //     for cell in r {
        //         print!("{} ", cell);
        //     }
        //     println!();
        // }

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

        let corners = Viewport::get_border_corners(border_color);
        let border = vec![
            corners[0],
            corners[1],
            corners[1],
            corners[2],
            corners[2],
            corners[3],
            corners[3],
            corners[0],
        ];

        let border_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Border Buffer"),
            contents: bytemuck::cast_slice(&border),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let identity_camera_bind_group = Viewport::set_camera_binding(&device, &camera_bind_group_layout);

        Viewport {
            x,
            y,
            width: w,
            height: h,
            aspect_ratio: w/h,
            camera_controller,
            projection,
            camera_uniform,
            camera_buffer,
            camera_bind_group,
            ortho_transform_matrix,
            ortho_transform_buffer,
            ortho_matrix_bind_group,
            vertex_buffer: border_buffer,
            color: border_color,
            identity_camera_bind_group,
            alignment,
        }
    }

    fn load_from_json(
        json: &serde_json::Value, 
        device: &wgpu::Device, 
        camera_bind_group_layout: &wgpu::BindGroupLayout, 
        ortho_matrix_bind_group_layout: &wgpu::BindGroupLayout, 
    ) -> Self {
        Viewport::new(
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

    fn get_border_corners(color: cgmath::Vector3<f32>) -> Vec<model::ModelVertex> {
        let color_arr = [color.x, color.y, color.z];

        let xmin = Viewport::BORDER_BUFFER;
        let ymin = Viewport::BORDER_BUFFER;
        let xmax = 1600.0;
        let ymax = 1200.0;
        // println!("{}, {}, {}, {}", xmin, ymin, xmax, ymax);

        let corners = vec![
            // top left
            model::ModelVertex { position: [xmin, ymin, 0.0], color: color_arr },
            // bottom left
            model::ModelVertex { position: [xmin, ymax, 0.0], color: color_arr },
            // bottom right
            model::ModelVertex { position: [xmax, ymax, 0.0], color: color_arr },
            // top right
            model::ModelVertex { position: [xmax, ymin, 0.0], color: color_arr },
        ];

        corners
    }

    fn set_camera_binding(device: &wgpu::Device, camera_bind_group_layout: &wgpu::BindGroupLayout) -> wgpu::BindGroup {
        let identity_camera = camera::CameraUniform::new();

        let identity_camera_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Identity Camera Buffer"),
            contents: bytemuck::cast_slice(&[identity_camera]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: identity_camera_buffer.as_entire_binding(),
            }],
            label: Some("Border Bind Group"),
        })
    }

    pub fn resize_from_window(&mut self, screen_width: f32, screen_height: f32, queue: &wgpu::Queue){
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
            // println!("resizing full-screen vp to {}, {}, {}, {}", self.x, self.y, screen_width, screen_height);
            self.width = screen_width;
            self.height = screen_height;
        }
        if self.alignment == BorderAlignment::TopRight || self.alignment == BorderAlignment::BottomRight {
            // println!("right-aligned border");
            self.x = screen_width - self.width;
            // println!("new x = {}", self.x);
        }

        if self.alignment == BorderAlignment::BottomLeft || self.alignment == BorderAlignment::BottomRight {
            self.y = screen_height - self.height;
        }

        self.projection.resize(self.width, self.height);

        let ortho_transform_arr: [[f32; 4]; 4] = self.ortho_transform_matrix.into();
        queue.write_buffer(
            &self.ortho_transform_buffer, 
            0, 
            bytemuck::cast_slice(&ortho_transform_arr)
        );
    }

    pub fn draw_background_and_border<'a>(
        &'a self,
        device: &wgpu::Device,
        render_pass: &mut wgpu::RenderPass<'a>,
        lines_render_pipeline: &'a wgpu::RenderPipeline,
        rect_render_pipeline: &'a wgpu::RenderPipeline,
    ) {
        render_pass.set_pipeline(rect_render_pipeline);
        
        let corners = Viewport::get_border_corners(Viewport::BACKGROUND_COLOR);

        let border = vec![
            corners[0],
            corners[1],
            corners[2],
            corners[0],
            corners[2],
            corners[3],
        ];

        let bg_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Border Buffer"),
            contents: bytemuck::cast_slice(&border),
            usage: wgpu::BufferUsages::VERTEX,
        });

        render_pass.set_vertex_buffer(0, bg_buffer.slice(..));
        render_pass.set_bind_group(0, &self.identity_camera_bind_group, &[]);
        render_pass.set_bind_group(1, &self.ortho_matrix_bind_group, &[]);
        render_pass.draw(0..6, 0..1);

        render_pass.set_pipeline(lines_render_pipeline);
        render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        render_pass.draw(0..Viewport::NUM_VERTICES, 0..1);
    }

    pub fn update_camera(
        &mut self, 
        dt: std::time::Duration, 
        queue: &wgpu::Queue
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
    pub viewports: Vec<Viewport>,
    timesteps: Option<usize>,
    data_counter: Option<usize>,
    frame_counter: usize,
    capture_buffers: Vec<wgpu::Buffer>,
    screen_recordings: Vec<Vec<u8>>,
}

impl Scene {
    pub fn new(
        entities: Vec<Entity>, 
        timesteps: Option<usize>, 
        data_counter: Option<usize>, 
        terrain: model::Terrain,
        viewports: Vec<Viewport>,
        device: &wgpu::Device, 
        queue: &wgpu::Queue,
        format: &wgpu::TextureFormat,
        text_bind_group_layout: &wgpu::BindGroupLayout,
        screen_width: u32,
        screen_height: u32,
    ) -> Self {
        let axes = model::Axes::new(device);
        let text_boxes = Scene::init_text_boxes(device, queue, format, text_bind_group_layout, 60.0);
        let frame_counter: usize = 0;
        let capture_buffers = Scene::init_capture_buffers(
            device, 
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
            viewports,
            timesteps,
            data_counter,
            frame_counter,
            capture_buffers,
            screen_recordings,
        }
    }

    fn init_text_boxes(
        device: &wgpu::Device, 
        queue: &wgpu::Queue,
        format: &wgpu::TextureFormat,
        text_bind_group_layout: &wgpu::BindGroupLayout, 
        framerate: f32,
    ) -> Vec<TextDisplay> {
        let (image_atlas, glyph_map) = text::load_font_atlas(&text::get_font(), 100.0);
        let glyph_map = Arc::new(glyph_map);
        let texture_atlas = Arc::new(text::create_texture_atlas(device, queue, format, image_atlas));
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let text_objects: Vec<TextDisplay> = vec![
            TextDisplay::new(
                framerate.to_string(), 
                glyph_map.clone(), 
                30.0, 
                100.0, 
                cgmath::Vector3::new(0.0, 255.0, 0.0),
                device,
                &texture_atlas,
                &atlas_sampler,
                text_bind_group_layout,
            )
        ];

        // TextDisplay::new("Hello World!".to_string(), glyph_map.clone(), texture_atlas.clone(), 0.0, 0.0, green_vec()),
        // TextDisplay::new("DATACOM VER 0.1.0".to_string(), glyph_map.clone(), texture_atlas.clone(), -1.0, 0.90, green_vec()),
        // TextDisplay::new((' '..='~').collect(), glyph_map.clone(), texture_atlas.clone(), -1.0, -1.0, green_vec()),
        // TextDisplay::new("FPS Counter: 0.0".to_string(), glyph_map.clone(), texture_atlas.clone(), 0.6, 0.9, cyan_vec()),

        text_objects
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
        device: &wgpu::Device, 
        queue: &wgpu::Queue,
        format: &wgpu::TextureFormat, 
        model_bind_group_layout: &wgpu::BindGroupLayout, 
        text_bind_group_layout: &wgpu::BindGroupLayout,
        camera_bind_group_layout: &wgpu::BindGroupLayout,
        ortho_matrix_bind_group_layout: &wgpu::BindGroupLayout,
        screen_width: u32,
        screen_height: u32,
    ) -> Self {
        if filepath.ends_with(".hdf5"){
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
        } else if filepath.ends_with(".json"){
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
            )
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
            )
            // Scene::load_scene_from_network(
            //     filepath, 
            //     device, 
            //     queue, 
            //     format, 
            //     model_bind_group_layout, 
            //     text_bind_group_layout, 
            //     screen_width, 
            //     screen_height, 
            // ).unwrap()
        }
    }

    fn load_scene_from_hdf5(
        filepath: &str, 
        device: &wgpu::Device, 
        queue: &wgpu::Queue,
        format: &wgpu::TextureFormat, 
        model_bind_group_layout: &wgpu::BindGroupLayout, 
        text_bind_group_layout: &wgpu::BindGroupLayout,
        camera_bind_group_layout: &wgpu::BindGroupLayout,
        ortho_matrix_bind_group_layout: &wgpu::BindGroupLayout,
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
            let entity = Entity::load_from_hdf5(name, data, device, model_bind_group_layout).unwrap();
            entity_vec.push(entity);
        }

        let terrain = model::Terrain::new(serde_json::Value::Null, &device);

        let viewports = vec![];

        let num_entities = entity_vec.len();
        println!("LOADED {} ENTITIES INTO SCENE", num_entities);

        // set timesteps
        let timesteps = Scene::find_timesteps(&entity_vec);
        let data_counter = timesteps.map(|_| 0 as usize);


        Ok(Scene::new(
            entity_vec,
            timesteps,
            data_counter,
            terrain,
            viewports,
            device,
            queue,
            format,
            text_bind_group_layout,
            screen_width, 
            screen_height,
        ))
    }

    fn find_timesteps(entity_vec: &Vec<Entity>) -> Option<usize>{
        for entity in entity_vec.iter() {
            let timesteps = entity.find_timesteps();
            if let Some(_) = timesteps {
                return timesteps;
            }
        }

        println!("could only find constant behavior");
        None
    }

    fn load_scene_from_json(
        filepath: &str, 
        device: &wgpu::Device, 
        queue: &wgpu::Queue,
        format: &wgpu::TextureFormat, 
        model_bind_group_layout: &wgpu::BindGroupLayout, 
        text_bind_group_layout: &wgpu::BindGroupLayout,
        camera_bind_group_layout: &wgpu::BindGroupLayout,
        ortho_matrix_bind_group_layout: &wgpu::BindGroupLayout,
        screen_width: u32,
        screen_height: u32,
    ) -> Scene {
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
        )
    }

    fn load_scene_from_json_str(
        json_unparsed: String, 
        device: &wgpu::Device, 
        queue: &wgpu::Queue,
        format: &wgpu::TextureFormat, 
        model_bind_group_layout: &wgpu::BindGroupLayout, 
        text_bind_group_layout: &wgpu::BindGroupLayout,
        camera_bind_group_layout: &wgpu::BindGroupLayout,
        ortho_matrix_bind_group_layout: &wgpu::BindGroupLayout,
        screen_width: u32,
        screen_height: u32,
    ) -> Scene {
        let mut json: serde_json::Value = serde_json::from_str(&json_unparsed).unwrap();
        let timesteps = json["timesteps"].as_u64();
        let timesteps = timesteps.map(|e| e as usize);
        let data_counter = timesteps.map(|_| 0 as usize);
        let terrain = model::Terrain::new(json["terrain"].take(), device);

        let viewport_temp: Vec<_> = json["viewports"]
            .as_array()
            .unwrap()
            .into_iter()
            .collect();
        let mut viewport_vec = Vec::new();
        for i in viewport_temp.iter() {
            viewport_vec.push(Viewport::load_from_json(*i, device, camera_bind_group_layout, ortho_matrix_bind_group_layout));
        }

        let entity_temp: Vec<_> = json["entities"]
            .as_array()
            .unwrap()
            .into_iter()
            .collect();
        let mut entity_vec = vec![];
        for i in entity_temp.iter() {
            entity_vec.push(Entity::load_from_json(*i, device, model_bind_group_layout));
        }

        Scene::new(
            entity_vec,
            timesteps,
            data_counter,
            terrain,
            viewport_vec,
            device,
            queue,
            format,
            text_bind_group_layout,
            screen_width, 
            screen_height,
        )
    }

    // pub fn load_scene_from_network(
    //     addr: &str, 
    //     device: &wgpu::Device, 
    //     queue: &wgpu::Queue,
    //     format: &wgpu::TextureFormat, 
    //     model_bind_group_layout: &wgpu::BindGroupLayout, 
    //     text_bind_group_layout: &wgpu::BindGroupLayout, 
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
        model_render_pipeline: &'a wgpu::RenderPipeline,
        text_render_pipeline: &'a wgpu::RenderPipeline,
        terrain_render_pipeline: &'a wgpu::RenderPipeline,
        queue: &wgpu::Queue,
    ){
        render_pass.set_pipeline(terrain_render_pipeline);
        render_pass.draw_terrain(&self.terrain, camera_bind_group);

        render_pass.set_pipeline(model_render_pipeline);
        for entity in self.entities.iter() {
            entity.draw(render_pass, camera_bind_group, queue);
        }

        // println!("preparing to draw text");
        render_pass.set_pipeline(text_render_pipeline);
        for text_box in self.text_boxes.iter() {
            // println!("drawing text");
            text_box.draw(ortho_matrix_bind_group, render_pass);
        }
    }

    fn init_capture_buffers(device: &wgpu::Device, num_buffers: usize, size: wgpu::BufferAddress) -> Vec<wgpu::Buffer> {
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

    pub fn read_and_write_capture_buffers(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, offscreen_texture: &wgpu::Texture, width: u32, height: u32){
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
            
            if self.data_counter > self.timesteps {
                println!("sim finished; time to save");
                self.read_remaining_buffers(device, width, height);
                println!("{} total frames recorded", self.screen_recordings.len());
                Scene::save_screen_data_to_file(&self.screen_recordings);
                // send window close event instead of panicking
                panic!();
            }

    }
    
    fn write_screen_to_capture_buf(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture, capture_buffers: &mut Vec<wgpu::Buffer>, width: u32, height: u32, index: usize){
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

    fn read_capture_buf(device: &wgpu::Device, capture_buffers: &Vec<wgpu::Buffer>, width: u32, height: u32, index: usize) -> Result<Vec<Vec<u8>>, Box<dyn std::error::Error>> {
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

    fn read_remaining_buffers(&mut self, device: &wgpu::Device, width: u32, height: u32){
        for i in 0..NUM_CAPTURE_BUFFERS {
            let index = (self.frame_counter + i) % NUM_CAPTURE_BUFFERS;
            let mut saved_frame = Scene::read_capture_buf(device, &self.capture_buffers, width, height, index).expect("problem with reading final few buffers");
            self.screen_recordings.append(&mut saved_frame);
        }
    }

    fn save_screen_data_to_file(screen_data: &Vec<Vec<u8>>){

        let mut ffmpeg_process = Command::new("ffmpeg")
            .args(&[
                "-f", "rawvideo", //    input is raw video pixels
                "-pix_fmt", "rgba", //  RGBA format
                "-s", "1600x1200", //   dimensions
                "-r", "60", //          fps
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