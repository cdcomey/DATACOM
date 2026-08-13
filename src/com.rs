use std::io::Write;
use std::fmt;
// use tokio;
// use tokio::time::sleep;
use std::net::{ToSocketAddrs, UdpSocket, IpAddr, SocketAddr};
// use std::error::Error;
use std::sync::mpsc::{Sender, Receiver};
use std::thread;
use std::fs::{self, File, OpenOptions};
use std::time::Duration;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use crate::ring_buffer;
use std::path::Path;
use toml::Value;
use log::{debug, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use uuid::Uuid;

const MESSAGE_TYPE_BYTE_WIDTH: usize = 2;
const FILE_ID_BYTE_WIDTH: usize = 16;
const FILE_NAME_LENGTH_BYTE_WIDTH: usize = 1;
// TODO: only public for test file; change later
pub const MAX_FILE_NAME_BYTE_WIDTH: usize = u8::max_value() as usize;
const FILE_LENGTH_BYTE_WIDTH: usize = 4;
const IS_DEFINITE_FILE_BYTE_WIDTH: usize = 1;
const FILE_START_METADATA_BYTE_WIDTH: usize = 
    MESSAGE_TYPE_BYTE_WIDTH + 
    FILE_ID_BYTE_WIDTH + 
    FILE_NAME_LENGTH_BYTE_WIDTH + 
    MAX_FILE_NAME_BYTE_WIDTH + 
    FILE_LENGTH_BYTE_WIDTH + 
    IS_DEFINITE_FILE_BYTE_WIDTH;

const CHUNK_OFFSET_BYTE_WIDTH: usize = 8;
const CHUNK_LENGTH_BYTE_WIDTH: usize = 4;
const CHUNK_METADATA_BYTE_WIDTH: usize = MESSAGE_TYPE_BYTE_WIDTH + FILE_ID_BYTE_WIDTH + CHUNK_OFFSET_BYTE_WIDTH + CHUNK_LENGTH_BYTE_WIDTH;
const FILE_END_METADATA_BYTE_WIDTH: usize = MESSAGE_TYPE_BYTE_WIDTH + FILE_ID_BYTE_WIDTH;
const RETRANSMIT_REQUEST_BYTE_WIDTH: usize = MESSAGE_TYPE_BYTE_WIDTH + FILE_ID_BYTE_WIDTH + CHUNK_OFFSET_BYTE_WIDTH;
const COMMAND_ID_BYTE_WIDTH: usize = 2;
const COMMAND_PAYLOAD_LENGTH_BYTE_WIDTH: usize = 4;
const COMMAND_HEADER_BYTE_WIDTH: usize =
    MESSAGE_TYPE_BYTE_WIDTH + COMMAND_ID_BYTE_WIDTH + COMMAND_PAYLOAD_LENGTH_BYTE_WIDTH;
/// Ceiling on a command's declared payload length.
///
/// A command payload is a short JSON object — a camera name and a behavior — so anything at this
/// scale is a corrupt length field rather than a real frame. Without the cap, a bogus length makes
/// the assembly thread wait out its full timeout for bytes that will never come, and then retry
/// forever on the same header.
const MAX_COMMAND_PAYLOAD_BYTE_WIDTH: usize = 64 * 1024;
const CHECKSUM_WIDTH: usize = 4;

const SECONDS_UNTIL_TIMEOUT: u64 = 30;
const TIMEOUT_THRESHOLD: Duration = Duration::from_secs(SECONDS_UNTIL_TIMEOUT);
const MAX_CHUNK_TRANSMIT_ATTEMPTS: u8 = 5;

// ports.toml lists the servers we *may* connect to, not the ones that necessarily exist, and a
// drone can come online at any point in a session. Unanswered ports therefore stay open for the
// whole run rather than being retired. An unbound port refuses instantly, so the re-ACK is paced
// rather than retried on every refusal — otherwise each unused port pins a core. This interval
// also bounds how long a server that just came up waits before it hears from us.
const ACK_RETRY_INTERVAL: Duration = Duration::from_millis(500);

// How long an idle assembly thread blocks waiting on its listener before looping again. Long
// enough that a silent stream costs nothing, short enough to add no real latency to a live one.
const ASSEMBLY_POLL_INTERVAL: Duration = Duration::from_millis(5);
static FILE_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

// Variant names mirror the wire-protocol constants in README.md's Message Type Reference rather
// than Rust casing, so grepping either one finds the other.
// TODO: only public for test file; change later
#[allow(non_camel_case_types)]
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    FILE_START,
    FILE_CHUNK,
    FILE_END,
    REQUEST_RETRANSMIT_CHUNK,
    TRANSMISSION_END,
    TRANSMISSION_ACK,
    COMMAND,
    ERROR,
}

impl MessageType {
    fn get_from_bytes(value: u16) -> Self {
        match value {
            0 => MessageType::FILE_START,
            1 => MessageType::FILE_CHUNK,
            2 => MessageType::FILE_END,
            3 => MessageType::REQUEST_RETRANSMIT_CHUNK,
            4 => MessageType::TRANSMISSION_END,
            5 => MessageType::TRANSMISSION_ACK,
            6 => MessageType::COMMAND,
            _ => MessageType::ERROR,
        }
    }

    // the sole encoder for the message type field — every frame we build goes through here so the
    // discriminants in this enum stay the single source of truth for the wire codes
    pub fn to_be_bytes(self) -> [u8; MESSAGE_TYPE_BYTE_WIDTH] {
        (self as u16).to_be_bytes()
    }
}

/// An operation the scene-wide state, rather than one file's transfer, is subject to.
///
/// Carried in a `COMMAND` frame's payload rather than as its own `MessageType` so that adding one
/// costs a value here instead of a top-level protocol code.
#[allow(non_camel_case_types)]
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerCommand {
    CLEAR_SCENE,
    UPDATE_CAMERA_POSITION_BEHAVIOR,
    UPDATE_CAMERA_ROTATION_BEHAVIOR,
    UPDATE_CAMERA_POSITION,
    UPDATE_CAMERA_ROTATION,
    /// Any code this build does not recognise. Kept as a value rather than an error so a newer
    /// server talking to an older client is ignored loudly instead of desynchronising the stream.
    UNKNOWN,
}

impl ServerCommand {
    fn get_from_bytes(value: u16) -> Self {
        match value {
            0 => ServerCommand::CLEAR_SCENE,
            1 => ServerCommand::UPDATE_CAMERA_POSITION_BEHAVIOR,
            2 => ServerCommand::UPDATE_CAMERA_ROTATION_BEHAVIOR,
            3 => ServerCommand::UPDATE_CAMERA_POSITION,
            4 => ServerCommand::UPDATE_CAMERA_ROTATION,
            _ => ServerCommand::UNKNOWN,
        }
    }

    pub fn to_be_bytes(self) -> [u8; COMMAND_ID_BYTE_WIDTH] {
        (self as u16).to_be_bytes()
    }
}

