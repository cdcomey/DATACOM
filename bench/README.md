# Renderer benchmark: wgpu vs the OpenGL version

Measures current `master` against the historical glium/OpenGL renderer on identical scenes,
and produces a report with per-frame timings, percentiles and a load-scaling curve.

```bash
bench/run.sh                       # full sweep, 1-128 drones, ~75 min
bench/run.sh --counts 16 64 --repeats 1 --frames 200 --max-seconds 8  # quick check, ~5 min
bench/run.sh --startup-only        # redo just the time-to-first-frame pass
python3 bench/analyze.py           # re-render the report from existing results
```

Results land in `bench/results/` (gitignored): one CSV per run, plus `report.md`.

The runs open real windows and take over the display for the duration. The historical
version has no offscreen path, so there is nothing to run headless against. Leave the
machine alone while it runs — anything compositing over these windows lands in the numbers.

## Which historical version, and why

**`bd12cb9` "Added shaders" (2025-03-12)**, 223 commits behind current `master`.

That is the last commit on `master`'s own ancestry that still compiles with the OpenGL
renderer. The glium branch tip (`6a0e95f`, "start of scope implementation") is a broken
work-in-progress — twelve compile errors across `plt.rs` and `dc.rs` — so it cannot be the
baseline, and anything earlier than `bd12cb9` would compare against a less complete version
of the OpenGL renderer than actually shipped.

`bench/setup_baseline.sh` puts it in a detached worktree at `bench/.baseline` and applies
`bench/patch_baseline.py`. Delete that directory to force a clean rebuild.

## What is measured

Both binaries write the same CSV:
`frame,t_ms,update_ms,acquire_ms,render_ms,gpu_ms,present_ms`.

| span | wgpu (`master`) | glium (`bd12cb9`) |
|---|---|---|
| `update_ms` | `update_cameras` + `run_behaviors` + text | always 0 — evaluated on a separate thread |
| `acquire_ms` | `surface.get_current_texture()` | `display.draw()` |
| `render_ms` | pass encoding + `queue.submit` | geometry upload + immediate-mode draw calls |
| `gpu_ms` | `device.poll(Wait)` after submit | always 0 — GPU cost is already inside its other spans |
| `present_ms` | `output.present()` | `Frame::finish()` |

**Only the total of those spans is comparable between the two**, because they put GPU time in
different places. `queue.submit` returns immediately, so wgpu's GPU cost lands in `gpu_ms`
offscreen, or in the *next* frame's `acquire_ms` onscreen. OpenGL has no such split: draw
calls block in the driver once its queue fills, so glium's GPU cost sits in `present_ms`
while the queue is short and migrates into `render_ms` as it fills. Quoting either version's
`render_ms` against the other's is comparing different things — the analyzer sums them.

Frame interval is not a column; it is the difference of consecutive `t_ms`, computed by
`analyze.py`. Storing the start timestamp rather than the interval keeps each interval
attributable to the frame that produced it.

Scene evaluation is split into its own span rather than folded into the render cost because
the two versions do it in different *places* — inline on the render thread on `master`, on a
separate thread behind an `RwLock` on the baseline. Without that split, the renderer
comparison would put a renderer-plus-simulation against a renderer alone.

Four runs per point, forming **two separate comparisons that do not give the same answer**.

*End to end* — each version doing everything it normally does, drawing to a window. This is
what a viewer experienced:

- **`master`** — current wgpu renderer.
- **`baseline`** — the historical version as it actually behaved, spinning update thread and
  all.

*Renderer against renderer* — application overheads stripped from both sides, so the
remaining difference is attributable to the graphics API:

- **`masteroffscreen`** — `DATACOM_BENCH_OFFSCREEN=1`: renders to the offscreen texture, never
  touches the swapchain, and waits for GPU completion after submit. Necessary because on
  macOS the window server gates drawable acquisition, so an onscreen frame measures the
  compositor — a post-submit wait there lands within a rounding error of the refresh interval
  whatever the scene contains.
- **`baselinenoupd`** — `DATACOM_BENCH_NO_UPDATE_THREAD=1`: suppresses the thread that spins
  with no sleep and contends for the scene `RwLock` every draw needs. With it running, the
  baseline's renderer is being starved rather than measured.

Neither stripped variant is a fair *product* comparison — the offscreen one skips present,
and the no-update one animates nothing. They exist to answer "how much of the difference is
the renderer", which the end-to-end ratio cannot.

## How the two are held level

The comparison is only worth anything if both versions are doing the same work. What is
controlled, and how:

- **Identical geometry.** Both load byte-identical `blizzard.obj` and `prop.obj` — verified
  by checksum across the two commits. One drone is 1 hull + 8 propellers = 58,032 triangles.
