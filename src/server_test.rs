use std::net::{UdpSocket, SocketAddr};
use std::fs::{self, File};
use std::path::Path;
use log::{debug, info};
use std::thread;
use std::time::Duration;
use std::io::Read;
use std::sync::Arc;

use uuid::Uuid;

use crate::com::{
    MAX_FILE_NAME_BYTE_WIDTH, MessageType, ServerCommand, encode_command, get_ports,
    scene_declares_authority,
};

/// Environment variable that schedules a `CLEAR_SCENE` in test mode: a whole number of seconds
/// after this server finishes its own transmission.
///
/// An environment variable rather than an argument because test mode already consumes its trailing
/// argument as the record-video flag and its middle ones as the JSON list, leaving nowhere to put
/// this without changing what an existing command line means.
const CLEAR_SCENE_DELAY_VAR: &str = "DATACOM_TEST_CLEAR_AFTER_SECS";

/// Path to a JSON file of timed camera commands, relative to the repo root.
///
/// The file is an array of `{ "after": <seconds>, "command": <name>, "payload": <object> }`, each
/// sent that many seconds after this server's own `TRANSMISSION_END`. The payload is passed to the
/// client verbatim, so this exercises the real decoder rather than a test-only shortcut — a
/// payload the client would reject is a payload this rejects too, in the same way.
const CAMERA_COMMAND_VAR: &str = "DATACOM_TEST_CAMERA_COMMANDS";

/// Comma-separated stream names to feed generated camera poses to: position first, rotation
/// second. `DATACOM_TEST_CAMERA_STREAMS=cam_pos.bin,cam_rot.bin` gives a `Streamed` camera on
/// either half something to read.
const CAMERA_STREAM_VAR: &str = "DATACOM_TEST_CAMERA_STREAMS";

/// Radius and rate of the generated camera path. Wide enough to hold a whole test scene in frame,
/// slow enough to watch.
const CAMERA_STREAM_RADIUS: f32 = 15.0;
const CAMERA_STREAM_RATE: f32 = 0.4;
/// Pace of the generated camera stream, matched to a 60fps client so one frame is consumed per
/// rendered frame rather than piling up in the ring buffer.
const CAMERA_STREAM_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone)]
pub enum StreamMode {
    File(String),
    Generated,
}

fn send_finite_test_data(socket: &UdpSocket, path_str: &str, addr: SocketAddr) {
    let full_path = format!("data/scene_loading/{}", path_str);
    let path = std::path::Path::new(&full_path);
    let test_command_data_main = fs::read_to_string(path).unwrap();
    let data_len = test_command_data_main.len();

    let file_id = *Uuid::new_v4().as_bytes();
    let file_name_string = format!("{}_main_scene.json", addr.port());
    let file_name_base = file_name_string.as_str();
    let file_name_length = file_name_base.len() as u8;
    let mut file_name = [0u8; MAX_FILE_NAME_BYTE_WIDTH];
    file_name[0..file_name_length as usize].copy_from_slice(file_name_base.as_bytes());
    let file_len = test_command_data_main.len() as u32;

    let mut test_command_data: Vec<u8> = Vec::new();
    test_command_data.extend_from_slice(&MessageType::FILE_START.to_be_bytes());
    test_command_data.extend_from_slice(&file_id);
    test_command_data.extend_from_slice(&[file_name_length]);
    test_command_data.extend_from_slice(&file_name);
    test_command_data.extend_from_slice(&file_len.to_be_bytes());
    test_command_data.extend_from_slice(&[1u8]);

    info!("{:?}: Sending file start frame to stream", file_id);
    debug!("{:?}", test_command_data);

    thread::sleep(Duration::from_millis(10));
    socket.send(&test_command_data).unwrap();

    let mut chunk_offset = 0u64;
    let chunk_length_default = 1024u32;
    while (chunk_offset as usize) < data_len {
        test_command_data.clear();
        test_command_data.extend_from_slice(&MessageType::FILE_CHUNK.to_be_bytes());
        test_command_data.extend_from_slice(&file_id);
        test_command_data.extend_from_slice(&chunk_offset.to_be_bytes());

        let chunk_offset_usize = chunk_offset as usize;

        let chunk_length: u32 = if chunk_offset_usize + chunk_length_default as usize > data_len {
            (data_len - chunk_offset_usize).try_into().unwrap()
        } else {
            chunk_length_default
        };

        test_command_data.extend_from_slice(&chunk_length.to_be_bytes());
        let max_bound = chunk_offset_usize + chunk_length as usize;
        debug!("indexing data from {} to {} out of {}", chunk_offset, max_bound, data_len);
        let payload = test_command_data_main[chunk_offset_usize..max_bound].as_bytes();
        test_command_data.extend_from_slice(payload);
        chunk_offset += chunk_length as u64;

        let checksum = crc32fast::hash(payload);
        test_command_data.extend_from_slice(&checksum.to_be_bytes());

        debug!("{:?}: Sending finite chunk to stream: {:?}", file_id, test_command_data);
        thread::sleep(Duration::from_millis(10));
        socket.send(&test_command_data).unwrap();
    }

    test_command_data.clear();
    test_command_data.extend_from_slice(&MessageType::FILE_END.to_be_bytes());
    test_command_data.extend_from_slice(&file_id);

    info!("{:?}: Sending file end to stream", file_id);
    debug!("{:?}", test_command_data);
    thread::sleep(Duration::from_millis(10));
    socket.send(&test_command_data).unwrap();

    test_command_data.clear();
    test_command_data.extend_from_slice(&MessageType::TRANSMISSION_END.to_be_bytes());

    info!("{:?}: Sending transmission end to stream", file_id);
    debug!("{:?}", test_command_data);
    thread::sleep(Duration::from_millis(10));
    socket.send(&test_command_data).unwrap();
}

