use winit::event::*;
use winit::keyboard::KeyCode;
use winit::dpi::PhysicalPosition;
use cgmath::*;
use std::time::Duration;
use std::collections::HashSet;
use log::debug;

use crate::behaviors_and_entities::Entity;
use crate::camera_behavior::{PositionBehavior, RotationBehavior};
use crate::ring_buffer;

#[rustfmt::skip]
pub const OPENGL_TO_WGPU_MATRIX: Matrix4<f32> = Matrix4::new(
    1.0, 0.0, 0.0, 0.0,
    0.0, 1.0, 0.0, 0.0,
    0.0, 0.0, 0.5, 0.5,
    0.0, 0.0, 0.0, 1.0,
);

/// Mouse-look scale a camera falls back to when it has never been under user control — the
/// sensitivity a `UserControlled` behavior would have declared. Only reached by a camera that was
/// handed to the user by the master server rather than declared user-driven in its scene.
const DEFAULT_SENSITIVITY: f32 = 0.4;

#[derive(Debug)]
pub struct Camera {
    pub position: Point3<f32>,
    rotation: Quaternion<f32>,
}

impl Camera {
    pub fn new<V: Into<Point3<f32>>, Q: Into<Quaternion<f32>>>(
        position: V,
        rotation: Q,
    ) -> Self {
        Self {
            position: position.into(),
            rotation: rotation.into(),
        }
    }

    pub fn rotation(&self) -> Quaternion<f32> { self.rotation }

    pub fn set_rotation(&mut self, rotation: Quaternion<f32>) { self.rotation = rotation; }

    pub fn calc_matrix(&self) -> Matrix4<f32> {
        // convert quaternion to matrix and adjust for the swapped axes (z=up, y=forward)
        // also invert y-axis, as +y should be forward
        let rot_default = Matrix4::from(self.rotation);
        let rot_corrected = Matrix4::from_cols(
            rot_default.x,
            rot_default.z,
            -rot_default.y,
            Vector4::unit_w()
        );

        // transform world space into camera space
        let rot_t = rot_corrected.transpose();
        let pos_inv = Matrix4::from_translation(-self.position.to_vec());
        let view = rot_t * pos_inv;
        view
    }
}

pub struct Projection {
    aspect: f32,
    fovy: Rad<f32>,
    znear: f32,
    zfar: f32,
}

impl Projection {
    pub fn new<F: Into<Rad<f32>>>(width: f32, height: f32, fovy: F, znear: f32, zfar: f32) -> Self {
        Self {
            aspect: width / height,
            fovy: fovy.into(),
            znear,
            zfar,
        }
    }

    pub fn resize(&mut self, width: f32, height: f32) {
        self.aspect = width / height;
    }

    pub fn calc_matrix(&self) -> Matrix4<f32> {
        OPENGL_TO_WGPU_MATRIX * perspective(self.fovy, self.aspect, self.znear, self.zfar)
    }
}

/// Everything the user is currently asking a camera to do.
///
/// Held separately from the behaviors because it accumulates whether or not anything reads it: a
/// key held down while the master server swaps a camera to `Linear` is still held when the user
/// gets it back, and the alternative — discarding input the moment a behavior stops accepting it —
/// would mean a camera handed back mid-keypress could never see the matching key-up.
#[derive(Debug, Default)]
pub struct CameraInput {
    pressed_keys: HashSet<KeyCode>,
    pub strafe_step: f32,
    pub forward_step: f32,
    pub up_step: f32,
    pub roll_step: f32,
    pub rotate_horizontal: f32,
    pub rotate_vertical: f32,
    pub scroll: f32,
}

impl CameraInput {
    fn opposed(&self, key1: &KeyCode, key2: &KeyCode, key3: &KeyCode, key4: &KeyCode) -> f32 {
        (
            ((self.pressed_keys.contains(key1) || self.pressed_keys.contains(key2)) as i32) -
            ((self.pressed_keys.contains(key3) || self.pressed_keys.contains(key4)) as i32)
        ) as f32
    }