/// Builds the `COMMAND` frame that carries `command` and its JSON `payload`.
///
/// The sole construction site for a command frame, mirroring `MessageType::to_be_bytes`: the frame
/// is small enough that hand-assembling it at each sender looks harmless, and a sender that laid
/// the fields out differently would decode as a valid frame carrying the wrong command rather
/// than as an error.
///
/// The length field is what lets a client skip a command it does not understand. Without it, a
/// newer server's command would leave its payload sitting in the buffer to be misread as the next
/// frame's message type — so the length is written even for the commands that take no arguments.
pub fn encode_command(command: ServerCommand, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(COMMAND_HEADER_BYTE_WIDTH + payload.len());
    frame.extend_from_slice(&MessageType::COMMAND.to_be_bytes());
    frame.extend_from_slice(&command.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

// object to be used in assembler thread's hash maps
// stores information about a file in the middle of being transmitted across a stream
// contains basic info, a Box for an arbitrary amount of data stored
#[derive(Debug)]
pub struct FileInfo {
    id: Uuid,
    name_length: u8,
    name: [u8; MAX_FILE_NAME_BYTE_WIDTH],
    is_definite: bool,
    length: u32, // this probably isn't necessary, but it may become useful in the future
    data: Box<[u8]>,
    next_expected_chunk_offset: u64,
    reorder_buffer: BTreeMap<u64, Vec<u8>>,
}

impl FileInfo {
    fn new(buf: &Vec<u8>) -> Self {
        let mut counter = 0usize;

        // we don't actually need the message type in FileInfo
        let _: [u8; MESSAGE_TYPE_BYTE_WIDTH] = buf[counter..counter+MESSAGE_TYPE_BYTE_WIDTH].try_into().unwrap();
        counter += MESSAGE_TYPE_BYTE_WIDTH;

        let id_bytes: [u8; FILE_ID_BYTE_WIDTH] = buf[counter..counter+FILE_ID_BYTE_WIDTH].try_into().unwrap();
        let id = Uuid::from_bytes(id_bytes);
        counter += FILE_ID_BYTE_WIDTH;

        let name_length_bytes: [u8; FILE_NAME_LENGTH_BYTE_WIDTH] = buf[counter..counter+FILE_NAME_LENGTH_BYTE_WIDTH].try_into().unwrap();
        let name_length = u8::from_be_bytes(name_length_bytes);
        let name_length_usize = name_length as usize;
        counter += FILE_NAME_LENGTH_BYTE_WIDTH;

        let mut name: [u8; MAX_FILE_NAME_BYTE_WIDTH] = [0; MAX_FILE_NAME_BYTE_WIDTH];
        name[0..name_length_usize].copy_from_slice(&buf[counter..counter+name_length_usize]);
        counter += MAX_FILE_NAME_BYTE_WIDTH;

        let file_length_bytes: [u8; FILE_LENGTH_BYTE_WIDTH] = buf[counter..counter+FILE_LENGTH_BYTE_WIDTH].try_into().unwrap();
        let file_length = u32::from_be_bytes(file_length_bytes);
        counter += FILE_LENGTH_BYTE_WIDTH;

        let is_definite_bytes: [u8; IS_DEFINITE_FILE_BYTE_WIDTH] = buf[counter..counter+IS_DEFINITE_FILE_BYTE_WIDTH].try_into().unwrap();
        let is_definite_byte = u8::from_be_bytes(is_definite_bytes);
        let is_definite = is_definite_byte != 0;
        // counter doesn't need to be incremented at this point

        FileInfo {
            id,
            name_length,
            name,
            is_definite,
            length: file_length,
            data: vec![0u8; file_length as usize].into_boxed_slice(),
            next_expected_chunk_offset: 0,
            reorder_buffer: BTreeMap::new(),
        }
    }

    fn name(&self) -> String {
        String::from_utf8(self.name[0..self.name_length as usize].to_vec()).unwrap()
    }
}

impl fmt::Display for FileInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ID: {}\nName: {} ({} bytes)\nDefinite: {}\nLength: {}", self.id, self.name(), self.name_length, self.is_definite, self.length)
    }
}

// TODO: only public for test file; change later
pub fn has_timed_out(start_time: std::time::Instant) -> bool {
    start_time.elapsed() >= TIMEOUT_THRESHOLD
}

pub fn connect_to_all_udp_sockets(file: String) -> Vec<Arc<UdpSocket>> {
    let ports = get_ports(file.as_str()).unwrap();

    ports.iter().filter_map(|remote_addr| {
        let local_addr: SocketAddr = if remote_addr.is_ipv6() {
            "[::]:0".parse().unwrap()
        } else {
            "0.0.0.0:0".parse().unwrap()
        };
        let start_time = std::time::Instant::now();
        loop {
            if has_timed_out(start_time) {
                info!("Timed out binding UDP socket for {}", remote_addr);
                return None;
            }
            if let Ok(socket) = UdpSocket::bind(local_addr) {
                socket.connect(remote_addr).unwrap();
                socket.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
                info!("Bound UDP socket for {}", remote_addr);
                return Some(Arc::new(socket));
            }
        }
    }).collect()
}