// Sends FILE_START then streams chunks from next_chunk until it returns None,
// then sends FILE_END and TRANSMISSION_END. For an infinite generator,
// next_chunk never returns None so the end frames are never sent.
fn send_streaming_data(
    socket: &UdpSocket,
    file_name_base: &str,
    is_definite: u8,
    file_len: u32,
    mut next_chunk: impl FnMut() -> Option<Vec<u8>>,
) {
    let file_id = *Uuid::new_v4().as_bytes();
    let file_name_length = file_name_base.len() as u8;
    let mut file_name = [0u8; MAX_FILE_NAME_BYTE_WIDTH];
    file_name[0..file_name_length as usize].copy_from_slice(file_name_base.as_bytes());

    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&MessageType::FILE_START.to_be_bytes());
    buf.extend_from_slice(&file_id);
    buf.extend_from_slice(&[file_name_length]);
    buf.extend_from_slice(&file_name);
    buf.extend_from_slice(&file_len.to_be_bytes());
    buf.extend_from_slice(&[is_definite]);

    info!("S: Sending file start frame to stream");
    debug!("{:?}", buf);
    thread::sleep(Duration::from_millis(10));
    socket.send(&buf).unwrap();

    let mut chunk_offset = 0u64;
    loop {
        let Some(payload) = next_chunk() else {
            debug!("chunk source exhausted");
            break;
        };

        let checksum = crc32fast::hash(&payload);
        buf.clear();
        buf.extend_from_slice(&MessageType::FILE_CHUNK.to_be_bytes());
        buf.extend_from_slice(&file_id);
        buf.extend_from_slice(&chunk_offset.to_be_bytes());
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&payload);
        buf.extend_from_slice(&checksum.to_be_bytes());
        chunk_offset += payload.len() as u64;

        info!("S: Sending chunk to stream");
        debug!("{:?}", buf);
        thread::sleep(Duration::from_millis(10));
        socket.send(&buf).unwrap();
    }

    buf.clear();
    buf.extend_from_slice(&MessageType::FILE_END.to_be_bytes());
    buf.extend_from_slice(&file_id);

    info!("S: Sending file end to stream");
    thread::sleep(Duration::from_millis(10));
    socket.send(&buf).unwrap();

    buf.clear();
    buf.extend_from_slice(&MessageType::TRANSMISSION_END.to_be_bytes());

    info!("S: Sending transmission end to stream");
    thread::sleep(Duration::from_millis(10));
    socket.send(&buf).unwrap();
}

/// How long to wait before issuing `CLEAR_SCENE`, or `None` to never issue one.
///
/// Unset means never, deliberately: a clear that fired on every test run would wipe the scene a
/// person is trying to look at, and the emptied window is indistinguishable from a stream that
/// stopped arriving. A malformed value is reported rather than treated as zero — silently clearing
/// immediately is the worst reading of a typo.
fn clear_scene_delay() -> Option<Duration> {
    let raw = std::env::var(CLEAR_SCENE_DELAY_VAR).ok()?;
    match raw.trim().parse::<u64>() {
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) => {
            info!("ignoring {}={:?}: expected a whole number of seconds", CLEAR_SCENE_DELAY_VAR, raw);
            None
        }
    }
}

