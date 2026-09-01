//! Frame-time instrumentation for the renderer benchmark suite in [bench/](../bench).
//!
//! This module is **inert unless `DATACOM_BENCH_CSV` is set** — with no bench environment
//! present, `Recorder::from_env` produces a disabled recorder whose `record` returns
//! immediately and whose `flush` writes nothing. It exists so the wgpu renderer and the
//! historical glium renderer (patched by `bench/baseline.patch`) emit byte-identical CSVs
//! from equivalent points in their frame loops, which is the only reason the two are
//! comparable at all.
//!
//! # Where the phase boundaries are
//!
//! A frame is split into three spans, chosen so each one means the same thing in both
//! renderers:
//!
//! | span | wgpu (this crate) | glium (baseline) |
//! |---|---|---|
//! | `update_ms` | `update_cameras` + `run_behaviors` + text | **always 0** — see below |
//! | `acquire_ms` | `surface.get_current_texture()` | `display.draw()` |
//! | `render_ms` | pass encoding + `queue.submit` | geometry upload + immediate-mode draw calls |
//! | `gpu_ms` | `device.poll(Wait)` after submit | **always 0** — see below |
//! | `present_ms` | `output.present()` | `Frame::finish()` |
//!
//! Scene evaluation is split into its own `update_ms` span rather than folded into
//! `render_ms`, because the two versions do it in different *places*: this crate evaluates
//! the scene inline on the render thread, while the baseline does it on a separate thread
//! that owns the scene behind an `RwLock`. There is therefore no per-frame update span to
//! record on the baseline at all, and it reports 0.
//!
//! That split is what makes `render_ms` a like-for-like renderer comparison — pure draw
//! cost on both sides, with no simulation folded into one and not the other. It is also why
//! `render_ms` alone is not the headline: on the baseline the update thread's cost does not
//! vanish, it reappears as lock contention inside `render_ms` and as idle time between
//! frames. Only the frame interval accounts for all of it.
//!
//! # Why GPU time is measured separately
//!
//! `render_ms` on this side is CPU encode time only — `queue.submit` returns without waiting
//! for the GPU, so GPU cost lands later, in `acquire_ms`, as swapchain back-pressure. On the
//! baseline there is no such separation: OpenGL draw calls block in the driver once its queue
//! fills, so GPU cost is already inside the baseline's `render_ms`. Comparing the two
//! `render_ms` columns directly would therefore flatter this side.
//!
//! `gpu_ms` closes that gap: `render` blocks in `device.poll(Wait)` after submitting and
//! records how long the GPU took to drain, so `render_ms + gpu_ms` is comparable to the
//! baseline's `render_ms`. The comparison it produces is conservative — waiting for full
//! completion every frame gives up the CPU/GPU overlap the baseline keeps, because nothing
//! forces the baseline to synchronize. A win measured this way is a win with a handicap.
//!
//! **This only means anything with `DATACOM_BENCH_OFFSCREEN=1`.** Rendering to the swapchain,
//! the command buffer writes into a drawable whose completion the window server gates, so a
//! post-submit wait measures the compositor rather than the GPU and lands within a rounding
//! error of the refresh interval no matter what the scene contains. Rendering to the
//! offscreen texture instead — no `get_current_texture`, no `present` — removes the
//! compositor from the measurement, which is the only way to get a GPU number on macOS in a
//! window. `gpu_ms` is therefore recorded only in offscreen mode and is 0 otherwise.
//!
//! The two modes answer different questions and the suite runs both: onscreen for observed
//! end-to-end throughput, offscreen for renderer cost.
//!
//! Frame interval is not stored; it is the difference of consecutive `t_ms` values, which
//! `bench/analyze.py` computes. Storing the start timestamp rather than the interval keeps
//! the interval attributable to the frame that produced it — recording "time since previous
//! frame" against frame *n* attributes frame *n-1*'s cost to it, which is off by one
//! everywhere the frame cost is not constant, i.e. everywhere interesting.
//!
//! # Environment
//!
//! - `DATACOM_BENCH_CSV` — output path. Its presence is what enables the recorder.
//! - `DATACOM_BENCH_FRAMES` — measured frames to collect before exiting (default 600).
//! - `DATACOM_BENCH_MAX_SECONDS` — give up after this much measured wall time and report
//!   whatever frames were collected (default 30). A frame costing several seconds is a
//!   result in its own right; without a budget the slowest points of a sweep decide how
//!   long the whole sweep takes.
//! - `DATACOM_BENCH_WARMUP` — frames discarded first (default 60), covering shader
//!   compilation, buffer allocation and the window settling at its final size.
//! - `DATACOM_BENCH_SIZE` — `WIDTHxHEIGHT` forced window size. Both renderers scale their
//!   fragment cost with window area, so comparing two differently-defaulted window sizes
//!   measures the defaults rather than the renderers.
//! - `DATACOM_BENCH_OFFSCREEN` — `1` to render to the offscreen texture and skip surface
//!   acquisition and present entirely, so `gpu_ms` measures the GPU rather than the window
//!   server. Implies GPU sync.
//! - `DATACOM_BENCH_GPU_SYNC` — `0` to skip the post-submit `device.poll(Wait)` even in
//!   offscreen mode.
//! - `DATACOM_BENCH_PRESENT` — `fifo` | `fifo-relaxed` | `immediate` | `mailbox` |
//!   `auto-vsync` | `auto-novsync`, if the surface offers it.
//!
//! Note that the present mode is **not** the only thing pinning this crate to the refresh
//! rate. The normal event loop leaves `ControlFlow` at its `Wait` default and drives frames
//! with `request_redraw`, which macOS coalesces to one redraw per display refresh no matter
//! what the swapchain is configured for. `run_scene_from_json` therefore switches to
//! `ControlFlow::Poll` when the recorder is enabled; without that, an uncapped present mode
//! still measures 60 FPS.

