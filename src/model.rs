use serde_derive::Deserialize;
use wgpu::util::DeviceExt;
use cgmath::{EuclideanSpace, InnerSpace, SquareMatrix};
use crate::camera;
use log::debug;

pub trait Vertex {
    fn desc() -> wgpu::VertexBufferLayout<'static>;
}

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ModelVertex {
    pub position: [f32; 3],
    pub color: [f32; 3],
}

impl Vertex for ModelVertex {
    fn desc() -> wgpu::VertexBufferLayout<'static> {
        use std::mem;
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<ModelVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    offset: mem::size_of::<[f32; 3]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x3,
                },
            ],
        }
    }
}

pub struct Mesh {
    #[allow(unused)]
    pub name: String,
    pub vertex_buffer: wgpu::Buffer,
    pub index_buffer: wgpu::Buffer,
    pub num_elements: u32,
}

pub fn load_mesh(
    file_name: &str,
    device: &wgpu::Device,
    color: cgmath::Vector3<f32>,
) -> anyhow::Result<Mesh> {
    debug!("Attempting to load mesh from {file_name}");
    let (models, _) = tobj::load_obj(
        file_name,
        &tobj::LoadOptions {
            triangulate: true,
            single_index: true,
            ..Default::default()
        },
    )?;


    let model = &models[0];
    let mesh = &model.mesh;

    let color_arr = [color[0], color[1], color[2]];
    let mut vertices = Vec::new();
    let positions = &mesh.positions;
    for i in 0..positions.len() / 3 {
        let position = [
            mesh.positions[i*3],
            mesh.positions[i*3 + 1],
            mesh.positions[i*3 + 2],
        ];

        vertices.push(ModelVertex {
            position: position,
            color: color_arr,
        });


    }

    let vertex_data = bytemuck::cast_slice(&vertices);
    let index_data = bytemuck::cast_slice(&mesh.indices);

    let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("Vertex Buffer"),
        contents: vertex_data,
        usage: wgpu::BufferUsages::VERTEX,
    });

    let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("Index Buffer"),
        contents: index_data,
        usage: wgpu::BufferUsages::INDEX,
    });

    Ok(Mesh {
        name: model.name.clone(),
        vertex_buffer,
        index_buffer,
        num_elements: mesh.indices.len() as u32,
    })
}

#[allow(dead_code)]
pub struct Model {
    pub name: String,
    pub obj: Mesh,
    pub position: cgmath::Point3<f32>,
    pub rotation: cgmath::Quaternion<f32>,
    pub color: cgmath::Vector3<f32>,
    pub uniform_buffer: wgpu::Buffer,
    pub bind_group: wgpu::BindGroup,
}