// Listeners run for the whole session and never retire their port, so a drone that comes online
// late is still picked up. They only ever exit by panicking, which the main thread treats as a
// genuine failure of that stream.
pub fn create_listener_thread(tx: Sender<Vec<u8>>, socket: Arc<UdpSocket>) -> Result<thread::JoinHandle<()>, std::io::Error> {
    let handle = thread::Builder::new().name("listener thread".to_string()).spawn(move || {
        let mut buffer = vec![0u8; 65536];
        loop {
            match socket.recv(&mut buffer) {
                Ok(bytes_read) => {
                    if bytes_read == 0 { continue; }
                    let packet = buffer[..bytes_read].to_vec();
                    let send_result = tx.send(packet);
                    match send_result {
                        Ok(_) => {},
                        Err(e) => {
                            debug!("Error attempting to send packet from listener to main thread: {}", e);
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                           || e.kind() == std::io::ErrorKind::TimedOut => {
                    continue;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    // Nothing bound on the far end yet. The server may still come up later in the
                    // session, so hold the port and keep announcing ourselves — but sleep first,
                    // because a refusal returns instantly and re-ACKing on every one spins a core.
                    thread::sleep(ACK_RETRY_INTERVAL);
                    let _ = socket.send(b"ACK");
                    continue;
                }
                Err(e) => panic!("Error in listener thread recv(): {}", e),
            }
        }
    })
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "Thread spawn failed"))?;

    Ok(handle)
}

pub enum AssemblyMessage {
    // this stream finished its initial file transfer; its entities are ready to join the scene
    StreamReady,
    // a stream that had already joined has signalled the end of its data
    StreamFinished,
    SceneFileAssembled(String),
    // a command from the stream holding command authority, with its JSON payload (empty for the
    // commands that take no arguments); already gated, so a receiver that sees one may act on it
    // without rechecking who sent it
    Command(ServerCommand, String),
}

// What a single `receive_file` dispatch produced for the assembly thread to act on. One datagram
// carries one message, so these are mutually exclusive — which is why this is an enum rather than
// the tuple of independent outputs the function used to return. New message types that the main
// thread has to hear about get a variant here instead of another slot in that tuple.
pub enum ReceiveOutcome {
    // nothing for the caller to do: a partial frame, an idle poll, or a message whose whole effect
    // landed in `active_files` or the ring-buffer registry
    Nothing,
    // a file finished assembling and turned out to be a scene JSON
    SceneFile { name: String, data: Vec<u8> },
    // the server signalled TRANSMISSION_END and has been ACKed
    TransmissionComplete,
    // a command frame with its payload still undecoded, not yet checked against this stream's
    // authority
    Command { command: ServerCommand, payload: Vec<u8> },
}

// A server declares itself the command authority with a top-level `"authority": true` in its scene
// JSON. Only that one key is inspected here — the renderer parses the rest of the scene on the main
// thread — so a scene that omits it, sets it to anything other than a JSON bool, or fails to parse
// at all simply yields false. Declaring is opt-in precisely so a peer fleet, where no stream is an
// operator, ends up with no authority rather than an arbitrary one.
/// Public so a server can ask the same question of the scene it is about to send, and reach the
/// same answer the client will — a server that sends commands its scene never claimed the right to
/// issue is a misconfiguration best caught on the sending side.
pub fn scene_declares_authority(data: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(data)
        .ok()
        .and_then(|scene| scene["authority"].as_bool())
        .unwrap_or(false)
}

pub fn create_assembly_thread(
    rx: Receiver<Vec<u8>>,
    tx_sender: Sender<Vec<u8>>,
    tx_main: Sender<AssemblyMessage>,
    registry: ring_buffer::BufferRegistry,
    base_scene_written: Arc<AtomicBool>,
    authority_claimed: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, std::io::Error> {
    let handle = thread::Builder::new().name("assembly thread".to_string()).spawn(move || {
        let mut active_files: HashMap<Uuid, FileInfo> = HashMap::new();
        let mut buf: Vec<u8> = Vec::new();
        // the first completed transmission is this stream joining; any later one is it wrapping up
        let mut has_joined = false;
        // Whether *this* stream holds command authority. Kept per-thread rather than shared so the
        // gate on an incoming global command is a local read: the stream that received the command
        // is the one that knows whether it may act on it.
        let mut has_authority = false;
        loop {
            match receive_file(&rx, &tx_sender, &mut active_files, &mut buf, &registry) {
                ReceiveOutcome::Nothing => {}

                // Gated here rather than on the main thread because authority is per-stream: this
                // thread is the only one that knows whether the socket the command arrived on is
                // the one holding it. Everything forwarded past this point is already authorised.
                ReceiveOutcome::Command { command, payload } => {
                    if !has_authority {
                        warn!("ignoring {:?} from a stream that does not hold command authority", command);
                    } else if command == ServerCommand::UNKNOWN {
                        warn!("ignoring an unrecognised command from the authority stream");
                    } else {
                        // Rejected at the wire rather than forwarded, because this is the one
                        // thing about a payload that can be judged without the scene: a command
                        // whose arguments are not text cannot be a command this client can act on.
                        match String::from_utf8(payload) {
                            Ok(payload) => {
                                info!("forwarding {:?} from the authority stream", command);
                                let _ = tx_main.send(AssemblyMessage::Command(command, payload));
                            }
                            Err(_) => warn!("ignoring {:?}: its payload is not valid UTF-8", command),
                        }
                    }
                }

                ReceiveOutcome::SceneFile { name, data } => {
                    debug!("A: assembled scene file {}", name);

                    // Checked on every scene rather than only the base one: an operator console is
                    // as likely to connect after the drones are already streaming as before them,
                    // and it still gets authority when it does.
                    if scene_declares_authority(&data) {
                        if authority_claimed.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                            has_authority = true;
                            info!("stream claimed command authority via scene {}", name);
                        } else if !has_authority {
                            // Re-declaring from the stream that already holds it is fine; a second
                            // stream declaring is a misconfiguration worth surfacing, since its
                            // global commands will be silently ignored from here on.
                            warn!("scene {} claims command authority, but another stream already holds it", name);
                        }
                    }

                    if base_scene_written.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                        append_to_file("main_scene.json".to_string(), data);
                    } else {
                        let json_str = String::from_utf8(data).unwrap_or_default();
                        let _ = tx_main.send(AssemblyMessage::SceneFileAssembled(json_str));
                    }
                }

                ReceiveOutcome::TransmissionComplete => {
                    let _ = tx_main.send(if has_joined {
                        AssemblyMessage::StreamFinished
                    } else {
                        has_joined = true;
                        AssemblyMessage::StreamReady
                    });
                }
            }
        }
    })
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "Thread spawn failed"))?;
    Ok(handle)
}

pub fn create_sender_thread(rx: Receiver<Vec<u8>>, socket: Arc<UdpSocket>) -> Result<thread::JoinHandle<()>, std::io::Error> {
    let handle = thread::Builder::new().name("sender thread".to_string()).spawn(move || {
        socket.send(b"ACK").unwrap();

        // block rather than poll — an idle stream should cost nothing while it waits for its drone
        while let Ok(msg) = rx.recv() {
            socket.send(&msg).unwrap();
        }
    })
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "Thread spawn failed"))?;

    Ok(handle)
}

pub fn get_ports(file: &str) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error>>{
    let contents = fs::read_to_string(file)?;
    let parsed: Value = contents.parse::<Value>()?;
    let mut result = Vec::new();

    // get server table
    if let Some(servers) = parsed.get("servers").and_then(|v| v.as_table()) {
        // each line contains an IP address and an array of ports
        for (ip, ports) in servers {
            // println!("analyzing {ip} and {ports}");
            if let Some(port_array) = ports.as_array() {
                for port in port_array {
                    if let Some(port_num) = port.as_integer() {
                        // Convert the IP and port into a SocketAddr
                        let port: u16 = port_num.try_into()?;
                        let socket_addr: SocketAddr = if ip == "localhost" {
                            let mut addrs = format!("{}:{}", ip, port).to_socket_addrs().unwrap();
                            addrs.next().unwrap()
                        } else {
                            let ip_addr = ip.parse::<IpAddr>()?;
                            SocketAddr::new(ip_addr, port)
                        };
                        // println!("adding {ip}:{port}");
                        result.push(socket_addr);
                    }
                }
            }
        }
    }

    // we want an Err to return if no IP addresses were found
    _ = result.get(0).ok_or("No IP address was found")?;
    Ok(result)
}

fn receive_file_metadata(rx: &Receiver<Vec<u8>>, buf: &mut Vec<u8>, start_time: std::time::Instant) -> Option<FileInfo> {
    while buf.len() < FILE_START_METADATA_BYTE_WIDTH && !has_timed_out(start_time){
        let Ok(msg) = rx.recv_timeout(ASSEMBLY_POLL_INTERVAL) else {
            return None
        };

        buf.extend_from_slice(&msg);
    }

    // debug!("metadata = {:?}", buf[0..FILE_START_METADATA_BYTE_WIDTH].to_vec());
    let metadata = FileInfo::new(buf);
    debug!("metadata = {}", metadata);

    let _ = buf.drain(0..FILE_START_METADATA_BYTE_WIDTH);

    Some(metadata)
}

fn write_to_registry(registry: &ring_buffer::BufferRegistry, name: &str, data: &[u8]) {
    if let Some(buf) = registry.lock().unwrap().get(name) {
        buf.lock().unwrap().write(data);
    }
}

