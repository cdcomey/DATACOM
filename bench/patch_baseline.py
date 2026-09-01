#!/usr/bin/env python3
"""Apply benchmark instrumentation to a checkout of the historical glium renderer.

Run against a worktree at `BASELINE_COMMIT` (see `setup_baseline.sh`). Every edit is an
exact-string replacement asserted to match exactly once, so a patcher run against the wrong
commit fails loudly instead of half-applying.

# What gets changed, and why each change is legitimate

The rule is that nothing here may make the historical renderer *faster* than it shipped; the
edits either add measurement, remove harness artifacts that are not properties of the
renderer, or lift caps that would otherwise clip both versions to the same number.

1. **Honour a pre-set `RUST_LOG`.** The original unconditionally overwrites it with `trace`
   in `main`, so out of the box it benchmarks its own logger — several formatted lines per
   frame to a terminal. Current `master` has no such override. Left alone, this is the single
   largest unfairness in the comparison, and it is a harness artifact, not OpenGL.

2. **Scene path from `argv[1]`.** The original hardcodes one scene. The sweep needs to drive
   it with generated scenes; nothing about loading is otherwise altered.

3. **Font path.** The original hardcodes a Linux path that does not exist on macOS, where it
   panics at startup. Falls back to the same font current `master` picks per platform.

4. **Window size from `DATACOM_BENCH_SIZE`.** Both renderers scale with window area and
   their defaults differ, so an unpinned size compares window defaults.

5. **Viewports from `DATACOM_BENCH_VIEWPORTS`.** The original hardcodes three viewports and
   their cameras in `main`. The sweep needs both versions framing the same grid from the same
   distance. Falls through to the original hardcoded list when unset.

6. **Frame-time recorder.** Mirrors `src/bench.rs` on `master` exactly — same three spans,
   same CSV columns — so the two outputs are comparable at all.

7. **`ControlFlow::Poll` while benchmarking.** The original pins itself to
   `WaitUntil(now + 16.67ms)`, i.e. 60 FPS. Since `master` is also uncapped for the
   measurement (`DATACOM_BENCH_PRESENT=immediate`), leaving this in place would report
   "60 vs 60" wherever the baseline was fast enough to reach the cap.

8. **Skip the TCP listener thread while benchmarking.** It parses `"localhost:8081"` as a
   `SocketAddr`, which fails and panics that thread on startup — unrelated to rendering, and
   it only adds noise to the measurement.

9. **Optional `DATACOM_BENCH_NO_UPDATE_THREAD`.** The historical design evaluates the scene
   on a thread that spins with no sleep, saturating a core and contending on the scene's
   `RwLock` with every draw. That cost is real and is *on* by default, because it is part of
   what the historical version was. The switch exists so the report can separate "OpenGL was
   slower" from "the update thread was starving the renderer" — a distinction the headline
   ratio alone cannot make.
"""

import argparse
import os
import sys

BASELINE_COMMIT = "bd12cb9"

# ---------------------------------------------------------------------------------------
# The instrumentation module, injected ahead of `fn main`. Deliberately a near-transcription
# of src/bench.rs on master: if the two recorders disagree about where a span starts, the
# comparison is measuring the instrumentation.
# ---------------------------------------------------------------------------------------

