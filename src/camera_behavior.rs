//! What moves a camera and what aims it, as two independent choices.
//!
//! A viewport's camera has exactly one `PositionBehavior` and exactly one `RotationBehavior`, and
//! the two never consult each other. That orthogonality is the point: a camera can orbit a point
//! while looking at a drone, or fly a straight line while spinning on its own axis, without either
//! combination being a mode someone had to think to add.
//!
//! Both are declared in scene JSON and can be replaced at runtime by the master server — see
//! `Scene::apply_camera_command`. The JSON decoders here serve both paths, so a behavior means the
//! same thing however it arrived.

use cgmath::*;
use std::time::Duration;

use crate::behaviors_and_entities::{self, Entity};
use crate::camera::{Camera, CameraInput};
use crate::ring_buffer;
use crate::transform_stream::{self, TransformStream};

/// Squared-magnitude floor below which a vector has no usable direction.
const APPROX_ZERO: f32 = 1e-8;

/// Radians per second of roll applied by `K`/`L` under `UserControlled`.
///
/// A constant rather than a JSON field: it reproduces the old free-roam roll rate exactly
/// (`0.3 × the default camera speed of 8`), and roll is a trim control rather than something a
/// scene author tunes. `sensitivity` stays what it has always been — the mouse-look scale.
const ROLL_SPEED: f32 = 2.4;

/// Where a camera is, each frame.
#[derive(Debug)]
pub enum PositionBehavior {
    /// Position read from a streamed `.bin`, one frame per rendered frame.
    Streamed { source: String, stream: TransformStream },
    /// Constant-velocity travel along a fixed world-space direction.
    Linear { speed: f32, direction: Vector3<f32> },
    /// Constant-speed travel around `point` at a fixed radius.
    ///
    /// `axis` is derived once, at install, from the requested direction of travel — see
    /// `PositionBehavior::prepare`. The radius is re-read from the camera's own position every
    /// frame rather than tracked separately, so a `update_camera_position` that displaces the
    /// camera is absorbed rather than fought.
    Orbit { speed: f32, point: Point3<f32>, distance: f32, axis: Vector3<f32> },
    /// Driven by the user's keyboard. The only position behavior that reads input at all.
    FreeRoam { speed: f32 },
    /// Held at a fixed world-space offset from an entity, moving with it.
    ///
    /// The offset is world-space deliberately: an entity-relative one would whip the camera around
    /// every time the entity spun.
    TrackingEntity { path: String, distance: f32, direction: Vector3<f32> },
}

/// Where a camera looks, each frame.
#[derive(Debug)]
pub enum RotationBehavior {
    /// Rotation read from a streamed `.bin`, one frame per rendered frame.
    Streamed { source: String, stream: TransformStream },
    FocusedOnEntity { path: String },
    FocusedOnPoint(Point3<f32>),
    /// Driven by the user's mouse and roll keys. The only rotation behavior that reads input.
    UserControlled { sensitivity: f32 },
    /// Constant-rate spin about a **camera-local** axis, where `+y` is forward and `+z` is up, so
    /// spinning about the camera's own up-axis is `[0, 0, 1]`.
    AboutOwnAxis { axis: Vector3<f32>, speed: f32 },
}

/// Rejects a direction with no usable orientation rather than letting a zero vector normalize to
/// NaN and poison every matrix downstream. Magnitude is otherwise ignored — every direction in a
/// camera behavior is a pure direction, with rate carried by its own `speed` field.
fn unit(raw: &[f32], field: &str) -> Result<Vector3<f32>, String> {
    let v = Vector3::new(raw[0], raw[1], raw[2]);
    if v.magnitude2() < APPROX_ZERO {
        return Err(format!("{} has no direction (zero-length vector)", field));
    }
    Ok(v.normalize())
}

/// Reads a fixed-length float array, so a short one is reported rather than panicking on index.
fn floats(json: &serde_json::Value, field: &str, expected: usize) -> Result<Vec<f32>, String> {
    let array = json[field]
        .as_array()
        .ok_or_else(|| format!("missing \"{}\"", field))?;

    if array.len() != expected {
        return Err(format!(
            "\"{}\" needs {} numbers, found {}", field, expected, array.len(),
        ));
    }

    array.iter()
        .map(|v| v.as_f64().map(|f| f as f32).ok_or_else(|| format!("\"{}\" is not numeric", field)))
        .collect()
}

