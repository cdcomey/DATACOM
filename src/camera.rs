use winit::event::*;
use winit::keyboard::KeyCode;
use winit::dpi::PhysicalPosition;
use cgmath::*;
use std::f32::consts::PI;
use std::rc::Rc;
use std::cell::RefCell;
use std::time::Duration;
use std::collections::HashSet;
use log::{info, debug};

use crate::behaviors_and_entities::Entity;
use crate::ring_buffer;
use crate::transform_stream::{self, TransformStream};

#[rustfmt::skip]
pub const OPENGL_TO_WGPU_MATRIX: Matrix4<f32> = Matrix4::new(
    1.0, 0.0, 0.0, 0.0,
    0.0, 1.0, 0.0, 0.0,
    0.0, 0.0, 0.5, 0.5,
    0.0, 0.0, 0.0, 1.0,
);

const APPROX_ZERO: f32 = 1e-8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CameraMode {
    FreeRoam,
    OrbitPoint,
    /// Transform driven by a streamed `.bin`, exactly as an entity's `ChangeTransform` is.
    /// Declared in scene JSON and locked — `switch_mode` will neither enter nor leave it.
    Stream,
}

impl CameraMode {
    /// Human-readable name, for on-screen messages.
    pub fn display_name(&self) -> &'static str {
        match self {
            CameraMode::FreeRoam => "Free Roam",
            CameraMode::OrbitPoint => "Orbit Point",
            CameraMode::Stream => "Stream",
        }
    }

    /// Whether the user can drive this camera. A stream-driven one takes its transform from the
    /// wire, so keyboard and mouse have nothing to act on.
    pub fn accepts_input(&self) -> bool {
        !matches!(self, CameraMode::Stream)
    }
}

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
        // println!("{:?}", view);
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

#[derive(Debug)]
pub struct CameraController {
    pressed_keys: HashSet<KeyCode>,
    h_translate_step: f32,
    l_translate_step: f32,
    v_translate_step: f32,
    rotate_horizontal: f32,
    rotate_vertical: f32,
    l_rotate_step: f32,
    scroll: f32,
    translate_speed: f32,
    rotate_speed: f32,
    sensitivity: f32,
    camera: Camera,
    mode: CameraMode,
    point_of_focus: Option<Rc<RefCell<Point3<f32>>>>,
    /// Index into the scene's entity list of what this viewport orbits, remembered across mode
    /// switches so returning to `OrbitPoint` resumes the same entity rather than resetting to
    /// the scene default. Per-controller, so two viewports can watch two different drones.
    orbit_target: Option<usize>,
    radius: Option<f32>,
    h_angle: Option<Rad<f32>>,
    v_angle: Option<Rad<f32>>,
    /// Frame source for `CameraMode::Stream`, `None` in every other mode. Set once from scene
    /// JSON via `attach_stream`; there is no path that adds or removes one at runtime.
    stream: Option<TransformStream>,
}

impl CameraController {
    pub fn new(speed: f32, sensitivity: f32, camera: Camera) -> Self {
        Self {
            pressed_keys: HashSet::new(),
            h_translate_step: 0.0,
            l_translate_step: 0.0,
            v_translate_step: 0.0,
            rotate_horizontal: 0.0,
            rotate_vertical: 0.0,
            l_rotate_step: 0.0,
            scroll: 0.0,
            translate_speed: speed,
            rotate_speed: 0.3 * speed,
            sensitivity,
            camera,
            mode: CameraMode::FreeRoam,
            point_of_focus: None,
            orbit_target: None,
            radius: None,
            h_angle: None,
            v_angle: None,
            stream: None,
        }
    }

    pub fn camera(&self) -> &Camera { &self.camera }

    pub fn mode(&self) -> CameraMode { self.mode }

    pub fn orbit_target(&self) -> Option<usize> { self.orbit_target }

    /// Puts this camera under stream control, permanently.
    ///
    /// Attaching the source and entering the mode are one operation so the two cannot disagree —
    /// a camera in `Stream` mode always has a stream, and a stream is never left unread in some
    /// other mode. The scene loader is the only caller.
    pub fn attach_stream(&mut self, stream: TransformStream) {
        self.stream = Some(stream);
        self.mode = CameraMode::Stream;
    }

    /// The registry key this camera reads, if it is stream-driven.
    pub fn stream_source(&self) -> Option<&str> {
        self.stream.as_ref().and_then(|s| s.source_name())
    }

