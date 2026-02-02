use std::io::{Read, Write};
use std::fmt;
// use tokio;
// use tokio::time::sleep;
use std::net::{ToSocketAddrs, TcpStream, IpAddr, TcpListener, SocketAddr};
// use std::error::Error;
use std::sync::mpsc::Sender;
use std::thread;
use std::fs::{self, File, OpenOptions};
use std::time::Duration;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use toml::Value;
use log::{debug, info};
use std::sync::mpsc::Receiver;

const MESSAGE_TYPE_BYTE_WIDTH: usize = 2;
const FILE_ID_BYTE_WIDTH: usize = 8;
const FILE_NAME_LENGTH_BYTE_WIDTH: usize = 1;
const MAX_FILE_NAME_BYTE_WIDTH: usize = u8::max_value() as usize;
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
const CHECKSUM_WIDTH: usize = 4;

const SECONDS_UNTIL_TIMEOUT: u64 = 10;
const TIMEOUT_THRESHOLD: Duration = Duration::from_secs(SECONDS_UNTIL_TIMEOUT);
const MAX_CHUNK_TRANSMIT_ATTEMPTS: u8 = 5;

#[repr(u16)]
#[derive(Debug)]
enum MessageType {
    FILE_START,
    FILE_CHUNK,
    FILE_END,
    REQUEST_RETRANSMIT_CHUNK,
    TRANSMISSION_END,
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
            _ => MessageType::ERROR,
        }
    }
}

#[derive(Debug)]
pub struct FileInfo {
    message_type: MessageType,
    id: u64,
    name_length: u8,
    name: [u8; MAX_FILE_NAME_BYTE_WIDTH],
    is_definite: bool,
    length: u32,
    data: Box<[u8]>,
    next_expected_chunk_offset: u64,
    reorder_buffer: BTreeMap<u64, Vec<u8>>,
}