impl Model {
    pub fn new(
        name: &str, 
        filepath: &str, 
        device: &wgpu::Device,
        position: cgmath::Point3<f32>, 
        rotation: cgmath::Quaternion<f32>, 
        color: cgmath::Vector3<f32>,
        bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Model {
        let mesh = load_mesh(filepath, device, color)
        .expect("Failed to load mesh in Model::new()");

        let model_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Model Uniform Buffer"),
            size: std::mem::size_of::<[[f32; 4]; 4]>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let model_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: model_uniform_buffer.as_entire_binding(),
            }],
            label: Some("Model Bind Group"),
        });
        
        Model {
            name: name.to_string(),
            obj: mesh,
            position: position,
            rotation: rotation,
            color: color,
            uniform_buffer: model_uniform_buffer,
            bind_group: model_bind_group,
        }
    }

    pub fn load_from_json_file(filepath: &str, device: &wgpu::Device, model_bind_group_layout: &wgpu::BindGroupLayout) -> Vec<Model> {
        let json_unparsed = std::fs::read_to_string(filepath).unwrap();
        let json_string = json_unparsed.as_str();
        let json_parsed: serde_json::Value = serde_json::from_str(json_string).unwrap();
        
        match &json_parsed["Models"].as_array() {
            Some(array) => {
                let model_temp: Vec<_> = array.into_iter().collect();
                let mut model_vec = vec![];
                for i in model_temp.iter() {
                    if let Some(m) = Model::load_from_json(*i, device, model_bind_group_layout) {
                        model_vec.push(m);
                    }
                }
                model_vec
            }
            None => vec![],
        }
    }

    pub fn load_from_json(json: &serde_json::Value, device: &wgpu::Device, model_bind_group_layout: &wgpu::BindGroupLayout) -> Option<Model> {
        let name = json["Name"].as_str().unwrap();
        let filepath_raw = json["ObjectFilePath"].as_str().unwrap();
        if filepath_raw.is_empty() {
            return None;
        }
        let filepath_owned;
        let filepath = if filepath_raw.ends_with(".obj") {
            filepath_owned = format!("data/object_loading/{}", filepath_raw);
            filepath_owned.as_str()
        } else {
            filepath_raw
        };

        let position_arr: Vec<f32> = json["Position"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        let position_vec = cgmath::Point3::<f32>::new(position_arr[0], position_arr[1], position_arr[2]);

        let rotation_arr: Vec<f32> = json["Rotation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        let rotation_vec = cgmath::Quaternion::new(rotation_arr[0], rotation_arr[1], rotation_arr[2], rotation_arr[3]);

        let color_temp: Vec<_> = json["Color"]
            .as_array()
            .unwrap()
            .into_iter()
            .collect();
        let mut color_vec = cgmath::Vector3::<f32>::new(0.0, 0.0, 0.0);
        for (i, color_comp) in color_temp.iter().enumerate() {
            color_vec[i] = color_comp.as_f64().unwrap() as f32;
        }

        // debug!("NAME: {}", name);
        // debug!("POSITION: {}", position_vec);
        // debug!("ROTATION: {}", rotation_vec);
        // debug!("COLOR: {}", color_vec);

        Some(Model::new(
            name,
            filepath,
            device,
            position_vec,
            rotation_vec,
            color_vec,
            model_bind_group_layout,
        ))
    }

    pub fn to_matrix(&self) -> cgmath::Matrix4<f32> {
        let translation = cgmath::Matrix4::from_translation(self.position.to_vec());
        let rotation = cgmath::Matrix4::from(self.rotation);
        translation * rotation
    }

    pub fn rotate(&mut self, rotation: cgmath::Quaternion<f32>){
        self.rotation = (self.rotation * rotation).normalize();
    }
}

pub struct Axes {
    pub vertex_buffer: wgpu::Buffer,
    pub num_vertices: u32,
    pub bind_group: wgpu::BindGroup,
}

impl Axes {
    pub fn new(
        device: &wgpu::Device,
    ) -> Axes {
        let vertices = vec![
            // x-axis: red
            ModelVertex { position: [0.0, 0.0, 0.0], color: [1.0, 0.0, 0.0] },
            ModelVertex { position: [1.0, 0.0, 0.0], color: [1.0, 0.0, 0.0] },

            // y-axis: green
            ModelVertex { position: [0.0, 0.0, 0.0], color: [0.0, 1.0, 0.0] },
            ModelVertex { position: [0.0, 1.0, 0.0], color: [0.0, 1.0, 0.0] },

            // z-axis: blue
            ModelVertex { position: [0.0, 0.0, 0.0], color: [0.0, 0.0, 1.0] },
            ModelVertex { position: [0.0, 0.0, 1.0], color: [0.0, 0.0, 1.0] },
        ];

        let vertex_data: &[u8] = bytemuck::cast_slice(&vertices);
        let num_vertices = vertices.len() as u32;

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Vertex Buffer"),
            contents: vertex_data,
            usage: wgpu::BufferUsages::VERTEX,
        });

        let identity_matrix: [[f32; 4]; 4] = cgmath::Matrix4::<f32>::identity().into();
        let uniform_matrix: &[u8] = bytemuck::cast_slice(&identity_matrix);
        // println!("Vertex data: {:?}", vertex_data);
        // println!("Uniform matrix: {:?}", uniform_matrix);
        // println!("Number of vertices: {:?}", num_vertices);

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Uniform Buffer"),
            contents: uniform_matrix,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group_layout = 
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
                label: Some("Axes Bind Group Layout"),
        });

        let model_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
            label: Some("Model Bind Group"),
        });

        Axes{
            vertex_buffer,
            num_vertices,
            bind_group: model_bind_group,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct TerrainConfig {
    z_pos: f32,
    width: u32,
    color: [f32; 3],
}

fn default_terrain_z_pos() -> f32 { -3.0 }
fn default_terrain_width() -> u32 { 1000 }
fn default_terrain_color() -> [f32; 3] { [255.0, 0.0, 0.0] }

impl Default for TerrainConfig {
    fn default() -> Self {
        TerrainConfig {
            z_pos: default_terrain_z_pos(),
            width: default_terrain_width(),
            color: default_terrain_color(),
        }
    }
}

pub struct Terrain {
    // position: Rc<RefCell<Point3<f32>>>,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    num_indices: u32,
}

impl Terrain {
    // TODO: could optimize this by drawing only 2w lines, though this implementation is more flexible and probably doesn't add much to the computation
    pub fn new(json: serde_json::Value, device: &wgpu::Device) -> Self {
        // let z_pos: f32 = json["z_pos"].as_f64().unwrap() as f32;
        // let width: u32 = json["width"].as_u64().unwrap() as u32;
        // let color_temp: Vec<&serde_json::Value> = json["color"]
        //     .as_array()
        //     .unwrap()
        //     .into_iter()
        //     .collect();
        // let color_arr = [
        //     color_temp[0].as_f64().unwrap() as f32,
        //     color_temp[1].as_f64().unwrap() as f32,
        //     color_temp[2].as_f64().unwrap() as f32,
        // ];

        let config: TerrainConfig = serde_json::from_value(json).unwrap_or_default();
        log::debug!("Terrain Config = {}, {}, {:?}", config.z_pos, config.width, config.color);

        let mut vertices: Vec<ModelVertex> = vec![];
        let min: i32 = config.width as i32 / -2;
        let max = (config.width as i32 - 1) / 2;
        for i in min..max+1 {
            for j in min..max+1 {
                vertices.push(ModelVertex { position:[i as f32, j as f32, config.z_pos], color: config.color });
                // println!("adding vertex at ({}, {})", i, j);
            }
        }

        let mut indices: Vec<u32> = vec![];
        let num_vertices = vertices.len() as u32;

        let mut i: u32 = 0;
        while i+config.width+1 < num_vertices {
            let tl = i;
            let tr = i+1;
            let bl = i+config.width;
            let br = i+config.width+1;
            // println!("adding indices ({}, {}), ({}, {}), ({}, {}), ({}, {})", tl, tr, tr, br, br, bl, bl, tl);
            indices.extend_from_slice(&[tl, tr, tr, br, br, bl, bl, tl]);
            i += 1;
            if i % config.width == config.width-1 {
                i += 1;
            }
        }

        let num_indices = indices.len() as u32;

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Vertex Buffer"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Index Buffer"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        Terrain {
            vertex_buffer,
            index_buffer,
            num_indices,
        }
    }
}

pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    color: cgmath::Vector3<f32>,
    vertex_buffer: wgpu::Buffer,
    identity_camera_bind_group: wgpu::BindGroup,
}

