use std::net::{TcpStream, TcpListener};
use std::fs::{self, File};
use std::path::Path;
use log::{debug, info};
use std::thread;
use std::time::Duration;
use std::io::{Read, Write};

use crate::com::{MAX_FILE_NAME_BYTE_WIDTH, get_ports, has_timed_out};


fn send_finite_test_data(mut stream: TcpStream, path_str: &str){
    let path = std::path::Path::new(path_str);
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

fn send_streamed_test_data(mut stream: TcpStream, path_str: &str){
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
    
    
    let path: &Path = std::path::Path::new(path_str);
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
}

pub fn create_server_thread(
    file: String,
    json_file_path: String,
    bin_file_path: String,
) -> Result<thread::JoinHandle<()>, std::io::Error>{
    let handle = thread::Builder::new().name("server thread".to_string()).spawn(move|| {
        info!("Opened server thread");
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
                        let stream_clone = stream.try_clone();
                        send_finite_test_data(stream, &json_file_path);
                        thread::sleep(Duration::from_secs(3));
                        send_streamed_test_data(stream_clone.unwrap(), &bin_file_path);
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
