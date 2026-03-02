
use cgmath::{Point3, Vector3, Quaternion, Matrix4};
use std::rc::Rc;
use std::cell::RefCell;
use std::path::Path;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek};
use log::{debug, info, error};
use cgmath::{EuclideanSpace, InnerSpace};
use ndarray::{ArrayBase, OwnedRepr, Dim};
use std::io::Write;

use crate::model;

use model::DrawModel;

pub const DATA_ARR_WIDTH: usize = 12;
const AVERAGE_REFRESH_RATE: usize = 16;
const F32_SIZE: usize = std::mem::size_of::<f32>();
const CHUNK_LENGTH: u64 = 1024;

pub fn create_and_clear_file(file_name: &str) {
    let path = Path::new(file_name);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .unwrap();
    debug!("clearing {file_name}");
    writeln!(file, "").unwrap();
}

#[derive(Debug, Copy, Clone)]
pub enum BehaviorType {
    EntityRotate,
    EntityTranslate,
    EntityChangeTransform,
    ComponentRotate,
    ComponentTranslate,
    ComponentRotateConstantSpeed,
    ComponentChangeColor,
    Null,
}

impl BehaviorType {
    pub fn match_from_string(input_string: &str) -> BehaviorType {
        match input_string {
            "EntityRotate" => BehaviorType::EntityRotate,
            "EntityTranslate" => BehaviorType::EntityTranslate,
            "EntityChangeTransform" => BehaviorType::EntityChangeTransform,
            "ComponentRotate" => BehaviorType::ComponentRotate,
            "ComponentTranslate" => BehaviorType::ComponentTranslate,
            "ComponentRotateConstantSpeed" => BehaviorType::ComponentRotateConstantSpeed,
            "ComponentChangeColor" => BehaviorType::ComponentChangeColor,
            _ => BehaviorType::Null,
        }
    }

    fn is_constant_behavior(behavior_type: BehaviorType) -> bool {
        match behavior_type {
            BehaviorType::EntityTranslate => true,
            BehaviorType::EntityRotate => true,
            BehaviorType::ComponentRotateConstantSpeed => true,
            _ => false,
        }
    }
}

pub struct Behavior {
    pub behavior_type: BehaviorType,
    pub data: Vec<f32>,
    pub is_constant_behavior: bool,
    data_file_path: Option<String>,
}

impl Behavior {
    pub fn new(behavior_type: BehaviorType, data: Vec<f32>, data_file_path: Option<String>) -> Behavior {
        let is_constant_behavior = BehaviorType::is_constant_behavior(behavior_type);
        // debug!("data in Behavior constructor of type {:?} = {:?}", behavior_type, data);
        Behavior {
            behavior_type,
            data,
            is_constant_behavior,
            data_file_path, 
        }
    }
    pub fn load_from_json(json: &serde_json::Value) -> Behavior {
        let behavior_type: BehaviorType =
            BehaviorType::match_from_string(json["behaviorType"].as_str().unwrap());
        let mut data_temp: Vec<_> = json["data"]
            .as_array()
            .unwrap()
            .into_iter()
            .collect();
        let mut data: Vec<f32> = vec![];
        let data_file_path = if !BehaviorType::is_constant_behavior(behavior_type) {
            let mut path = data_temp.remove(0).to_string();
            path = path[1..path.len()-1].to_string();
            path.insert_str(0, "data/scene_loading/");
            create_and_clear_file(path.as_str());
            debug!("cropped path to {}", path);
            Some(path)
        } else {
            None
        };

        for data_point in data_temp.iter() {
            data.push(data_point.as_f64().unwrap() as f32);
        }

        Behavior::new(behavior_type, data, data_file_path)
    }

    pub fn load_from_hdf5(data: &ArrayBase<OwnedRepr<[f32; 12]>, Dim<[usize; 1]>>) -> hdf5::Result<Behavior> {
        let behavior_type = BehaviorType::EntityChangeTransform;
        let a = 0;
        let b = DATA_ARR_WIDTH;
        let data_vec: Vec<f32> = data
            .iter()
            .flat_map(|arrs| arrs[a..b].iter().cloned())
            .collect();

        Ok(Behavior::new(behavior_type, data_vec, None))
    }

