
use cgmath::{Point3, Vector3, Quaternion, Matrix4};
use std::rc::Rc;
use std::cell::RefCell;
use std::path::Path;
use std::fs::OpenOptions;
use std::sync::{Arc, Mutex};
use log::{debug, info, error};
use cgmath::{EuclideanSpace, InnerSpace, SquareMatrix};
use ndarray::{ArrayBase, OwnedRepr, Dim};

use crate::ring_buffer;

use wgpu::util::DeviceExt;

use crate::model;

use model::DrawModel;

pub const DATA_ARR_WIDTH: usize = 12;
const AVERAGE_REFRESH_RATE: usize = 16;
const F32_SIZE: usize = std::mem::size_of::<f32>();
const CHUNK_LENGTH: u64 = 1024;
const MAX_TRAIL_LENGTH: usize = 500;

pub fn create_and_clear_file(file_name: &str) {
    let path = Path::new(file_name);
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .unwrap();
    debug!("clearing {file_name}");
}

#[derive(Debug, Copy, Clone)]
pub enum BehaviorType {
    Rotate,
    Translate,
    ChangeTransform,
    RotateConstantSpeed,
    ChangeColor,
    Null,
}

impl BehaviorType {
    pub fn match_from_string(input_string: &str) -> BehaviorType {
        match input_string {
            "Rotate" => BehaviorType::Rotate,
            "Translate" => BehaviorType::Translate,
            "ChangeTransform" => BehaviorType::ChangeTransform,
            "RotateConstantSpeed" => BehaviorType::RotateConstantSpeed,
            "ChangeColor" => BehaviorType::ChangeColor,
            _ => BehaviorType::Null,
        }
    }

    fn is_constant_behavior(behavior_type: BehaviorType) -> bool {
        match behavior_type {
            BehaviorType::Translate => true,
            BehaviorType::Rotate => true,
            BehaviorType::RotateConstantSpeed => true,
            _ => false,
        }
    }
}

pub struct Behavior {
    pub behavior_type: BehaviorType,
    pub data: Vec<f32>,
    pub is_constant_behavior: bool,
    data_buffer: Option<ring_buffer::SharedBuffer>,
}

impl Behavior {
    pub fn new(behavior_type: BehaviorType, data: Vec<f32>, data_buffer: Option<ring_buffer::SharedBuffer>) -> Behavior {
        let is_constant_behavior = BehaviorType::is_constant_behavior(behavior_type);
        Behavior {
            behavior_type,
            data,
            is_constant_behavior,
            data_buffer,
        }
    }

    pub fn load_from_json(json: &serde_json::Value, registry: &ring_buffer::BufferRegistry) -> Behavior {
        let behavior_type: BehaviorType =
            BehaviorType::match_from_string(json["behaviorType"].as_str().unwrap());
        let mut data_temp: Vec<_> = json["data"]
            .as_array()
            .unwrap()
            .into_iter()
            .collect();
        let mut data: Vec<f32> = vec![];
        let data_buffer = if !BehaviorType::is_constant_behavior(behavior_type) {
            let raw = data_temp.remove(0).to_string();
            let name = raw[1..raw.len()-1].to_string();
            if name.ends_with(".hdf5") {
                let file = hdf5::File::open(&name).unwrap();
                let dataset = file.dataset("states").unwrap();
                let data_array: ndarray::Array2<f32> = dataset.read_2d().unwrap();
                for row in data_array.rows() {
                    data.extend(row.iter().copied());
                }
                None
            } else {
                let buf = registry.lock().unwrap()
                    .entry(name)
                    .or_insert_with(|| Arc::new(Mutex::new(ring_buffer::RingBuffer::new())))
                    .clone();
                Some(buf)
            }
        } else {
            None
        };

        for data_point in data_temp.iter() {
            data.push(data_point.as_f64().unwrap() as f32);
        }

        Behavior::new(behavior_type, data, data_buffer)
    }

    pub fn load_from_hdf5(data: &ArrayBase<OwnedRepr<[f32; 12]>, Dim<[usize; 1]>>) -> hdf5::Result<Behavior> {
        let behavior_type = BehaviorType::ChangeTransform;
        let a = 0;
        let b = DATA_ARR_WIDTH;
        let data_vec: Vec<f32> = data
            .iter()
            .flat_map(|arrs| arrs[a..b].iter().cloned())
            .collect();

        Ok(Behavior::new(behavior_type, data_vec, None))
    }

    pub fn is_exhausted(&self) -> bool {
        let buffer_empty = self.data_buffer.as_ref()
            .map_or(true, |buf| buf.lock().unwrap().is_empty());
        buffer_empty && self.data.len() < DATA_ARR_WIDTH
    }
}