BENCH_MODULE = r'''
// ===========================================================================================
// Benchmark instrumentation -- added by bench/patch_baseline.py, not part of commit bd12cb9.
// Mirrors src/bench.rs on master so both renderers emit identical CSV from equivalent points.
// ===========================================================================================
mod bench {
    use std::time::{Duration, Instant};

    pub fn font_path() -> String {
        if let Ok(p) = std::env::var("DATACOM_BENCH_FONT") {
            return p;
        }
        if cfg!(target_os = "macos") {
            "/Library/Fonts/Arial Unicode.ttf".to_string()
        } else {
            "/usr/share/fonts/truetype/futura/JetBrainsMono-Bold.ttf".to_string()
        }
    }

    /// `WIDTHxHEIGHT` from `DATACOM_BENCH_SIZE`.
    pub fn window_size() -> Option<(u32, u32)> {
        let raw = std::env::var("DATACOM_BENCH_SIZE").ok()?;
        let (w, h) = raw.split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    }

    pub fn flag(key: &str) -> bool {
        matches!(std::env::var(key).as_deref(), Ok("1") | Ok("true"))
    }

    /// One viewport from the sidecar written by bench/gen_scenes.py.
    pub struct ViewportSpec {
        pub root: [f64; 2],
        pub width: f64,
        pub height: f64,
        pub camera_position: [f64; 3],
        pub camera_target: [f64; 3],
    }

    /// Viewport layout from `DATACOM_BENCH_VIEWPORTS`, or None to keep the hardcoded list.
    pub fn viewport_specs() -> Option<Vec<ViewportSpec>> {
        let path = std::env::var("DATACOM_BENCH_VIEWPORTS").ok()?;
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("[bench] cannot read {}: {}", path, e));
        let parsed: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("[bench] cannot parse {}: {}", path, e));

        let arr = parsed["viewports"].as_array().expect("[bench] missing \"viewports\"");
        let f3 = |v: &serde_json::Value| -> [f64; 3] {
            let a = v.as_array().expect("[bench] expected 3 floats");
            [
                a[0].as_f64().unwrap(),
                a[1].as_f64().unwrap(),
                a[2].as_f64().unwrap(),
            ]
        };

        Some(
            arr.iter()
                .map(|v| {
                    let root = v["root"].as_array().unwrap();
                    ViewportSpec {
                        root: [root[0].as_f64().unwrap(), root[1].as_f64().unwrap()],
                        width: v["width"].as_f64().unwrap(),
                        height: v["height"].as_f64().unwrap(),
                        camera_position: f3(&v["camera_position"]),
                        camera_target: f3(&v["camera_target"]),
                    }
                })
                .collect(),
        )
    }

    struct Sample {
        t_ms: f64,
        acquire_ms: f64,
        render_ms: f64,
        present_ms: f64,
    }

    pub struct Recorder {
        csv_path: Option<String>,
        warmup: usize,
        limit: usize,
        max_seconds: f64,
        seen: usize,
        origin: Option<Instant>,
        samples: Vec<Sample>,
        finished: bool,
        flushed: bool,
    }

    impl Recorder {
        pub fn from_env() -> Self {
            let csv_path = std::env::var("DATACOM_BENCH_CSV").ok().filter(|p| !p.is_empty());
            let limit = env_usize("DATACOM_BENCH_FRAMES", 600);
            let warmup = env_usize("DATACOM_BENCH_WARMUP", 60);
            let max_seconds = std::env::var("DATACOM_BENCH_MAX_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30.0);
            Recorder {
                csv_path,
                warmup,
                limit,
                max_seconds,
                seen: 0,
                origin: None,
                samples: Vec::with_capacity(limit),
                finished: false,
                flushed: false,
            }
        }

        pub fn enabled(&self) -> bool {
            self.csv_path.is_some()
        }

        pub fn finished(&self) -> bool {
            self.finished
        }

        /// No `update` span: this version evaluates its scene on a separate thread, so
        /// there is no per-frame update to time here. The column is emitted as 0 to keep the
        /// CSV schema identical to master's, and its cost shows up instead as lock
        /// contention inside `render` and as idle time between frames.
        pub fn record(&mut self, start: Instant, acquire: Duration, render: Duration, present: Duration) {
            if !self.enabled() || self.finished {
                return;
            }
            self.seen += 1;
            if self.seen <= self.warmup {
                self.origin = Some(start);
                return;
            }
            let origin = *self.origin.get_or_insert(start);
            self.samples.push(Sample {
                t_ms: start.duration_since(origin).as_secs_f64() * 1e3,
                acquire_ms: acquire.as_secs_f64() * 1e3,
                render_ms: render.as_secs_f64() * 1e3,
                present_ms: present.as_secs_f64() * 1e3,
            });
            if self.samples.len() >= self.limit
                || start.duration_since(origin).as_secs_f64() >= self.max_seconds
            {
                self.finished = true;
            }
        }

        pub fn flush(&mut self) {
            if self.flushed {
                return;
            }
            let Some(path) = self.csv_path.clone() else { return };
            self.flushed = true;
            // update_ms and gpu_ms are both structurally 0 here: the scene is evaluated on
            // another thread, and OpenGL draw calls block in the driver so GPU cost is
            // already inside render_ms rather than needing a separate wait.
            let mut out =
                String::from("frame,t_ms,update_ms,acquire_ms,render_ms,gpu_ms,present_ms\n");
            for (i, s) in self.samples.iter().enumerate() {
                out.push_str(&format!(
                    "{},{:.6},0.000000,{:.6},{:.6},0.000000,{:.6}\n",
                    i + 1, s.t_ms, s.acquire_ms, s.render_ms, s.present_ms
                ));
            }
            match std::fs::write(&path, out) {
                Ok(()) => eprintln!("[bench] wrote {} frames to {}", self.samples.len(), path),
                Err(e) => eprintln!("[bench] FAILED to write {}: {}", path, e),
            }
        }
    }

    fn env_usize(key: &str, default: usize) -> usize {
        std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    }
}

'''

