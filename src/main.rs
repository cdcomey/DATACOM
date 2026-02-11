// /*  TO DO:
//     // indicates completed.
//     -Camera
//         - Implement various camera behaviors
//             - Tracking
//                 - Camera with a static position locks on to a moving object
//             - Following 
//                 - Camera is static relative to moving current_frame
//     - Behaviors
//         // - Create command that maps to behavior
//         // - Create command that modifies behaviors in situ
//         - Create command that deletes behaviors in situ
//         // - Allow behaviors to be created from json file
//         // - Create function to iterate over all behaviors in an entity
//     - 2D Display
//         - Basics
//             - Create 2D Viewport
//             - Create 2D render context
//         - More Advanced
//             - Flatten 3D scene and display as 2D
//             - 
//         - 
//     - Shader rework
//         - Apparently it's fine to use many different shaders for different things 
//             - Refactor to add shader for viewport boxes (reduce complexity)
//     - JSON parsing and loading
//         // - Make models loadable from JSON
//         // - Make entities loadable from JSON
//         // - Make behaviors loadable from JSON
//         // - Make scenes loadable from JSON
//         // - Entities commandable from JSON
//         // - All entities in scene can be commanded over JSON
//     - Networking
//         // - Commands sendable over TCP connectio/n
//         // - Commands are receivable over TCP connection
//         // - Multiple commands can be sent in the same json
//         // - Load Scene from network
//         - Reset models and clear scene from command over network
//         - 
//     - Scene Playback
//         - Load playback scene from file
//             - Play scene in real time
//             - Play scene at half, double speed
//             - Play scene frame-by-frame, with ability to advance frame
//     - Text Rendering
//         // - Render text
//         - Render characters individually to control spacing appropriately
//         - Dynamically size font depeneding on window size
//         - Text boxes and text wrapping l
//         ];]

// */

use datacom::{run_scene_from_hdf5, run_scene_from_json, run_scene_from_network};

fn main() {
    pretty_env_logger::init();
    let args: Vec<String> = std::env::args().collect();

    let should_save_to_file = args.len() > 2 && args[2] == "y";

    if args.len() > 1 {
        if args[1].ends_with(".hdf5") {
            // run hdf5 code
            pollster::block_on(run_scene_from_hdf5(args, should_save_to_file));
        } else if args[1].ends_with(".json") {
            // run json code
            pollster::block_on(run_scene_from_json(args));
        } else {
            // assume user wants the scene constructed from a TCP connection
            pollster::block_on(run_scene_from_network(args));
        }
    } else {
        // assume user wants the scene constructed from a TCP connection
        pollster::block_on(run_scene_from_network(args));
    }
}