#[allow(dead_code)]
pub struct Entity {
    name: String,
    position: Rc<RefCell<Point3<f32>>>,
    rotation: Quaternion<f32>,
    scale: Vector3<f32>,
    pub model: Option<model::Model>,
    behavior: Option<Behavior>,
    pub children: Vec<Entity>,
    trail: Vec<Point3<f32>>,
    trail_bind_group: Option<wgpu::BindGroup>,
    trail_uniform_buffer: Option<wgpu::Buffer>,
}

fn has_movement_behavior(behavior: &Option<Behavior>) -> bool {
    behavior.as_ref().map_or(false, |b| matches!(b.behavior_type,
        BehaviorType::Translate | BehaviorType::ChangeTransform
    ))
}

fn create_trail_resources(device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> (wgpu::Buffer, wgpu::BindGroup) {
    let identity: [[f32; 4]; 4] = Matrix4::identity().into();
    let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("Trail Uniform Buffer"),
        contents: bytemuck::cast_slice(&[identity]),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        layout,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }],
        label: Some("Trail Bind Group"),
    });
    (buffer, bind_group)
}

impl Entity {
    fn new_root(
        name: String,
        position: Point3<f32>,
        rotation: Quaternion<f32>,
        scale: Vector3<f32>,
        behavior: Option<Behavior>,
        children: Vec<Entity>,
        device: &wgpu::Device,
        model_bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let (trail_uniform_buffer, trail_bind_group) = if has_movement_behavior(&behavior) {
            let (buf, bg) = create_trail_resources(device, model_bind_group_layout);
            (Some(buf), Some(bg))
        } else {
            (None, None)
        };
        Entity {
            name,
            position: Rc::new(RefCell::new(position)),
            rotation,
            scale,
            model: None,
            behavior,
            children,
            trail: Vec::new(),
            trail_bind_group,
            trail_uniform_buffer,
        }
    }

    fn new_child(
        name: String,
        position: Point3<f32>,
        rotation: Quaternion<f32>,
        model: Option<model::Model>,
        behavior: Option<Behavior>,
    ) -> Self {
        Entity {
            name,
            position: Rc::new(RefCell::new(position)),
            rotation,
            scale: Vector3::new(1.0, 1.0, 1.0),
            model,
            behavior,
            children: Vec::new(),
            trail: Vec::new(),
            trail_bind_group: None,
            trail_uniform_buffer: None,
        }
    }

    pub fn load_from_json(json: &serde_json::Value, device: &wgpu::Device, model_bind_group_layout: &wgpu::BindGroupLayout, registry: &ring_buffer::BufferRegistry) -> Entity {
        let name = json["Name"].to_string();
        debug!("loading entity {name}");

        let position_arr: Vec<f32> = json["Position"].as_array().unwrap()
            .iter().map(|v| v.as_f64().unwrap() as f32).collect();
        let position = Point3::new(position_arr[0], position_arr[1], position_arr[2]);

        let rotation_arr: Vec<f32> = json["Rotation"].as_array().unwrap()
            .iter().map(|v| v.as_f64().unwrap() as f32).collect();
        let rotation = Quaternion::new(rotation_arr[0], rotation_arr[1], rotation_arr[2], rotation_arr[3]);

        let scale_arr: Vec<f32> = json["Scale"].as_array().unwrap()
            .iter().map(|v| v.as_f64().unwrap() as f32).collect();
        let scale = Vector3::new(scale_arr[0], scale_arr[1], scale_arr[2]);

        let entity_behavior = json["Behavior"].as_object()
            .map(|_| Behavior::load_from_json(&json["Behavior"], registry));

        let children: Vec<Entity> = match json["Children"].as_array() {
            Some(array) => array.iter().map(|c| {
                let child_name = c["Name"].as_str().unwrap_or("").to_string();

                let child_pos_arr: Vec<f32> = c["Position"].as_array().unwrap()
                    .iter().map(|v| v.as_f64().unwrap() as f32).collect();
                let child_pos = Point3::new(child_pos_arr[0], child_pos_arr[1], child_pos_arr[2]);

                let child_rot_arr: Vec<f32> = c["Rotation"].as_array().unwrap()
                    .iter().map(|v| v.as_f64().unwrap() as f32).collect();
                let child_rot = Quaternion::new(child_rot_arr[0], child_rot_arr[1], child_rot_arr[2], child_rot_arr[3]);

                let child_model = c["ObjectFilePath"].as_str().filter(|p| !p.is_empty()).map(|raw| {
                    let filepath = if raw.ends_with(".obj") {
                        format!("data/object_loading/{}", raw)
                    } else {
                        raw.to_string()
                    };
                    let color_arr: Vec<f32> = c["Color"].as_array().unwrap()
                        .iter().map(|v| v.as_f64().unwrap() as f32).collect();
                    let color = cgmath::Vector3::new(color_arr[0], color_arr[1], color_arr[2]);
                    model::Model::new(&child_name, &filepath, device, color, model_bind_group_layout)
                });

                let child_behavior = c["Behavior"].as_object()
                    .map(|_| Behavior::load_from_json(&c["Behavior"], registry));

                Entity::new_child(child_name, child_pos, child_rot, child_model, child_behavior)
            }).collect(),
            None => vec![],
        };

        Entity::new_root(name, position, rotation, scale, entity_behavior, children, device, model_bind_group_layout)
    }