impl Rect {
    const NUM_VERTICES: u32 = 8;
    const BORDER_BUFFER: f32 = 0.1;
    const BACKGROUND_COLOR: cgmath::Vector3<f32> = cgmath::Vector3::<f32>::new(0.0, 0.0, 0.0);

    pub fn new(
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: cgmath::Vector3<f32>,
        device: &wgpu::Device,
        camera_bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let corners = Rect::get_border_corners(x, y, w, h, color);
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

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Border Buffer"),
            contents: bytemuck::cast_slice(&border),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let identity_camera_bind_group = Rect::set_camera_binding(device, camera_bind_group_layout);

        Rect {
            x,
            y,
            width: w,
            height: h,
            color,
            vertex_buffer,
            identity_camera_bind_group,
        }
    }

    fn get_border_corners(x: f32, y: f32, w: f32, h: f32, color: cgmath::Vector3<f32>) -> Vec<ModelVertex> {
        let color_arr = [color.x, color.y, color.z];

        let xmin = x + Rect::BORDER_BUFFER;
        let ymin = y + Rect::BORDER_BUFFER;
        let xmax = x + w;
        let ymax = y + h;

        vec![
            ModelVertex { position: [xmin, ymin, 0.0], color: color_arr },
            ModelVertex { position: [xmin, ymax, 0.0], color: color_arr },
            ModelVertex { position: [xmax, ymax, 0.0], color: color_arr },
            ModelVertex { position: [xmax, ymin, 0.0], color: color_arr },
        ]
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

    pub fn draw_background_and_border<'a>(
        &'a self,
        device: &wgpu::Device,
        render_pass: &mut wgpu::RenderPass<'a>,
        lines_render_pipeline: &'a wgpu::RenderPipeline,
        rect_render_pipeline: &'a wgpu::RenderPipeline,
        ortho_matrix_bind_group: &'a wgpu::BindGroup,
    ) {
        render_pass.set_pipeline(rect_render_pipeline);

        let corners = Rect::get_border_corners(self.x, self.y, self.width, self.height, Rect::BACKGROUND_COLOR);
        let bg = vec![
            corners[0],
            corners[1],
            corners[2],
            corners[0],
            corners[2],
            corners[3],
        ];

        let bg_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Border Buffer"),
            contents: bytemuck::cast_slice(&bg),
            usage: wgpu::BufferUsages::VERTEX,
        });

        render_pass.set_vertex_buffer(0, bg_buffer.slice(..));
        render_pass.set_bind_group(0, &self.identity_camera_bind_group, &[]);
        render_pass.set_bind_group(1, ortho_matrix_bind_group, &[]);
        render_pass.draw(0..6, 0..1);

        render_pass.set_pipeline(lines_render_pipeline);
        render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        render_pass.draw(0..Rect::NUM_VERTICES, 0..1);
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct ProgressBarConfig {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    outline_color: [f32; 3],
    fill_color: [f32; 3],
}