- **Identical scenes.** `bench/gen_scenes.py` emits both schemas from one definition. The
  formats diverged completely between the two versions, and hand-maintaining a matched pair
  is how a benchmark quietly ends up comparing two different workloads.
- **Identical framing.** Same three viewports, same camera positions and look-at targets.
  The historical version hardcodes its viewports in `main.rs`, so the patch teaches it to
  read the layout `gen_scenes.py` writes alongside each scene.
- **Identical window size**, pinned by `DATACOM_BENCH_SIZE`. Both scale with window area and
  their defaults differ.
- **Logging off in both.** The historical `main` unconditionally forces `RUST_LOG=trace`,
  which benchmarks its own logger — several formatted lines per frame to a terminal. The
  patch makes that conditional so an externally set `RUST_LOG` wins.
- **Frame caps lifted on both.** The historical version pins itself to
  `WaitUntil(now + 16.67ms)`; `master` leaves `ControlFlow` at `Wait` and drives frames with
  `request_redraw`, which macOS coalesces to one redraw per display refresh. Both switch to
  `ControlFlow::Poll` while benchmarking. Without this the answer is "60 vs 60" wherever the
  baseline is fast enough.

Everything the patch does is either measurement, removal of a harness artifact, or lifting a
cap — never anything that makes the historical renderer look worse than it was. The full
list, with justification per edit, is in the `bench/patch_baseline.py` docstring.

## Known limitations

**The refresh-rate ceiling.** Neither version can be made to exceed the display refresh rate
reliably in a window on macOS. For `master`, `get_current_texture` blocks on the window
server regardless of `ControlFlow::Poll` and `PresentMode::Immediate`, and whether a given
run gets the full rate or is pinned to 60 Hz varies between runs — it is not under the
benchmark's control. For the baseline, GL swap sits on the refresh interval too.

The renderer table sidesteps this by rendering offscreen on the wgpu side, and by only
concluding from scene sizes where *both* frame costs are well above the refresh interval.
`analyze.py` marks the rest **capped** and says so explicitly. The end-to-end table cannot
sidestep it and should be read as approximate.

**Small scenes prove nothing.** Below roughly 16 drones at 1280x720 on this hardware, at
least one side is sitting on the display rather than working. Those rows are printed for
completeness, not for conclusions.

**The comparison is single-machine.** Everything here is one Apple-silicon Mac. The
architectural finding below travels; the exact ratios do not.

## Interpreting the result

The suite reports the end-to-end ratio and the renderer-only ratio separately because on
this pair **they are very different numbers**, and that difference is the most useful thing
it has to say.

The large end-to-end speedup is real — it is what a viewer experienced — but it is almost
entirely *not* the graphics API. It is the historical version's scene-update thread, which
spins with no sleep and holds the `RwLock` every draw needs, starving its own renderer.
Suppress that one thread and the baseline's throughput improves by an order of magnitude
with nothing about its rendering changed.

Renderer against renderer, once both sides are stripped of application overhead, **there is
no speedup at all**. Across the four scene sizes where the comparison is valid the two land
within 19% of each other, and the sign changes: wgpu is ahead at 16 drones, glium is ahead at
32, 64 and 128. That is measurement noise around parity, not a difference.

The current version's other clear wins are not average throughput:

- **Tail latency.** The baseline's p99 frame interval runs into whole seconds where `master`
  stays in tens of milliseconds. A visualization that stalls for four seconds is broken in a
  way an average does not capture.
- **Load time.** 2.3x to 8.1x faster to first frame, growing with scene size.

Quoting the end-to-end number as evidence that "wgpu is 40x faster than OpenGL" is not
supported by this data. Quoting the renderer number as the improvement users got is equally
wrong. Both belong in any summary.

One asymmetry worth knowing about, visible in the renderer table: wgpu carries a fixed
per-frame cost of roughly 9 ms independent of scene size, while its marginal cost per drone
is slightly lower than glium's. That is why it trails on trivial scenes and catches up as
load grows. The suite does not attribute that fixed cost to anything — three render passes
each clearing a full-size depth attachment is a candidate, but it has not been profiled.

## Files

| file | |
|---|---|
| `run.sh` | drives the matrix; regenerates scenes and rebuilds both binaries first |
| `setup_baseline.sh` | worktree at `bd12cb9`, patch, build |
| `patch_baseline.py` | the instrumentation patch, one asserted-unique edit at a time |
| `gen_scenes.py` | paired scene generator, both schemas from one definition |
| `analyze.py` | CSVs to `report.md` |
| `scenes/` | generated; safe to delete |
| `results/` | generated; gitignored |

Instrumentation on the `master` side lives in [`src/bench.rs`](../src/bench.rs), with hooks
in `state.rs` and `lib.rs`. It is inert unless `DATACOM_BENCH_CSV` is set.