    pub fn load_from_hdf5(name: String, data: hdf5::Dataset, device: &wgpu::Device, model_bind_group_layout: &wgpu::BindGroupLayout) -> hdf5::Result<Entity> {
        println!("NAME: {}", name);

        let data_array: ArrayBase<OwnedRepr<[f32; 12]>, Dim<[usize; 1]>> = data.read()?;
        let initial_transform: [f32; 12] = data_array[0];
        let position = Point3::new(initial_transform[0], initial_transform[1], initial_transform[2]);
        println!("POSITION: {:?}", position);

        let rotation_vec = Vector3::new(initial_transform[7], initial_transform[6], initial_transform[8]);
        println!("ROTATION: {:?}", rotation_vec);
        let rotation = Quaternion::from_sv(
            (1.0 - rotation_vec.magnitude2()).max(0.0).sqrt(),
            rotation_vec,
        );

        let scale = Vector3::new(1.0, 1.0, 1.0);

        let mut name_root = name.clone();
        if let Some(val) = name_root.find('_') { name_root.truncate(val) }
        println!("NAME STR: {}", name_root);

        let children: Vec<Entity> = match name_root.as_str() {
            "Blizzard" => {
                model::Model::load_from_json_file("data/object_loading/blizzard_initialize_full.json", device, model_bind_group_layout)
                    .into_iter()
                    .map(|(m, pos, rot)| Entity::new_child(m.name.clone(), pos, rot, Some(m), None))
                    .collect()
            }
            _ => vec![],
        };

        let behavior = match name_root.as_str() {
            "Blizzard" => Some(Behavior::load_from_hdf5(&data_array).unwrap()),
            _ => None,
        };

        Ok(Entity::new_root(name, position, rotation, scale, behavior, children, device, model_bind_group_layout))
    }

    pub fn get_position(&self) -> Rc<RefCell<Point3<f32>>> { Rc::clone(&self.position) }

    pub fn has_movement_behavior(&self) -> bool { has_movement_behavior(&self.behavior) }

    pub fn set_position(&mut self, new_position: Point3<f32>) {
        if self.trail_bind_group.is_some() {
            self.trail.push(*self.position.borrow());
            if self.trail.len() > MAX_TRAIL_LENGTH {
                self.trail.remove(0);
            }
        }
        *self.position.borrow_mut() = new_position;
        debug!("new entity transform is {:?}, {:?}, {:?}", *self.position.borrow(), self.rotation, self.scale);
    }