EDITS = [
    # -- 1. respect an externally set RUST_LOG ------------------------------------------
    (
        'std::env::set_var("RUST_LOG", "DATACOM=trace, warn cargo run");                                 // Initialize logger',
        # The original forces trace logging on, which benchmarks the logger rather than the
        # renderer. Current master sets nothing, so the benchmark sets nothing here either.
        'if std::env::var("RUST_LOG").is_err() {\n'
        '        std::env::set_var("RUST_LOG", "DATACOM=trace, warn cargo run");\n'
        '    }',
    ),
    # -- 2. scene path from argv --------------------------------------------------------
    (
        # Anchored on the leading indentation: the same call also appears commented out two
        # lines below, and an unanchored match would hit both.
        '    let test_scene = scenes_and_entities::Scene::load_from_json_file("data/scene_loading/test_scene.json");',
        '    let scene_path = std::env::args()\n'
        '        .nth(1)\n'
        '        .unwrap_or_else(|| "data/scene_loading/test_scene.json".to_string());\n'
        '    let test_scene = scenes_and_entities::Scene::load_from_json_file(&scene_path);',
    ),
    # -- 3. font path -------------------------------------------------------------------
    (
        'let (image_atlas, glyph_map) = text::load_font_atlas("/usr/share/fonts/truetype/futura/JetBrainsMono-Bold.ttf", 100.0);',
        'let (image_atlas, glyph_map) = text::load_font_atlas(&bench::font_path(), 100.0);',
    ),
    # -- 8. skip the listener thread while benchmarking ---------------------------------
    (
        '    let listener_thread = thread::Builder::new().name("listener thread".to_string()).spawn(move || {\n'
        '        info!("Opened listener thread");\n'
        '        let addr: SocketAddr = "localhost:8081".parse().unwrap();\n'
        '        com::run_server(scene_ref.clone(), addr);\n'
        '    });',
        '    // `"localhost:8081".parse::<SocketAddr>()` fails and panics this thread on startup.\n'
        '    // Unrelated to rendering; skipped so it does not add noise to the measurement.\n'
        '    let listener_thread = if bench_enabled {\n'
        '        None\n'
        '    } else {\n'
        '        Some(thread::Builder::new().name("listener thread".to_string()).spawn(move || {\n'
        '            info!("Opened listener thread");\n'
        '            let addr: SocketAddr = "localhost:8081".parse().unwrap();\n'
        '            com::run_server(scene_ref.clone(), addr);\n'
        '        }))\n'
        '    };\n'
        '    let _ = &listener_thread;',
    ),
    # -- 9. optional suppression of the spinning update thread --------------------------
    (
        '    let calculation_thread = thread::Builder::new().name("calculation thread".to_string()).spawn(move || {\n'
        '        info!("Started calculation thread");\n'
        '        loop {',
        '    // This thread spins with no sleep, saturating a core and contending on the scene\n'
        '    // RwLock that every draw needs. That is how the historical version worked and it is\n'
        '    // left on by default; DATACOM_BENCH_NO_UPDATE_THREAD=1 turns it off so the report can\n'
        '    // attribute the gap between renderer cost and update-thread starvation.\n'
        '    let skip_updates = bench::flag("DATACOM_BENCH_NO_UPDATE_THREAD");\n'
        '    let calculation_thread = thread::Builder::new().name("calculation thread".to_string()).spawn(move || {\n'
        '        info!("Started calculation thread");\n'
        '        if skip_updates {\n'
        '            return;\n'
        '        }\n'
        '        loop {',
    ),
    # -- 6. recorder construction -------------------------------------------------------
    (
        '    let cursor_pos: Option<(f64, f64)> = None;',
        '    let cursor_pos: Option<(f64, f64)> = None;\n'
        '    let mut bench = bench::Recorder::from_env();',
    ),
    # -- 6. span boundaries: acquire ----------------------------------------------------
    (
        '        let mut current_frame = gui.display.draw();\n'
        '        // current_frame.clear_color(1.0, 1.0, 1.0, 1.0);\n'
        '\n'
        '        current_frame.clear_color_and_depth((0.0, 0.0, 0.0, 1.0), 1.0);',
        '        let bench_frame_start = std::time::Instant::now();\n'
        '        let mut current_frame = gui.display.draw();\n'
        '        let bench_acquire = bench_frame_start.elapsed();\n'
        '        let bench_render_start = std::time::Instant::now();\n'
        '\n'
        '        current_frame.clear_color_and_depth((0.0, 0.0, 0.0, 1.0), 1.0);',
    ),
    # -- 6/7. span boundaries: render + present, then exit and control flow -------------
    (
        '        current_frame.finish().expect("Frame finishing failed");\n'
        '        let frame_time = Instant::now().duration_since(frame_start_time).as_secs_f64();\n'
        '        text_objects[3].change_text(format!("FPS Counter: {:.1}", 1.0 / frame_time));',
        '        let bench_render = bench_render_start.elapsed();\n'
        '        let bench_present_start = std::time::Instant::now();\n'
        '        current_frame.finish().expect("Frame finishing failed");\n'
        '        bench.record(\n'
        '            bench_frame_start,\n'
        '            bench_acquire,\n'
        '            bench_render,\n'
        '            bench_present_start.elapsed(),\n'
        '        );\n'
        '        if bench.finished() {\n'
        '            bench.flush();\n'
        '            window_target.exit();\n'
        '            return;\n'
        '        }\n'
        '        let frame_time = Instant::now().duration_since(frame_start_time).as_secs_f64();\n'
        '        text_objects[3].change_text(format!("FPS Counter: {:.1}", 1.0 / frame_time));',
    ),
    (
        '        window_target.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(next_frame_time));',
        '        // The original pins itself to 60 FPS here. Uncapped while benchmarking, or the\n'
        '        // measurement reports the cap rather than the renderer.\n'
        '        if bench.enabled() {\n'
        '            window_target.set_control_flow(winit::event_loop::ControlFlow::Poll);\n'
        '        } else {\n'
        '            window_target.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(next_frame_time));\n'
        '        }',
    ),
]