    fn recompute_steps(&mut self) {
        self.strafe_step = self.opposed(
            &KeyCode::KeyD, &KeyCode::ArrowRight, &KeyCode::KeyA, &KeyCode::ArrowLeft,
        );
        self.forward_step = self.opposed(
            &KeyCode::KeyW, &KeyCode::ArrowUp, &KeyCode::KeyS, &KeyCode::ArrowDown,
        );
        self.up_step = self.opposed(
            &KeyCode::Space, &KeyCode::Space, &KeyCode::ShiftLeft, &KeyCode::ShiftLeft,
        );
        self.roll_step = self.opposed(
            &KeyCode::KeyL, &KeyCode::KeyL, &KeyCode::KeyK, &KeyCode::KeyK,
        );
    }

    /// Drops every in-flight input, as if the user had let go of everything.
    fn release_all(&mut self) {
        self.pressed_keys.clear();
        self.strafe_step = 0.0;
        self.forward_step = 0.0;
        self.up_step = 0.0;
        self.roll_step = 0.0;
        self.rotate_horizontal = 0.0;
        self.rotate_vertical = 0.0;
        self.scroll = 0.0;
    }
}

/// One camera, the two behaviors driving it, and the user input those behaviors may read.
///
/// There is exactly one of these per viewport, and the master server addresses it by the
/// viewport's camera name.
#[derive(Debug)]
pub struct CameraController {
    camera: Camera,
    position_behavior: PositionBehavior,
    rotation_behavior: RotationBehavior,
    input: CameraInput,
    /// Mouse-look scale to restore when `Enter` hands the camera back to the user. Remembered
    /// rather than re-derived so a camera that toggles away from `UserControlled` and back does
    /// not silently pick up a different feel from the one its scene declared.
    user_sensitivity: f32,
    /// The entity `Enter` last focused on, so a round trip through user control returns to it
    /// instead of resetting to whatever the scene default happens to be now.
    last_focus_target: Option<String>,
}

impl CameraController {
    pub fn new(
        camera: Camera,
        position_behavior: PositionBehavior,
        rotation_behavior: RotationBehavior,
    ) -> Self {
        let user_sensitivity = match rotation_behavior {
            RotationBehavior::UserControlled { sensitivity } => sensitivity,
            _ => DEFAULT_SENSITIVITY,
        };

        Self {
            camera,
            position_behavior,
            rotation_behavior,
            input: CameraInput::default(),
            user_sensitivity,
            last_focus_target: None,
        }
    }

    pub fn camera(&self) -> &Camera { &self.camera }

    pub fn position_behavior(&self) -> &PositionBehavior { &self.position_behavior }

    pub fn rotation_behavior(&self) -> &RotationBehavior { &self.rotation_behavior }

    /// Both behaviors named together, for on-screen messages: `Free Roam / User Controlled`.
    pub fn describe(&self) -> String {
        format!(
            "{} / {}",
            self.position_behavior.display_name(),
            self.rotation_behavior.display_name(),
        )
    }

    /// Whether the user can drive any part of this camera. False when the master server owns both
    /// halves of it, which is what a click on such a viewport is told.
    pub fn accepts_input(&self) -> bool {
        self.position_behavior.accepts_input() || self.rotation_behavior.accepts_input()
    }

    /// Installs a new position behavior, returning the one replaced so the caller can release its
    /// stream. Held input is dropped: a key held while the behavior changed was addressed to the
    /// old one, and carrying it over would kick the camera on the first frame of the new one.
    pub fn set_position_behavior(&mut self, behavior: PositionBehavior) -> PositionBehavior {
        self.input.release_all();
        std::mem::replace(&mut self.position_behavior, behavior)
    }

    pub fn set_rotation_behavior(&mut self, behavior: RotationBehavior) -> RotationBehavior {
        if let RotationBehavior::UserControlled { sensitivity } = behavior {
            self.user_sensitivity = sensitivity;
        }
        self.input.release_all();
        std::mem::replace(&mut self.rotation_behavior, behavior)
    }

    pub fn set_position(&mut self, position: Point3<f32>) { self.camera.position = position; }

    pub fn set_rotation(&mut self, rotation: Quaternion<f32>) { self.camera.set_rotation(rotation); }