fn default_progress_bar_x() -> f32 { 100.0 }
fn default_progress_bar_y() -> f32 { 985.0 }
fn default_progress_bar_width() -> f32 { 1400.0 }
fn default_progress_bar_height() -> f32 { 30.0 }
fn default_progress_bar_outline_color() -> [f32; 3] { [1.0, 1.0, 1.0] }
fn default_progress_bar_fill_color() -> [f32; 3] { [1.0, 0.0, 1.0] }

impl Default for ProgressBarConfig {
    fn default() -> Self {
        ProgressBarConfig {
            x: default_progress_bar_x(),
            y: default_progress_bar_y(),
            width: default_progress_bar_width(),
            height: default_progress_bar_height(),
            outline_color: default_progress_bar_outline_color(),
            fill_color: default_progress_bar_fill_color(),
        }
    }
}

pub struct ProgressBar {
    outline_rect: Rect,
    fill_rect: Rect,
    max_timesteps: usize,
    pub current_transform: cgmath::Matrix4<f32>,
}

impl ProgressBar {
    pub fn new(
        json: serde_json::Value,
        device: &wgpu::Device,
        camera_bind_group_layout: &wgpu::BindGroupLayout,
        max_timesteps: usize,
    ) -> Self {
        let config: ProgressBarConfig = serde_json::from_value(json).unwrap_or_default();
        let outline_color = cgmath::Vector3::new(config.outline_color[0], config.outline_color[1], config.outline_color[2]);
        let fill_color = cgmath::Vector3::new(config.fill_color[0], config.fill_color[1], config.fill_color[2]);

        ProgressBar {
            outline_rect: Rect::new(config.x, config.y, config.width, config.height, outline_color, device, camera_bind_group_layout),
            fill_rect: Rect::new(config.x, config.y, config.width, config.height, fill_color, device, camera_bind_group_layout),
            max_timesteps,
            current_transform: cgmath::Matrix4::from_nonuniform_scale(0.0, 1.0, 1.0),
        }
    }

    pub fn get_transform_matrix(&self, current_timestep: usize) -> cgmath::Matrix4<f32> {
        let scale = current_timestep as f32 / self.max_timesteps as f32;
        cgmath::Matrix4::from_nonuniform_scale(scale, 1.0, 1.0)
    }