/// Sends one `CLEAR_SCENE` on a timer from its own thread.
///
/// A thread rather than a sleep in the streaming loop because that loop never returns for a
/// generated source — there is no later point in it to reach. The socket is already connected to
/// the client, so this shares it rather than opening another.
///
/// Sent once. A scene is transmitted once per stream, so nothing re-populates what the clear
/// removes and a repeat would land on an already-empty scene.
fn spawn_clear_scene_timer(socket: Arc<UdpSocket>, delay: Duration) {
    spawn_command_timer(socket, delay, ServerCommand::CLEAR_SCENE, Vec::new());
}

/// Sends one command on a timer from its own thread.
///
/// A thread rather than a sleep in the streaming loop because that loop never returns for a
/// generated source — there is no later point in it to reach. The socket is already connected to
/// the client, so this shares it rather than opening another.
fn spawn_command_timer(
    socket: Arc<UdpSocket>,
    delay: Duration,
    command: ServerCommand,
    payload: Vec<u8>,
) {
    thread::Builder::new()
        .name(format!("server {:?} timer", command))
        .spawn(move || {
            thread::sleep(delay);
            info!("S: sending {:?}", command);
            if let Err(e) = socket.send(&encode_command(command, &payload)) {
                info!("S: {:?} was not sent: {}", command, e);
            }
        })
        .unwrap();
}

/// Maps a name in the command script onto its wire command.
///
/// Spelled out rather than derived from the enum so the script file reads as prose and a typo is
/// reported against the list of what it could have meant.
fn command_from_name(name: &str) -> Option<ServerCommand> {
    match name {
        "clear_scene" => Some(ServerCommand::CLEAR_SCENE),
        "update_camera_position_behavior" => Some(ServerCommand::UPDATE_CAMERA_POSITION_BEHAVIOR),
        "update_camera_rotation_behavior" => Some(ServerCommand::UPDATE_CAMERA_ROTATION_BEHAVIOR),
        "update_camera_position" => Some(ServerCommand::UPDATE_CAMERA_POSITION),
        "update_camera_rotation" => Some(ServerCommand::UPDATE_CAMERA_ROTATION),
        _ => None,
    }
}

/// Schedules every command in the script named by `DATACOM_TEST_CAMERA_COMMANDS`.
///
/// Delays are measured from this server's own `TRANSMISSION_END`, matching the clear timer, so a
/// command always lands in the live phase however long the initial transfer took.
fn schedule_camera_commands(socket: &Arc<UdpSocket>, scene_path: &str) {
    let Ok(path) = std::env::var(CAMERA_COMMAND_VAR) else { return };

    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) => {
            info!("S: cannot read {}={:?}: {}", CAMERA_COMMAND_VAR, path, e);
            return;
        }
    };

    let script: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(script) => script,
        Err(e) => {
            info!("S: {:?} is not valid JSON: {}", path, e);
            return;
        }
    };

    let Some(steps) = script.as_array() else {
        info!("S: {:?} must be an array of commands", path);
        return;
    };

    // Same reasoning as the clear timer: the client would ignore these anyway, and the reason
    // belongs on the side that has to change.
    if !scene_holds_authority(scene_path) {
        info!(
            "S: not scheduling {} camera command(s) — {} does not set \"authority\": true, so the \
             client would ignore them",
            steps.len(), scene_path,
        );
        return;
    }

    for (i, step) in steps.iter().enumerate() {
        let Some(command) = step["command"].as_str().and_then(command_from_name) else {
            info!(
                "S: skipping command {} of {:?}: {:?} is not one of clear_scene, \
                 update_camera_position_behavior, update_camera_rotation_behavior, \
                 update_camera_position, update_camera_rotation",
                i, path, step["command"],
            );
            continue;
        };

        let delay = Duration::from_secs_f64(step["after"].as_f64().unwrap_or(0.0));
        let payload = step["payload"].to_string().into_bytes();

        info!("S: {:?} scheduled for {:?} from transmission end", command, delay);
        spawn_command_timer(Arc::clone(socket), delay, command, payload);
    }
}

fn scene_holds_authority(scene_path: &str) -> bool {
    let full_path = format!("data/scene_loading/{}", scene_path);
    fs::read(&full_path).map(|scene| scene_declares_authority(&scene)).unwrap_or(false)
}