fn float(json: &serde_json::Value, field: &str) -> Result<f32, String> {
    json[field]
        .as_f64()
        .map(|v| v as f32)
        .ok_or_else(|| format!("missing or non-numeric \"{}\"", field))
}

fn string(json: &serde_json::Value, field: &str) -> Result<String, String> {
    json[field]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing or non-string \"{}\"", field))
}

fn point(json: &serde_json::Value, field: &str) -> Result<Point3<f32>, String> {
    let v = floats(json, field, 3)?;
    Ok(Point3::new(v[0], v[1], v[2]))
}

/// The rotation that puts `target` in front of a camera at `from`, with world `+z` as up.
///
/// `None` when the two coincide: there is no direction to look in, and the caller holds its
/// current rotation rather than snapping to an arbitrary one.
fn look_at_rotation(from: Point3<f32>, target: Point3<f32>) -> Option<Quaternion<f32>> {
    let forward = target - from;
    if forward.magnitude2() < APPROX_ZERO {
        return None;
    }
    let forward = forward.normalize();

    // Looking straight up or down leaves no horizontal component to take a right vector from;
    // any perpendicular will do, so long as it is not the zero vector.
    let mut right = forward.cross(Vector3::unit_z());
    if right.magnitude2() < APPROX_ZERO {
        right = forward.cross(Vector3::unit_x());
    }
    let right = right.normalize();
    let up = right.cross(forward);

    // Columns are the camera's own basis: x right, y forward, z up.
    Some(Quaternion::from(Matrix3::from_cols(right, forward, up)).normalize())
}

impl PositionBehavior {
    /// Decodes one `position_behavior` object, from scene JSON or from a command payload.
    pub fn from_json(
        json: &serde_json::Value,
        registry: &ring_buffer::BufferRegistry,
    ) -> Result<PositionBehavior, String> {
        let kind = string(json, "type")?;

        match kind.as_str() {
            "Streamed" => {
                let source = string(json, "stream")?;
                let stream = TransformStream::from_registry(&source, registry);
                Ok(PositionBehavior::Streamed { source, stream })
            }
            "Linear" => Ok(PositionBehavior::Linear {
                speed: float(json, "speed")?,
                direction: unit(&floats(json, "direction", 3)?, "direction")?,
            }),
            "Orbit" => {
                let distance = float(json, "distance")?;
                if distance <= 0.0 {
                    return Err(format!("\"distance\" must be positive, found {}", distance));
                }
                Ok(PositionBehavior::Orbit {
                    speed: float(json, "speed")?,
                    point: point(json, "point")?,
                    distance,
                    // Replaced by `prepare`, which is the only place that knows where the camera
                    // is and can therefore turn a direction of travel into an orbital axis.
                    axis: unit(&floats(json, "direction", 3)?, "direction")?,
                })
            }
            "FreeRoam" => Ok(PositionBehavior::FreeRoam { speed: float(json, "speed")? }),
            "TrackingEntity" => {
                let distance = float(json, "distance")?;
                if distance < 0.0 {
                    return Err(format!("\"distance\" must not be negative, found {}", distance));
                }
                Ok(PositionBehavior::TrackingEntity {
                    path: string(json, "entity")?,
                    distance,
                    direction: unit(&floats(json, "direction", 3)?, "direction")?,
                })
            }
            other => Err(format!("unknown position behavior type {:?}", other)),
        }
    }