impl FileInfo {
    fn new(buf: &Vec<u8>) -> Self {
        let mut counter = 0usize;

        let message_type_bytes: [u8; MESSAGE_TYPE_BYTE_WIDTH] = buf[counter..counter+MESSAGE_TYPE_BYTE_WIDTH].try_into().unwrap();
        let message_type_num: u16 = u16::from_be_bytes(message_type_bytes);
        let message_type: MessageType = MessageType::get_from_bytes(message_type_num);
        counter += MESSAGE_TYPE_BYTE_WIDTH;

        let id_bytes: [u8; FILE_ID_BYTE_WIDTH] = buf[counter..counter+FILE_ID_BYTE_WIDTH].try_into().unwrap();
        let id = u64::from_be_bytes(id_bytes);
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
        counter += IS_DEFINITE_FILE_BYTE_WIDTH;

        FileInfo {
            message_type,
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

pub fn from_network(mut stream: &TcpStream) -> Vec<u8>{
    // debug!("Handle Commands called");
    let mut buffer = [0; 600000];
    match stream.read(&mut buffer){
        Ok(bytes_read) => {
            let packet = buffer[..bytes_read].to_vec();
            // println!("{}",packet);
            packet
        },
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            panic!("Error (WouldBlock) in from_network(): {}", e)
        },
        Err(e) => panic!("Error in from_network(): {}", e),
    }
}

// Rewrite me to be better
pub fn from_network_with_protocol(stream: &mut TcpStream) -> Result<(), &str> {
    // Get file name first, return an error if there are no more files
    const CHUNK_SIZE: usize = 4096;
    let mut name_buffer = [0; 400];
    let name_bytes_read = stream.read(&mut name_buffer).unwrap();
    let name = String::from_utf8_lossy(&name_buffer[..name_bytes_read]).to_string();
    // debug!("Name: {}", name);
    if name == "END" {
        return Err("Done");
    }
    else {
        // Open the file
        // debug!("Name: {}", name);
        let mut file = File::create(&name).unwrap();
        
        // Get file size next
        let mut file_size_buffer = [0; 8];
        stream.read_exact(&mut file_size_buffer).expect("Failed to read buffer");
        let mut bytes_received = 0;
        let file_size = u64::from_be_bytes(file_size_buffer);
        // debug!("Anticipated file size: {} bytes", file_size);

        

        let mut buffer = [0; CHUNK_SIZE];
        let mut num_packets_recieved = 0;
        while bytes_received < file_size {
            let bytes_read = stream.read(&mut buffer).unwrap();
            if bytes_read == 0 {
                break;
            }
            file.write_all(&buffer[..bytes_read]).unwrap();
            bytes_received += bytes_read as u64;
            // debug!("Received packet #{}", num_packets_recieved);
            num_packets_recieved += 1;
        }
        
        // Recieve file in chunks
        

    }
    Ok(())
}

fn has_timed_out(start_time: std::time::Instant) -> bool {
    start_time.elapsed() >= TIMEOUT_THRESHOLD
}

fn send_finite_test_data(mut stream: TcpStream){
    let path = std::path::Path::new("data/scene_loading/test_scene_2.json");
    let test_command_data_main = fs::read_to_string(path).unwrap();
    let data_len = test_command_data_main.len();

    let message_type = 0u16;
    let file_id = 0123456789u64;
    let file_name_base = "data/scene_loading/main_scene.json";
    let file_name_length = file_name_base.len() as u8;
    let mut file_name = [0u8; MAX_FILE_NAME_BYTE_WIDTH];
    file_name[0..file_name_length as usize].copy_from_slice(file_name_base.as_bytes());
    let file_len = test_command_data_main.len() as u32;

    let mut test_command_data: Vec<u8> = Vec::new();
    test_command_data.extend_from_slice(&message_type.to_be_bytes());
    test_command_data.extend_from_slice(&file_id.to_be_bytes());
    test_command_data.extend_from_slice(&[file_name_length]);
    test_command_data.extend_from_slice(&file_name);
    test_command_data.extend_from_slice(&file_len.to_be_bytes());
    test_command_data.extend_from_slice(&[1u8]);

    info!("Sending file start frame to stream");
    debug!("{:?}", test_command_data);

    thread::sleep(Duration::from_millis(10));
    stream.write_all(&test_command_data[..]).unwrap();
    stream.flush().unwrap();
    
    let message_type = 1u16;
    let mut chunk_offset = 0u64;
    let chunk_length_default = 1024u32;
    while (chunk_offset as usize) < data_len {
        test_command_data.clear();
        test_command_data.extend_from_slice(&message_type.to_be_bytes());
        test_command_data.extend_from_slice(&file_id.to_be_bytes());
        test_command_data.extend_from_slice(&chunk_offset.to_be_bytes());

        let chunk_offset_usize = chunk_offset as usize;

        let chunk_length: u32 = if chunk_offset_usize + chunk_length_default as usize > data_len {
            (data_len - chunk_offset_usize).try_into().unwrap()
        } else {
            chunk_length_default
        };

        test_command_data.extend_from_slice(&chunk_length.to_be_bytes());
        let max_bound = chunk_offset_usize+chunk_length as usize;
        debug!("indexing data from {} to {} out of {}", chunk_offset, max_bound, data_len);
        let payload = test_command_data_main[chunk_offset_usize..max_bound].as_bytes();
        test_command_data.extend_from_slice(payload);
        chunk_offset += chunk_length as u64;

        let checksum = crc32fast::hash(payload);
        let checksum_bytes = checksum.to_be_bytes();
        test_command_data.extend_from_slice(&checksum_bytes);

        debug!("Sending finite chunk to stream: {:?}", test_command_data);
        thread::sleep(Duration::from_millis(10));
        stream.write_all(&test_command_data[..]).unwrap();
        stream.flush().unwrap();
    }

    let message_type = 2u16;
    test_command_data.clear();
    test_command_data.extend_from_slice(&message_type.to_be_bytes());
    test_command_data.extend_from_slice(&file_id.to_be_bytes());

    info!("Sending file end to stream");
    debug!("{:?}", test_command_data);
    thread::sleep(Duration::from_millis(10));
    stream.write_all(&test_command_data[..]).unwrap();
    stream.flush().unwrap();

    let message_type = 4u16;
    test_command_data.clear();
    test_command_data.extend_from_slice(&message_type.to_be_bytes());

    info!("Sending transmission end to stream");
    debug!("{:?}", test_command_data);
    thread::sleep(Duration::from_millis(10));
    stream.write_all(&test_command_data[..]).unwrap();
    stream.flush().unwrap();
}

fn send_streamed_test_data(mut stream: TcpStream){
    let message_type = 0u16;
    let file_id = 1212121212u64;
    let file_name_base = "data/scene_loading/entity_pos.bin";
    let file_name_length = file_name_base.len() as u8;
    let mut file_name = [0u8; MAX_FILE_NAME_BYTE_WIDTH];
    file_name[0..file_name_length as usize].copy_from_slice(file_name_base.as_bytes());
    let file_len = 0u32;

    let mut test_command_data: Vec<u8> = Vec::new();
    test_command_data.extend_from_slice(&message_type.to_be_bytes());
    test_command_data.extend_from_slice(&file_id.to_be_bytes());
    test_command_data.extend_from_slice(&[file_name_length]);
    test_command_data.extend_from_slice(&file_name);
    test_command_data.extend_from_slice(&file_len.to_be_bytes());
    test_command_data.extend_from_slice(&[0u8]);

    info!("S: Sending file start frame to stream");
    debug!("{:?}", test_command_data);

    thread::sleep(Duration::from_millis(10));
    stream.write_all(&test_command_data[..]).unwrap();
    stream.flush().unwrap();

    // let message_type = 4u16;
    // test_command_data.clear();
    // test_command_data.extend_from_slice(&message_type.to_be_bytes());

    // info!("Sending transmission end to stream");
    // thread::sleep(Duration::from_millis(10));
    // stream.write_all(&test_command_data[..]).unwrap();
    // stream.flush().unwrap();
    
    
    let path: &Path = std::path::Path::new("data/scene_loading/obj.bin");
    let mut file = File::open(path).unwrap();
    let metadata = fs::metadata(path).unwrap();
    let file_len = metadata.len();
    debug!("src file len = {file_len}");

    let message_type = 1u16;
    let mut chunk_offset = 0u64;
    let chunk_length = 1024u32;
    let mut buffer = vec![0u8; chunk_length as usize];

    loop {
        let bytes_read = file.read(&mut buffer).unwrap();

        if bytes_read == 0 {
            debug!("no longer reading streamed file");
            break;
        }

        let payload = &buffer[0..bytes_read];
        let checksum = crc32fast::hash(payload);

        test_command_data.clear();
        test_command_data.extend_from_slice(&message_type.to_be_bytes());
        test_command_data.extend_from_slice(&file_id.to_be_bytes());
        test_command_data.extend_from_slice(&chunk_offset.to_be_bytes());
        test_command_data.extend_from_slice(&(bytes_read as u32).to_be_bytes());
        test_command_data.extend_from_slice(&payload);
        test_command_data.extend_from_slice(&checksum.to_be_bytes());
        chunk_offset += bytes_read as u64;

        info!("S: Sending streamed chunk to stream");
        debug!("{:?}", test_command_data);
        thread::sleep(Duration::from_millis(10));
        stream.write_all(&test_command_data[..]).unwrap();
        stream.flush().unwrap();
    }

    // let mut i = 0usize;

    // loop {
    //     test_command_data.clear();
    //     test_command_data.extend_from_slice(&message_type.to_be_bytes());
    //     test_command_data.extend_from_slice(&file_id.to_be_bytes());
    //     test_command_data.extend_from_slice(&chunk_offset.to_be_bytes());

    //     test_command_data.extend_from_slice(&chunk_length.to_be_bytes());
    //     let nums = counter..counter + chunk_length as usize;
    //     let range_bytes = nums
    //         .map(|i| i as f32)
    //         .flat_map(|i| i.to_be_bytes()) // Convert each number to a String
    //         .collect::<Vec<u8>>(); // Collect into a vector of Strings
    //     test_command_data.extend_from_slice(&range_bytes);
    //     chunk_offset += chunk_length as u64;
    //     counter += chunk_length as usize;

    //     info!("Sending chunk to stream");
    //     thread::sleep(Duration::from_millis(10));
    //     stream.write_all(&test_command_data[..]).unwrap();
    //     stream.flush().unwrap();
    //     i += 1;

    //     if i > 1000 {
    //         break;
    //     }
    // }
}

pub fn create_server_thread(file: String) -> Result<thread::JoinHandle<()>, std::io::Error>{
    /*
    get ports
    get addr
    listener = TcpListener::bind(addr)
    for stream in listener.incoming()
        stream.read_exact(ack)
        loop {
            stream.write_all(file_data)
        }
     */

    let handle = thread::Builder::new().name("server thread".to_string()).spawn(move|| {
        info!("Opened server thread");
        thread::sleep(Duration::from_secs(7));
        let ports = get_ports(file.as_str()).unwrap();
        let addrs_iter = &(ports[..]);
        debug!("got addr");
        
        let listener = TcpListener::bind(addrs_iter).unwrap();
        info!("Connection successful!");
        // listener.set_nonblocking(true).unwrap();
        let start_time = std::time::Instant::now();

        for stream in listener.incoming() {
            info!("received TCP stream!");
            match stream {
                Ok(mut stream) => {
                    info!("TCP stream is Ok");
                    stream.set_nodelay(true).unwrap();
                    let mut ack = [0u8; 3];
                    stream.read_exact(&mut ack).unwrap();
                    if &ack == b"ACK" {
                        info!("server thread received ACK");

                        // there was originally a loop here
                        let stream_clone = stream.try_clone();
                        send_finite_test_data(stream);
                        thread::sleep(Duration::from_secs(3));
                        send_streamed_test_data(stream_clone.unwrap());
                    }
                    
                },
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    info!("TCP stream is WouldBlock");
                    if has_timed_out(start_time) {
                        break;
                    }
                }
                Err(_) => {
                    info!("TCP stream is other Err");
                    break
                },
            }
        }
    })
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "Thread spawn failed"))?;

    Ok(handle)
}