fn receive_file_chunk(
    rx: &Receiver<Vec<u8>>,
    buf: &mut Vec<u8>,
    start_time: std::time::Instant,
    active_files: &mut HashMap<Uuid, FileInfo>,
    registry: &ring_buffer::BufferRegistry,
) -> Option<Vec<u8>> {
    while buf.len() < CHUNK_METADATA_BYTE_WIDTH && !has_timed_out(start_time){
        let Ok(msg) = rx.recv_timeout(ASSEMBLY_POLL_INTERVAL) else {
            return None
        };

        buf.extend_from_slice(&msg);
    }
    debug!("L: received chunk metadata");

    let mut counter = MESSAGE_TYPE_BYTE_WIDTH;
    let file_id_bytes: [u8; FILE_ID_BYTE_WIDTH] = buf[counter..counter+FILE_ID_BYTE_WIDTH]
        .try_into()
        .unwrap();
    counter += FILE_ID_BYTE_WIDTH;
    let chunk_offset_bytes: [u8; CHUNK_OFFSET_BYTE_WIDTH] = buf[counter..counter+CHUNK_OFFSET_BYTE_WIDTH]
        .try_into()
        .unwrap();
    counter += CHUNK_OFFSET_BYTE_WIDTH;
    let chunk_length_bytes: [u8; CHUNK_LENGTH_BYTE_WIDTH] = buf[counter..counter+CHUNK_LENGTH_BYTE_WIDTH]
        .try_into()
        .unwrap();
    debug!("L: parsed chunk metadata");

    let file_id = Uuid::from_bytes(file_id_bytes);
    let chunk_offset = u64::from_be_bytes(chunk_offset_bytes);
    let chunk_offset_us = chunk_offset as usize;
    let chunk_length = u32::from_be_bytes(chunk_length_bytes) as usize;
    debug!("L: ID = {}, offset = {}, length = {}", file_id, chunk_offset_us, chunk_length);

    let file_data = active_files.get_mut(&file_id).expect("invalid file");
    
    while buf.len() < CHUNK_METADATA_BYTE_WIDTH+(chunk_length as usize)+CHECKSUM_WIDTH && !has_timed_out(start_time){
        let Ok(msg) = rx.recv_timeout(ASSEMBLY_POLL_INTERVAL) else {
            return None
        };

        buf.extend_from_slice(&msg);
    }
    
    let payload = &buf[CHUNK_METADATA_BYTE_WIDTH..CHUNK_METADATA_BYTE_WIDTH+chunk_length];
    let checksum_bytes: [u8; CHECKSUM_WIDTH] = buf[CHUNK_METADATA_BYTE_WIDTH+chunk_length..CHUNK_METADATA_BYTE_WIDTH+chunk_length+CHECKSUM_WIDTH]
        .try_into()
        .unwrap();
    let checksum_actual = u32::from_be_bytes(checksum_bytes);
    let checksum_expected = crc32fast::hash(payload);
    debug!("checking expected checksum {} against actual checksum {}", checksum_expected, checksum_actual);
    
    if checksum_expected != checksum_actual {
        debug!("checksum failed");
        buf.drain(0..CHUNK_METADATA_BYTE_WIDTH + chunk_length + CHECKSUM_WIDTH);
        let mut request_buf = Vec::<u8>::new();
        request_buf.extend_from_slice(&MessageType::REQUEST_RETRANSMIT_CHUNK.to_be_bytes());
        request_buf.extend_from_slice(&file_id_bytes);
        request_buf.extend_from_slice(&chunk_offset_bytes);
        return Some(request_buf)
    }

    debug!("L: received chunk payload");

    if !file_data.is_definite && chunk_offset != file_data.next_expected_chunk_offset {
        debug!("L: found out-of-order chunk");
        file_data.reorder_buffer.insert(chunk_offset, payload.to_vec());
    } else if !file_data.is_definite {
        write_to_registry(registry, &file_data.name(), payload);
        file_data.next_expected_chunk_offset += chunk_length as u64;
        debug!("L: wrote chunk to ring buffer");

        loop {
            let next = file_data.next_expected_chunk_offset;
            if let Some(chunk) = file_data.reorder_buffer.remove(&next) {
                file_data.next_expected_chunk_offset += chunk.len() as u64;
                write_to_registry(registry, &file_data.name(), &chunk);
                debug!("L: wrote queued chunk to ring buffer");
            } else {
                break;
            }
        }
    } else {
        file_data.data[chunk_offset_us..chunk_offset_us+chunk_length].copy_from_slice(payload);
    }

    debug!("L: draining {}+{}+{} elements from buf", CHUNK_METADATA_BYTE_WIDTH, chunk_length, CHECKSUM_WIDTH);
    buf.drain(0..CHUNK_METADATA_BYTE_WIDTH+chunk_length+CHECKSUM_WIDTH);
    debug!("L: buf now contains {} elements", buf.len());

    None
}

fn finish_receiving_file(rx: &Receiver<Vec<u8>>, buf: &mut Vec<u8>, start_time: std::time::Instant, active_files: &mut HashMap<Uuid, FileInfo>) -> Option<(String, Vec<u8>)> {
    while buf.len() < FILE_END_METADATA_BYTE_WIDTH && !has_timed_out(start_time){
        let Ok(msg) = rx.recv_timeout(ASSEMBLY_POLL_INTERVAL) else {
            return None
        };

        buf.extend_from_slice(&msg);
    }

    let file_id_bytes: [u8; FILE_ID_BYTE_WIDTH] = buf[MESSAGE_TYPE_BYTE_WIDTH..MESSAGE_TYPE_BYTE_WIDTH+FILE_ID_BYTE_WIDTH]
        .try_into()
        .unwrap();
    let file_id = Uuid::from_bytes(file_id_bytes);
    let file_data = active_files.remove(&file_id).unwrap();
    let name = file_data.name();
    buf.drain(0..FILE_END_METADATA_BYTE_WIDTH);

    if name.ends_with("_main_scene.json") {
        Some((name, file_data.data.to_vec()))
    } else {
        append_to_file(name, file_data.data.to_vec());
        None
    }
}

fn finish_receiving_transmission(buf: &mut Vec<u8>){
    buf.drain(0..MESSAGE_TYPE_BYTE_WIDTH);
}

/// Reads the command id and payload that follow a `COMMAND` message type.
///
/// Returns `None` only when the rest of the frame has not arrived yet, leaving `buf` untouched so
/// the next call resumes where this one stopped. A command frame is small and arrives in one
/// datagram, so that is a formality rather than a path worth expecting.
///
/// The payload is returned undecoded. What is in it depends on the command, and the two consumers
/// that could check it — a camera name and an entity path — are both scene state living on the
/// main thread, so validating half of it here would only split the reporting in two.
fn receive_command(
    rx: &Receiver<Vec<u8>>,
    buf: &mut Vec<u8>,
    start_time: std::time::Instant,
) -> Option<(ServerCommand, Vec<u8>)> {
    while buf.len() < COMMAND_HEADER_BYTE_WIDTH && !has_timed_out(start_time) {
        let Ok(msg) = rx.recv_timeout(ASSEMBLY_POLL_INTERVAL) else {
            return None
        };

        buf.extend_from_slice(&msg);
    }

    if buf.len() < COMMAND_HEADER_BYTE_WIDTH {
        return None;
    }

    let mut counter = MESSAGE_TYPE_BYTE_WIDTH;
    let id_bytes: [u8; COMMAND_ID_BYTE_WIDTH] = buf[counter..counter+COMMAND_ID_BYTE_WIDTH]
        .try_into()
        .unwrap();
    counter += COMMAND_ID_BYTE_WIDTH;
    let length_bytes: [u8; COMMAND_PAYLOAD_LENGTH_BYTE_WIDTH] = buf[counter..counter+COMMAND_PAYLOAD_LENGTH_BYTE_WIDTH]
        .try_into()
        .unwrap();

    let command = ServerCommand::get_from_bytes(u16::from_be_bytes(id_bytes));
    let payload_length = u32::from_be_bytes(length_bytes) as usize;

    // A length this large is a corrupt field, not a payload. Drop the header and resynchronise on
    // whatever follows rather than blocking on bytes that are never going to arrive.
    if payload_length > MAX_COMMAND_PAYLOAD_BYTE_WIDTH {
        warn!(
            "discarding a {:?} frame declaring a {} byte payload; the ceiling is {}",
            command, payload_length, MAX_COMMAND_PAYLOAD_BYTE_WIDTH,
        );
        drain_frame(buf, COMMAND_HEADER_BYTE_WIDTH);
        return None;
    }

    let frame_width = COMMAND_HEADER_BYTE_WIDTH + payload_length;
    while buf.len() < frame_width && !has_timed_out(start_time) {
        let Ok(msg) = rx.recv_timeout(ASSEMBLY_POLL_INTERVAL) else {
            return None
        };

        buf.extend_from_slice(&msg);
    }

    if buf.len() < frame_width {
        return None;
    }

    let payload = buf[COMMAND_HEADER_BYTE_WIDTH..frame_width].to_vec();
    buf.drain(0..frame_width);

    Some((command, payload))
}