    /// Settles behavior geometry that depends on where the camera is standing, moving the camera
    /// if the behavior requires it. See `PositionBehavior::prepare`.
    pub fn prepare_position_behavior(&mut self) -> Result<(), String> {
        if let Some(snap) = self.position_behavior.prepare(self.camera.position)? {
            self.camera.position = snap;
        }
        Ok(())
    }

    /// Reconciles the behaviors with a position handed down by the master server.
    ///
    /// Three things have to happen together, and each is a way for the command to be quietly
    /// undone on the next frame: a streamed camera replays its queued frames straight off the
    /// commanded pose, an orbiting one is pulled back to its old radius, and a tracking one is
    /// pulled back to its old offset.
    pub fn reconcile_commanded_position(&mut self, entities: &[Entity]) -> Result<(), String> {
        self.position_behavior.clear_stream();
        self.position_behavior.adopt_current_radius(&self.camera);
        self.position_behavior.retarget_from_position(&self.camera, entities)
    }

    /// The rotation counterpart. Only a streamed camera has anything to reconcile — the two
    /// look-at behaviors deliberately overwrite a commanded rotation on the next frame, since
    /// where they aim is not the server's to set one frame at a time.
    pub fn reconcile_commanded_rotation(&mut self) {
        self.rotation_behavior.clear_stream();
    }

    /// Re-binds both streams to the registry after a purge. See `TransformStream::rebind` for why
    /// a camera needs this and an entity does not.
    pub fn rebind_streams(&mut self, registry: &ring_buffer::BufferRegistry) {
        self.position_behavior.rebind(registry);
        self.rotation_behavior.rebind(registry);
    }

    /// `(source name, which half reads it)` for every live stream this camera consumes.
    pub fn stream_sources(&self) -> Vec<(String, &'static str)> {
        let mut sources = Vec::new();
        if let Some(name) = self.position_behavior.stream_source() {
            sources.push((name.to_string(), "position"));
        }
        if let Some(name) = self.rotation_behavior.stream_source() {
            sources.push((name.to_string(), "rotation"));
        }
        sources
    }

    /// Entity paths both behaviors depend on, so the scene can report one that names nothing.
    pub fn entity_paths(&self) -> Vec<&str> {
        [self.position_behavior.entity_path(), self.rotation_behavior.entity_path()]
            .into_iter()
            .flatten()
            .collect()
    }

    pub fn process_keyboard(&mut self, key: KeyCode, state: ElementState, entities: &[Entity]) -> bool {
        match state {
            ElementState::Pressed => {
                self.input.pressed_keys.insert(key);

                if key == KeyCode::Enter {
                    self.toggle_focus(entities);
                }

                if key == KeyCode::KeyT {
                    self.cycle_focus_target(entities);
                }

                if key == KeyCode::KeyC {
                    let p = self.camera.position;
                    let r = self.camera.rotation();
                    println!("\"position\": [{}, {}, {}]", p.x, p.y, p.z);
                    println!("\"rotation\": [{}, {}, {}, {}]", r.s, r.v.x, r.v.y, r.v.z);
                }
            }
            ElementState::Released => {
                self.input.pressed_keys.remove(&key);
            }
        }

        self.input.recompute_steps();

        state == ElementState::Pressed
    }

    /// Drops every in-flight input, as if the user had let go of everything.
    ///
    /// Called on the viewport losing focus. Input is routed to one viewport at a time, so a
    /// controller that is focused while a key is held never sees that key's `Released` event once
    /// focus moves on — it would keep the key held and translate forever. The rotate accumulators
    /// go too: only `UserControlled` zeroes them, so a controller blurred while under any other
    /// rotation behavior would otherwise apply a stale mouse delta as a one-frame kick whenever it
    /// next came back under user control.
    pub fn release_all_input(&mut self) {
        self.input.release_all();
    }

    pub fn process_mouse(&mut self, mouse_dx: f64, mouse_dz: f64) {
        self.input.rotate_horizontal = mouse_dx as f32;
        self.input.rotate_vertical = mouse_dz as f32;
    }

