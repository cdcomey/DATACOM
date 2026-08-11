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
mod transform_stream;
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
    // Command authority is declared by a server rather than raced for, so it is claimed at most
    // once per run by whichever stream declares it first. A peer fleet has no operator, declares
    // nothing, and leaves this false — which is what makes global commands inert there.
    let authority_claimed = Arc::new(AtomicBool::new(false));

    // the assembly-main communication is high-level, and only needs to send an enum
    // bind UDP sockets for all configured addresses
    let (tx_assembly, rx_assembly) = mpsc::channel::<com::AssemblyMessage>();
    let sockets = com::connect_to_all_udp_sockets(port_string);
    let configured_streams = sockets.len();
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
        com::create_assembly_thread(rx_listener, tx_sender, tx_assembly.clone(), Arc::clone(&registry), Arc::clone(&base_scene_written), Arc::clone(&authority_claimed)).unwrap();
    }

    // initial file transfer
    // A drone may connect at any point in the session, so we cannot know up front how many of the
    // configured ports will ever be used. Block here until the first one has delivered its scene;
    // every later arrival joins through the event loop instead.
    info!("waiting for the first of {} configured stream(s) to connect...", configured_streams);
    let mut ready_streams = 0usize;
    let mut extra_scene_jsons: Vec<String> = Vec::new();
    loop {
        // listeners now only ever exit by panicking, so losing all of them means nothing can
        // arrive on any port and waiting longer is pointless
        if listeners.iter().all(|l| l.is_finished()) {
            remove_file("data/scene_loading/main_scene.json").unwrap();
            panic!("every listener thread terminated before any stream connected");
        }

        // drain everything already queued before deciding, so drones that came up together are
        // counted as one batch instead of the first one racing ahead of its peers
        while let Ok(msg) = rx_assembly.try_recv() {
            match msg {
                com::AssemblyMessage::StreamReady => ready_streams += 1,
                com::AssemblyMessage::SceneFileAssembled(json) => extra_scene_jsons.push(json),
                com::AssemblyMessage::StreamFinished => {}
                // This phase exists to assemble the scene and object files the renderer is built
                // from; there is no scene to act on yet. Warned rather than dropped silently: the
                // symptom of ignoring one is that pre-command scene data stays on screen, which
                // looks exactly like a command that ran and did nothing.
                com::AssemblyMessage::Command(command) => {
                    log::warn!(
                        "ignoring {:?}: commands are only accepted once a stream is live, not \
                         during the initial file transfer",
                        command,
                    );
                }
            }
        }

        if ready_streams > 0 { break; }

        // startup is not latency-sensitive, and this keeps the wait from pinning a core while
        // we sit here for however long it takes the first drone to come online
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    debug!("finished assembling files!");
    info!("{} stream(s) connected; starting renderer", ready_streams);

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
    // streams that have joined and not yet signalled the end of their data; late joiners push this
    // back up, so "everything has finished" only holds once it drops to zero again
    let mut active_streams = ready_streams;
    // Running out of data is not a reason to quit — a drone can connect at any point in a session,
    // so the client idles and keeps rendering instead. This only tracks whether we have already
    // said so, to keep it to one line per idle period rather than one per frame.
    let mut idle_reported = false;
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
            // drain everything queued rather than one message per frame — a message we drop here
            // is a drone that silently never appears
            while let Ok(msg) = rx_assembly.try_recv() {
                match msg {
                    com::AssemblyMessage::StreamReady => {
                        active_streams += 1;
                        idle_reported = false;
                        info!("stream connected mid-session ({} active)", active_streams);
                    }
                    com::AssemblyMessage::StreamFinished => {
                        active_streams = active_streams.saturating_sub(1);
                    }
                    com::AssemblyMessage::SceneFileAssembled(json) => {
                        // a drone that connected after the window opened; merge its entities into
                        // the scene that is already being rendered
                        let added = state.scene.append_entities_from_json_str(&json, &registry);
                        info!("merged {} entities from a mid-session stream", added);
                    }
                    com::AssemblyMessage::Command(com::ServerCommand::CLEAR_SCENE) => {
                        // Reported rather than done quietly: a stream that joined moments before
                        // this lands is wiped by it, and the count is the only trace of that. The
                        // server is not told, so the log is where a vanished drone shows up.
                        let removed = state.scene.entities.len();
                        state.scene.clear(&registry);
                        info!("cleared the scene ({} entities removed)", removed);
                    }
                    com::AssemblyMessage::Command(command) => {
                        log::warn!("no handler for {:?}", command);
                    }
                }
            }
            // A stream signalling the end of its data does not mean the scene has stopped moving:
            // buffered frames keep draining for a while after the last chunk lands. Wait for both
            // before calling it idle, and say so once so a quiet window reads as "waiting for a
            // drone" rather than as a hang.
            if !idle_reported && active_streams == 0 && state.scene.all_streams_exhausted() {
                idle_reported = true;
                info!("all streams finished and buffered data consumed; idling for new connections");
            }

            // Only the operator ends the session. Unanswered ports stay open for the whole run, so
            // there is always the possibility of another drone connecting.
            if !capture_saved && force_exit {
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