    /// Settles the geometry that depends on where the camera is standing, before the first frame,
    /// returning the position the camera must be moved to (if any) for the behavior to hold.
    ///
    /// Only `Orbit` has any: the JSON gives a *direction of travel*, and the orbital axis it
    /// implies depends on which side of `point` the camera is on. Deriving the axis once and
    /// keeping it — rather than recomputing it from the tangent each frame — is what keeps the
    /// path a circle instead of letting rounding walk it into a spiral.
    ///
    /// **Call this exactly once per behavior.** It replaces the stored direction of travel with
    /// the axis derived from it, so a second call would read that axis as a direction and derive
    /// a different orbit from it. Returning the snap rather than applying it is what lets a
    /// command validate a behavior before installing it, without needing a second call to commit.
    pub fn prepare(&mut self, position: Point3<f32>) -> Result<Option<Point3<f32>>, String> {
        let PositionBehavior::Orbit { point, distance, axis, .. } = self else {
            return Ok(None);
        };

        let radial = position - *point;
        if radial.magnitude2() < APPROX_ZERO {
            return Err("camera sits exactly on the orbit point, so there is no orbit to run".into());
        }
        let radial = radial.normalize();

        let normal = radial.cross(*axis);
        if normal.magnitude2() < APPROX_ZERO {
            return Err(
                "orbit direction points along the radius, which defines no orbital plane; give a \
                 direction of travel across the orbit rather than toward or away from its centre"
                    .into(),
            );
        }

        *axis = normal.normalize();
        // The declared distance is authoritative on install, so a scene gets the orbit it asked
        // for rather than whatever radius the camera's start position happened to imply.
        Ok(Some(*point + radial * *distance))
    }

    pub fn apply(
        &mut self,
        camera: &mut Camera,
        input: &CameraInput,
        dt: Duration,
        entities: &[Entity],
    ) {
        let dt = dt.as_secs_f32();

        match self {
            // Takes no `dt`: the stream sets its own pace at one frame per rendered frame, exactly
            // as an entity's `ChangeTransform` does, so a slow render loop plays the motion slowly
            // rather than skipping through it. A starved stream holds the last pose.
            PositionBehavior::Streamed { stream, .. } => {
                if let Some(frame) = stream.next_frame() {
                    camera.position = transform_stream::frame_to_transform(&frame).0;
                }
            }

            PositionBehavior::Linear { speed, direction } => {
                camera.position += *direction * *speed * dt;
            }

            PositionBehavior::Orbit { speed, point, distance, axis } => {
                let radial = camera.position - *point;
                if radial.magnitude2() < APPROX_ZERO {
                    return;
                }
                let step = Rad(*speed / *distance * dt);
                let rotated = Quaternion::from_axis_angle(*axis, step).rotate_vector(radial.normalize());
                camera.position = *point + rotated * *distance;
            }

            PositionBehavior::FreeRoam { speed } => {
                let forward = camera.rotation().rotate_vector(Vector3::unit_y());
                let right = camera.rotation().rotate_vector(Vector3::unit_x()).normalize();

                // Flattened, so looking at the ground and walking forward does not fly you into
                // it. Vertical movement is Space/Shift and stays on the world axis.
                let mut flat = Vector3::new(forward.x, forward.y, 0.0);
                flat = if flat.magnitude2() < APPROX_ZERO { Vector3::unit_y() } else { flat.normalize() };

                camera.position += flat * input.forward_step * *speed * dt;
                camera.position += Vector3::unit_z() * input.up_step * *speed * dt;
                camera.position += right * input.strafe_step * *speed * dt;
            }

            // A target that resolves to nothing leaves the camera where it is. That covers an
            // entity cleared out from under it and one whose stream has not connected yet, and
            // holding still is the readable failure for both.
            PositionBehavior::TrackingEntity { path, distance, direction } => {
                if let Some(target) = behaviors_and_entities::world_position(entities, path) {
                    camera.position = target + *direction * *distance;
                }
            }
        }
    }

    /// Re-derives the offset so the camera keeps the position it was just given.
    ///
    /// `update_camera_position` on a tracking camera would otherwise be undone on the very next
    /// frame, since the offset it was holding still describes the old position.
    pub fn retarget_from_position(
        &mut self,
        camera: &Camera,
        entities: &[Entity],
    ) -> Result<(), String> {
        let PositionBehavior::TrackingEntity { path, distance, direction } = self else {
            return Ok(());
        };

        let target = behaviors_and_entities::world_position(entities, path)
            .ok_or_else(|| format!("entity {:?} is not in the scene", path))?;

        let offset = camera.position - target;
        if offset.magnitude2() < APPROX_ZERO {
            return Err(format!(
                "the new position is exactly on entity {:?}, which leaves no direction to track \
                 it from", path,
            ));
        }

        *distance = offset.magnitude();
        *direction = offset.normalize();
        Ok(())
    }