fn connect_to_ip_addr(addrs: &[SocketAddr], timeout: Duration) -> std::io::Result<TcpStream> {
    let mut last_err: Option<std::io::Error> = None;
    for addr in addrs {
        info!("attempting to connect to TCP stream through {addr}");
        match TcpStream::connect_timeout(addr, timeout) {
            Ok(stream) => {
                info!("Connected to {}", addr);
                return Ok(stream)
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput, "Could not connect to any valid IP address"
        )
    }))
}

pub fn connect_to_tcp_stream(file: String) -> TcpStream {
    let ports = get_ports(file.as_str()).unwrap();
    let addrs_iter = &(ports[..]);
    let port_connect_timeout = Duration::from_millis(100);
    // let mut addrs_iter = "localhost:8081".to_socket_addrs().unwrap();
    // let addr = addrs_iter.next().unwrap();
    // thread::sleep(Duration::from_secs(1));

    let start_time = std::time::Instant::now();
    let mut stream_result = None;

    while !has_timed_out(start_time) {
        if let Ok(s) = connect_to_ip_addr(addrs_iter, port_connect_timeout) {
            stream_result = Some(s);
            break;
        }
    }
    stream_result.unwrap()
}

pub fn create_listener_thread<'a>(tx: Sender<Vec<u8>>, stream: TcpStream) -> Result<thread::JoinHandle<()>, std::io::Error>{
    /*
    get addr
    stream = TcpStream::connect(addr)
    stream.write(ACK)
        loop {
            packet = from_network(stream)
            tx.send(packet)
        }


     */
    let handle = thread::Builder::new().name("listener thread".to_string()).spawn(move || {
        
        loop {
            let packet = from_network(&stream);
            // debug!("Packet: {}", packet);
            // if !packet.starts_with("Error"){
            // scene_reference.write().unwrap().bhvr_msg_str(&packet.as_str());
            // debug!("Sending packet through tx");
            let send_result = tx.send(packet.to_vec());
            match send_result {
                Ok(_) => {},
                Err(e) => {
                    debug!("Error attempting to send packet from listener to main thread: {}", e);
                }
            }
        }
    })
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "Thread spawn failed"))?;

    Ok(handle)
}