    pub fn process_scroll(&mut self, delta: &MouseScrollDelta) {
        self.input.scroll = match delta {
            // I'm assuming a line is about 100 pixels
            MouseScrollDelta::LineDelta(_, scroll) => -scroll * 0.5,
            MouseScrollDelta::PixelDelta(PhysicalPosition { y: scroll, .. }) => -*scroll as f32,
        };
    }

    /// What a viewport focuses on before the user has chosen anything.
    ///
    /// A moving entity is the better guess for what someone wants to watch, so the first mover
    /// wins; a scene of nothing but static geometry falls back to the first entity. This only picks
    /// the *default* — cycling reaches every root.
    fn default_focus_target(entities: &[Entity]) -> Option<String> {
        let index = entities.iter().position(|e| e.has_movement_behavior()).unwrap_or(0);
        entities.get(index).map(|e| e.qualified_name())
    }

    /// `Enter`: hands the camera's aim back and forth between the user and an entity.
    ///
    /// Only ever moves between these two rotation behaviors. The other three belong to the master
    /// server, and a keypress that yanked a camera out of a server-set behavior would leave the
    /// server believing it still held a camera it no longer drives.
    ///
    /// Unlike the orbit mode this replaced, focusing does not move the camera — it only re-aims
    /// from where the camera already is.
    fn toggle_focus(&mut self, entities: &[Entity]) {
        match self.rotation_behavior {
            RotationBehavior::UserControlled { .. } => {
                let target = self.last_focus_target.clone()
                    .filter(|path| crate::behaviors_and_entities::world_position(entities, path).is_some())
                    .or_else(|| CameraController::default_focus_target(entities));

                // Nothing to look at: a scene with no entities keeps the camera under user
                // control rather than aiming it at a path that resolves to nothing.
                let Some(path) = target else { return };

                self.last_focus_target = Some(path.clone());
                self.rotation_behavior = RotationBehavior::FocusedOnEntity { path };
            }
            RotationBehavior::FocusedOnEntity { .. } => {
                self.rotation_behavior = RotationBehavior::UserControlled {
                    sensitivity: self.user_sensitivity,
                };
            }
            _ => {}
        }
    }

    /// `T`: points this viewport at the next root entity, wrapping at the end of the scene.
    ///
    /// Roots only, though a command may name any path. Cycling one key-press at a time through
    /// every propeller of a ten-drone fleet is not a control anyone can use, and the root is what
    /// someone watching a scene means by "the next one".
    ///
    /// Only meaningful while focused, so it is a no-op otherwise rather than silently changing
    /// state the user cannot see.
    fn cycle_focus_target(&mut self, entities: &[Entity]) {
        let RotationBehavior::FocusedOnEntity { ref path } = self.rotation_behavior else { return };
        if entities.is_empty() {
            return;
        }

        // A path that no longer names anything — its entity was cleared — restarts at the top
        // rather than stranding the key.
        let current = entities.iter().position(|e| &e.qualified_name() == path);
        let next = current.map_or(0, |i| (i + 1) % entities.len());
        let next_path = entities[next].qualified_name();

        self.last_focus_target = Some(next_path.clone());
        self.rotation_behavior = RotationBehavior::FocusedOnEntity { path: next_path };
    }

    /// The entity this camera is currently aimed at, for on-screen messages.
    pub fn focus_target(&self) -> Option<&str> {
        match self.rotation_behavior {
            RotationBehavior::FocusedOnEntity { ref path } => Some(path),
            _ => None,
        }
    }