    /// Re-binds a stream-driven camera to the registry after a purge. See
    /// `TransformStream::rebind` for why the surviving viewport needs this and entities do not.
    pub fn rebind_stream(&mut self, registry: &ring_buffer::BufferRegistry) {
        if let Some(ref mut stream) = self.stream {
            stream.rebind(registry);
        }
    }

    fn process_opposite_keys(pressed_keys: &HashSet<KeyCode>, key1: &KeyCode, key2: &KeyCode, key3: &KeyCode, key4: &KeyCode) -> f32 {
        (
            ((pressed_keys.contains(key1) || pressed_keys.contains(key2)) as i32) - 
            ((pressed_keys.contains(key3) || pressed_keys.contains(key4)) as i32)
        ) as f32
    }

    pub fn process_keyboard(&mut self, key: KeyCode, state: ElementState, scene: &Vec<Entity>) -> bool {
        match state {
            ElementState::Pressed => {
                if !self.pressed_keys.contains(&key) {
                    self.pressed_keys.insert(key);
                }

                if key == KeyCode::Enter {
                    self.switch_mode(scene);
                }

                if key == KeyCode::KeyT {
                    self.cycle_orbit_target(scene);
                }

                if key == KeyCode::KeyC {
                    let p = self.camera.position;
                    let r = self.camera.rotation;
                    println!("\"position\": [{}, {}, {}]", p.x, p.y, p.z);
                    println!("\"rotation\": [{}, {}, {}, {}]", r.s, r.v.x, r.v.y, r.v.z);
                }
            }
            ElementState::Released => {
                self.pressed_keys.remove(&key);
            }
        }

        self.h_translate_step = CameraController::process_opposite_keys(
            &self.pressed_keys, 
            &KeyCode::KeyD, 
            &KeyCode::ArrowRight,
            &KeyCode::KeyA,
            &KeyCode::ArrowLeft,
        );

        let w_s_up_down = CameraController::process_opposite_keys(
            &self.pressed_keys,
            &KeyCode::KeyW,
            &KeyCode::ArrowUp,
            &KeyCode::KeyS,
            &KeyCode::ArrowDown,
        );

        let space_shift = CameraController::process_opposite_keys(
            &self.pressed_keys,
            &KeyCode::Space,
            &KeyCode::Space,
            &KeyCode::ShiftLeft,
            &KeyCode::ShiftLeft
        );

        self.l_rotate_step = CameraController::process_opposite_keys(
            &self.pressed_keys,
            &KeyCode::KeyL,
            &KeyCode::KeyL,
            &KeyCode::KeyK,
            &KeyCode::KeyK,
        );

        // Stream leaves both at zero. Nothing reads them in that mode — `update_camera_stream`
        // takes the whole transform off the wire — but zeroing keeps the state honest rather
        // than accumulating input that silently goes nowhere.
        self.l_translate_step = match self.mode {
            CameraMode::FreeRoam => w_s_up_down,
            CameraMode::OrbitPoint => space_shift,
            CameraMode::Stream => 0.0,
        };

        self.v_translate_step = match self.mode {
            CameraMode::FreeRoam => space_shift,
            CameraMode::OrbitPoint => w_s_up_down,
            CameraMode::Stream => 0.0,
        };

        state == ElementState::Pressed
    }

    /// Drops every in-flight input, as if the user had let go of everything.
    ///
    /// Called on the viewport losing focus. Input is routed to one viewport at a time, so a
    /// controller that is focused while a key is held never sees that key's `Released` event
    /// once focus moves on — it would keep the key in `pressed_keys` and translate forever.
    /// The rotate accumulators go too: only `update_camera_freeroam` zeroes them, so a
    /// controller blurred while orbiting would otherwise apply a stale mouse delta as a
    /// one-frame kick whenever it next returned to free roam.
    pub fn release_all_input(&mut self) {
        self.pressed_keys.clear();
        self.h_translate_step = 0.0;
        self.l_translate_step = 0.0;
        self.v_translate_step = 0.0;
        self.l_rotate_step = 0.0;
        self.rotate_horizontal = 0.0;
        self.rotate_vertical = 0.0;
        self.scroll = 0.0;
    }

    pub fn process_mouse(&mut self, mouse_dx: f64, mouse_dz: f64) {
        self.rotate_horizontal = mouse_dx as f32;
        self.rotate_vertical = mouse_dz as f32;
    }

