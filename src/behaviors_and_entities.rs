
use cgmath::{Point3, Vector3, Quaternion, Matrix4};
use std::rc::Rc;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::fs::OpenOptions;
use log::{debug, info, warn, error};
use cgmath::{EuclideanSpace, InnerSpace, SquareMatrix};
use ndarray::{ArrayBase, OwnedRepr, Dim};

use crate::ring_buffer;
use crate::transform_stream::{self, TransformStream};

use wgpu::util::DeviceExt;

use crate::model;

use model::DrawModel;

pub use crate::transform_stream::DATA_ARR_WIDTH;

const AVERAGE_REFRESH_RATE: usize = 16;
const CHUNK_LENGTH: u64 = 1024;
const MAX_TRAIL_LENGTH: usize = 500;

pub fn create_and_clear_file(file_name: &str) -> std::io::Result<()> {
    let path = Path::new(file_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .unwrap();
    debug!("clearing {file_name}");

    Ok(())
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
    /// Parameters for the constant behaviors — `Translate` and `Rotate` read the first few slots
    /// fresh every frame and never consume them. Frame data for `ChangeTransform` lives in
    /// `stream` instead; the two roles used to share this field.
    pub data: Vec<f32>,
    /// Present exactly for the behaviors that consume frame data, which is what "not a constant
    /// behavior" used to mean. A cached `is_constant_behavior` flag became redundant once the
    /// distinction was structural, and could only ever disagree with `behavior_type`.
    stream: Option<TransformStream>,
}

impl Behavior {
    pub fn new(behavior_type: BehaviorType, data: Vec<f32>, stream: Option<TransformStream>) -> Behavior {
        Behavior {
            behavior_type,
            data,
            stream,
        }
    }

    pub fn load_from_json(json: &serde_json::Value, registry: &ring_buffer::BufferRegistry) -> Behavior {
        let behavior_type: BehaviorType =
            BehaviorType::match_from_string(json["behaviorType"].as_str().unwrap());
        let data_temp: Vec<_> = json["data"]
            .as_array()
            .unwrap()
            .into_iter()
            .collect();

        // A constant behavior's data is parameters; anything else names a frame source in slot 0
        // and the stream owns the whole array from there.
        if BehaviorType::is_constant_behavior(behavior_type) {
            let data = data_temp.iter().map(|v| v.as_f64().unwrap() as f32).collect();
            Behavior::new(behavior_type, data, None)
        } else {
            let stream = TransformStream::from_json_data(&data_temp, registry);
            Behavior::new(behavior_type, vec![], Some(stream))
        }
    }

    pub fn load_from_hdf5(data: &ArrayBase<OwnedRepr<[f32; 12]>, Dim<[usize; 1]>>) -> hdf5::Result<Behavior> {
        let behavior_type = BehaviorType::ChangeTransform;
        let a = 0;
        let b = DATA_ARR_WIDTH;
        let data_vec: Vec<f32> = data
            .iter()
            .flat_map(|arrs| arrs[a..b].iter().cloned())
            .collect();

        Ok(Behavior::new(behavior_type, vec![], Some(TransformStream::from_values(data_vec))))
    }

    /// Frames still in hand, for a behavior that has a finite source. `None` when there is no
    /// stream at all, which is every constant behavior.
    pub fn frames_remaining(&self) -> Option<usize> {
        self.stream.as_ref().map(|s| s.remaining())
    }

    pub fn is_exhausted(&self) -> bool {
        self.stream.as_ref().map_or(true, |s| s.is_exhausted())
    }

    pub fn stream_source(&self) -> Option<&str> {
        self.stream.as_ref().and_then(|s| s.source_name())
    }
}

#[allow(dead_code)]
/// Separates the segments of an entity path: `namespace/root/child`.
///
/// A name containing one would make every path it appears in ambiguous, so `normalize_name`
/// substitutes it rather than let a command resolve to an entity nobody meant.
pub const PATH_SEPARATOR: char = '/';

/// Marks a segment the client had to make unique: `propeller#0`, `propeller#1`.
///
/// Every collision this scheme resolves — duplicate siblings, duplicate namespaces, entities with
/// no name at all — is resolved the same way, so an operator who learns the convention from one
/// warning can read all of them.
pub const DISAMBIGUATOR: char = '#';

/// Turns one JSON `Name` into a usable path segment.
///
/// `index` is the entity's position in the array that declared it, which is what an entity with no
/// name is addressed by. That index is the *declaring server's own* ordering, so a server can
/// predict it; the entity's eventual index in the merged scene cannot be predicted by anyone,
/// since it depends on which stream wins the arrival race.
fn normalize_name(raw: &str, index: usize) -> String {
    if raw.is_empty() {
        return format!("{}{}", DISAMBIGUATOR, index);
    }

    if raw.contains(PATH_SEPARATOR) {
        let substituted = raw.replace(PATH_SEPARATOR, "_");
        warn!(
            "entity name {:?} contains '{}', which separates path segments; it is addressable as \
             {:?} instead",
            raw, PATH_SEPARATOR, substituted,
        );
        return substituted;
    }

    raw.to_string()
}

/// Renames colliding entities in one sibling set until every name in it is distinct.
///
/// Every member of a colliding group is suffixed, including the first. Leaving one holding the
/// bare name would let a command that names it resolve to an arbitrary member of the group — and a
/// command that silently hits the wrong drone is worse than one that visibly hits nothing, which
/// is the whole reason this scheme exists.
pub fn disambiguate_names(entities: &mut [Entity], context: &str) {
    let mut totals: HashMap<String, usize> = HashMap::new();
    for entity in entities.iter() {
        *totals.entry(entity.name.clone()).or_insert(0) += 1;
    }

    let mut seen: HashMap<String, usize> = HashMap::new();
    for entity in entities.iter_mut() {
        let total = totals[&entity.name];
        if total < 2 {
            continue;
        }

        let occurrence = *seen.entry(entity.name.clone()).or_insert(0);
        if occurrence == 0 {
            warn!(
                "{} declares {} entities named {:?}; they are addressable as {}{}0 through {}{}{}",
                context, total, entity.name,
                entity.name, DISAMBIGUATOR,
                entity.name, DISAMBIGUATOR, total - 1,
            );
        }
        *seen.get_mut(&entity.name).unwrap() += 1;
        entity.name = format!("{}{}{}", entity.name, DISAMBIGUATOR, occurrence);
    }
}

/// World-space position of the entity a path names, or `None` if it names nothing.
///
/// Resolved fresh on every call rather than cached behind a handle, because a cached one cannot be
/// right for a child: an entity's stored position is relative to its parent, so a child's world
/// position only exists once the ancestor chain above it has been composed. Recomputing also means
/// a camera aimed at an entity that has not connected yet starts working the moment it does, and
/// stops — holding still rather than following a corpse — when a scene clear takes it away.
pub fn world_position(entities: &[Entity], path: &str) -> Option<Point3<f32>> {
    for root in entities {
        let prefix = root.qualified_name();
        if path == prefix {
            return root.world_position_of("", Matrix4::identity());
        }
        if let Some(rest) = path.strip_prefix(&prefix).and_then(|r| r.strip_prefix(PATH_SEPARATOR)) {
            return root.world_position_of(rest, Matrix4::identity());
        }
    }
    None
}

pub struct Entity {
    name: String,
    /// The namespace this entity's subtree is addressed under, set on roots only. A child's
    /// qualification comes from the ancestor chain a path walks through, not from a field of its
    /// own, so there is one place per subtree that can disagree with the scene that declared it.
    namespace: Option<String>,
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
            namespace: None,
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
            namespace: None,
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

    /// A model-less entity, for tests that need a scene but not a GPU.
    ///
    /// Every other constructor takes a `wgpu::Device` — `new_child` does not, but it is private,
    /// and callers outside this module have no way in. Camera tests in particular need a scene
    /// of positioned, orbitable entities and nothing else.
    #[cfg(test)]
    pub fn for_test(name: &str, position: Point3<f32>, behavior: Option<Behavior>) -> Entity {
        Entity::new_child(
            name.to_string(),
            position,
            Quaternion::new(1.0, 0.0, 0.0, 0.0),
            None,
            behavior,
        )
    }

    /// Hangs `children` off a test entity, for the path tests — `new_root` is the only constructor
    /// that takes children and it needs a GPU.
    #[cfg(test)]
    pub fn with_children(mut self, children: Vec<Entity>) -> Entity {
        self.children = children;
        self
    }

    /// `index` is this entity's position in the `entities` or `Children` array that declared it,
    /// used to address it if it has no name of its own.
    pub fn load_from_json(json: &serde_json::Value, index: usize, device: &wgpu::Device, model_bind_group_layout: &wgpu::BindGroupLayout, registry: &ring_buffer::BufferRegistry) -> Entity {
        // as_str, not to_string: Value::to_string on a JSON string keeps the quotes, and this
        // name is displayed. The child branch below has always done it this way.
        let name = normalize_name(json["Name"].as_str().unwrap_or(""), index);
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

        let mut children: Vec<Entity> = match json["Children"].as_array() {
            Some(array) => array.iter().enumerate().map(|(child_index, c)| {
                let child_name = normalize_name(c["Name"].as_str().unwrap_or(""), child_index);

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

        // Scoped to one parent, because that is the scope a child path segment is resolved in:
        // every drone in a fleet having a `propeller RRT` is fine, two of them under *one* drone
        // is not. Recursion is implicit — each nested load has already settled its own children.
        disambiguate_names(&mut children, &format!("entity {:?}", name));

        Entity::new_root(name, position, rotation, scale, entity_behavior, children, device, model_bind_group_layout)
    }

    pub fn load_from_hdf5(name: String, data: hdf5::Dataset, device: &wgpu::Device, model_bind_group_layout: &wgpu::BindGroupLayout) -> hdf5::Result<Entity> {
        println!("NAME: {}", name);

        let data_array: ArrayBase<OwnedRepr<[f32; 12]>, Dim<[usize; 1]>> = data.read()?;
        let initial_transform: [f32; 12] = data_array[0];
        let position = Point3::new(initial_transform[0], initial_transform[1], initial_transform[2]);
        println!("POSITION: {:?}", position);

        let rotation_vec = Vector3::new(initial_transform[6], initial_transform[7], initial_transform[8]);
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

    pub fn name(&self) -> &str { &self.name }

    /// Places this entity's subtree in `namespace`. The scene loaders are the only callers, and
    /// they call it on roots only — see the field.
    pub fn set_namespace(&mut self, namespace: String) {
        self.namespace = Some(namespace);
    }

    /// This entity's path as a root: its name, qualified by its namespace when it has a non-empty
    /// one. A single-source run declares nothing and addresses entities by bare name, exactly as
    /// it did before namespaces existed.
    pub fn qualified_name(&self) -> String {
        match &self.namespace {
            Some(namespace) if !namespace.is_empty() => {
                format!("{}{}{}", namespace, PATH_SEPARATOR, self.name)
            }
            _ => self.name.clone(),
        }
    }

    /// Resolves a path relative to this entity — `propeller RRT`, or `arm/propeller RRT` deeper in.
    ///
    /// Sibling names are unique after loading, so the first match at each level is the only match.
    pub fn find_descendant_mut(&mut self, path: &str) -> Option<&mut Entity> {
        let (segment, rest) = match path.split_once(PATH_SEPARATOR) {
            Some((head, tail)) => (head, Some(tail)),
            None => (path, None),
        };

        let child = self.children.iter_mut().find(|c| c.name == segment)?;
        match rest {
            Some(tail) => child.find_descendant_mut(tail),
            None => Some(child),
        }
    }

    /// World-space position of the descendant `path` names, or of this entity when `path` is empty.
    ///
    /// `parent_matrix` is the composed transform of everything above this entity, matching what
    /// `draw_with_parent_matrix` builds — so the answer is where the entity is actually drawn,
    /// not the parent-relative offset its own field holds.
    pub fn world_position_of(&self, path: &str, parent_matrix: Matrix4<f32>) -> Option<Point3<f32>> {
        let own_matrix = parent_matrix * self.to_matrix();

        if path.is_empty() {
            return Some(Point3::from_vec(own_matrix.w.truncate()));
        }

        let (segment, rest) = match path.split_once(PATH_SEPARATOR) {
            Some((head, tail)) => (head, tail),
            None => (path, ""),
        };

        let child = self.children.iter().find(|c| c.name == segment)?;
        child.world_position_of(rest, own_matrix)
    }

    /// Appends this entity's full path and every descendant's, depth first.
    ///
    /// `prefix` is the path of the parent, or empty for a root — which is the only case that
    /// consults the namespace, since a child inherits its qualification through `prefix`.
    pub fn collect_paths(&self, prefix: &str, out: &mut Vec<String>) {
        let path = if prefix.is_empty() {
            self.qualified_name()
        } else {
            format!("{}{}{}", prefix, PATH_SEPARATOR, self.name)
        };

        out.push(path.clone());
        for child in &self.children {
            child.collect_paths(&path, out);
        }
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
            // A live stream reports zero — its total is unknowable, so it cannot size a
            // progress bar and is skipped exactly as an empty `data` array used to be.
            match b.frames_remaining() {
                Some(n) if n > 0 => return Some(n),
                _ => {}
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
                let frame = self.behavior.as_mut()
                    .and_then(|b| b.stream.as_mut())
                    .and_then(|s| s.next_frame());

                if let Some(data) = frame {
                    debug!("reading transform: x:{} y:{} z:{} v6:{} v7:{} v8:{}",
                        data[0], data[1], data[2], data[6], data[7], data[8]);
                    let (new_position, rotation) = transform_stream::frame_to_transform(&data);
                    self.rotation = rotation;
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

    /// Appends `(source name, description)` for every live stream this entity and its children
    /// read, so the scene can spot two consumers sharing one buffer.
    pub fn collect_stream_sources(&self, out: &mut Vec<(String, String)>) {
        if let Some(name) = self.behavior.as_ref().and_then(|b| b.stream_source()) {
            out.push((name.to_string(), format!("entity '{}'", self.name)));
        }
        for child in &self.children {
            child.collect_stream_sources(out);
        }
    }

    pub fn all_streams_exhausted(&self) -> bool {
        let own = self.behavior.as_ref().map_or(true, |b| b.is_exhausted());
        own && self.children.iter().all(|c| c.all_streams_exhausted())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cgmath::EuclideanSpace;

    fn entity(name: &str) -> Entity {
        Entity::for_test(name, Point3::origin(), None)
    }

    fn names(entities: &[Entity]) -> Vec<&str> {
        entities.iter().map(|e| e.name()).collect()
    }

    // An entity nobody named still has to be addressable, and the index used is the one the
    // declaring server can predict: its position in that server's own array.
    #[test]
    fn an_unnamed_entity_is_addressed_by_its_declaration_index() {
        assert_eq!(normalize_name("", 0), "#0");
        assert_eq!(normalize_name("", 3), "#3");
        assert_eq!(normalize_name("Drone_01", 3), "Drone_01", "a real name ignores the index");
    }

    // A separator inside a segment would make the path it sits in parse into different segments
    // than the author meant, and resolve to an entity nobody named.
    #[test]
    fn a_name_containing_the_separator_is_substituted() {
        assert_eq!(normalize_name("wing/left", 0), "wing_left");
        assert_eq!(normalize_name("a/b/c", 0), "a_b_c");
    }

    // The first duplicate is suffixed too. Leaving it holding the bare name would let a command
    // naming it hit one arbitrary member of the group, which is worse than hitting nothing.
    #[test]
    fn every_colliding_sibling_is_suffixed_including_the_first() {
        let mut siblings = vec![entity("propeller"), entity("tail"), entity("propeller")];

        disambiguate_names(&mut siblings, "a test");

        assert_eq!(names(&siblings), vec!["propeller#0", "tail", "propeller#1"]);
    }

    #[test]
    fn a_unique_sibling_set_is_left_alone() {
        let mut siblings = vec![entity("body"), entity("tail")];

        disambiguate_names(&mut siblings, "a test");

        assert_eq!(names(&siblings), vec!["body", "tail"]);
    }

    // Suffixes follow declaration order, so the same scene always produces the same addresses —
    // a server has to be able to predict what its own entities are called.
    #[test]
    fn disambiguation_follows_declaration_order() {
        let mut siblings = vec![entity("a"), entity("a"), entity("a")];

        disambiguate_names(&mut siblings, "a test");

        assert_eq!(names(&siblings), vec!["a#0", "a#1", "a#2"]);
    }

    #[test]
    fn paths_are_qualified_by_namespace_and_hierarchy() {
        let mut root = entity("Drone_01").with_children(vec![
            entity("body"),
            entity("propeller").with_children(vec![entity("blade")]),
        ]);
        root.set_namespace("fleet_a".to_string());

        let mut paths = Vec::new();
        root.collect_paths("", &mut paths);

        assert_eq!(paths, vec![
            "fleet_a/Drone_01",
            "fleet_a/Drone_01/body",
            "fleet_a/Drone_01/propeller",
            "fleet_a/Drone_01/propeller/blade",
        ]);
    }

    // The single-source case: nothing declared, so nothing is prepended and the paths are what
    // someone reading the scene file would already have guessed.
    #[test]
    fn an_undeclared_namespace_adds_no_segment() {
        let mut root = entity("Drone_01").with_children(vec![entity("body")]);
        root.set_namespace(String::new());

        let mut paths = Vec::new();
        root.collect_paths("", &mut paths);

        assert_eq!(paths, vec!["Drone_01", "Drone_01/body"]);
    }

    #[test]
    fn a_descendant_resolves_at_any_depth() {
        let mut root = entity("Drone_01").with_children(vec![
            entity("body"),
            entity("arm").with_children(vec![entity("propeller RRT")]),
        ]);

        assert_eq!(root.find_descendant_mut("body").map(|e| e.name().to_string()), Some("body".to_string()));
        assert_eq!(
            root.find_descendant_mut("arm/propeller RRT").map(|e| e.name().to_string()),
            Some("propeller RRT".to_string()),
        );
    }

    // A miss has to stay a miss. Resolving a wrong-but-plausible entity is the failure this whole
    // scheme is built to prevent.
    #[test]
    fn a_path_that_names_nothing_resolves_to_nothing() {
        let mut root = entity("Drone_01").with_children(vec![entity("body")]);

        assert!(root.find_descendant_mut("wing").is_none());
        assert!(root.find_descendant_mut("body/bolt").is_none(), "a leaf has no children to search");
        assert!(root.find_descendant_mut("").is_none());
    }
}