# -- 5. viewports from the generated sidecar ------------------------------------------
#
# Replaces the whole hardcoded three-viewport literal, whose camera positions are exactly
# what needs to become data. Located by its opening line and matching bracket rather than by
# literal text: the original is peppered with trailing whitespace and commented-out entries,
# and reproducing that byte-for-byte in a string constant is not a thing anyone should have
# to maintain.
VIEWPORT_OPEN = "    let mut viewport_refactor = vec![\n"
VIEWPORT_CLOSE = "\n    ];\n"

VIEWPORT_HEAD = """    // The benchmark suite supplies the layout so both renderers frame the same grid from
    // the same distance; with DATACOM_BENCH_VIEWPORTS unset the original hardcoded list is
    // used, so an unbenchmarked build still behaves exactly as it did at bd12cb9.
    fn original_viewports(
        scene_ref: Arc<RwLock<scenes_and_entities::Scene>>,
    ) -> Vec<dc::Viewport> {
        let _ = &scene_ref;
        vec![
"""

VIEWPORT_TAIL = """
        ]
    }

    let mut viewport_refactor = match bench::viewport_specs() {
        Some(specs) => specs
            .into_iter()
            .map(|s| {
                dc::Viewport::new_with_camera(
                    na::Point2::new(s.root[0], s.root[1]),
                    s.height,
                    s.width,
                    scene_ref.clone(),
                    na::Point3::new(
                        s.camera_position[0],
                        s.camera_position[1],
                        s.camera_position[2],
                    ),
                    na::Point3::new(s.camera_target[0], s.camera_target[1], s.camera_target[2]),
                )
            })
            .collect::<Vec<_>>(),
        None => original_viewports(scene_ref.clone()),
    };
"""