fn json_pretty(value: &serde_json::Value, depth: usize) -> String {
    let pad = "  ".repeat(depth);
    let inner_pad = "  ".repeat(depth + 1);
    match value {
        serde_json::Value::Object(map) => {
            if map.is_empty() { return "{}".to_string(); }
            let entries: Vec<String> = map.iter()
                .map(|(k, v)| format!("{}{}: {}", inner_pad, serde_json::to_string(k).unwrap(), json_pretty(v, depth + 1)))
                .collect();
            format!("{{\n{}\n{}}}", entries.join(",\n"), pad)
        }
        serde_json::Value::Array(arr) => {
            if arr.is_empty() { return "[]".to_string(); }
            let all_primitive = arr.iter().all(|v| !v.is_array() && !v.is_object());
            if all_primitive && arr.len() <= 4 {
                let items: Vec<String> = arr.iter().map(|v| v.to_string()).collect();
                format!("[{}]", items.join(", "))
            } else {
                let items: Vec<String> = arr.iter()
                    .map(|v| format!("{}{}", inner_pad, json_pretty(v, depth + 1)))
                    .collect();
                format!("[\n{}\n{}]", items.join(",\n"), pad)
            }
        }
        _ => value.to_string(),
    }
}

fn append_to_file(file_name: String, data: Vec<u8>){
    let dir = if file_name.ends_with(".obj") {
        "data/object_loading"
    } else if file_name.ends_with(".json") || file_name.ends_with(".bin") {
        "data/scene_loading"
    } else {
        "."
    };
    let full_path = format!("{}/{}", dir, file_name);
    debug!("appending file {full_path}");
    let path = Path::new(&full_path);
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();

    // let file_contents = String::from_utf8(data).unwrap();
    let metadata = fs::metadata(path).unwrap();
    let file_len = metadata.len();
    debug!("file length before adding chunk: {file_len}");

    let write_data: Vec<u8> = if file_name.ends_with(".json") {
        serde_json::from_slice::<serde_json::Value>(&data)
            .ok()
            .map(|v| json_pretty(&v, 0).into_bytes())
            .unwrap_or(data)
    } else {
        data
    };
    file.write_all(&write_data).unwrap();
    let metadata = fs::metadata(path).unwrap();
    let file_len = metadata.len();
    debug!("file length after adding chunk: {file_len}");
    // let _ = writeln!(&mut file, "{}", file_contents.as_str());
}

// `buf` persists across receive_file() calls, so every dispatch arm has to consume its own frame
// out of the front of it. An arm that returns without draining leaves the same message type sitting
// at offset 0, the next call re-dispatches it, and the assembly thread spins on it forever.
// Clamped to the buffer length because the arms that use this handle frames that should never
// reach the client at all — dropping a partial one beats pinning a core on it.
fn drain_frame(buf: &mut Vec<u8>, width: usize) {
    buf.drain(0..width.min(buf.len()));
}