    /// Adopts the camera's current radius, for a position command that displaced an orbit.
    ///
    /// Snapping back to the declared distance would make the command look ignored; the orbit
    /// simply continues at whatever radius it was moved to.
    pub fn adopt_current_radius(&mut self, camera: &Camera) {
        if let PositionBehavior::Orbit { point, distance, .. } = self {
            let radius = (camera.position - *point).magnitude();
            if radius > 0.0 {
                *distance = radius;
            }
        }
    }

    pub fn accepts_input(&self) -> bool {
        matches!(self, PositionBehavior::FreeRoam { .. })
    }

    pub fn stream_source(&self) -> Option<&str> {
        match self {
            PositionBehavior::Streamed { source, .. } => Some(source),
            _ => None,
        }
    }

    /// The entity path this behavior needs, so the scene can report one that names nothing.
    pub fn entity_path(&self) -> Option<&str> {
        match self {
            PositionBehavior::TrackingEntity { path, .. } => Some(path),
            _ => None,
        }
    }

    /// Empties the backing buffer without unbinding it, so frames arriving after the clear still
    /// land. Nothing to do for the behaviors that have no stream.
    pub fn clear_stream(&mut self) {
        if let PositionBehavior::Streamed { stream, .. } = self {
            stream.clear_buffer();
        }
    }

    pub fn rebind(&mut self, registry: &ring_buffer::BufferRegistry) {
        if let PositionBehavior::Streamed { stream, .. } = self {
            stream.rebind(registry);
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            PositionBehavior::Streamed { source, .. } => format!("Streamed({})", source),
            PositionBehavior::Linear { .. } => "Linear".to_string(),
            PositionBehavior::Orbit { .. } => "Orbit".to_string(),
            PositionBehavior::FreeRoam { .. } => "Free Roam".to_string(),
            PositionBehavior::TrackingEntity { path, .. } => format!("Tracking {}", path),
        }
    }
}

impl RotationBehavior {
    pub fn from_json(
        json: &serde_json::Value,
        registry: &ring_buffer::BufferRegistry,
    ) -> Result<RotationBehavior, String> {
        let kind = string(json, "type")?;

        match kind.as_str() {
            "Streamed" => {
                let source = string(json, "stream")?;
                let stream = TransformStream::from_registry(&source, registry);
                Ok(RotationBehavior::Streamed { source, stream })
            }
            "FocusedOnEntity" => Ok(RotationBehavior::FocusedOnEntity {
                path: string(json, "entity")?,
            }),
            "FocusedOnPoint" => Ok(RotationBehavior::FocusedOnPoint(point(json, "point")?)),
            "UserControlled" => Ok(RotationBehavior::UserControlled {
                sensitivity: float(json, "sensitivity")?,
            }),
            "AboutOwnAxis" => Ok(RotationBehavior::AboutOwnAxis {
                axis: unit(&floats(json, "axis", 3)?, "axis")?,
                speed: float(json, "speed")?,
            }),
            other => Err(format!("unknown rotation behavior type {:?}", other)),
        }
    }