def replace_viewports(src):
    """Lift the hardcoded viewport vec! into a fallback fn, in front of a data-driven one."""
    open_at = src.index(VIEWPORT_OPEN)
    close_at = src.index(VIEWPORT_CLOSE, open_at)
    body = src[open_at + len(VIEWPORT_OPEN):close_at]
    return (
        src[:open_at]
        + VIEWPORT_HEAD
        + body
        + VIEWPORT_TAIL
        + src[close_at + len(VIEWPORT_CLOSE):]
    )


# -- 4. forced window size --------------------------------------------------------------

WINDOW_OLD = """        let (window, display) = glium::backend::glutin::SimpleWindowBuilder::new()
            .with_title("DATACOM - Data Communications and Visual Terminal")
            .build(event_loop);"""

WINDOW_NEW = """        // Pinned by DATACOM_BENCH_SIZE so both renderers shade the same number of pixels;
        // otherwise the two versions' differing window defaults are what gets compared.
        let mut builder = glium::backend::glutin::SimpleWindowBuilder::new()
            .with_title("DATACOM - Data Communications and Visual Terminal");
        if let Ok(raw) = std::env::var("DATACOM_BENCH_SIZE") {
            if let Some((w, h)) = raw.split_once('x') {
                if let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
                    builder = builder.with_inner_size(w, h);
                }
            }
        }
        let (window, display) = builder.build(event_loop);"""


def apply(text, old, new, label):
    count = text.count(old)
    if count != 1:
        sys.exit(
            f"patch_baseline.py: '{label}' matched {count} times, expected exactly 1.\n"
            f"Is the worktree at {BASELINE_COMMIT}?"
        )
    return text.replace(old, new, 1)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("worktree", help="path to the baseline worktree")
    args = ap.parse_args()

    main_rs = os.path.join(args.worktree, "src", "main.rs")
    dc_rs = os.path.join(args.worktree, "src", "dc.rs")
    for p in (main_rs, dc_rs):
        if not os.path.exists(p):
            sys.exit(f"patch_baseline.py: {p} not found")

    src = open(main_rs).read()
    if "mod bench {" in src:
        print("baseline already patched; nothing to do")
        return

    src = apply(src, "fn main() {\n", BENCH_MODULE + "fn main() {\n", "bench module")
    for i, (old, new) in enumerate(EDITS):
        src = apply(src, old, new, f"edit {i}")

    if src.count(VIEWPORT_OPEN) != 1:
        sys.exit(f"patch_baseline.py: viewport list not found. Is the worktree at {BASELINE_COMMIT}?")
    src = replace_viewports(src)

    # `bench_enabled` is read by the listener-thread edit above, which sits before the
    # recorder is built; hoist a plain bool to the top of start_program.
    src = apply(
        src,
        "    let scene_ref = Arc::new(RwLock::new(scene));",
        "    let bench_enabled = std::env::var(\"DATACOM_BENCH_CSV\").is_ok();\n"
        "    let scene_ref = Arc::new(RwLock::new(scene));",
        "bench_enabled hoist",
    )

    open(main_rs, "w").write(src)

    dc = open(dc_rs).read()
    dc = apply(dc, WINDOW_OLD, WINDOW_NEW, "window size")
    open(dc_rs, "w").write(dc)

    print(f"patched {main_rs}")
    print(f"patched {dc_rs}")


if __name__ == "__main__":
    main()
