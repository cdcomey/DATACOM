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
}

impl CameraMode {
    /// Human-readable name, for on-screen messages.
    pub fn display_name(&self) -> &'static str {
        match self {
            CameraMode::FreeRoam => "Free Roam",
            CameraMode::OrbitPoint => "Orbit Point",
        }
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
    radius: Option<f32>,
    h_angle: Option<Rad<f32>>,
    v_angle: Option<Rad<f32>>,
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
            radius: None,
            h_angle: None,
            v_angle: None,
        }
    }

    pub fn camera(&self) -> &Camera { &self.camera }

    pub fn mode(&self) -> CameraMode { self.mode }

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

        self.l_translate_step = match self.mode {
            CameraMode::FreeRoam => w_s_up_down,
            CameraMode::OrbitPoint => space_shift,
        };

        self.v_translate_step = match self.mode {
            CameraMode::FreeRoam => space_shift,
            CameraMode::OrbitPoint => w_s_up_down,
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

    pub fn switch_mode(&mut self, scene: &Vec<Entity>){
        // OrbitPoint needs something to orbit, and the search below indexes scene[0].
        // With no entities there is nothing to focus on, so stay in the current mode.
        if scene.is_empty() {
            return;
        }

        match self.mode {
            CameraMode::FreeRoam => {
                self.mode = CameraMode::OrbitPoint;
                let target = scene.iter().find(|e| e.has_movement_behavior()).unwrap_or(&scene[0]);
                self.point_of_focus = Some(target.get_position());
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
            }
        }
    }

    pub fn update_camera(&mut self, dt: Duration){
        match self.mode {
            CameraMode::FreeRoam => self.update_camera_freeroam(dt),
            CameraMode::OrbitPoint => self.update_camera_orbit(dt),
        }
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

    fn test_controller() -> CameraController {
        let camera = Camera::new((0.0, 0.0, 0.0), Quaternion::new(1.0, 0.0, 0.0, 0.0));
        CameraController::new(8.0, 1.0, camera)
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

    #[test]
    fn camera_mode_display_names() {
        assert_eq!(CameraMode::FreeRoam.display_name(), "Free Roam");
        assert_eq!(CameraMode::OrbitPoint.display_name(), "Orbit Point");
    }
}