    /// Runs **after** the position behavior every frame. The two look-at variants aim from wherever
    /// the camera ended up, so aiming first would leave them one frame stale.
    pub fn apply(
        &mut self,
        camera: &mut Camera,
        input: &CameraInput,
        dt: Duration,
        entities: &[Entity],
    ) {
        let dt_secs = dt.as_secs_f32();

        match self {
            RotationBehavior::Streamed { stream, .. } => {
                if let Some(frame) = stream.next_frame() {
                    camera.set_rotation(transform_stream::frame_to_transform(&frame).1);
                }
            }

            RotationBehavior::FocusedOnEntity { path } => {
                if let Some(target) = behaviors_and_entities::world_position(entities, path) {
                    if let Some(rotation) = look_at_rotation(camera.position, target) {
                        camera.set_rotation(rotation);
                    }
                }
            }

            RotationBehavior::FocusedOnPoint(target) => {
                if let Some(rotation) = look_at_rotation(camera.position, *target) {
                    camera.set_rotation(rotation);
                }
            }

            RotationBehavior::UserControlled { sensitivity } => {
                let right = camera.rotation().rotate_vector(Vector3::unit_x()).normalize();
                let forward = camera.rotation().rotate_vector(Vector3::unit_y()).normalize();

                let yaw = Quaternion::from_axis_angle(
                    Vector3::unit_z(),
                    Rad(-input.rotate_horizontal) * *sensitivity * dt_secs,
                );
                let pitch = Quaternion::from_axis_angle(
                    right,
                    Rad(-input.rotate_vertical) * *sensitivity * dt_secs,
                );
                let roll = Quaternion::from_axis_angle(
                    forward,
                    Rad(-input.roll_step * ROLL_SPEED * dt_secs),
                );

                camera.set_rotation((yaw * pitch * roll * camera.rotation()).normalize());
            }

            // Right-multiplied, which is what makes the axis camera-local: `[0, 0, 1]` spins the
            // camera about its own up-axis wherever it happens to be pointing.
            RotationBehavior::AboutOwnAxis { axis, speed } => {
                let step = Quaternion::from_axis_angle(*axis, Rad(*speed * dt_secs));
                camera.set_rotation((camera.rotation() * step).normalize());
            }
        }
    }

    pub fn accepts_input(&self) -> bool {
        matches!(self, RotationBehavior::UserControlled { .. })
    }

    pub fn stream_source(&self) -> Option<&str> {
        match self {
            RotationBehavior::Streamed { source, .. } => Some(source),
            _ => None,
        }
    }

    pub fn entity_path(&self) -> Option<&str> {
        match self {
            RotationBehavior::FocusedOnEntity { path } => Some(path),
            _ => None,
        }
    }

    pub fn clear_stream(&mut self) {
        if let RotationBehavior::Streamed { stream, .. } = self {
            stream.clear_buffer();
        }
    }

    pub fn rebind(&mut self, registry: &ring_buffer::BufferRegistry) {
        if let RotationBehavior::Streamed { stream, .. } = self {
            stream.rebind(registry);
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            RotationBehavior::Streamed { source, .. } => format!("Streamed({})", source),
            RotationBehavior::FocusedOnEntity { path } => format!("Focused on {}", path),
            RotationBehavior::FocusedOnPoint(_) => "Focused on Point".to_string(),
            RotationBehavior::UserControlled { .. } => "User Controlled".to_string(),
            RotationBehavior::AboutOwnAxis { .. } => "About Own Axis".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).unwrap()
    }

    fn registry() -> ring_buffer::BufferRegistry {
        ring_buffer::new_registry()
    }

    fn camera_at(x: f32, y: f32, z: f32) -> Camera {
        Camera::new((x, y, z), Quaternion::new(1.0, 0.0, 0.0, 0.0))
    }