    fn retrieve_data_chunk(&mut self) {
    // fn retrieve_data_chunk(behavior: &mut Behavior, target_file_path_str: &String){
        if let Some(path_str) = &self.data_file_path {
            let target_file_path = Path::new(path_str);
            // let mut file = std::fs::File::create(target_file_path).unwrap();
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(target_file_path)
                .unwrap();

            let mut byte_buffer = [0; CHUNK_LENGTH as usize];
            let num_bytes_read = file.read(&mut byte_buffer).unwrap();
            if num_bytes_read == 0 {
                debug!("could not find any bytes to read");
                return;
            }

            debug!("byte buffer: {:?}", byte_buffer);
            let mut float_buffer = [0.0; CHUNK_LENGTH as usize / F32_SIZE];
            let mut i = 0usize;
            while i < float_buffer.len() {
                // debug!("byte buffer len = {}", byte_buffer.len());
                // debug!("attempting to slice from {} to {}", FLOAT_SIZE*i, FLOAT_SIZE*(i+1));
                // let bytes_raw = byte_buffer[FLOAT_SIZE*i..(FLOAT_SIZE+1)*i];
                let bytes: [u8; F32_SIZE] = byte_buffer[F32_SIZE*i..F32_SIZE*(i+1)].try_into().unwrap();
                float_buffer[i] = f32::from_be_bytes(bytes);
                i += 1;
            }

            debug!("adding {:?} to entity data", &float_buffer[0..num_bytes_read / F32_SIZE]);
            self.data.extend_from_slice(&float_buffer[0..num_bytes_read / F32_SIZE]);
            // debug!("data = {:?}", &self.data);

            
            // delete the chunk from the file
            let temp_path = target_file_path.with_extension("tmp");
            let mut temp_file = std::fs::File::create(&temp_path).unwrap();
            let metadata = fs::metadata(&target_file_path).unwrap();
            let file_len = metadata.len();
            debug!("file length before deleting chunk: {file_len}");

            file.seek(std::io::SeekFrom::Start(num_bytes_read as u64)).unwrap();
            std::io::copy(&mut file, &mut temp_file).unwrap();
            temp_file.sync_all().unwrap();
            std::fs::rename(&temp_path, target_file_path).unwrap();
            let metadata = fs::metadata(&target_file_path).unwrap();
            let file_len = metadata.len();
            debug!("file length before deleting chunk: {file_len}");

        }
    }
}

#[allow(dead_code)]
pub struct Entity {
    name: String,
    position: Rc<RefCell<Point3<f32>>>,
    rotation: Quaternion<f32>,
    scale: Vector3<f32>,
    models: Vec<model::Model>,
    behaviors: Vec<Behavior>,
}