    pub fn draw<'a>(
        &'a self,
        device: &wgpu::Device,
        render_pass: &mut wgpu::RenderPass<'a>,
        lines_render_pipeline: &'a wgpu::RenderPipeline,
        rect_render_pipeline: &'a wgpu::RenderPipeline,
        ortho_matrix_bind_group: &'a wgpu::BindGroup,
    ) {
        // debug!("drawing progress bar at ({}, {})", self.outline_rect.x, self.outline_rect.y);
        // Draw static outline over the full bar extent
        render_pass.set_pipeline(lines_render_pipeline);
        render_pass.set_vertex_buffer(0, self.outline_rect.vertex_buffer.slice(..));
        render_pass.set_bind_group(0, &self.outline_rect.identity_camera_bind_group, &[]);
        render_pass.set_bind_group(1, ortho_matrix_bind_group, &[]);
        render_pass.draw(0..Rect::NUM_VERTICES, 0..1);

        // Extract x-scale from the pre-computed transform (column 0, row 0)
        let scale = self.current_transform[0][0];
        let fill_width = self.fill_rect.width * scale;
        let color_arr = [self.fill_rect.color.x, self.fill_rect.color.y, self.fill_rect.color.z];

        let x  = self.fill_rect.x;
        let y  = self.fill_rect.y;
        let x2 = x + fill_width;
        let y2 = y + self.fill_rect.height;

        let fill_vertices = vec![
            ModelVertex { position: [x,  y,  0.0], color: color_arr },
            ModelVertex { position: [x,  y2, 0.0], color: color_arr },
            ModelVertex { position: [x2, y2, 0.0], color: color_arr },
            ModelVertex { position: [x,  y,  0.0], color: color_arr },
            ModelVertex { position: [x2, y2, 0.0], color: color_arr },
            ModelVertex { position: [x2, y,  0.0], color: color_arr },
        ];

        let fill_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Progress Fill Buffer"),
            contents: bytemuck::cast_slice(&fill_vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        render_pass.set_pipeline(rect_render_pipeline);
        render_pass.set_vertex_buffer(0, fill_buffer.slice(..));
        render_pass.set_bind_group(0, &self.fill_rect.identity_camera_bind_group, &[]);
        render_pass.set_bind_group(1, ortho_matrix_bind_group, &[]);
        render_pass.draw(0..6, 0..1);
    }
}

pub trait DrawModel<'a> {
    fn draw_mesh(
        &mut self,
        mesh: &'a Mesh,
        camera_bind_group: &'a wgpu::BindGroup,
        model_bind_group: &'a wgpu::BindGroup,
    );

    fn draw_axes(
        &mut self,
        axes: &'a Axes,
        camera_bind_group: &'a wgpu::BindGroup,
    );

    fn draw_terrain(
        &mut self,
        terrain: &'a Terrain,
        camera_bind_group: &wgpu::BindGroup,
    );
}

impl<'a, 'b> DrawModel<'b> for wgpu::RenderPass<'a>
where
    'b: 'a,
{
    fn draw_mesh(
        &mut self,
        mesh: &'b Mesh,
        camera_bind_group: &'b wgpu::BindGroup,
        model_bind_group: &'b wgpu::BindGroup,
    ) {
        self.set_vertex_buffer(0, mesh.vertex_buffer.slice(..));
        self.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        self.set_bind_group(0, camera_bind_group, &[]);
        self.set_bind_group(1, model_bind_group, &[]);
        self.draw_indexed(0..mesh.num_elements, 0, 0..1);
    }

    fn draw_axes(
        &mut self,
        axes: &'b Axes,
        camera_bind_group: &'b wgpu::BindGroup,
    ){
        self.set_vertex_buffer(0, axes.vertex_buffer.slice(..));
        self.set_bind_group(0, camera_bind_group, &[]);
        self.set_bind_group(1, &axes.bind_group, &[]);
        self.draw(0..axes.num_vertices, 0..1);
    }
    
    fn draw_terrain(
        &mut self,
        terrain: &'b Terrain, 
        camera_bind_group: &wgpu::BindGroup,
    ){
        self.set_vertex_buffer(0, terrain.vertex_buffer.slice(..));
        self.set_index_buffer(terrain.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        self.set_bind_group(0, camera_bind_group, &[]);
        self.draw_indexed(0..terrain.num_indices, 0, 0..1);
    }
}