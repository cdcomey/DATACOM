use std::net::{TcpStream, TcpListener};
use std::fs::{self, File};
use std::path::Path;
use log::{debug, info};
use std::thread;
use std::time::Duration;
use std::io::{Read, Write};

use uuid::Uuid;

use crate::com::{MAX_FILE_NAME_BYTE_WIDTH, get_ports, has_timed_out};


fn send_finite_test_data(mut stream: TcpStream, path_str: &str, addr: std::net::SocketAddr){
    let full_path = format!("data/scene_loading/{}", path_str);
    let path = std::path::Path::new(&full_path);
    let test_command_data_main = fs::read_to_string(path).unwrap();
    let data_len = test_command_data_main.len();

    let message_type = 0u16;
    let file_id = *Uuid::new_v4().as_bytes();
    let file_name_string = format!("{}_main_scene.json", addr.port());
    let file_name_base = file_name_string.as_str();
    let file_name_length = file_name_base.len() as u8;
    let mut file_name = [0u8; MAX_FILE_NAME_BYTE_WIDTH];
    file_name[0..file_name_length as usize].copy_from_slice(file_name_base.as_bytes());
    let file_len = test_command_data_main.len() as u32;

    let mut test_command_data: Vec<u8> = Vec::new();
    test_command_data.extend_from_slice(&message_type.to_be_bytes());
    test_command_data.extend_from_slice(&file_id);
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
        test_command_data.extend_from_slice(&file_id);
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
    test_command_data.extend_from_slice(&file_id);

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

fn send_streamed_test_data(mut stream: TcpStream, path_str: &str){
    let message_type = 0u16;
    let file_id = *Uuid::new_v4().as_bytes();
    let file_name_base = "entity_pos.bin";
    let file_name_length = file_name_base.len() as u8;
    let mut file_name = [0u8; MAX_FILE_NAME_BYTE_WIDTH];
    file_name[0..file_name_length as usize].copy_from_slice(file_name_base.as_bytes());
    let file_len = 0u32;

    let mut test_command_data: Vec<u8> = Vec::new();
    test_command_data.extend_from_slice(&message_type.to_be_bytes());
    test_command_data.extend_from_slice(&file_id);
    test_command_data.extend_from_slice(&[file_name_length]);
    test_command_data.extend_from_slice(&file_name);
    test_command_data.extend_from_slice(&file_len.to_be_bytes());
    test_command_data.extend_from_slice(&[0u8]);

    info!("S: Sending file start frame to stream");
    debug!("{:?}", test_command_data);

    thread::sleep(Duration::from_millis(10));
    stream.write_all(&test_command_data[..]).unwrap();
    stream.flush().unwrap();

    let full_path = format!("data/object_loading/{}", path_str);
    let path: &Path = std::path::Path::new(&full_path);
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
        test_command_data.extend_from_slice(&file_id);
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

    let message_type = 2u16;
    test_command_data.clear();
    test_command_data.extend_from_slice(&message_type.to_be_bytes());
    test_command_data.extend_from_slice(&file_id);

    info!("S: Sending file end to stream");
    thread::sleep(Duration::from_millis(10));
    stream.write_all(&test_command_data[..]).unwrap();
    stream.flush().unwrap();

    let message_type = 4u16;
    test_command_data.clear();
    test_command_data.extend_from_slice(&message_type.to_be_bytes());

    info!("S: Sending transmission end to stream");
    thread::sleep(Duration::from_millis(10));
    stream.write_all(&test_command_data[..]).unwrap();
    stream.flush().unwrap();
}

pub fn create_server_thread(
    file: String,
    json_file_paths: Vec<String>,
    bin_file_path: String,
) -> Result<Vec<thread::JoinHandle<()>>, std::io::Error> {
    let ports = get_ports(file.as_str()).unwrap();
    info!("Spawning {} server thread(s)", json_file_paths.len());

    let mut handles = Vec::new();

    for (i, json_file_path) in json_file_paths.into_iter().enumerate() {
        let addr = ports[i];
        let bin_path = bin_file_path.clone();

        let handle = thread::Builder::new()
            .name(format!("server thread ({})", json_file_path))
            .spawn(move || {
                info!("Server thread binding to {} for {}", addr, json_file_path);
                let listener = TcpListener::bind(addr).unwrap();
                let start_time = std::time::Instant::now();

                for stream in listener.incoming() {
                    match stream {
                        Ok(mut stream) => {
                            info!("Received connection on {} for {}", addr, json_file_path);
                            stream.set_nodelay(true).unwrap();
                            let mut ack = [0u8; 3];
                            stream.read_exact(&mut ack).unwrap();
                            if &ack == b"ACK" {
                                info!("Server thread received ACK for {}", json_file_path);
                                let stream_clone = stream.try_clone().unwrap();
                                send_finite_test_data(stream, &json_file_path, addr);
                                thread::sleep(Duration::from_secs(1));
                                send_streamed_test_data(stream_clone, &bin_path);
                            }
                            break;
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if has_timed_out(start_time) {
                                info!("Server thread timed out for {}", json_file_path);
                                break;
                            }
                        }
                        Err(e) => {
                            info!("Server thread accept error for {}: {}", json_file_path, e);
                            break;
                        }
                    }
                }
            })
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "Thread spawn failed"))?;

        handles.push(handle);
    }

    Ok(handles)
}