impl Entity {
    pub fn load_from_json(json: &serde_json::Value, device: &wgpu::Device, model_bind_group_layout: &wgpu::BindGroupLayout) -> Entity {
        let name = json["Name"].to_string();
        debug!("loading entity {name}");

        // Position
        let position_temp: Vec<f32> = json["Position"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        let position = Point3::new(position_temp[0], position_temp[1], position_temp[2]);

        // Rotation
        let rotation_arr: Vec<f32> = json["Rotation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        let rotation = Quaternion::new(rotation_arr[0], rotation_arr[1], rotation_arr[2], rotation_arr[3]);

        // Scale
        let scale_temp = json["Scale"]
            .as_array()
            .unwrap()
            .into_iter();
        let mut scale_vec = Vector3::<f32>::new(0.0, 0.0, 0.0);
        for (i, scale_comp) in scale_temp.enumerate() {
            scale_vec[i] = scale_comp.as_f64().unwrap() as f32;
        }

        let model_vec: Vec<_> = match json["Models"].as_array() {
            Some(array) => {
                let model_temp: Vec<_> = array.into_iter().collect();
                let mut model_vec = vec![];
                for i in model_temp.iter() {
                    model_vec.push(model::Model::load_from_json(*i, device, model_bind_group_layout));
                }
                model_vec
            }
            None => vec![],
        };

        let behavior_vec: Vec<_> = match json["Behaviors"].as_array() {
            Some(array) => {
                let behavior_temp: Vec<_> = array.into_iter().collect();
                let mut behavior_vec = vec![];
                for i in behavior_temp.iter() {
                    behavior_vec.push(Behavior::load_from_json(*i));
                }
                behavior_vec
            }
            None => vec![],
        };

        Entity {
            name: name,
            position: Rc::new(RefCell::new(position)),
            rotation,
            scale: scale_vec,
            models: model_vec,
            behaviors: behavior_vec,
        }
    }

    pub fn load_from_hdf5(name: String, data: hdf5::Dataset, device: &wgpu::Device, model_bind_group_layout: &wgpu::BindGroupLayout) -> hdf5::Result<Entity> {
        // name
        println!("NAME: {}", name);

        // position
        let data_array: ArrayBase<OwnedRepr<[f32; 12]>, Dim<[usize; 1]>>  = data.read()?;
        let initial_transform: [f32; 12] = data_array[0];
        let position = Point3::<f32>::new(initial_transform[0], initial_transform[1], initial_transform[2]);
        println!("POSITION: {:?}", position);

        // rotation
        let rotation = Vector3::<f32>::new(initial_transform[7], initial_transform[6], initial_transform[8]);
        println!("ROTATION: {:?}", rotation);

        // scale
        let scale = Vector3::<f32>::new(1.0, 1.0, 1.0);

        // model vec
        let mut name_root = name.clone();
        if let Some(val) = name_root.find("_"){
            name_root.truncate(val)
        }
        let name_root_str = name_root.as_str();
        println!("NAME STR: {}", name_root_str);
        let model_vec: Vec<_> = match name_root_str {
            "Blizzard" => {
                // scale = Vector3::<f32>::new(1.0, 1.0, 1.0);
                model::Model::load_from_json_file("data/object_loading/blizzard_initialize_full.json", device, model_bind_group_layout)
            }
            _ => vec![],
        };

        // behavior vec
        let behavior_vec: Vec<_> = match name_root_str {
            "Blizzard" => {
                // load entire data array into behavior and set type to SetPosition or similar
                vec![Behavior::load_from_hdf5(&data_array).unwrap()]
            }
            _ => vec![],
        };
        // println!("BEHAVIOR: {:?}", behavior_vec[0].data);

        // return entity
        Ok(
            Entity {
                name: name,
                position: Rc::new(RefCell::new(position)),
                rotation: Quaternion::from_sv(1.0, rotation),
                scale: scale,
                models: model_vec,
                behaviors: behavior_vec,
            }
        )
    }

    // pub fn load_from_network() -> Entity {

    // }

    pub fn get_position(&self) -> Rc<RefCell<Point3<f32>>> { Rc::clone(&self.position) }

    pub fn set_position(&mut self, new_position: Point3<f32>) {
        *self.position.borrow_mut() = new_position;
        // println!("new position: ({}, {}, {})", new_position[0], new_position[1], new_position[2]);
    }

    pub fn rotate(&mut self, rotation: cgmath::Quaternion<f32>){
        self.rotation = (self.rotation * rotation).normalize();
    }

    fn to_matrix(&self) -> Matrix4<f32> {
        let translation = Matrix4::from_translation(self.position.borrow().to_vec());
        let rotation = Matrix4::from(self.rotation);
        let scale = Matrix4::from_nonuniform_scale(self.scale.x, self.scale.y, self.scale.z);
        // let rotation_correction = Matrix4::from_angle_x(Deg(-90.0));
        // rotation_correction * translation * rotation * scale
        translation * rotation * scale
    }

    pub fn find_timesteps(&self) -> Option<usize> {
        for behavior in self.behaviors.iter() {
            if !behavior.is_constant_behavior {
                return Some(behavior.data.len())
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
        let entity_matrix = self.to_matrix();
        for model in &self.models {
            // println!("drawing {}", model.name);
            let model_matrix = model.to_matrix();
            let full_transform = entity_matrix * model_matrix;
            let full_uniform: [[f32; 4]; 4] = full_transform.into();

            queue.write_buffer(
                &model.uniform_buffer,
                0,
                bytemuck::cast_slice(&[full_uniform]),
            );

            render_pass.draw_mesh(&model.obj, camera_bind_group, &model.bind_group);
        }
    }

    pub fn run_behavior(&mut self, behavior_index: usize, data_counter: Option<usize>) {
        // the borrow checker means that we have to refer to the behavior with self.behaviors[behavior_index] every time
        let behavior = &mut self.behaviors[behavior_index];

        match behavior.behavior_type {
            // Translate entity by vector
            BehaviorType::EntityTranslate => {
                let data = &mut self.behaviors[behavior_index].data;
                let old_position = *self.position.borrow();
                let offset = Vector3::<f32>::new(data[0], data[1], data[2]);
                self.set_position(old_position + offset);
            }

            BehaviorType::EntityRotate => {
                // debug!("EntityRotate data = {:?}", self.behaviors[behavior_index].data);
                let data = &mut self.behaviors[behavior_index].data;
                let rotation_factor = data[0];
                let new_quaternion_vector = Vector3::<f32>::new(
                    (rotation_factor * data[1]) as f32,
                    (rotation_factor * data[3]) as f32,
                    (rotation_factor * data[2]) as f32,
                );
                let new_quaternion = Quaternion::<f32>::from_sv(1.0, new_quaternion_vector);

                self.rotation = (self.rotation * new_quaternion).normalize();
            }

            // Change position to input
            BehaviorType::EntityChangeTransform => {
                let counter = data_counter.expect("Error in Entity::run_behavior : data counter is None");
                let data_len = behavior.data.len();
                debug!("counter = {}, data len = {}", counter, data_len);

                if data_len < (CHUNK_LENGTH as usize) * DATA_ARR_WIDTH {
                    // self.behaviors[behavior_index].data.drain(0..counter+DATA_ARR_WIDTH);
                    debug!("data len of {} is less than threshold of {}", data_len, (CHUNK_LENGTH as usize) * DATA_ARR_WIDTH);
                    behavior.retrieve_data_chunk();
                }

                if data_len >= DATA_ARR_WIDTH {
                    // let new_position = Point3::<f32>::new(behavior.data[counter], behavior.data[counter+1], behavior.data[counter+2]);
                    // let rotation = Vector3::<f32>::new(behavior.data[counter+6], behavior.data[counter+7], behavior.data[counter+8]);
                    let new_position = Point3::<f32>::new(behavior.data[0], behavior.data[1], behavior.data[2]);
                    let rotation = Vector3::<f32>::new(behavior.data[6], behavior.data[7], behavior.data[8]);
                    self.rotation = Quaternion::from_sv(1.0, rotation);
                    // debug!("x: {}, y: {}, z: {}, v0: {}, v1: {}, v2: {}", behavior.data[0], behavior.data[1], behavior.data[2], behavior.data[6], behavior.data[7], behavior.data[8]);
                    debug!("x: {}, y: {}, z: {}, v0: {}, v1: {}, v2: {}", new_position.x, new_position.y, new_position.z, rotation.x, rotation.y, rotation.z);
                    behavior.data.drain(0..DATA_ARR_WIDTH);
                    
                    self.set_position(new_position);
                } else {
                    debug!("Entity has run out of data and is stalling at last known transform");
                }

            }

            // Rotate item at constant speed
            BehaviorType::ComponentRotateConstantSpeed => {
                // debug!("ComponentRotateConstantSpeed data = {:?}", self.behaviors[behavior_index].data);
                let model_id = self.behaviors[behavior_index].data[0] as u64;
                let rotation_factor = self.behaviors[behavior_index].data[1];
                let new_quaternion_vector = Vector3::<f32>::new(
                    (rotation_factor * self.behaviors[behavior_index].data[2]) as f32,
                    (rotation_factor * self.behaviors[behavior_index].data[4]) as f32,
                    (rotation_factor * self.behaviors[behavior_index].data[3]) as f32,
                );
                let new_quaternion = Quaternion::<f32>::from_sv(1.0, new_quaternion_vector);

                self.get_model(model_id).rotate(new_quaternion);
            }

            _ => return,
        }
    }

    pub fn run_behaviors(&mut self, data_counter: Option<usize>) {
        for i in 0..self.behaviors.len() {
            self.run_behavior(i, data_counter);
        }
    }

    pub fn get_model(&mut self, model_component_id: u64) -> &mut model::Model {
        &mut self.models[model_component_id as usize]
    }
}