    /// Advances both behaviors, position first.
    ///
    /// The order is load-bearing rather than incidental: `FocusedOnEntity` and `FocusedOnPoint`
    /// aim from wherever the camera ended up this frame, so running rotation first would leave
    /// every look-at camera aiming one frame behind its own position.
    pub fn update_camera(&mut self, dt: Duration, entities: &[Entity]) {
        self.position_behavior.apply(&mut self.camera, &self.input, dt, entities);
        self.rotation_behavior.apply(&mut self.camera, &self.input, dt, entities);

        // Zeroed after use, not before: if `process_mouse` is not called every frame these would
        // otherwise persist and keep rotating the camera while it merely translates.
        self.input.rotate_horizontal = 0.0;
        self.input.rotate_vertical = 0.0;

        debug!(
            "camera at ({}, {}, {})",
            self.camera.position.x, self.camera.position.y, self.camera.position.z,
        );
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CameraUniform {
    view_position: [f32; 4],
    view_proj: [[f32; 4]; 4],
}

impl CameraUniform {
    pub fn new() -> Self {
        Self {
            view_position: [0.0; 4],
            view_proj: Matrix4::identity().into(),
        }
    }

    pub fn update_view_proj(&mut self, camera: &Camera, projection: &Projection) {
        self.view_position = camera.position.to_homogeneous().into();
        self.view_proj = (projection.calc_matrix() * camera.calc_matrix()).into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::behaviors_and_entities::{Behavior, BehaviorType};
    use crate::transform_stream::{DATA_ARR_WIDTH, TransformStream};

    fn free_roam_controller() -> CameraController {
        CameraController::new(
            Camera::new((0.0, 0.0, 0.0), Quaternion::new(1.0, 0.0, 0.0, 0.0)),
            PositionBehavior::FreeRoam { speed: 8.0 },
            RotationBehavior::UserControlled { sensitivity: 0.4 },
        )
    }

    /// Builds a scene from `(name, x, moves)` triples. Entities are spread along x so that a
    /// camera's position identifies which one it settled on.
    fn test_scene(spec: &[(&str, f32, bool)]) -> Vec<Entity> {
        spec.iter()
            .map(|(name, x, moves)| {
                let behavior = moves.then(||
                    Behavior::new(BehaviorType::Translate, vec![0.0, 0.0, 0.0], None)
                );
                let mut entity = Entity::for_test(name, Point3::new(*x, 0.0, 0.0), behavior);
                entity.set_namespace(String::new());
                entity
            })
            .collect()
    }

    /// A camera under stream control on both halves, fed from `frames`.
    fn streamed_controller(frames: &[Vec<f32>]) -> CameraController {
        let values: Vec<f32> = frames.iter().flatten().copied().collect();
        CameraController::new(
            Camera::new((0.0, 0.0, 0.0), Quaternion::new(1.0, 0.0, 0.0, 0.0)),
            PositionBehavior::Streamed {
                source: "camera_pos.bin".to_string(),
                stream: TransformStream::from_values(values),
            },
            RotationBehavior::UserControlled { sensitivity: 0.4 },
        )
    }

    /// A frame placing the camera at `x` with no rotation.
    fn frame_at(x: f32) -> Vec<f32> {
        let mut frame = vec![0.0; DATA_ARR_WIDTH];
        frame[0] = x;
        frame
    }

    #[test]
    fn a_streamed_camera_takes_its_position_from_the_frame() {
        let mut controller = streamed_controller(&[frame_at(10.0), frame_at(20.0)]);

        controller.update_camera(Duration::from_millis(16), &[]);
        assert_eq!(controller.camera().position, Point3::new(10.0, 0.0, 0.0));

        controller.update_camera(Duration::from_millis(16), &[]);
        assert_eq!(controller.camera().position, Point3::new(20.0, 0.0, 0.0));
    }

    // Freezing is readable; snapping to an origin would look like a crash every late packet.
    #[test]
    fn a_starved_stream_holds_the_last_pose() {
        let mut controller = streamed_controller(&[frame_at(10.0)]);
        let dt = Duration::from_millis(16);

        controller.update_camera(dt, &[]);
        controller.update_camera(dt, &[]);
        controller.update_camera(dt, &[]);

        assert_eq!(controller.camera().position, Point3::new(10.0, 0.0, 0.0));
    }

    // The wire is the only authority on where a streamed camera is. Keys reach a focused viewport
    // whatever drives it, and must not displace one the server owns.
    #[test]
    fn input_does_not_move_a_streamed_camera() {
        let scene = test_scene(&[("drone1", 0.0, true)]);
        let mut controller = streamed_controller(&[frame_at(10.0)]);
        let dt = Duration::from_millis(16);

        controller.update_camera(dt, &scene);
        controller.process_keyboard(KeyCode::KeyW, ElementState::Pressed, &scene);
        controller.update_camera(dt, &scene);

        assert_eq!(
            controller.camera().position, Point3::new(10.0, 0.0, 0.0),
            "held keys must not displace a stream-driven camera",
        );
    }

    // Input is routed to one viewport at a time, so a controller that loses focus with a key held
    // never receives that key's Released event. Without the explicit release it would translate
    // forever, on a viewport the user is no longer even addressing.
    #[test]
    fn a_blurred_controller_stops_moving() {
        let mut controller = free_roam_controller();
        let dt = Duration::from_millis(16);
        controller.process_keyboard(KeyCode::KeyW, ElementState::Pressed, &[]);

        // premise: the held key does drive movement, so the assertion below means something
        controller.update_camera(dt, &[]);
        assert_ne!(controller.camera().position, Point3::new(0.0, 0.0, 0.0));

        let drifted_to = controller.camera().position;
        controller.release_all_input();
        controller.update_camera(dt, &[]);

        assert!(controller.input.pressed_keys.is_empty(), "the held key must not survive the blur");
        assert_eq!(
            controller.camera().position, drifted_to,
            "a viewport that lost focus mid-keypress must stop where it was",
        );
    }

    // Only UserControlled consumes the rotate accumulators, so a controller blurred under any
    // other rotation behavior would keep the last mouse delta and apply it as a kick later.
    #[test]
    fn release_all_input_clears_pending_rotation() {
        let mut controller = free_roam_controller();
        controller.process_mouse(5.0, -3.0);
        assert_ne!(controller.input.rotate_horizontal, 0.0);

        controller.release_all_input();

        assert_eq!(controller.input.rotate_horizontal, 0.0);
        assert_eq!(controller.input.rotate_vertical, 0.0);
        assert_eq!(controller.input.scroll, 0.0);
    }

    // A behavior swap is a handover. A key held across it was addressed to the old behavior, and
    // carrying it over kicks the camera on the new one's first frame.
    #[test]
    fn swapping_a_behavior_drops_held_input() {
        let mut controller = free_roam_controller();
        controller.process_keyboard(KeyCode::KeyW, ElementState::Pressed, &[]);

        controller.set_position_behavior(PositionBehavior::FreeRoam { speed: 8.0 });
        controller.update_camera(Duration::from_millis(16), &[]);

        assert_eq!(controller.camera().position, Point3::new(0.0, 0.0, 0.0));
    }

    #[test]
    fn enter_hands_the_camera_between_the_user_and_an_entity() {
        let scene = test_scene(&[("prop", 0.0, false), ("drone1", 100.0, true)]);
        let mut controller = free_roam_controller();

        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);
        assert_eq!(controller.focus_target(), Some("drone1"), "the first mover is the default");

        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);
        assert!(controller.rotation_behavior().accepts_input(), "and back to the user");
    }

    // The master server's behaviors are not the user's to leave. A keypress that yanked a camera
    // out of one would leave the server believing it still drove a camera it no longer does.
    #[test]
    fn enter_is_inert_on_a_server_set_rotation_behavior() {
        let scene = test_scene(&[("drone1", 0.0, true)]);
        let mut controller = free_roam_controller();
        controller.set_rotation_behavior(RotationBehavior::AboutOwnAxis {
            axis: Vector3::unit_z(),
            speed: 1.0,
        });

        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);

        assert!(matches!(controller.rotation_behavior(), RotationBehavior::AboutOwnAxis { .. }));
    }

