use winit::{
    event::*,
    event_loop::EventLoop,
    keyboard::{KeyCode, PhysicalKey},
};
use std::sync::{mpsc, Arc};
use std::sync::atomic::AtomicBool;
use log::{info, debug};
use std::fs::remove_file;

mod behaviors_and_entities;
mod ring_buffer;
mod scene;
mod state;
mod model;
mod camera;
mod com;
mod text;
mod server_test;


pub fn run_scene_from_json(args: Vec<String>, should_save_to_file: bool) {
    debug!("Running lib.rs::run_scene_from_json()");

    let event_loop = EventLoop::new().unwrap();
    let title = env!("CARGO_PKG_NAME");
    let window = winit::window::WindowBuilder::new()
        .with_title(title)
        .build(&event_loop)
        .unwrap();

    let mut scene_file_string = String::from("data/scene_loading/");
    scene_file_string.push_str(&args[1]);
    let scene_file = scene_file_string.as_str();

    // State::new uses async code, so we're going to wait for it to finish
    let mut state = pollster::block_on(state::State::new(&window, scene_file, ring_buffer::new_registry()));
    let mut last_render_time = std::time::Instant::now();

    event_loop
        .run(move |event, control_flow| {
            match event {
                Event::DeviceEvent {
                    ref event,
                    .. // We're not using device_id currently
                } => {
                    state.device_input(event);
                }
                Event::WindowEvent {
                    ref event,
                    window_id,
                } if window_id == state.window().id() && !state.window_input(event) => {
                    match event {
                        WindowEvent::CloseRequested
                        | WindowEvent::KeyboardInput {
                            event:
                                KeyEvent {
                                    state: ElementState::Pressed,
                                    physical_key: PhysicalKey::Code(KeyCode::Escape),
                                    ..
                                },
                            ..
                        } => {
                            debug!("Attempting to close window");
                            if should_save_to_file {
                                state.scene.finish_capture(state.size.width, state.size.height);
                            }
                            control_flow.exit()
                        },
                        WindowEvent::Resized(physical_size) => {
                            state.resize(*physical_size);
                        }
                        WindowEvent::RedrawRequested => {
                            // This tells winit that we want another frame after this one
                            state.window().request_redraw();
                            let now = std::time::Instant::now();
                            let dt = now - last_render_time;
                            last_render_time = now;
                            info!("dt = {:?}", dt);
                            state.update(dt, should_save_to_file);

                            match state.render(should_save_to_file) {
                                Ok(_) => {}
                                // Reconfigure the surface if it's lost or outdated
                                Err(
                                    wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated,
                                ) => state.resize(state.size),
                                // The system is out of memory, we should probably quit
                                Err(wgpu::SurfaceError::OutOfMemory | wgpu::SurfaceError::Other) => {
                                    log::error!("OutOfMemory");
                                    control_flow.exit();
                                }

                                // This happens when the a frame takes too long to present
                                Err(wgpu::SurfaceError::Timeout) => {
                                    log::warn!("Surface timeout")
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }

            // while let Ok(message) = rx.try_recv() {
            //     let msg_str = String::from_utf8(message).unwrap();
            //     info!("message received from listener thread: {msg_str}");
            // }
        })
        .unwrap();
}

pub fn run_scene_from_hdf5(args: Vec<String>, should_save_to_file: bool) {
    info!("Program Start!");

    let event_loop = EventLoop::new().unwrap();
    let title = env!("CARGO_PKG_NAME");
    let window = winit::window::WindowBuilder::new()
        .with_title(title)
        .build(&event_loop)
        .unwrap();

    let mut scene_file_string = String::from("data/");
    scene_file_string.push_str(&args[1]);
    let scene_file = scene_file_string.as_str();

    let mut state = pollster::block_on(state::State::new(&window, scene_file, ring_buffer::new_registry()));
    let mut last_render_time = std::time::Instant::now();

    event_loop
        .run(move |event, control_flow| {
            match event {
                Event::DeviceEvent {
                    ref event,
                    .. // We're not using device_id currently
                } => {
                    state.device_input(event);
                }
                Event::WindowEvent {
                    ref event,
                    window_id,
                } if window_id == state.window().id() && !state.window_input(event) => {
                    match event {
                        WindowEvent::CloseRequested
                        | WindowEvent::KeyboardInput {
                            event:
                                KeyEvent {
                                    state: ElementState::Pressed,
                                    physical_key: PhysicalKey::Code(KeyCode::Escape),
                                    ..
                                },
                            ..
                        } => control_flow.exit(),
                        WindowEvent::Resized(physical_size) => {
                            state.resize(*physical_size);
                        }
                        WindowEvent::RedrawRequested => {
                            // This tells winit that we want another frame after this one
                            state.window().request_redraw();
                            let now = std::time::Instant::now();
                            let dt = now - last_render_time;
                            // println!("dt = {}", dt.as_millis());
                            last_render_time = now;
                            state.update(dt, should_save_to_file);

                            match state.render(should_save_to_file) {
                                Ok(_) => {}
                                // Reconfigure the surface if it's lost or outdated
                                Err(
                                    wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated,
                                ) => state.resize(state.size),
                                // The system is out of memory, we should probably quit
                                Err(wgpu::SurfaceError::OutOfMemory | wgpu::SurfaceError::Other) => {
                                    log::error!("OutOfMemory");
                                    control_flow.exit();
                                }

                                // This happens when the a frame takes too long to present
                                Err(wgpu::SurfaceError::Timeout) => {
                                    log::warn!("Surface timeout")
                                }
                            }

                            if should_save_to_file && state.scene.capture_complete() {
                                state.scene.finish_capture(state.size.width, state.size.height);
                                control_flow.exit();
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        })
        .unwrap();
}

pub fn run_scene_from_network(args: Vec<String>, should_save_to_file: bool){
    debug!("Running lib.rs::run_scene_from_network()");

    // create empty scene file
    let scene_file_string = String::from("data/scene_loading/main_scene.json");
    let scene_file = scene_file_string.as_str();
    behaviors_and_entities::create_and_clear_file(scene_file);

    let port_string = "data/ports.toml".to_string();

    // spawn server threads
    debug!("Spawning server threads...");
    if args.len() >= 3 && args[1] == "test" {
        let json_paths: Vec<String> = args[2..args.len()-1].to_vec();
        println!("test mode: {} JSON source(s)", json_paths.len());
        let _server_handles = server_test::create_server_thread(
            port_string.clone(),
            json_paths,
            server_test::StreamMode::Generated,
        ).unwrap();
    }

    let registry = ring_buffer::new_registry();
    let base_scene_written = Arc::new(AtomicBool::new(false));

    // the assembly-main communication is high-level, and only needs to send an enum
    // bind UDP sockets for all configured addresses
    let (tx_assembly, rx_assembly) = mpsc::channel::<com::AssemblyMessage>();
    let sockets = com::connect_to_all_udp_sockets(port_string);
    let num_streams = sockets.len();
    let mut listeners = Vec::new();

    // for every successful connection, spawn a listener, sender, and assembler thread for that connection
    debug!("spawning client threads...");
    for socket in sockets {
        // the communication between sender, listener, and assembler is lower-level, and require byte vectors
        let (tx_listener, rx_listener) = mpsc::channel::<Vec<u8>>();
        let (tx_sender, rx_sender) = mpsc::channel::<Vec<u8>>();
        let listener = com::create_listener_thread(tx_listener, Arc::clone(&socket)).unwrap();
        listeners.push(listener);
        com::create_sender_thread(rx_sender, Arc::clone(&socket)).unwrap();
        com::create_assembly_thread(rx_listener, tx_sender, tx_assembly.clone(), Arc::clone(&registry), Arc::clone(&base_scene_written)).unwrap();
    }

    // initial file transfer
    // wait until all initial transmissions are complete before proceeding
    debug!("assembling initial files...");
    let mut completed = 0;
    let mut extra_scene_jsons: Vec<String> = Vec::new();
    loop {
        if listeners.iter().any(|l| l.is_finished()) { break; }
        match rx_assembly.try_recv() {
            Ok(com::AssemblyMessage::TransmissionComplete) => {
                completed += 1;
                if completed == num_streams { break; }
            }
            Ok(com::AssemblyMessage::SceneFileAssembled(json)) => {
                extra_scene_jsons.push(json);
            }
            _ => {}
        }
    }
    debug!("finished assembling files!");

    if listeners.iter().any(|l| l.is_finished()) {
        remove_file("data/scene_loading/main_scene.json").unwrap();
        panic!("listener thread terminated before event loop could start");
    }

    // create window and event loop
    let event_loop = EventLoop::new().unwrap();
    let title = env!("CARGO_PKG_NAME");
    let window = winit::window::WindowBuilder::new()
        .with_title(title)
        .build(&event_loop)
        .unwrap();

    // create wgpu state
    // State::new uses async code, so we're going to wait for it to finish
    debug!("building State...");
    let mut state = pollster::block_on(state::State::new(&window, scene_file, Arc::clone(&registry)));
    debug!("State finished!");

    // each server will be sending its own scene file, containing entities, viewports, etc
    // we want the entities from every scene file, but only one actual scene file
    // so we use the first received scene file, and merely add the entities from every other file into that scene
    for json in extra_scene_jsons {
        state.scene.append_entities_from_json_str(&json, &registry);
    }

    let mut last_render_time = std::time::Instant::now();
    let mut transmission_ended = false;
    let mut transmissions_remaining = num_streams;
    let mut force_exit = false;
    let mut capture_saved = false;

    // com::create_listener_thread(tx).unwrap();
    debug!("about to start event loop");

    event_loop
        .run(move |event, control_flow| {
            match event {
                Event::DeviceEvent {
                    ref event,
                    .. // We're not using device_id currently
                } => {
                    state.device_input(event);
                }
                Event::WindowEvent {
                    ref event,
                    window_id,
                } if window_id == state.window().id() && !state.window_input(event) => {
                    match event {
                        WindowEvent::CloseRequested
                        | WindowEvent::KeyboardInput {
                            event:
                                KeyEvent {
                                    state: ElementState::Pressed,
                                    physical_key: PhysicalKey::Code(KeyCode::Escape),
                                    ..
                                },
                            ..
                        } => {
                            debug!("Attempting to close window");
                            force_exit = true;
                        },
                        WindowEvent::Resized(physical_size) => {
                            state.resize(*physical_size);
                        }
                        WindowEvent::RedrawRequested => {
                            // This tells winit that we want another frame after this one
                            state.window().request_redraw();
                            let now = std::time::Instant::now();
                            let dt = now - last_render_time;
                            last_render_time = now;
                            state.update(dt, should_save_to_file);

                            match state.render(should_save_to_file) {
                                Ok(_) => {}
                                // Reconfigure the surface if it's lost or outdated
                                Err(
                                    wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated,
                                ) => state.resize(state.size),
                                // The system is out of memory, we should probably quit
                                Err(wgpu::SurfaceError::OutOfMemory | wgpu::SurfaceError::Other) => {
                                    log::error!("OutOfMemory");
                                    control_flow.exit();
                                }

                                // This happens when the a frame takes too long to present
                                Err(wgpu::SurfaceError::Timeout) => {
                                    log::warn!("Surface timeout")
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            
            debug!("M: reading streamed files...");
            if matches!(rx_assembly.try_recv(), Ok(com::AssemblyMessage::TransmissionComplete)) {
                transmissions_remaining -= 1;
                if transmissions_remaining == 0 {
                    transmission_ended = true;
                }
            }
            if !capture_saved && (force_exit || (transmission_ended && state.scene.all_streams_exhausted())) {
                capture_saved = true;
                if should_save_to_file {
                    state.scene.finish_capture(state.size.width, state.size.height);
                }
                control_flow.exit();
            }
        })
        .unwrap();

    // debug!("waiting for threads to wrap up");
    // listener.join().unwrap();
    // debug!("Listener thread closed");
    // sender.join().unwrap();
    // debug!("Sender thread closed");

    _ = remove_file("data/scene_loading/main_scene.json");
}