    pub fn process_scroll(&mut self, delta: &MouseScrollDelta) {
        self.scroll = match delta {
            // I'm assuming a line is about 100 pixels
            MouseScrollDelta::LineDelta(_, scroll) => -scroll * 0.5,
            MouseScrollDelta::PixelDelta(PhysicalPosition { y: scroll, .. }) => -*scroll as f32,
        };
    }

    /// What a viewport orbits before the user has chosen anything.
    ///
    /// A moving entity is the better guess for what someone wants to watch on entering orbit
    /// mode, so the first mover wins; a scene of nothing but static geometry falls back to the
    /// first entity. This only picks the *default* — cycling reaches every entity.
    fn default_orbit_target(scene: &Vec<Entity>) -> usize {
        scene.iter().position(|e| e.has_movement_behavior()).unwrap_or(0)
    }

    pub fn switch_mode(&mut self, scene: &Vec<Entity>){
        // OrbitPoint needs something to orbit, and the search below indexes into the scene.
        // With no entities there is nothing to focus on, so stay in the current mode.
        if scene.is_empty() {
            return;
        }

        match self.mode {
            CameraMode::FreeRoam => {
                // Resume this viewport's own target if it still exists — entities are only ever
                // appended, so a live index stays valid, but a scene clear invalidates it.
                let target = self.orbit_target
                    .filter(|i| *i < scene.len())
                    .unwrap_or_else(|| CameraController::default_orbit_target(scene));

                self.mode = CameraMode::OrbitPoint;
                self.orbit_target = Some(target);
                self.point_of_focus = Some(scene[target].get_position());
                self.radius = Some(5.0);
                self.h_angle = Some(Rad(PI));
                self.v_angle = Some(Rad(0.0));

                // self.v_angle = Some(Rad(1.5751947));
                // let forward = (point - self.camera.position).normalize();
                // self.camera.yaw = Rad(forward.z.atan2(forward.x));
            },
            CameraMode::OrbitPoint => {
                self.mode = CameraMode::FreeRoam;
                self.point_of_focus = None;
                self.radius = None;
                self.h_angle = None;
                self.v_angle = None;
                // orbit_target is deliberately kept, so a round trip through free roam comes
                // back to the same entity instead of snapping to the scene default.
            }
            // Locked in both directions: a stream-driven camera's transform comes off the wire,
            // so there is no free-roam or orbit state to switch into, and nothing would ever put
            // it back. Matched here rather than guarded above so the compiler keeps the decision
            // in view if a fourth mode is added.
            CameraMode::Stream => {}
        }
    }

    /// Points this viewport at the next entity, wrapping at the end of the scene.
    ///
    /// Every entity is reachable, not just the ones that move. Static geometry is frequently the
    /// thing worth inspecting — a scene's landmark is often the fixed object and the drone is what
    /// flies past it — and skipping it strands scenes whose only mover is already the target.
    ///
    /// Only meaningful while orbiting, so it is a no-op in free roam rather than silently
    /// changing state the user cannot see. The radius and angles carry over, so the camera keeps
    /// its framing and the new entity simply slides into the position the old one occupied.
    pub fn cycle_orbit_target(&mut self, scene: &Vec<Entity>) {
        if self.mode != CameraMode::OrbitPoint || scene.is_empty() {
            return;
        }

        // A stale index from a cleared scene still lands somewhere valid.
        let next = self.orbit_target.map_or(0, |t| (t + 1) % scene.len());

        self.orbit_target = Some(next);
        self.point_of_focus = Some(scene[next].get_position());
    }

    pub fn update_camera(&mut self, dt: Duration){
        match self.mode {
            CameraMode::FreeRoam => self.update_camera_freeroam(dt),
            CameraMode::OrbitPoint => self.update_camera_orbit(dt),
            CameraMode::Stream => self.update_camera_stream(),
        }
    }