    fn orbit(direction: [f32; 3], from: (f32, f32, f32)) -> (PositionBehavior, Camera) {
        let mut behavior = PositionBehavior::from_json(
            &json(&format!(
                r#"{{"type": "Orbit", "speed": 1.0, "direction": [{}, {}, {}],
                     "point": [0, 0, 0], "distance": 5.0}}"#,
                direction[0], direction[1], direction[2],
            )),
            &registry(),
        ).unwrap();
        let mut camera = camera_at(from.0, from.1, from.2);
        if let Some(snap) = behavior.prepare(camera.position).unwrap() {
            camera.position = snap;
        }
        (behavior, camera)
    }

    // The worked example from the design: a camera south of the origin told to travel along its
    // own right vector should orbit counter-clockwise in the horizontal plane, i.e. move +x first.
    #[test]
    fn orbit_travels_in_the_direction_it_was_given() {
        let (mut behavior, mut camera) = orbit([1.0, 0.0, 0.0], (0.0, -5.0, 0.0));

        behavior.apply(&mut camera, &CameraInput::default(), Duration::from_millis(100), &[]);

        assert!(camera.position.x > 0.0, "should have moved +x, went to {:?}", camera.position);
        assert!(camera.position.z.abs() < 1e-5, "should stay in the horizontal plane");
    }

    // Reversing the direction of travel has to reverse the orbit rather than mirror it into some
    // other plane — the axis is derived from a cross product, and a sign error there is invisible
    // until someone watches the camera go the wrong way round.
    #[test]
    fn orbit_reverses_with_its_direction() {
        let (mut behavior, mut camera) = orbit([-1.0, 0.0, 0.0], (0.0, -5.0, 0.0));

        behavior.apply(&mut camera, &CameraInput::default(), Duration::from_millis(100), &[]);

        assert!(camera.position.x < 0.0, "should have moved -x, went to {:?}", camera.position);
    }

    #[test]
    fn orbit_holds_its_radius_over_a_full_revolution() {
        // one revolution: speed/distance rad per second, so 2π·distance/speed seconds
        let (mut behavior, mut camera) = orbit([1.0, 0.0, 0.0], (0.0, -5.0, 0.0));
        let step = Duration::from_millis(10);

        for _ in 0..3000 {
            behavior.apply(&mut camera, &CameraInput::default(), step, &[]);
            let radius = (camera.position - Point3::new(0.0, 0.0, 0.0)).magnitude();
            assert!((radius - 5.0).abs() < 1e-3, "radius drifted to {}", radius);
        }
    }

    // The declared distance is what the scene asked for; starting the camera somewhere else must
    // not silently give it a different orbit.
    #[test]
    fn preparing_an_orbit_snaps_to_the_declared_distance() {
        let (_, camera) = orbit([1.0, 0.0, 0.0], (0.0, -50.0, 0.0));

        assert_eq!(camera.position, Point3::new(0.0, -5.0, 0.0));
    }

    // Travelling along the radius defines no plane. Left unguarded the cross product is zero and
    // normalizing it yields NaN, which spreads to every matrix the camera touches.
    #[test]
    fn an_orbit_along_its_own_radius_is_rejected() {
        let mut behavior = PositionBehavior::from_json(
            &json(r#"{"type": "Orbit", "speed": 1.0, "direction": [0, 1, 0],
                      "point": [0, 0, 0], "distance": 5.0}"#),
            &registry(),
        ).unwrap();

        assert!(behavior.prepare(Point3::new(0.0, -5.0, 0.0)).is_err());
    }

    #[test]
    fn an_orbit_from_its_own_centre_is_rejected() {
        let mut behavior = PositionBehavior::from_json(
            &json(r#"{"type": "Orbit", "speed": 1.0, "direction": [1, 0, 0],
                      "point": [0, 0, 0], "distance": 5.0}"#),
            &registry(),
        ).unwrap();

        assert!(behavior.prepare(Point3::new(0.0, 0.0, 0.0)).is_err());
    }

    // A position command that displaces an orbiting camera adopts the new radius rather than
    // snapping back, so the command is visibly honoured.
    #[test]
    fn an_orbit_adopts_a_commanded_radius() {
        let (mut behavior, mut camera) = orbit([1.0, 0.0, 0.0], (0.0, -5.0, 0.0));
        camera.position = Point3::new(0.0, -20.0, 0.0);

        behavior.adopt_current_radius(&camera);
        behavior.apply(&mut camera, &CameraInput::default(), Duration::from_millis(100), &[]);

        let radius = (camera.position - Point3::new(0.0, 0.0, 0.0)).magnitude();
        assert!((radius - 20.0).abs() < 1e-3, "radius should stay at 20, was {}", radius);
    }

    #[test]
    fn linear_travel_ignores_the_magnitude_of_its_direction() {
        let mut behavior = PositionBehavior::from_json(
            &json(r#"{"type": "Linear", "speed": 2.0, "direction": [100.0, 0.0, 0.0]}"#),
            &registry(),
        ).unwrap();
        let mut camera = camera_at(0.0, 0.0, 0.0);

        behavior.apply(&mut camera, &CameraInput::default(), Duration::from_secs(1), &[]);

        assert!((camera.position.x - 2.0).abs() < 1e-5, "2 units in 1s, got {}", camera.position.x);
    }

    #[test]
    fn a_zero_direction_is_rejected_rather_than_normalized_to_nan() {
        let result = PositionBehavior::from_json(
            &json(r#"{"type": "Linear", "speed": 2.0, "direction": [0.0, 0.0, 0.0]}"#),
            &registry(),
        );

        assert!(result.is_err());
    }

    #[test]
    fn unknown_behavior_types_are_reported_rather_than_defaulted() {
        assert!(PositionBehavior::from_json(&json(r#"{"type": "Teleport"}"#), &registry()).is_err());
        assert!(RotationBehavior::from_json(&json(r#"{"type": "Wobble"}"#), &registry()).is_err());
    }

    // Malformed JSON must come back as an error the loader can report, not a panic on a missing
    // index — a scene file is written by hand and a three-element array with two entries is the
    // ordinary typo.
    #[test]
    fn a_short_vector_is_reported_rather_than_panicking() {
        let result = PositionBehavior::from_json(
            &json(r#"{"type": "Linear", "speed": 2.0, "direction": [1.0, 0.0]}"#),
            &registry(),
        );

        assert!(result.is_err());
    }

    #[test]
    fn look_at_aims_the_cameras_forward_axis_at_its_target() {
        let rotation = look_at_rotation(
            Point3::new(0.0, -5.0, 0.0),
            Point3::new(0.0, 0.0, 0.0),
        ).unwrap();

        let forward = rotation.rotate_vector(Vector3::unit_y());
        assert!((forward - Vector3::new(0.0, 1.0, 0.0)).magnitude() < 1e-5, "forward was {:?}", forward);
    }

    // Looking straight down leaves no horizontal component to build a right vector from, and the
    // obvious cross product collapses to zero.
    #[test]
    fn look_at_survives_a_target_directly_below() {
        let rotation = look_at_rotation(
            Point3::new(0.0, 0.0, 10.0),
            Point3::new(0.0, 0.0, 0.0),
        ).unwrap();

        let forward = rotation.rotate_vector(Vector3::unit_y());
        assert!((forward - Vector3::new(0.0, 0.0, -1.0)).magnitude() < 1e-5, "forward was {:?}", forward);
    }

    #[test]
    fn look_at_a_coincident_target_yields_nothing() {
        assert!(look_at_rotation(Point3::new(1.0, 2.0, 3.0), Point3::new(1.0, 2.0, 3.0)).is_none());
    }

    // Camera-local, so the spin follows wherever the camera is pointing rather than a world axis.
    #[test]
    fn about_own_axis_spins_in_the_cameras_own_frame() {
        let mut behavior = RotationBehavior::from_json(
            &json(r#"{"type": "AboutOwnAxis", "axis": [0, 0, 1], "speed": 1.5708}"#),
            &registry(),
        ).unwrap();
        // rolled onto its side: the camera's own up-axis is world -y
        let mut camera = Camera::new(
            (0.0, 0.0, 0.0),
            Quaternion::from_axis_angle(Vector3::unit_y(), Rad(std::f32::consts::FRAC_PI_2)),
        );
        let before = camera.rotation().rotate_vector(Vector3::unit_z());

        behavior.apply(&mut camera, &CameraInput::default(), Duration::from_secs(1), &[]);

        let after = camera.rotation().rotate_vector(Vector3::unit_z());
        assert!(
            (after - before).magnitude() < 1e-4,
            "spinning about the local up-axis must leave that axis fixed: {:?} -> {:?}", before, after,
        );
    }

    #[test]
    fn only_free_roam_and_user_controlled_accept_input() {
        let r = registry();
        let accepts = |raw: &str| PositionBehavior::from_json(&json(raw), &r).unwrap().accepts_input();

        assert!(accepts(r#"{"type": "FreeRoam", "speed": 8.0}"#));
        assert!(!accepts(r#"{"type": "Linear", "speed": 1.0, "direction": [1, 0, 0]}"#));
        assert!(!accepts(r#"{"type": "Streamed", "stream": "a.bin"}"#));

        let accepts = |raw: &str| RotationBehavior::from_json(&json(raw), &r).unwrap().accepts_input();
        assert!(accepts(r#"{"type": "UserControlled", "sensitivity": 0.4}"#));
        assert!(!accepts(r#"{"type": "FocusedOnPoint", "point": [0, 0, 0]}"#));
        assert!(!accepts(r#"{"type": "AboutOwnAxis", "axis": [0, 0, 1], "speed": 1.0}"#));
    }
}