use std::time::{Duration, Instant};

/// One frame's timing, in milliseconds.
struct Sample {
    t_ms: f64,
    update_ms: f64,
    acquire_ms: f64,
    render_ms: f64,
    gpu_ms: f64,
    present_ms: f64,
}

/// Collects per-frame timings and writes them as CSV once the requested frame count is in.
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
        let csv_path = std::env::var("DATACOM_BENCH_CSV")
            .ok()
            .filter(|p| !p.is_empty());
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

    /// True once `DATACOM_BENCH_FRAMES` measured frames have been collected. The caller is
    /// expected to `flush` and exit; nothing here terminates the process on its own.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Record one frame: when it began, and how long each of its four spans took.
    pub fn record(
        &mut self,
        start: Instant,
        update: Duration,
        acquire: Duration,
        render: Duration,
        gpu: Duration,
        present: Duration,
    ) {
        if !self.enabled() || self.finished {
            return;
        }

        self.seen += 1;
        if self.seen <= self.warmup {
            // The origin keeps moving until warmup is over, so `t_ms` starts at 0 on the
            // first measured frame instead of carrying the warmup's stalls as an offset.
            self.origin = Some(start);
            return;
        }

        let origin = *self.origin.get_or_insert(start);
        self.samples.push(Sample {
            t_ms: start.duration_since(origin).as_secs_f64() * 1e3,
            update_ms: update.as_secs_f64() * 1e3,
            acquire_ms: acquire.as_secs_f64() * 1e3,
            render_ms: render.as_secs_f64() * 1e3,
            gpu_ms: gpu.as_secs_f64() * 1e3,
            present_ms: present.as_secs_f64() * 1e3,
        });

        if self.samples.len() >= self.limit
            || start.duration_since(origin).as_secs_f64() >= self.max_seconds
        {
            self.finished = true;
        }
    }

    /// Write the collected samples. Repeated calls are no-ops, because a render loop may
    /// dispatch further events between `finished` going true and the loop actually exiting.
    pub fn flush(&mut self) {
        if self.flushed {
            return;
        }
        let Some(path) = self.csv_path.clone() else {
            return;
        };
        self.flushed = true;

        let mut out =
            String::from("frame,t_ms,update_ms,acquire_ms,render_ms,gpu_ms,present_ms\n");
        for (i, s) in self.samples.iter().enumerate() {
            out.push_str(&format!(
                "{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}\n",
                i + 1,
                s.t_ms,
                s.update_ms,
                s.acquire_ms,
                s.render_ms,
                s.gpu_ms,
                s.present_ms
            ));
        }

        match std::fs::write(&path, out) {
            Ok(()) => eprintln!("[bench] wrote {} frames to {}", self.samples.len(), path),
            Err(e) => eprintln!("[bench] FAILED to write {}: {}", path, e),
        }
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Forced window size from `DATACOM_BENCH_SIZE=WIDTHxHEIGHT`, if set and parseable.
pub fn window_size() -> Option<(u32, u32)> {
    let raw = std::env::var("DATACOM_BENCH_SIZE").ok()?;
    let (w, h) = raw.split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Present mode requested by `DATACOM_BENCH_PRESENT`, if the surface supports it.
///
/// Falls back to the surface's own first-listed mode and says so on stderr, rather than
/// panicking: an unsupported mode is a property of the machine under test, and a run that
/// silently reports vsync-capped numbers as uncapped is worse than one that admits it could
/// not lift the cap.
pub fn present_mode(available: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    let default = available.first().copied().unwrap_or(wgpu::PresentMode::Fifo);

    let Ok(raw) = std::env::var("DATACOM_BENCH_PRESENT") else {
        return default;
    };

    let requested = match raw.to_ascii_lowercase().as_str() {
        "fifo" => wgpu::PresentMode::Fifo,
        "fifo-relaxed" => wgpu::PresentMode::FifoRelaxed,
        "immediate" => wgpu::PresentMode::Immediate,
        "mailbox" => wgpu::PresentMode::Mailbox,
        "auto-novsync" => wgpu::PresentMode::AutoNoVsync,
        "auto-vsync" => wgpu::PresentMode::AutoVsync,
        other => {
            eprintln!("[bench] unknown DATACOM_BENCH_PRESENT={other:?}, using {default:?}");
            return default;
        }
    };

    // AutoVsync/AutoNoVsync are resolved by wgpu itself and are never listed in a surface's
    // capabilities, so they bypass the support check.
    let is_auto = matches!(
        requested,
        wgpu::PresentMode::AutoVsync | wgpu::PresentMode::AutoNoVsync
    );

    if is_auto || available.contains(&requested) {
        eprintln!("[bench] present mode: {requested:?} (available: {available:?})");
        requested
    } else {
        eprintln!(
            "[bench] present mode {requested:?} unsupported on this surface \
             (available: {available:?}); falling back to {default:?}"
        );
        default
    }
}

/// Render to the offscreen texture instead of the swapchain, skipping acquire and present.
///
/// This is what makes `gpu_ms` a GPU measurement rather than a compositor measurement — see
/// the module docs.
pub fn offscreen() -> bool {
    matches!(
        std::env::var("DATACOM_BENCH_OFFSCREEN").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Whether to block on GPU completion after submitting, so `gpu_ms` is real.
///
/// Only ever true in offscreen mode: onscreen, the wait measures the window server. Opt out
/// within offscreen mode with `DATACOM_BENCH_GPU_SYNC=0`.
pub fn gpu_sync() -> bool {
    offscreen()
        && !matches!(
            std::env::var("DATACOM_BENCH_GPU_SYNC").as_deref(),
            Ok("0") | Ok("false")
        )
}