    /// Takes the camera's whole transform from the next streamed frame.
    ///
    /// Takes no `dt`: the stream defines its own pacing at one frame per render frame, exactly as
    /// `ChangeTransform` does, so a slow render loop plays the motion slowly rather than skipping.
    ///
    /// A starved stream holds the last pose. That matches how entities stall, and it is the right
    /// failure for a camera — freezing is readable, whereas snapping to an origin would look like
    /// a crash every time a packet was late.
    fn update_camera_stream(&mut self) {
        let Some(frame) = self.stream.as_mut().and_then(|s| s.next_frame()) else {
            debug!("camera stream starved, holding pose at {:?}", self.camera.position);
            return;
        };

        let (position, rotation) = transform_stream::frame_to_transform(&frame);
        debug!("camera stream: x:{} y:{} z:{}", position.x, position.y, position.z);
        self.camera.position = position;
        self.camera.rotation = rotation;
    }

    fn update_camera_freeroam(&mut self, dt: Duration) {
        let dt = dt.as_secs_f32();

        let true_forward = self.camera.rotation.rotate_vector(Vector3::unit_y()).normalize();
        let true_up = self.camera.rotation.rotate_vector(Vector3::unit_z()).normalize();
        let right = self.camera.rotation.rotate_vector(Vector3::unit_x()).normalize();
        // println!("forward = {:?}, up = {:?}, right = {:?}", forward, up, right);

        let forward = Vector3::<f32>::new(true_forward.x, true_forward.y, 0.0).normalize();
        let up = Vector3::<f32>::new(0.0, 0.0, true_up.z).normalize();

        self.camera.position += forward * (self.l_translate_step) * self.translate_speed * dt;
        self.camera.position += up * (self.v_translate_step) * self.translate_speed * dt;
        self.camera.position += right * (self.h_translate_step) * self.translate_speed * dt;

        // Move in/out (aka. "zoom")
        // Note: this isn't an actual zoom. The camera's position
        // changes when zooming. I've added this to make it easier
        // to get closer to an object you want to focus on.
        // let scrollward =
        //     -1.0 * Vector3::new(pitch_cos * yaw_cos, pitch_cos * yaw_sin, pitch_sin).normalize();
        // // println!("scrollward: ({}, {}, {})", scrollward.x, scrollward.y, scrollward.z);
        // self.camera.position += scrollward * self.scroll * self.translate_speed * self.sensitivity * dt;
        // self.scroll = 0.0;

        // rotate
        let yaw = Quaternion::from_axis_angle(Vector3::unit_z(), Rad(-self.rotate_horizontal) * self.sensitivity * dt);
        let pitch = Quaternion::from_axis_angle(right, Rad(-self.rotate_vertical) * self.sensitivity * dt);
        let roll = Quaternion::from_axis_angle(forward, Rad(-self.l_rotate_step * self.rotate_speed * dt));

        // Apply them in order
        self.camera.rotation = yaw * pitch * roll * self.camera.rotation;

        // If process_mouse isn't called every frame, these values
        // will not get set to zero, and the camera will rotate
        // when moving in a non cardinal direction.
        self.rotate_horizontal = 0.0;
        self.rotate_vertical = 0.0;

        // Keep the camera's angle from going too high/low.
        // if self.camera.pitch < -Rad(SAFE_FRAC_PI_2) {
        //     self.camera.pitch = -Rad(SAFE_FRAC_PI_2);
        // } else if self.camera.pitch > Rad(SAFE_FRAC_PI_2) {
        //     self.camera.pitch = Rad(SAFE_FRAC_PI_2);
        // }

        debug!("new camera position: ({}, {}, {})", self.camera.position[0], self.camera.position[1], self.camera.position[2]);
        info!("new camera rotation: {:?}", self.camera.rotation);
    }