pub fn create_sender_thread(rx: Receiver<Vec<u8>>, mut stream: TcpStream) -> Result<thread::JoinHandle<()>, std::io::Error>{
    let handle = thread::Builder::new().name("sender thread".to_string()).spawn(move || {
        stream.write_all(b"ACK").unwrap();
        stream.flush().unwrap();

        loop {
            let Ok(msg) = rx.try_recv() else {
                continue
            };

            stream.write_all(&msg).unwrap();
            stream.flush().unwrap();
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
        let Ok(msg) = rx.try_recv() else {
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

fn receive_file_chunk(
    rx: &Receiver<Vec<u8>>, 
    buf: &mut Vec<u8>, 
    start_time: std::time::Instant, 
    active_files: &mut HashMap<u64, FileInfo>
) -> Option<Vec<u8>> {
    while buf.len() < CHUNK_METADATA_BYTE_WIDTH && !has_timed_out(start_time){
        let Ok(msg) = rx.try_recv() else {
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

    let file_id = u64::from_be_bytes(file_id_bytes);
    let chunk_offset = u64::from_be_bytes(chunk_offset_bytes);
    let chunk_offset_us = chunk_offset as usize;
    let chunk_length = u32::from_be_bytes(chunk_length_bytes) as usize;
    debug!("L: ID = {}, offset = {}, length = {}", file_id, chunk_offset_us, chunk_length);

    let file_data = active_files.get_mut(&file_id).expect("invalid file");
    
    while buf.len() < CHUNK_METADATA_BYTE_WIDTH+(chunk_length as usize)+CHECKSUM_WIDTH && !has_timed_out(start_time){
        let Ok(msg) = rx.try_recv() else {
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
        debug!("cheksum failed");
        let msg_type = MessageType::REQUEST_RETRANSMIT_CHUNK;
        let mut request_buf = Vec::<u8>::new();
        request_buf.extend_from_slice(&(msg_type as u16).to_be_bytes());
        request_buf.extend_from_slice(&file_id_bytes);
        request_buf.extend_from_slice(&chunk_offset_bytes);
        return Some(request_buf)
    }

    debug!("L: received chunk payload");

    if !file_data.is_definite && chunk_offset != file_data.next_expected_chunk_offset {
        debug!("L: found out-of-order chunk");
        file_data.reorder_buffer.insert(chunk_offset, payload.to_vec());
    } else if !file_data.is_definite {
        append_to_file(file_data.name(), payload.to_vec());
        file_data.next_expected_chunk_offset += chunk_length as u64;
        debug!("L: wrote chunk to file");

        // TODO: clean up
        loop {
            let first_chunk = file_data.reorder_buffer.first_key_value();
            if let Some((offset, _)) = first_chunk {
                if chunk_offset == *offset {
                    let chunk = file_data.reorder_buffer.remove(&chunk_offset).unwrap();
                    append_to_file(file_data.name(), chunk);
                    debug!("L: wrote chunk in queue to file");
                } else {
                    break;
                }
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

fn finish_receiving_file(rx: &Receiver<Vec<u8>>, buf: &mut Vec<u8>, start_time: std::time::Instant, active_files: &mut HashMap<u64, FileInfo>){
    while buf.len() < FILE_END_METADATA_BYTE_WIDTH && !has_timed_out(start_time){
        let Ok(msg) = rx.try_recv() else {
            return
        };

        buf.extend_from_slice(&msg);
    }

    let file_id_bytes: [u8; FILE_ID_BYTE_WIDTH] = buf[MESSAGE_TYPE_BYTE_WIDTH..MESSAGE_TYPE_BYTE_WIDTH+FILE_ID_BYTE_WIDTH]
        .try_into()
        .unwrap();
    let file_id = u64::from_be_bytes(file_id_bytes);
    let file_data = active_files.remove(&file_id).unwrap();
    let name = file_data.name();
    append_to_file(name, file_data.data.to_vec());
    buf.drain(0..FILE_END_METADATA_BYTE_WIDTH);
}

fn finish_receiving_transmission(buf: &mut Vec<u8>){
    buf.drain(0..MESSAGE_TYPE_BYTE_WIDTH);
}

fn append_to_file(file_name: String, data: Vec<u8>){
    let path = Path::new(&file_name);
    let metadata = fs::metadata(path).unwrap();
    let file_len = metadata.len();
    debug!("file length before adding chunk: {file_len}");
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    // let file_contents = String::from_utf8(data).unwrap();
    file.write_all(&data).unwrap();
    let metadata = fs::metadata(path).unwrap();
    let file_len = metadata.len();
    debug!("file length after adding chunk: {file_len}");
    // let _ = writeln!(&mut file, "{}", file_contents.as_str());
}

pub fn receive_file(
    rx_main: &Receiver<Vec<u8>>, 
    tx_sender: &Sender<Vec<u8>>,
    active_files: &mut HashMap<u64, FileInfo>, 
    buf: &mut Vec<u8>
) -> bool {
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
        let Ok(msg) = rx_main.try_recv() else {
            return false
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
    
    let transmission_over = match message_type {
        MessageType::FILE_START => {
            debug!("L: received FILE_START");
            if let Some(file) = receive_file_metadata(&rx_main, buf, start_time) {
                debug!("L: adding {} to active files", file.id);
                active_files.insert(file.id, file);
            }
            false
        },
        MessageType::FILE_CHUNK => {
            debug!("L: received FILE_CHUNK");
            let vec_opt = receive_file_chunk(&rx_main, buf, start_time, active_files);
            if let Some(vec) = vec_opt {
                info!("Requesting chunk to be retransmitted");
                let send_result = tx_sender.send(vec);
                if let Err(e) = send_result {
                    debug!("Error attempting to send packet from listener to main thread: {}", e);
                }
            }
            false
        },
        MessageType::FILE_END => {
            debug!("L: received FILE_END");
            finish_receiving_file(&rx_main, buf, start_time, active_files);
            false
        },
        MessageType::REQUEST_RETRANSMIT_CHUNK => {
            debug!("L: received REQUEST_RETRANSMIT_CHUNK");
            false
        },
        MessageType::TRANSMISSION_END => {
            debug!("L: received TRANSMISSION_END");
            finish_receiving_transmission(buf);
            true
        }
        MessageType::ERROR => {
            debug!("L: received ERROR");
            false
        },
    };

    info!("L: finished processing message");
    transmission_over
}

#[cfg(test)]
mod tests {
    use std::{path::Path, net::{SocketAddr, TcpListener, TcpStream}, sync::mpsc, thread};
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