/// One transform frame in the wire layout: 12 big-endian f32, position in slots 0-2 and the
/// quaternion's vector part in slots 6-8.
fn transform_frame(position: [f32; 3], rotation_vector: [f32; 3]) -> Vec<u8> {
    let values = [
        position[0], position[1], position[2],
        0.0, 0.0, 0.0,
        rotation_vector[0], rotation_vector[1], rotation_vector[2],
        0.0, 0.0, 0.0,
    ];
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// A camera circling the origin and looking inward, as position and rotation frames.
///
/// The scalar part is dropped on the wire and reconstructed by the client as `sqrt(1 - |v|²)`,
/// which is always non-negative — so a quaternion with a negative scalar is negated first. `q` and
/// `-q` name the same rotation, so this loses nothing, but sending the negative-scalar half
/// unchanged would have the client reconstruct a different rotation entirely.
fn camera_pose(t: f32) -> ([f32; 3], [f32; 3]) {
    use cgmath::{InnerSpace, Matrix3, Quaternion, Vector3};

    let angle = t * CAMERA_STREAM_RATE;
    let position = [
        CAMERA_STREAM_RADIUS * angle.cos(),
        CAMERA_STREAM_RADIUS * angle.sin(),
        CAMERA_STREAM_RADIUS * 0.4,
    ];

    let forward = -Vector3::new(position[0], position[1], position[2]).normalize();
    let right = forward.cross(Vector3::unit_z()).normalize();
    let up = right.cross(forward);
    let q = Quaternion::from(Matrix3::from_cols(right, forward, up)).normalize();
    let q = if q.s < 0.0 { -q } else { q };

    (position, [q.v.x, q.v.y, q.v.z])
}

/// Feeds generated camera poses to the stream names in `DATACOM_TEST_CAMERA_STREAMS`.
///
/// One thread per name, sharing the socket: each datagram carries one whole frame and is sent
/// atomically, so the client demultiplexes them by file id exactly as it does two drones'.
fn spawn_camera_streams(socket: &Arc<UdpSocket>) {
    let Ok(names) = std::env::var(CAMERA_STREAM_VAR) else { return };

    let names: Vec<String> = names
        .split(',')
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();

    for (half, name) in names.into_iter().enumerate() {
        let socket = Arc::clone(socket);
        // Slot 0 is the position stream and slot 1 the rotation stream. They are separate names
        // because a single buffer read by two consumers is drained by whichever gets there first,
        // so the two halves would each see every other frame.
        let is_position = half == 0;

        thread::Builder::new()
            .name(format!("server camera stream ({})", name))
            .spawn(move || {
                info!("S: streaming generated camera {} to {}",
                    if is_position { "positions" } else { "rotations" }, name);
                let mut frame: u64 = 0;
                send_streaming_data(&socket, &name, 0, 0, || {
                    let (position, rotation) = camera_pose(frame as f32 * CAMERA_STREAM_INTERVAL.as_secs_f32());
                    frame += 1;
                    thread::sleep(CAMERA_STREAM_INTERVAL);
                    Some(if is_position {
                        transform_frame(position, [0.0; 3])
                    } else {
                        transform_frame([0.0; 3], rotation)
                    })
                });
            })
            .unwrap();
    }
}

/// Schedules the clear only if this server's own scene claims authority, mirroring the rule the
/// client enforces.
///
/// Sending it regardless would work — the client ignores commands from a stream without authority
/// — but the symptom of a missing declaration would then be a scene that simply does not clear,
/// with the explanation only in the client's log. Checked here so the reason is visible on the
/// side that has to be fixed.
fn schedule_clear_scene(socket: &Arc<UdpSocket>, scene_path: &str) {
    let Some(delay) = clear_scene_delay() else { return };

    if scene_holds_authority(scene_path) {
        info!("S: {} holds command authority; CLEAR_SCENE in {:?}", scene_path, delay);
        spawn_clear_scene_timer(Arc::clone(socket), delay);
    } else {
        info!(
            "S: not scheduling CLEAR_SCENE — {} does not set \"authority\": true, so the client \
             would ignore the command",
            scene_path,
        );
    }
}

pub fn create_server_thread(
    file: String,
    json_file_paths: Vec<String>,
    mode: StreamMode,
) -> Result<Vec<thread::JoinHandle<()>>, std::io::Error> {
    let ports = get_ports(file.as_str()).unwrap();
    info!("Spawning {} server thread(s)", json_file_paths.len());

    let mut handles = Vec::new();

    for (i, json_file_path) in json_file_paths.into_iter().enumerate() {
        let addr = ports[i];
        let stream_name = format!("entity_pos_{:02}", i);
        let mode_i = mode.clone();

        let handle = thread::Builder::new()
            .name(format!("server thread ({})", json_file_path))
            .spawn(move || {
                info!("Server thread binding UDP socket to {} for {}", addr, json_file_path);
                let socket = Arc::new(UdpSocket::bind(addr).unwrap());

                // Block until the client sends its ACK datagram; learn the client address from it
                let mut ack_buf = [0u8; 16];
                let (_, client_addr) = match socket.recv_from(&mut ack_buf) {
                    Ok(r) => r,
                    Err(e) => {
                        info!("Server thread recv_from error for {}: {}", json_file_path, e);
                        return;
                    }
                };
                info!("Received ACK from {} on {} for {}", client_addr, addr, json_file_path);
                socket.connect(client_addr).unwrap();
                socket.set_read_timeout(Some(Duration::from_millis(5))).unwrap();

                // Listener thread: receives incoming messages from the client (e.g. retransmit
                // requests) and logs them. Mirrors the client's create_listener_thread pattern.
                let listener_socket = Arc::clone(&socket);
                thread::Builder::new()
                    .name(format!("server listener ({})", json_file_path))
                    .spawn(move || {
                        let mut buf = vec![0u8; 64];
                        loop {
                            match listener_socket.recv(&mut buf) {
                                Ok(n) if n >= 2 && u16::from_be_bytes([buf[0], buf[1]]) == 3 => {
                                    info!("S: received REQUEST_RETRANSMIT_CHUNK (not retransmitting)");
                                }
                        Ok(n) if n >= 2 && u16::from_be_bytes([buf[0], buf[1]]) == 5 => {
                                    info!("S: received TRANSMISSION_ACK");
                                }
                                Ok(_) => {}
                                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                                           || e.kind() == std::io::ErrorKind::TimedOut => {}
                                Err(e) => {
                                    debug!("S: server listener ended: {}", e);
                                    break;
                                }
                            }
                        }
                    })
                    .unwrap();

                send_finite_test_data(&*socket, &json_file_path, addr);

                // Timed from here rather than from the thread starting, so the delay is measured
                // against the live phase the client will actually honour a command in: anything
                // sent before this server's TRANSMISSION_END is dropped as initial-transfer noise.
                schedule_clear_scene(&socket, &json_file_path);
                schedule_camera_commands(&socket, &json_file_path);
                spawn_camera_streams(&socket);

                thread::sleep(Duration::from_secs(1));

                match mode_i {
                    StreamMode::File(path) => {
                        let full_path = format!("data/object_loading/{}", path);
                        let mut file = File::open(&full_path).unwrap();
                        let mut buffer = vec![0u8; 1024];
                        send_streaming_data(&*socket, &stream_name, 0, 0, || {
                            let n = file.read(&mut buffer).unwrap();
                            if n == 0 { return None; }
                            Some(buffer[..n].to_vec())
                        });
                    }
                    StreamMode::Generated => {
                        use rand::Rng;
                        const BYTES_PER_FRAME: usize = 12 * 4;
                        const CHUNK_FRAMES: usize = 1;
                        let mut rng = rand::thread_rng();
                        let speed = rng.gen_range(0.02f64..0.15);
                        let raw = [
                            rng.gen_range(-1.0..1.0f64),
                            rng.gen_range(-1.0..1.0),
                            rng.gen_range(-1.0..1.0),
                        ];
                        let mag = (raw[0]*raw[0] + raw[1]*raw[1] + raw[2]*raw[2]).sqrt();
                        let dir = [raw[0]/mag, raw[1]/mag, raw[2]/mag];
                        let mut frame: u64 = 0;
                        send_streaming_data(&*socket, &stream_name, 0, 0, || {
                            let mut chunk = Vec::with_capacity(CHUNK_FRAMES * BYTES_PER_FRAME);
                            for _ in 0..CHUNK_FRAMES {
                                let t = frame as f64 * speed;
                                let x = (dir[0] * t) as f32;
                                let y = (dir[1] * t) as f32;
                                let z = (dir[2] * t) as f32;
                                for v in [x, y, z, 0.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0] {
                                    chunk.extend_from_slice(&v.to_be_bytes());
                                }
                                frame += 1;
                            }
                            Some(chunk)
                        });
                    }
                }
            })
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "Thread spawn failed"))?;

        handles.push(handle);
    }

    Ok(handles)
}
