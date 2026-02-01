use winit::{
    event::*,
    event_loop::EventLoop,
    keyboard::{KeyCode, PhysicalKey},
};
use std::sync::mpsc;
use std::collections::HashMap;
use log::{info, debug};
use std::fs::{File, remove_file};
use std::path::Path;
use std::io::Write;

mod behaviors_and_entities;
mod scene;
mod state;
mod model;
mod camera;
mod com;
mod text;


pub async fn run_scene_from_json(args: Vec<String>) {
    debug!("Running lib.rs::run_scene_from_json()");

    // let (tx, rx): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) = mpsc::channel();

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
    let mut state = state::State::new(&window, scene_file).await;
    let mut last_render_time = std::time::Instant::now();

    // com::create_listener_thread(tx).unwrap();

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
                            state.update(dt, false);

                            match state.render(false) {
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

pub async fn run_scene_from_hdf5(args: Vec<String>, should_save_to_file: bool) {
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

    let mut state = state::State::new(&window, scene_file).await;
    let mut last_render_time = std::time::Instant::now();

    // State::update() calls scene.run_behaviors(), which calls entity.run_behaviors() on every entity
    // in the JSON implementation, objects get their existence and behaviors from JSONs
    // in this implementation, objects get their existence from the Vehicles section
    // and their behaviors from the data below
    /*
        our data is divided into timesteps and data point (pos, rot)
        we want to run state.update() every timestep
        we want every timestep to run in time (eg timestep 1.002 should occur 1.002 seconds after starting)
        step 1: figure out how to make a loop run every timestep
            start = now
            func()
            end = now
            sleep(timestep - (end-start))

        step 2: figure out how to get each entity its data
            lib.rs has the data
            State->Scene->Entity
            state.update()
                scene.run_behaviors()
                    entity.run_behaviors()
            give each entity its data
        
        root->Vehicles->vehicle_name (eg Blizzard_0)

        step 3: construct an initial scene given HDF5 info
            lib.rs::run_scene_from_hdf5 takes in a filepath
            runs scenes_and_entities::State::new(&window, filepath, filetype)
            if filetype = hdf5:
                run Scene::load_scene_from_hdf5(filepath, &device, &model_bind_group_layout)
        
        for entity in scene.entities:
            entity.set_pos(scene.pos_data[entity_index][timestep])
            
     */

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
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        })
        .unwrap();
}

pub async fn run_scene_from_network(args: Vec<String>){
    debug!("Running lib.rs::run_scene_from_network()");

    // TODO: change to something more generic
    let scene_file_string = String::from("data/scene_loading/main_scene.json");
    let scene_file = scene_file_string.as_str();
    behaviors_and_entities::create_and_clear_file(scene_file);

    let (tx_sender, rx_sender): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) = mpsc::channel();
    let (tx_listener, rx_listener): (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) = mpsc::channel();

    let port_string = "data/ports.toml".to_string();
    // let server_test_result = com::create_server_thread(port_string.clone());
    // let server_test = server_test_result.unwrap();

    let stream = com::connect_to_tcp_stream(port_string);
    let listener_result = com::create_listener_thread(tx_listener, stream.try_clone().unwrap());
    let listener = listener_result.unwrap();

    let sender_result = com::create_sender_thread(rx_sender, stream);

    // files that the receiver is getting data about and writing to
    let mut active_files: HashMap<u64, com::FileInfo> = HashMap::new();
    let mut buf: Vec<u8> = Vec::new();
    
    // initial file transfer
    loop {
        // debug!("active files len = {}", active_files.len());
        if listener.is_finished() || com::receive_file(&rx_listener, &tx_sender, &mut active_files, &mut buf){
            break;
        }
    }

    if listener.is_finished() {
        // clean up
        remove_file("data/scene_loading/main_scene.json").unwrap();
        panic!("listener thread terminated before event loop could start");
    }

    // run_scene_from_json(modified_args).await;

    let event_loop = EventLoop::new().unwrap();
    let title = env!("CARGO_PKG_NAME");
    let window = winit::window::WindowBuilder::new()
        .with_title(title)
        .build(&event_loop)
        .unwrap();

    // State::new uses async code, so we're going to wait for it to finish
    let mut state = state::State::new(&window, scene_file).await;
    let mut last_render_time = std::time::Instant::now();

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
                            state.update(dt, false);

                            match state.render(false) {
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
            com::receive_file(&rx_listener, &tx_sender, &mut active_files, &mut buf);
        })
        .unwrap();

    // debug!("waiting for threads to wrap up");
    // listener.join().unwrap();
    // debug!("Listener thread closed");
    // sender.join().unwrap();
    // debug!("Sender thread closed");

    _ = remove_file("data/scene_loading/main_scene.json");
}