    fn update_camera_orbit(&mut self, dt: Duration){
        // unwrap data
        let point_option = self.point_of_focus.as_ref().map(|rc| rc.borrow());
        let target = *point_option.expect("Error: camera is attempting to orbit a point that does not exist");
        let mut h_angle = self.h_angle.unwrap();
        let mut v_angle = self.v_angle.unwrap();
        let mut radius = self.radius.unwrap();
        let dt = dt.as_secs_f32();

        // update the radius based on forward/backward movement
        // we subtract from the radius (ie forward = smaller radius, backward = larger radius)
        radius -= self.l_translate_step * self.translate_speed * dt;
        // radius += self.scroll * self.translate_speed * self.sensitivity * dt;
        // self.scroll = 0.0;

        // update the roll
        // let roll_step = Rad(self.l_rotate_step) * self.rotate_speed * dt;

        let h_angle_step_base = self.h_translate_step * self.translate_speed/radius * dt;
        let v_angle_step_base = self.v_translate_step * self.translate_speed/radius * dt;
        let h_angle_step = Rad(h_angle_step_base);
        let v_angle_step = Rad(v_angle_step_base);
        // println!("h base = {}, v base = {}, h step = {}, v step = {}", h_angle_step_base, v_angle_step_base, h_angle_step.0, v_angle_step.0);
        // println!("magnitude check: {} - {} = {}", h_angle_step_base, h_angle_step.0 + v_angle_step.0, h_angle_step_base - (h_angle_step.0 + v_angle_step.0));

        h_angle += h_angle_step;
        v_angle += v_angle_step;
        // println!("radius = {}; angles = ({}π, {}π)", radius, h_angle.0 / PI, v_angle.0 / PI);

        let (sin_h, cos_h) = h_angle.0.sin_cos();
        let (sin_v, cos_v) = v_angle.0.sin_cos();
        
        let offset = Vector3::new(
            radius * cos_h * cos_v,
            radius * sin_h * cos_v,
            radius * sin_v,
        );
        // println!("new offset: ({}, {}, {})", offset[0], offset[1], offset[2]);

        self.camera.position = target + offset;
        self.radius = Some(radius);
        self.h_angle = Some(h_angle);
        self.v_angle = Some(v_angle);

        // self.camera.rotation = Quaternion::look_at(forward, Vector3::unit_z());
        // println!("new camera rotation: {:?}", self.camera.rotation);
        let forward = -offset.normalize();
        let up_world = Vector3::unit_z();
        let mut right = forward.cross(up_world);
        if right.magnitude2() < APPROX_ZERO {
            let alt_up = Vector3::unit_x();
            right = forward.cross(alt_up);
        }

        right = right.normalize();
        let up: Vector3<f32> = right.cross(forward);
        let rot_mat = Matrix3::from_cols(right, forward, up);
        let q = Quaternion::from(rot_mat).normalize();
        self.camera.rotation = q;

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

    fn test_controller() -> CameraController {
        let camera = Camera::new((0.0, 0.0, 0.0), Quaternion::new(1.0, 0.0, 0.0, 0.0));
        CameraController::new(8.0, 1.0, camera)
    }

    /// Builds a scene from `(name, x, moves)` triples. Entities are spread along x so that a
    /// camera's position identifies which one it settled on.
    fn test_scene(spec: &[(&str, f32, bool)]) -> Vec<Entity> {
        spec.iter()
            .map(|(name, x, moves)| {
                let behavior = moves.then(||
                    Behavior::new(BehaviorType::Translate, vec![0.0, 0.0, 0.0], None)
                );
                Entity::for_test(name, Point3::new(*x, 0.0, 0.0), behavior)
            })
            .collect()
    }

    /// Where the camera ends up once an orbit frame has been applied.
    fn orbit_position(controller: &mut CameraController) -> Point3<f32> {
        controller.update_camera(Duration::from_millis(16));
        controller.camera().position
    }

    #[test]
    fn switch_mode_ignores_empty_scene() {
        let mut controller = test_controller();
        assert_eq!(controller.mode(), CameraMode::FreeRoam);

        // Must not panic: the OrbitPoint branch indexes scene[0] to pick a focus target.
        controller.switch_mode(&vec![]);

        assert_eq!(
            controller.mode(),
            CameraMode::FreeRoam,
            "a scene with no entities has nothing to orbit, so the mode must not change",
        );
    }

    // Input is routed to one viewport at a time, so a controller that loses focus with a key
    // held never receives that key's Released event. Without the explicit release it would
    // translate forever, on a viewport the user is no longer even addressing.
    #[test]
    fn a_blurred_controller_stops_moving() {
        let mut controller = test_controller();
        let dt = Duration::from_millis(16);
        controller.process_keyboard(KeyCode::KeyW, ElementState::Pressed, &vec![]);

        // premise: the held key does drive movement, so the assertion below means something
        controller.update_camera(dt);
        assert_ne!(controller.camera().position, Point3::new(0.0, 0.0, 0.0));

        let drifted_to = controller.camera().position;
        controller.release_all_input();
        controller.update_camera(dt);

        assert!(controller.pressed_keys.is_empty(), "the held key must not survive the blur");
        assert_eq!(
            controller.camera().position, drifted_to,
            "a viewport that lost focus mid-keypress must stop where it was",
        );
    }

    // Only update_camera_freeroam zeroes the rotate accumulators, so a controller blurred while
    // orbiting would keep the last mouse delta and apply it as a kick on returning to free roam.
    #[test]
    fn release_all_input_clears_pending_rotation() {
        let mut controller = test_controller();
        controller.process_mouse(5.0, -3.0);
        assert_ne!(controller.rotate_horizontal, 0.0);

        controller.release_all_input();

        assert_eq!(controller.rotate_horizontal, 0.0);
        assert_eq!(controller.rotate_vertical, 0.0);
        assert_eq!(controller.scroll, 0.0);
    }

    // The point of a per-controller orbit target: two viewports can watch two different drones.
    // Previously every OrbitPoint camera resolved the same scene-global first mover.
    #[test]
    fn two_viewports_orbit_independently() {
        let scene = test_scene(&[("drone1", 0.0, true), ("drone2", 100.0, true)]);
        let mut left = test_controller();
        let mut right = test_controller();

        left.switch_mode(&scene);
        right.switch_mode(&scene);
        assert_eq!(left.orbit_target(), Some(0), "both default to the first mover");
        assert_eq!(right.orbit_target(), Some(0));

        right.cycle_orbit_target(&scene);

        assert_eq!(right.orbit_target(), Some(1));
        assert_eq!(left.orbit_target(), Some(0), "cycling one viewport must not move the other");

        // the targets are 100 apart, so proximity is proof the focus point actually rebound
        // rather than only the index changing
        assert!((orbit_position(&mut left) - Point3::new(0.0, 0.0, 0.0)).magnitude() < 6.0);
        assert!((orbit_position(&mut right) - Point3::new(100.0, 0.0, 0.0)).magnitude() < 6.0);
    }

    // A viewport's target is its own setting, not a transient of the current mode — dipping into
    // free roam to reposition and coming back should not silently retarget the scene default.
    #[test]
    fn orbit_target_survives_a_round_trip_through_free_roam() {
        let scene = test_scene(&[("drone1", 0.0, true), ("drone2", 100.0, true)]);
        let mut controller = test_controller();

        controller.switch_mode(&scene);
        controller.cycle_orbit_target(&scene);
        assert_eq!(controller.orbit_target(), Some(1));

        controller.switch_mode(&scene); // back to free roam
        controller.switch_mode(&scene); // and into orbit again

        assert_eq!(controller.orbit_target(), Some(1));
        assert!((orbit_position(&mut controller) - Point3::new(100.0, 0.0, 0.0)).magnitude() < 6.0);
    }

    // The shape of golden_gate_scene.json: one static landmark, one drone. Cycling must reach the
    // landmark — it is the more interesting thing to orbit, and restricting candidates to movers
    // left T with nowhere to go and no visible effect at all.
    #[test]
    fn cycling_reaches_static_entities() {
        let scene = test_scene(&[("Golden Gate Bridge", 0.0, false), ("Blizzard", 100.0, true)]);
        let mut controller = test_controller();

        controller.switch_mode(&scene);
        assert_eq!(controller.orbit_target(), Some(1), "the mover is still the default");

        controller.cycle_orbit_target(&scene);
        assert_eq!(controller.orbit_target(), Some(0), "the static landmark is reachable");
        assert!((orbit_position(&mut controller) - Point3::new(0.0, 0.0, 0.0)).magnitude() < 6.0);

        controller.cycle_orbit_target(&scene);
        assert_eq!(controller.orbit_target(), Some(1), "and it wraps back around");
    }

    // Cycling walks the scene in order, wrapping at the end.
    #[test]
    fn cycling_wraps_through_every_entity() {
        let scene = test_scene(&[
            ("prop", 0.0, false),
            ("drone1", 100.0, true),
            ("drone2", 200.0, true),
        ]);
        let mut controller = test_controller();

        controller.switch_mode(&scene);
        assert_eq!(controller.orbit_target(), Some(1), "first mover is the default");

        for expected in [2, 0, 1, 2] {
            controller.cycle_orbit_target(&scene);
            assert_eq!(controller.orbit_target(), Some(expected));
        }
    }

    // With nothing moving there is still something to look at: the default falls back to the
    // first entity, preserving what the old scene-global search did.
    #[test]
    fn a_scene_of_static_entities_still_orbits() {
        let scene = test_scene(&[("prop_a", 0.0, false), ("prop_b", 100.0, false)]);
        let mut controller = test_controller();

        controller.switch_mode(&scene);
        assert_eq!(controller.orbit_target(), Some(0));

        controller.cycle_orbit_target(&scene);
        assert_eq!(controller.orbit_target(), Some(1));
    }

    // T is meaningless outside orbit mode. Acting anyway would change state the user cannot see,
    // so a later Enter would drop them onto an entity they never chose.
    #[test]
    fn cycling_is_a_no_op_in_free_roam() {
        let scene = test_scene(&[("drone1", 0.0, true), ("drone2", 100.0, true)]);
        let mut controller = test_controller();

        controller.cycle_orbit_target(&scene);

        assert_eq!(controller.mode(), CameraMode::FreeRoam);
        assert_eq!(controller.orbit_target(), None);
    }

    /// A controller already under stream control, fed from `frames`.
    fn streamed_controller(frames: &[Vec<f32>]) -> CameraController {
        let mut controller = test_controller();
        let values: Vec<f32> = frames.iter().flatten().copied().collect();
        controller.attach_stream(TransformStream::from_values(values));
        controller
    }

    /// A frame placing the camera at `x` with no rotation.
    fn frame_at(x: f32) -> Vec<f32> {
        let mut frame = vec![0.0; crate::transform_stream::DATA_ARR_WIDTH];
        frame[0] = x;
        frame
    }

    #[test]
    fn a_streamed_camera_takes_its_transform_from_the_frame() {
        let mut controller = streamed_controller(&[frame_at(10.0), frame_at(20.0)]);
        assert_eq!(controller.mode(), CameraMode::Stream, "attaching a stream enters the mode");

        controller.update_camera(Duration::from_millis(16));
        assert_eq!(controller.camera().position, Point3::new(10.0, 0.0, 0.0));

        controller.update_camera(Duration::from_millis(16));
        assert_eq!(controller.camera().position, Point3::new(20.0, 0.0, 0.0));
    }

    // Freezing is readable; snapping to an origin would look like a crash every late packet.
    #[test]
    fn a_starved_stream_holds_the_last_pose() {
        let mut controller = streamed_controller(&[frame_at(10.0)]);
        let dt = Duration::from_millis(16);

        controller.update_camera(dt);
        controller.update_camera(dt);
        controller.update_camera(dt);

        assert_eq!(controller.camera().position, Point3::new(10.0, 0.0, 0.0));
    }

    // The mode is declared in scene JSON and locked. Enter must not take a stream camera out of
    // it, or the transform would freeze at whatever frame happened to be showing.
    #[test]
    fn stream_mode_cannot_be_switched_out_of() {
        let scene = test_scene(&[("drone1", 0.0, true)]);
        let mut controller = streamed_controller(&[frame_at(10.0)]);

        controller.switch_mode(&scene);
        assert_eq!(controller.mode(), CameraMode::Stream);

        controller.process_keyboard(KeyCode::Enter, ElementState::Pressed, &scene);
        assert_eq!(controller.mode(), CameraMode::Stream, "Enter is inert here");
    }

    // Keyboard input reaches a focused stream viewport but must not move it — the wire is the
    // only authority on where this camera is.
    #[test]
    fn input_does_not_move_a_streamed_camera() {
        let scene = test_scene(&[("drone1", 0.0, true)]);
        let mut controller = streamed_controller(&[frame_at(10.0)]);
        let dt = Duration::from_millis(16);

        controller.update_camera(dt);
        controller.process_keyboard(KeyCode::KeyW, ElementState::Pressed, &scene);
        controller.process_mouse(50.0, 50.0);
        controller.update_camera(dt);

        assert_eq!(
            controller.camera().position, Point3::new(10.0, 0.0, 0.0),
            "held keys and mouse drags must not displace a stream-driven camera",
        );
    }

    #[test]
    fn camera_mode_display_names() {
        assert_eq!(CameraMode::FreeRoam.display_name(), "Free Roam");
        assert_eq!(CameraMode::OrbitPoint.display_name(), "Orbit Point");
        assert_eq!(CameraMode::Stream.display_name(), "Stream");
    }

    #[test]
    fn only_stream_mode_refuses_input() {
        assert!(CameraMode::FreeRoam.accepts_input());
        assert!(CameraMode::OrbitPoint.accepts_input());
        assert!(!CameraMode::Stream.accepts_input());
    }
}