    #[test]
    fn focusing_does_not_move_the_camera() {
        let scene = test_scene(&[("drone1", 100.0, true)]);
        let mut controller = free_roam_controller();

        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);
        controller.update_camera(Duration::from_millis(16), &scene);

        assert_eq!(
            controller.camera().position, Point3::new(0.0, 0.0, 0.0),
            "focusing re-aims from where the camera is; it does not teleport it",
        );
    }

    // A focused camera aims at its entity, and keeps aiming as that entity moves.
    #[test]
    fn a_focused_camera_tracks_its_entity() {
        let scene = test_scene(&[("drone1", 0.0, true)]);
        let mut controller = free_roam_controller();
        controller.set_position(Point3::new(0.0, -5.0, 0.0));
        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);

        controller.update_camera(Duration::from_millis(16), &scene);

        let forward = controller.camera().rotation().rotate_vector(Vector3::unit_y());
        assert!((forward - Vector3::new(0.0, 1.0, 0.0)).magnitude() < 1e-5, "forward was {:?}", forward);
    }

    // The point of a per-viewport target: two viewports can watch two different drones.
    #[test]
    fn two_viewports_focus_independently() {
        let scene = test_scene(&[("drone1", 0.0, true), ("drone2", 100.0, true)]);
        let mut left = free_roam_controller();
        let mut right = free_roam_controller();

        left.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);
        right.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);
        assert_eq!(left.focus_target(), Some("drone1"), "both default to the first mover");

        right.process_keyboard(KeyCode::KeyT, ElementState::Pressed, &scene);

        assert_eq!(right.focus_target(), Some("drone2"));
        assert_eq!(left.focus_target(), Some("drone1"), "cycling one viewport must not move the other");
    }

    // The shape of golden_gate_scene.json: one static landmark, one drone. Cycling must reach the
    // landmark — it is the more interesting thing to look at, and restricting candidates to movers
    // left T with nowhere to go and no visible effect at all.
    #[test]
    fn cycling_reaches_static_entities_and_wraps() {
        let scene = test_scene(&[("Golden Gate Bridge", 0.0, false), ("Blizzard", 100.0, true)]);
        let mut controller = free_roam_controller();

        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);
        assert_eq!(controller.focus_target(), Some("Blizzard"), "the mover is still the default");

        controller.process_keyboard(KeyCode::KeyT, ElementState::Pressed, &scene);
        assert_eq!(controller.focus_target(), Some("Golden Gate Bridge"));

        controller.process_keyboard(KeyCode::KeyT, ElementState::Pressed, &scene);
        assert_eq!(controller.focus_target(), Some("Blizzard"), "and it wraps back around");
    }

    // A viewport's target is its own setting, not a transient of the current behavior — dipping
    // back into user control and returning should not retarget the scene default.
    #[test]
    fn a_focus_target_survives_a_round_trip_through_user_control() {
        let scene = test_scene(&[("drone1", 0.0, true), ("drone2", 100.0, true)]);
        let mut controller = free_roam_controller();

        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);
        controller.process_keyboard(KeyCode::KeyT, ElementState::Pressed, &scene);
        assert_eq!(controller.focus_target(), Some("drone2"));

        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);
        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);

        assert_eq!(controller.focus_target(), Some("drone2"));
    }

    // T is meaningless while the user holds the camera. Acting anyway would change state the user
    // cannot see, so a later Enter would drop them onto an entity they never chose.
    #[test]
    fn cycling_is_a_no_op_under_user_control() {
        let scene = test_scene(&[("drone1", 0.0, true), ("drone2", 100.0, true)]);
        let mut controller = free_roam_controller();

        controller.process_keyboard(KeyCode::KeyT, ElementState::Pressed, &scene);

        assert_eq!(controller.focus_target(), None);
        assert!(controller.rotation_behavior().accepts_input());
    }

    // Nothing to look at must not leave the camera aimed at a path that resolves to nothing.
    #[test]
    fn enter_is_inert_in_an_empty_scene() {
        let mut controller = free_roam_controller();

        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &[]);

        assert_eq!(controller.focus_target(), None);
    }

    #[test]
    fn a_camera_reports_both_of_its_stream_sources() {
        let mut controller = streamed_controller(&[frame_at(1.0)]);
        controller.set_rotation_behavior(RotationBehavior::Streamed {
            source: "camera_rot.bin".to_string(),
            stream: TransformStream::from_values(vec![]),
        });

        assert_eq!(
            controller.stream_sources(),
            vec![
                ("camera_pos.bin".to_string(), "position"),
                ("camera_rot.bin".to_string(), "rotation"),
            ],
        );
    }
}