    pub fn draw_trail<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        device: &wgpu::Device,
    ) {
        let Some(ref trail_bg) = self.trail_bind_group else { return };
        if self.trail.len() < 2 { return }

        let color = self.children.first()
            .and_then(|c| c.model.as_ref())
            .map_or([1.0f32, 1.0, 1.0], |m| [m.color.x, m.color.y, m.color.z]);

        let mut verts: Vec<model::ModelVertex> = Vec::with_capacity((self.trail.len() - 1) * 2);
        for w in self.trail.windows(2) {
            verts.push(model::ModelVertex { position: [w[0].x, w[0].y, w[0].z], color });
            verts.push(model::ModelVertex { position: [w[1].x, w[1].y, w[1].z], color });
        }

        let trail_vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Trail Vertex Buffer"),
            contents: bytemuck::cast_slice(&verts),
            usage: wgpu::BufferUsages::VERTEX,
        });

        render_pass.set_vertex_buffer(0, trail_vbuf.slice(..));
        render_pass.set_bind_group(0, camera_bind_group, &[]);
        render_pass.set_bind_group(1, trail_bg, &[]);
        render_pass.draw(0..verts.len() as u32, 0..1);
    }

    fn to_matrix(&self) -> Matrix4<f32> {
        let translation = Matrix4::from_translation(self.position.borrow().to_vec());
        let rotation = Matrix4::from(self.rotation);
        let scale = Matrix4::from_nonuniform_scale(self.scale.x, self.scale.y, self.scale.z);
        translation * rotation * scale
    }

    pub fn find_timesteps(&self) -> Option<usize> {
        if let Some(ref b) = self.behavior {
            if !b.is_constant_behavior && !b.data.is_empty() {
                return Some(b.data.len());
            }
        }
        for child in &self.children {
            if let Some(ts) = child.find_timesteps() {
                return Some(ts);
            }
        }
        None
    }

    pub fn draw<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        queue: &wgpu::Queue,
    ) {
        self.draw_with_parent_matrix(render_pass, camera_bind_group, queue, Matrix4::identity());
    }

    fn draw_with_parent_matrix<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        camera_bind_group: &'a wgpu::BindGroup,
        queue: &wgpu::Queue,
        parent_matrix: Matrix4<f32>,
    ) {
        let own_matrix = parent_matrix * self.to_matrix();
        if let Some(ref model) = self.model {
            let uniform: [[f32; 4]; 4] = own_matrix.into();
            queue.write_buffer(&model.uniform_buffer, 0, bytemuck::cast_slice(&[uniform]));
            render_pass.draw_mesh(&model.obj, camera_bind_group, &model.bind_group);
        }
        for child in &self.children {
            child.draw_with_parent_matrix(render_pass, camera_bind_group, queue, own_matrix);
        }
    }

    fn run_own_behavior(&mut self, data_counter: Option<usize>) {
        let Some(behavior_type) = self.behavior.as_ref().map(|b| b.behavior_type) else { return };

        match behavior_type {
            BehaviorType::Translate => {
                let old_position = *self.position.borrow();
                let data = &self.behavior.as_ref().unwrap().data;
                let offset = Vector3::new(data[0], data[1], data[2]);
                self.set_position(old_position + offset);
            }

            BehaviorType::Rotate | BehaviorType::RotateConstantSpeed => {
                let data = &self.behavior.as_ref().unwrap().data;
                let rotation_factor = data[0];
                let v = Vector3::new(
                    rotation_factor * data[1],
                    rotation_factor * data[3],
                    rotation_factor * data[2],
                );
                self.rotation = (self.rotation * Quaternion::from_sv(1.0, v)).normalize();
            }

            BehaviorType::ChangeTransform => {
                let behavior = self.behavior.as_mut().unwrap();

                let frame: Option<[f32; DATA_ARR_WIDTH]> = if let Some(ref buf) = behavior.data_buffer {
                    let bytes = buf.lock().unwrap().read(DATA_ARR_WIDTH * F32_SIZE);
                    if bytes.len() == DATA_ARR_WIDTH * F32_SIZE {
                        let mut arr = [0f32; DATA_ARR_WIDTH];
                        for i in 0..DATA_ARR_WIDTH {
                            arr[i] = f32::from_be_bytes(bytes[i*F32_SIZE..(i+1)*F32_SIZE].try_into().unwrap());
                        }
                        Some(arr)
                    } else {
                        None
                    }
                } else if behavior.data.len() >= DATA_ARR_WIDTH {
                    let mut arr = [0f32; DATA_ARR_WIDTH];
                    arr.copy_from_slice(&behavior.data[..DATA_ARR_WIDTH]);
                    behavior.data.drain(0..DATA_ARR_WIDTH);
                    Some(arr)
                } else {
                    None
                };

                if let Some(data) = frame {
                    debug!("reading transform: x:{} y:{} z:{} v6:{} v7:{} v8:{}",
                        data[0], data[1], data[2], data[6], data[7], data[8]);
                    let new_position = Point3::new(data[0], data[1], data[2]);
                    let rot_vec = Vector3::new(data[6], data[7], data[8]);
                    let w = (1.0 - rot_vec.magnitude2()).max(0.0).sqrt();
                    self.rotation = Quaternion::from_sv(w, rot_vec);
                    self.set_position(new_position);
                } else {
                    let p = *self.position.borrow();
                    debug!("out of data, stalling at ({}, {}, {})", p.x, p.y, p.z);
                }
            }

            _ => {}
        }
    }

    pub fn run_behaviors(&mut self, data_counter: Option<usize>) {
        self.run_own_behavior(data_counter);
        for child in &mut self.children {
            child.run_own_behavior(data_counter);
        }
    }

    pub fn all_streams_exhausted(&self) -> bool {
        let own = self.behavior.as_ref().map_or(true, |b| b.is_exhausted());
        own && self.children.iter().all(|c| c.all_streams_exhausted())
    }
}