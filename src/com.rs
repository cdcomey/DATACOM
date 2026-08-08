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
use log::{debug, info};
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

// TODO: only public for test file; change later
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    FILE_START,
    FILE_CHUNK,
    FILE_END,
    REQUEST_RETRANSMIT_CHUNK,
    TRANSMISSION_END,
    TRANSMISSION_ACK,
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
            _ => MessageType::ERROR,
        }
    }

    // the sole encoder for the message type field — every frame we build goes through here so the
    // discriminants in this enum stay the single source of truth for the wire codes
    pub fn to_be_bytes(self) -> [u8; MESSAGE_TYPE_BYTE_WIDTH] {
        (self as u16).to_be_bytes()
    }
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
}

pub fn create_assembly_thread(
    rx: Receiver<Vec<u8>>,
    tx_sender: Sender<Vec<u8>>,
    tx_main: Sender<AssemblyMessage>,
    registry: ring_buffer::BufferRegistry,
    base_scene_written: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, std::io::Error> {
    let handle = thread::Builder::new().name("assembly thread".to_string()).spawn(move || {
        let mut active_files: HashMap<Uuid, FileInfo> = HashMap::new();
        let mut buf: Vec<u8> = Vec::new();
        // the first completed transmission is this stream joining; any later one is it wrapping up
        let mut has_joined = false;
        loop {
            match receive_file(&rx, &tx_sender, &mut active_files, &mut buf, &registry) {
                ReceiveOutcome::Nothing => {}

                ReceiveOutcome::SceneFile { name, data } => {
                    debug!("A: assembled scene file {}", name);
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