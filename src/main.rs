use datacom::{run_scene_from_hdf5, run_scene_from_json, run_scene_from_network};

fn main() {
    pretty_env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let should_save_to_file = args[args.len()-1] == "y";

    if args.len() > 1 {
        if args[1].ends_with(".hdf5") {
            // run hdf5 code
            run_scene_from_hdf5(args, should_save_to_file);
        } else if args[1].ends_with(".json") {
            // run json code
            run_scene_from_json(args, should_save_to_file);
        } else {
            // assume user wants the scene constructed from a TCP connection
            run_scene_from_network(args, should_save_to_file);
        }
    } else {
        // assume user wants the scene constructed from a TCP connection
        run_scene_from_network(args, should_save_to_file);
    }
}