pub fn receive_file(
    rx_main: &Receiver<Vec<u8>>,
    tx_sender: &Sender<Vec<u8>>,
    active_files: &mut HashMap<Uuid, FileInfo>,
    buf: &mut Vec<u8>,
    registry: &ring_buffer::BufferRegistry,
) -> ReceiveOutcome {
    // debug!("Preparing to receive file");
    let start_time = std::time::Instant::now();

    let mut bytes_read = buf.len();
    if bytes_read > 0 {
        debug!("L: bytes read before attempting to read from stream: {bytes_read}");
        if bytes_read < 100 {
            debug!("L: {:?}", buf);
        }
    }
    while bytes_read < MESSAGE_TYPE_BYTE_WIDTH && !has_timed_out(start_time) {
        let Ok(msg) = rx_main.recv_timeout(ASSEMBLY_POLL_INTERVAL) else {
            return ReceiveOutcome::Nothing
        };

        let msg_len = msg.len();
        if msg_len > 0 {
            debug!("read in {:?}", msg);
            buf.extend_from_slice(&msg);
            bytes_read += msg_len;
            debug!("L: receive_file(): buf len = {}, contents = {:?}", buf.len(), buf);
        }
    }

    let message_type = MessageType::get_from_bytes(
        u16::from_be_bytes(
            buf[0..MESSAGE_TYPE_BYTE_WIDTH]
            .try_into()
            .unwrap()
        )
    );
    
    match message_type {
        MessageType::FILE_START => {
            debug!("L: received FILE_START");
            if let Some(file) = receive_file_metadata(&rx_main, buf, start_time) {
                if !file.is_definite {
                    registry.lock().unwrap()
                        .entry(file.name())
                        .or_insert_with(|| Arc::new(Mutex::new(ring_buffer::RingBuffer::new())));
                }
                debug!("L: adding {} to active files", file.id);
                active_files.insert(file.id, file);
            }
            ReceiveOutcome::Nothing
        },
        MessageType::FILE_CHUNK => {
            debug!("L: received FILE_CHUNK");
            let vec_opt = receive_file_chunk(&rx_main, buf, start_time, active_files, registry);
            if let Some(vec) = vec_opt {
                info!("Requesting chunk to be retransmitted");
                let send_result = tx_sender.send(vec);
                if let Err(e) = send_result {
                    debug!("Error attempting to send packet from listener to main thread: {}", e);
                }
            }
            ReceiveOutcome::Nothing
        },
        MessageType::FILE_END => {
            debug!("L: received FILE_END");
            match finish_receiving_file(&rx_main, buf, start_time, active_files) {
                Some((name, data)) => ReceiveOutcome::SceneFile { name, data },
                None => ReceiveOutcome::Nothing,
            }
        },
        MessageType::REQUEST_RETRANSMIT_CHUNK => {
            // the client is the one that sends these; a server echoing one back has nothing for us
            // to do, but the frame still has to come off the buffer
            debug!("L: received REQUEST_RETRANSMIT_CHUNK (unexpected on client)");
            drain_frame(buf, RETRANSMIT_REQUEST_BYTE_WIDTH);
            ReceiveOutcome::Nothing
        },
        MessageType::COMMAND => {
            debug!("L: received COMMAND");
            match receive_command(&rx_main, buf, start_time) {
                Some((command, payload)) => ReceiveOutcome::Command { command, payload },
                None => ReceiveOutcome::Nothing,
            }
        },
        MessageType::TRANSMISSION_END => {
            debug!("L: received TRANSMISSION_END");
            finish_receiving_transmission(buf);
            let mut ack = Vec::new();
            ack.extend_from_slice(&MessageType::TRANSMISSION_ACK.to_be_bytes());
            let _ = tx_sender.send(ack);
            ReceiveOutcome::TransmissionComplete
        }
        MessageType::TRANSMISSION_ACK => {
            debug!("L: received TRANSMISSION_ACK (unexpected on client)");
            drain_frame(buf, MESSAGE_TYPE_BYTE_WIDTH);
            ReceiveOutcome::Nothing
        }
        MessageType::ERROR => {
            // any code we don't recognise lands here, which is where a newer server talking to an
            // older client shows up. We can't know the frame's real length, so drop just the type
            // field and resynchronise on whatever follows rather than re-dispatching these two
            // bytes on every iteration.
            debug!("L: received ERROR (unrecognised message type)");
            drain_frame(buf, MESSAGE_TYPE_BYTE_WIDTH);
            ReceiveOutcome::Nothing
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, net::SocketAddr, sync::mpsc, thread};
//     use std::io::{Write, Read};
    use std::fs::{File, OpenOptions, remove_file};

//     use crate::{dc, glutin, scene_composer, scenes_and_entities::{self, ModelComponent}};

    use super::*;
    use std::collections::HashSet;


//     #[test]
//     fn unit_quaternion() {
//         let unit_quaternion: na::UnitQuaternion<f64> = na::UnitQuaternion::identity();
//         info!("{}", unit_quaternion);
//     }

//     fn load_from_json(){
//         scenes_and_entities::ModelComponent::load_from_json_file(&"data/object_loading.blizzard_initialize.json");
        
//     }

//     #[test]
//     fn color_change() {
//         let mut test_scene = scene_composer::test_scene();
//         let color_cmd = scenes_and_entities::Command::new(
//             scenes_and_entities::CommandType::ComponentChangeColor,
//             vec![0.0, 1.0, 1.0, 1.0, 1.0]
//         );
//         assert_eq!(
//             test_scene.get_entity(0).unwrap().get_model(0).get_color(),
//             na::Vector4::<f32>::new(0.0, 1.0, 0.0, 1.0),
//             "Base color is green"
//         );
//         test_scene.get_entity(0).unwrap().command(color_cmd);
//         assert_eq!(
//             test_scene.get_entity(0).unwrap().get_model(0).get_color(),
//             na::Vector4::<f32>::new(1.0, 1.0, 1.0, 1.0),
//             "New color is white"
//         );
//     }

//     #[test]
//     fn position_change() {
//         let mut test_scene = scene_composer::test_scene();
//         let pos_cmd = scenes_and_entities::Command::new(
//             scenes_and_entities::CommandType::EntityChangeTransform,
//             vec![1.0, 1.0, 1.0]
//         );
//         assert_eq!(
//             test_scene.get_entity(0).unwrap().get_position(),
//             &na::Point3::<f64>::origin(),
//             "Initial Position is Origin"
//         );
//         test_scene.get_entity(0).unwrap().command(pos_cmd);
//         assert_eq!(
//             test_scene.get_entity(0).unwrap().get_position(),
//             &na::Point3::<f64>::new(1.0, 1.0, 1.0),
//             "Position commanded successfully"
//         );
//     }

//     #[test]
//     fn change_command() {
//         let mut test_scene = scene_composer::test_scene();
//         let change_command = scenes_and_entities::Command::new(
//             scenes_and_entities::CommandType::ModifyBehavior, 
//             vec![0.0, ]
//         );
//     }

//     #[test]
//     fn load_font() {
        
//     }

    // Feeds one already-buffered frame through receive_file and returns what is left in `buf`.
    // The channel is never read: with the frame already in `buf`, receive_file dispatches on it
    // without touching the listener.
    fn dispatch_buffered_frame(frame: Vec<u8>) -> (ReceiveOutcome, Vec<u8>) {
        let (_tx_listener, rx_listener) = mpsc::channel::<Vec<u8>>();
        let (tx_sender, _rx_sender) = mpsc::channel::<Vec<u8>>();
        let mut active_files: HashMap<Uuid, FileInfo> = HashMap::new();
        let mut buf = frame;
        let registry = ring_buffer::new_registry();

        let outcome = receive_file(&rx_listener, &tx_sender, &mut active_files, &mut buf, &registry);
        (outcome, buf)
    }

    // An arm that returns without draining leaves its message type at offset 0, so the assembly
    // thread's loop re-dispatches the same bytes forever and pins a core. Unrecognised codes are
    // the likeliest way to hit this — that is what a newer server's message looks like here.
    #[test]
    fn unknown_message_type_is_drained() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&999u16.to_be_bytes());

        let (outcome, buf) = dispatch_buffered_frame(frame);

        assert!(matches!(outcome, ReceiveOutcome::Nothing));
        assert!(buf.is_empty(), "ERROR arm left {} byte(s) in buf to be re-dispatched", buf.len());
    }

    // The client is the sender of these, but a server echoing one back must not wedge it either.
    #[test]
    fn retransmit_request_is_drained() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&MessageType::REQUEST_RETRANSMIT_CHUNK.to_be_bytes());
        frame.extend_from_slice(&[0u8; FILE_ID_BYTE_WIDTH]);
        frame.extend_from_slice(&0u64.to_be_bytes());
        assert_eq!(frame.len(), RETRANSMIT_REQUEST_BYTE_WIDTH);

        let (outcome, buf) = dispatch_buffered_frame(frame);

        assert!(matches!(outcome, ReceiveOutcome::Nothing));
        assert!(buf.is_empty(), "REQUEST_RETRANSMIT_CHUNK arm left {} byte(s) in buf", buf.len());
    }

    // A short frame must not panic the drain, and must still leave nothing to re-dispatch.
    #[test]
    fn partial_frame_drain_does_not_panic() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&MessageType::REQUEST_RETRANSMIT_CHUNK.to_be_bytes());
        frame.extend_from_slice(&[0u8; 4]);

        let (_outcome, buf) = dispatch_buffered_frame(frame);

        assert!(buf.is_empty());
    }

    // Authority is opt-in: anything that is not an explicit JSON `true` leaves the stream without
    // it. A peer fleet's scenes omit the key entirely, which is the case that has to stay false.
    #[test]
    fn authority_is_declared_only_by_an_explicit_true() {
        let cases = [
            (r#"{"authority": true, "entities": []}"#, true),
            (r#"{"authority": false, "entities": []}"#, false),
            // the ordinary peer-fleet scene: no key at all
            (r#"{"viewports": [], "entities": []}"#, false),
            // a truthy-looking value in another type must not count
            (r#"{"authority": "true"}"#, false),
            (r#"{"authority": 1}"#, false),
            (r#"{"authority": null}"#, false),
            // malformed or non-object JSON must not panic or claim
            ("not json at all", false),
            ("[1, 2, 3]", false),
            ("", false),
        ];

        for (scene, expected) in cases {
            assert_eq!(
                scene_declares_authority(scene.as_bytes()),
                expected,
                "scene {:?} should{} declare authority",
                scene,
                if expected { "" } else { " not" },
            );
        }
    }

    // The declaration is read straight off the assembled bytes, so it has to survive a scene large
    // enough to be chunked, with the key nowhere near the front.
    #[test]
    fn authority_is_found_late_in_a_large_scene() {
        let filler: String = (0..2000)
            .map(|i| format!(r#"{{"Name": "Drone_{}"}}"#, i))
            .collect::<Vec<_>>()
            .join(",");
        let scene = format!(r#"{{"entities": [{}], "authority": true}}"#, filler);
        assert!(scene.len() > 1024, "scene should span multiple chunks");

        assert!(scene_declares_authority(scene.as_bytes()));
    }

    #[test]
    fn a_command_frame_is_decoded_and_drained() {
        let frame = encode_command(ServerCommand::CLEAR_SCENE, b"");

        let (outcome, buf) = dispatch_buffered_frame(frame);

        assert!(matches!(
            outcome,
            ReceiveOutcome::Command { command: ServerCommand::CLEAR_SCENE, .. },
        ));
        assert!(buf.is_empty(), "the command frame must not be re-dispatched");
    }

    #[test]
    fn a_payload_survives_the_round_trip() {
        let json = br#"{"camera": "chase", "position": [1.0, 2.0, 3.0]}"#;
        let frame = encode_command(ServerCommand::UPDATE_CAMERA_POSITION, json);

        let (outcome, buf) = dispatch_buffered_frame(frame);

        match outcome {
            ReceiveOutcome::Command { command, payload } => {
                assert_eq!(command, ServerCommand::UPDATE_CAMERA_POSITION);
                assert_eq!(payload, json.to_vec());
            }
            _ => panic!("payload command did not decode"),
        }
        assert!(buf.is_empty());
    }

    // A newer server's command reaching an older client. The length field is what makes this
    // recoverable: the client cannot interpret the payload but can still step over it, so the
    // bytes after it are read as the next frame rather than as garbage.
    #[test]
    fn an_unrecognised_command_skips_its_payload() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&MessageType::COMMAND.to_be_bytes());
        frame.extend_from_slice(&999u16.to_be_bytes());
        frame.extend_from_slice(&12u32.to_be_bytes());
        frame.extend_from_slice(b"who knows???");

        let (outcome, buf) = dispatch_buffered_frame(frame);

        assert!(matches!(
            outcome,
            ReceiveOutcome::Command { command: ServerCommand::UNKNOWN, .. },
        ));
        assert!(buf.is_empty(), "the unknown command's payload must not be re-dispatched");
    }

    // The one case that must *not* drain: half a frame is resumable, and dropping those bytes
    // would leave the rest of the header at offset 0 to be misread as a message type.
    #[test]
    fn a_partial_command_frame_is_left_for_the_next_call() {
        let frame = MessageType::COMMAND.to_be_bytes().to_vec();

        let (outcome, buf) = dispatch_buffered_frame(frame);

        assert!(matches!(outcome, ReceiveOutcome::Nothing));
        assert_eq!(buf.len(), MESSAGE_TYPE_BYTE_WIDTH, "the type field waits for its payload");
    }

    // A header whose payload has not landed yet is resumable too, and must not be consumed.
    #[test]
    fn a_command_awaiting_its_payload_is_left_for_the_next_call() {
        let mut frame = encode_command(ServerCommand::UPDATE_CAMERA_POSITION, b"{\"a\": 1}");
        frame.truncate(COMMAND_HEADER_BYTE_WIDTH + 2);
        let expected = frame.len();

        let (outcome, buf) = dispatch_buffered_frame(frame);

        assert!(matches!(outcome, ReceiveOutcome::Nothing));
        assert_eq!(buf.len(), expected, "a half-arrived payload waits for the rest");
    }

    // A corrupt length field would otherwise block the assembly thread on bytes that are never
    // coming, and then re-block on the same header on every retry.
    #[test]
    fn an_oversized_payload_length_is_discarded() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&MessageType::COMMAND.to_be_bytes());
        frame.extend_from_slice(&ServerCommand::UPDATE_CAMERA_POSITION.to_be_bytes());
        frame.extend_from_slice(&u32::MAX.to_be_bytes());

        let (outcome, buf) = dispatch_buffered_frame(frame);

        assert!(matches!(outcome, ReceiveOutcome::Nothing));
        assert!(buf.is_empty(), "the bogus header must not be re-dispatched forever");
    }

    // The encoder and the decoder are the two halves of one frame layout, written in different
    // files. A field width or order that drifted on one side would still decode here as a valid
    // frame carrying the wrong command, so pin them against each other rather than separately.
    #[test]
    fn every_encoded_command_decodes_back_to_itself() {
        for command in [
            ServerCommand::CLEAR_SCENE,
            ServerCommand::UPDATE_CAMERA_POSITION_BEHAVIOR,
            ServerCommand::UPDATE_CAMERA_ROTATION_BEHAVIOR,
            ServerCommand::UPDATE_CAMERA_POSITION,
            ServerCommand::UPDATE_CAMERA_ROTATION,
        ] {
            let (outcome, buf) = dispatch_buffered_frame(encode_command(command, b"{}"));

            match outcome {
                ReceiveOutcome::Command { command: decoded, payload } => {
                    assert_eq!(decoded, command);
                    assert_eq!(payload, b"{}".to_vec());
                }
                _ => panic!("{:?} did not encode to a command frame", command),
            }
            assert!(buf.is_empty(), "{:?} left bytes behind", command);
        }
    }

    // The codes are the protocol contract in README.md; the enum discriminants encode them, so a
    // variant inserted in the middle would silently renumber every later command.
    #[test]
    fn server_command_codes_match_the_wire_protocol() {
        for (command, code) in [
            (ServerCommand::CLEAR_SCENE, 0u16),
            (ServerCommand::UPDATE_CAMERA_POSITION_BEHAVIOR, 1),
            (ServerCommand::UPDATE_CAMERA_ROTATION_BEHAVIOR, 2),
            (ServerCommand::UPDATE_CAMERA_POSITION, 3),
            (ServerCommand::UPDATE_CAMERA_ROTATION, 4),
        ] {
            assert_eq!(command.to_be_bytes(), code.to_be_bytes(), "{:?} encodes wrongly", command);
            assert_eq!(ServerCommand::get_from_bytes(code), command, "code {} decodes wrongly", code);
        }
        assert_eq!(ServerCommand::get_from_bytes(999), ServerCommand::UNKNOWN);
    }

    // The wire codes are the protocol contract in README.md; the enum discriminants now encode
    // them, so a variant inserted in the middle would silently renumber every later message.
    #[test]
    fn message_type_codes_match_the_wire_protocol() {
        for (msg_type, code) in [
            (MessageType::FILE_START, 0u16),
            (MessageType::FILE_CHUNK, 1),
            (MessageType::FILE_END, 2),
            (MessageType::REQUEST_RETRANSMIT_CHUNK, 3),
            (MessageType::TRANSMISSION_END, 4),
            (MessageType::TRANSMISSION_ACK, 5),
            (MessageType::COMMAND, 6),
        ] {
            assert_eq!(msg_type.to_be_bytes(), code.to_be_bytes(), "{:?} encodes to the wrong code", msg_type);
            assert_eq!(MessageType::get_from_bytes(code), msg_type, "code {} decodes to the wrong variant", code);
        }
    }

    fn vectors_match(v1: Result<Vec<SocketAddr>, Box<dyn std::error::Error>>, v2: Result<Vec<SocketAddr>, Box<dyn std::error::Error>>) -> bool{
        match v1{
            Ok(_) => {},
            Err(ref e) => println!("Error msg: {e:?}"),
        };
        if v1.is_err() && v2.is_err(){
            return true;
        }
        if !(v1.is_ok() && v2.is_ok()){
            println!("returning false: case 2");
            return false;
        }

        let vec1 = v1.unwrap();
        let vec2 = v2.unwrap();

        let set1: HashSet<_> = vec1.iter().collect();
        let set2: HashSet<_> = vec2.iter().collect();
        set1 == set2
    }

    fn get_ports_template(toml_name: &str, toml_contents: &str, expected: Result<Vec<SocketAddr>, Box<dyn std::error::Error>>){
        let file_name_string = format!("{}{}", toml_name, ".toml");
        let file_name = file_name_string.as_str();
        let file_path = Path::new(file_name);
        let mut file = File::create(&file_path).unwrap();
        _ = writeln!(file, "{}", toml_contents);
        let actual = get_ports(file_name);
        assert!(vectors_match(actual, expected));
        _ = remove_file(&file_path);
    }

    #[test]
    fn get_ports_basic(){
        let toml_name = "get_ports_basic";
        let toml_contents = "[servers]
\"10.0.0.5\" = [22]";
        let expected: Result<Vec<SocketAddr>, _> = Ok(vec![SocketAddr::from(([10, 0, 0, 5], 22))]);
        get_ports_template(toml_name, toml_contents, expected);
    }

    #[test]
    fn get_ports_one_ip_multiple_ports(){
        let toml_name = "get_ports_one_ip_multiple_ports";
        let toml_contents = "[servers]
\"10.0.0.5\" = [22, 8080]";
        let s1 = SocketAddr::from(([10, 0, 0, 5], 22));
        let s2 = SocketAddr::from(([10, 0, 0, 5], 8080));
        let expected: Result<Vec<SocketAddr>, _> = Ok(vec![s1, s2]);
        get_ports_template(toml_name, toml_contents, expected);
    }

    #[test]
    fn get_ports_multiple_ip_one_port(){
        let toml_name = "get_ports_multiple_ip_one_port";
        let toml_contents = "[servers]
\"192.168.0.1\" = [443]
\"10.0.0.5\" = [22]";
        let s1 = SocketAddr::from(([192, 168, 0, 1], 443));
        let s2 = SocketAddr::from(([10, 0, 0, 5], 22));
        let expected: Result<Vec<SocketAddr>, _> = Ok(vec![s1, s2]);
        get_ports_template(toml_name, toml_contents, expected);
    }

    #[test]
    fn get_ports_multiple_ip_multiple_ports(){
        let toml_name = "get_ports_multiple_ip_multiple_ports";
        let toml_contents = "[servers]
\"192.168.0.1\" = [80, 443]
\"10.0.0.5\" = [22]
\"172.16.1.100\" = [21, 8080, 3000]
\"127.0.0.1\" = [8000, 8001, 8002]
\"203.0.113.42\" = [53]";
        let s1 = SocketAddr::from(([192, 168, 0, 1], 80));
        let s2 = SocketAddr::from(([192, 168, 0, 1], 443));
        let s3 = SocketAddr::from(([10, 0, 0, 5], 22));
        let s4 = SocketAddr::from(([172, 16, 1, 100], 21));
        let s5 = SocketAddr::from(([172, 16, 1, 100], 8080));
        let s6 = SocketAddr::from(([172, 16, 1, 100], 3000));
        let s7 = SocketAddr::from(([127, 0, 0, 1], 8000));
        let s8 = SocketAddr::from(([127, 0, 0, 1], 8001));
        let s9 = SocketAddr::from(([127, 0, 0, 1], 8002));
        let s10 = SocketAddr::from(([203, 0, 113, 42], 53));
        let expected: Result<Vec<SocketAddr>, _> = Ok(vec![s1, s2, s3, s4, s5, s6, s7, s8, s9, s10]);
        get_ports_template(toml_name, toml_contents, expected);
    }

    #[test]
    fn get_ports_localhost(){
        let toml_name = "get_ports_localhost";
        let toml_contents = "[servers]
\"localhost\" = [8081]";
        let mut addrs = "localhost:8081".to_socket_addrs().unwrap(); 
        let s1 = addrs.next().unwrap();
        let expected = Ok(vec![s1]);
        get_ports_template(toml_name, toml_contents, expected);
    }

    #[test]
    fn get_ports_no_server(){
        let toml_name = "get_ports_no_server";
        let toml_contents = "[somethingelse]
irrelevant = content";

        let err = "invalid = [".parse::<toml::Value>().unwrap_err();
        let expected: Result<Vec<SocketAddr>, Box<dyn std::error::Error>> = Err(Box::new(err));

        // let expected: Result<Vec<SocketAddr>, _> = Ok(vec![SocketAddr::from(([10, 0, 0, 5], 22))]);
        get_ports_template(toml_name, toml_contents, expected);
    }

    #[test]
    fn get_ports_too_high(){
        let toml_name = "get_ports_too_high";
        let toml_contents = "[servers]
\"10.0.0.5\" = [999999999]";

        let err = "invalid = [".parse::<toml::Value>().unwrap_err();
        let expected: Result<Vec<SocketAddr>, Box<dyn std::error::Error>> = Err(Box::new(err));
        get_ports_template(toml_name, toml_contents, expected);
    }

    #[test]
    fn get_ports_negative(){
        let toml_name = "get_ports_negative";
        let toml_contents = "[servers]
\"10.0.0.5\" = [-1]";

        let err = "invalid = [".parse::<toml::Value>().unwrap_err();
        let expected: Result<Vec<SocketAddr>, Box<dyn std::error::Error>> = Err(Box::new(err));
        get_ports_template(toml_name, toml_contents, expected);
    }

    #[test]
    fn get_ports_bad_format(){
        let toml_name = "get_ports_bad_format";
        let toml_contents = "[servers]
10005 = [80]";

        let err = "invalid = [".parse::<toml::Value>().unwrap_err();
        let expected: Result<Vec<SocketAddr>, Box<dyn std::error::Error>> = Err(Box::new(err));
        get_ports_template(toml_name, toml_contents, expected);
    }

    #[test]
    fn get_ports_empty(){
        let toml_name = "get_ports_empty";
        let toml_contents = "[servers]";

        let err = "invalid = [".parse::<toml::Value>().unwrap_err();
        let expected: Result<Vec<SocketAddr>, Box<dyn std::error::Error>> = Err(Box::new(err));
        get_ports_template(toml_name, toml_contents, expected);
    }

    // fn create_listener_thread_template(toml_name: &str, toml_contents: &str){
    //     let _ = pretty_env_logger::try_init();
    //     let (tx, rx) = mpsc::channel();

    //     let file_name_string = format!("{}{}", toml_name, ".toml");
    //     let file_name_string_listener = file_name_string.clone();
    //     let file_name_string_server = file_name_string.clone();
    //     let file_name = file_name_string.as_str();
    //     let file_path = Path::new(file_name);
    //     let mut file = File::create(&file_path).unwrap();
    //     _ = writeln!(file, "{}", toml_contents);

    //     let listener = create_listener_thread(tx, file_name_string_listener);
    //     listener.unwrap();
    //     let server_test = create_server_thread(file_name_string_server);
    //     server_test.unwrap();
    //     // let join_result = handle.join();
    //     let start_time = std::time::Instant::now();
    //     let mut passed = false;
    //     while !has_timed_out(start_time){
    //         let received = rx.recv().unwrap();
    //         let received_str = String::from_utf8(received).unwrap();
    //         info!("RECEIVED = {}", received_str);
    //         if received_str.len() > 0 {
    //             passed = true;
    //             break;
    //         }
    //     }
    //     assert!(passed);

    //     _ = remove_file(&file_path);
    //     // join_result.unwrap();
    // }

//     #[test]
//     fn create_listener_thread_success(){
//         let toml_name = "create_listener_thread_success";
//         let toml_contents = "[servers]
// \"localhost\" = [8081]";
//         create_listener_thread_template(toml_name, toml_contents);
//     }

//     #[test]
//     #[should_panic]
//     fn create_listener_thread_failure(){
//         let toml_name = "create_listener_thread_failure";
//         let toml_contents = "[somethingelse]
// irrelevant = content";
//         create_listener_thread_template(toml_name, toml_contents);
//     }
}