#!/usr/bin/env python3
"""Aggregate benchmark CSVs into a comparison report.

Reads `bench/results/*.csv` as written by `run.sh` and emits a Markdown report to stdout
(and to `bench/results/report.md`).

# What gets reported, and why

**Two comparisons, and they do not agree.** *End-to-end throughput* is each version doing
everything it normally does, drawing to a window — what a viewer experienced. *Renderer cost*
strips the application overheads from both sides, leaving what is attributable to the graphics
API. On this pair those two answers differ by more than an order of magnitude, and the gap
between them is the most useful thing the suite has to say.

**Total frame cost, not one span.** The two renderers put GPU time in different places, so
`acquire + render + gpu + present` is summed before comparing. Quoting either version's
`render_ms` against the other's compares different things — see `bench/README.md`.

**Effective FPS, not `1000 / median`.** The baseline's frame times are bimodal: its render
thread alternates between winning and losing the scene lock, so a median lands arbitrarily on
one mode and swings by two orders of magnitude between adjacent points of the sweep. Total
frames over total wall time is what a viewer actually got, and is stable.

**Capped rows are marked and excluded from conclusions.** Below a certain scene size at least
one version is sitting on the display refresh interval rather than working. Those rows measure
the display.

**Percentiles, not means.** Frame time distributions here are heavily right-tailed: the
baseline's render thread contends with its update thread for the scene lock, so it alternates
between very fast frames and multi-hundred-millisecond stalls. A mean folds those together
into a number that describes no actual frame. p95 and p99 are quoted because a visualization
that averages 30 FPS but stalls for half a second is not a 30 FPS visualization.

**Repeats are pooled, not averaged.** Each (version, count) cell pools every frame from every
repeat before taking percentiles. Averaging per-run medians would discard exactly the tail
this is trying to characterize.
"""

import argparse
import csv
import glob
import json
import math
import os
import re
import statistics as st
from collections import defaultdict

# End-to-end pair: each version as it actually behaves, drawing to a window.
APP_VERSIONS = [
    ("master", "wgpu (master)"),
    ("baseline", "glium (bd12cb9)"),
]

# Renderer-cost pair: application overheads stripped from both sides. `masteroffscreen`
# bypasses the swapchain (on macOS the window server gates every onscreen frame);
# `baselinenoupd` suppresses the update thread that otherwise starves its own renderer.
RENDER_VERSIONS = [
    ("masteroffscreen", "wgpu, offscreen"),
    ("baselinenoupd", "glium, no update thread"),
]

VERSIONS = APP_VERSIONS + RENDER_VERSIONS

NAME_RE = re.compile(r"^(masteroffscreen|master|baselinenoupd|baseline)_n(\d+)_r(\d+)\.csv$")

# Below this frame cost a run may be sitting on the display refresh rather than working.
# 120 Hz displays make 8.3 ms the floor; anything near it is reported as capped rather than
# quoted as a measurement.
REFRESH_FLOOR_MS = 9.0


def percentile(values, q):
    """Nearest-rank percentile. No interpolation: every quoted figure is a frame that ran."""
    if not values:
        return float("nan")
    ordered = sorted(values)
    k = max(0, min(len(ordered) - 1, math.ceil(q / 100.0 * len(ordered)) - 1))
    return ordered[k]


def load(results_dir):
    """-> {version: {count: {"intervals": [...], "render": [...], "runs": n, "frames": n}}}"""
    def blank():
        return {
            "intervals": [],
            "render": [],
            "acquire": [],
            "update": [],
            "gpu": [],
            "present": [],
            "cost": [],
            "runs": 0,
            "frames": 0,
            "elapsed_ms": 0.0,
        }

    data = defaultdict(lambda: defaultdict(blank))

    for path in sorted(glob.glob(os.path.join(results_dir, "*.csv"))):
        m = NAME_RE.match(os.path.basename(path))
        if not m:
            continue
        version, count, _rep = m.group(1), int(m.group(2)), int(m.group(3))

        with open(path) as f:
            rows = list(csv.DictReader(f))
        if len(rows) < 2:
            # A run killed by the timeout, or one that never got past warmup. Counted as a
            # missing point rather than folded in as a suspiciously short sample.
            continue

        t = [float(r["t_ms"]) for r in rows]
        cell = data[version][count]
        # Intervals are computed within a run only; the gap between the end of one run and
        # the start of the next is process teardown, not a frame.
        cell["intervals"].extend(t[i + 1] - t[i] for i in range(len(t) - 1))
        cell["render"].extend(float(r["render_ms"]) for r in rows)
        cell["acquire"].extend(float(r["acquire_ms"]) for r in rows)
        cell["update"].extend(float(r.get("update_ms", 0.0)) for r in rows)
        cell["gpu"].extend(float(r.get("gpu_ms", 0.0)) for r in rows)
        cell["present"].extend(float(r["present_ms"]) for r in rows)
        # Total per-frame cost. Summed rather than read off one column because the two
        # renderers put GPU time in different places: wgpu in `gpu_ms` (offscreen) or
        # `acquire_ms` (onscreen), glium in `render_ms` once its driver queue fills and in
        # `present_ms` while it has not. Only the sum means the same thing on both sides.
        cell["cost"].extend(
            float(r["acquire_ms"])
            + float(r["render_ms"])
            + float(r.get("gpu_ms", 0.0))
            + float(r["present_ms"])
            for r in rows
        )
        cell["runs"] += 1
        cell["frames"] += len(rows)
        cell["elapsed_ms"] += t[-1] - t[0]

    return data


def load_startup(results_dir):
    path = os.path.join(results_dir, "startup.csv")
    if not os.path.exists(path):
        return {}
    out = defaultdict(lambda: defaultdict(list))
    with open(path) as f:
        for row in csv.DictReader(f):
            if row["startup_ms"] in ("", "null"):
                continue
            out[row["version"]][int(row["drones"])].append(float(row["startup_ms"]))
    return out


def effective_fps(cell):
    """Frames actually delivered per second of wall time, pooled over repeats.

    Preferred over `1000 / median(interval)` because the baseline's distribution is bimodal
    — its render thread alternates between winning and losing the scene lock — and a median
    over that lands arbitrarily on one mode or the other, swinging by two orders of magnitude
    between adjacent points of the sweep. Total frames over total time is what a viewer
    actually got, and is stable.
    """
    if not cell or cell["elapsed_ms"] <= 0:
        return None
    return 1000.0 * (cell["frames"] - cell["runs"]) / cell["elapsed_ms"]


def fmt(x, places=1):
    if x is None or (isinstance(x, float) and math.isnan(x)):
        return "—"
    return f"{x:,.{places}f}"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--results",
        default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "results"),
    )
    args = ap.parse_args()

    data = load(args.results)
    startup = load_startup(args.results)
    if not data:
        raise SystemExit(f"no result CSVs found in {args.results}; run bench/run.sh first")

    config = {}
    cfg_path = os.path.join(args.results, "config.json")
    if os.path.exists(cfg_path):
        config = json.load(open(cfg_path))

    counts = sorted({c for v in data.values() for c in v})
    manifest_path = os.path.join(os.path.dirname(args.results), "scenes", "manifest.json")
    tris_per_drone = 58032
    if os.path.exists(manifest_path):
        tris_per_drone = json.load(open(manifest_path))["triangles_per_drone"]

    out = []
    w = out.append

    w("# Renderer benchmark: wgpu vs OpenGL\n")
    if config:
        win = config.get("window", [None, None])
        w(f"- **Current:** `{config.get('master_commit')}` (wgpu)")
        w(f"- **Baseline:** `{config.get('baseline_commit')}` (glium/OpenGL)")
        w(f"- **Host:** {config.get('host')}")
        w(f"- **Window:** {win[0]}x{win[1]}, present mode `{config.get('present_mode')}`")
        w(f"- **Sampling:** up to {config.get('frames')} frames after "
          f"{config.get('warmup')} warmup frames, {config.get('repeats')} repeats per point, "
          f"{config.get('max_seconds')}s budget per run")
        w(f"- **Generated:** {config.get('generated')}\n")

    # --- headline -------------------------------------------------------------------------

    def cell(key, n):
        return data.get(key, {}).get(n)

    base_n = counts[0]
    top_n = counts[-1]

    # Points where both renderer-cost runs are far enough above the display refresh interval
    # that neither can be sitting on it. Everything below is reported but not concluded from.
    uncapped = [
        n for n in counts
        if cell("masteroffscreen", n) and cell("baselinenoupd", n)
        and st.median(cell("masteroffscreen", n)["cost"]) > REFRESH_FLOOR_MS * 1.5
        and st.median(cell("baselinenoupd", n)["cost"]) > REFRESH_FLOOR_MS * 1.5
    ]

    # End-to-end ratio across every scene size. Quoting one point misrepresents it: the
    # ratio is strongly non-monotonic, because what dominates the baseline changes with load
    # — lock contention in the middle of the range, GPU work at the top.
    ratios = []
    for n in counts:
        m, b = cell("master", n), cell("baseline", n)
        mf, bf = effective_fps(m), effective_fps(b)
        if mf and bf:
            ratios.append((n, mf / bf, mf, bf))

    w("## Headline\n")
    if ratios:
        lo = min(ratios, key=lambda r: r[1])
        hi = max(ratios, key=lambda r: r[1])
        w("**End to end** — each version doing everything it normally does, across scenes from "
          f"{counts[0]} to {counts[-1]} drones ({counts[0] * tris_per_drone:,} to "
          f"{counts[-1] * tris_per_drone:,} triangles):\n")
        w(f"- The current version is between **{lo[1]:.0f}x and {hi[1]:.0f}x faster**, "
          f"depending on scene size — narrowest at {lo[0]} drones "
          f"({fmt(lo[3], 2)} -> {fmt(lo[2], 2)} FPS), widest at {hi[0]} "
          f"({fmt(hi[3], 2)} -> {fmt(hi[2], 2)} FPS).")
        w("- The ratio is not monotonic because what limits the old version changes with load: "
          "scene-lock contention dominates in the middle of the range, GPU work at the top.\n")

    if uncapped:
        pairs = []
        for n in uncapped:
            mo_c = st.median(cell("masteroffscreen", n)["cost"])
            bn_c = st.median(cell("baselinenoupd", n)["cost"])
            pairs.append((n, mo_c, bn_c))
        worst = max(pairs, key=lambda p: max(p[1] / p[2], p[2] / p[1]))
        w("**Renderer against renderer** — application overheads stripped from both sides, at "
          f"the {len(uncapped)} scene sizes where neither is sitting on the display refresh "
          f"({', '.join(str(n) for n in uncapped)} drones):\n")
        for n, mo_c, bn_c in pairs:
            rel = f"wgpu {bn_c / mo_c:.2f}x faster" if mo_c < bn_c else f"glium {mo_c / bn_c:.2f}x faster"
            w(f"- {n} drones: {fmt(mo_c, 2)} ms vs {fmt(bn_c, 2)} ms — {rel}")
        spread = max(max(p[1] / p[2], p[2] / p[1]) for p in pairs)
        w(f"\nThe two renderers are within {(spread - 1) * 100:.0f}% of each other everywhere "
          "the comparison is valid, in both directions. **There is no renderer-level speedup "
          "in this data.**\n")
        w("Those two findings are the point of the suite. The end-to-end improvement is real "
          "and large; it is almost entirely application architecture rather than the graphics "
          "API. See *Reading these numbers*.\n")

    # --- end to end -----------------------------------------------------------------------

    w("## End-to-end throughput\n")
    w("Frames delivered per second of wall time, each version drawing to a window and doing\n"
      "everything it normally does. Higher is better. This is what a viewer experienced.\n")
    w("| drones | triangles |" + "".join(f" {label} |" for _, label in APP_VERSIONS) + " speedup |")
    w("|---:|---:|" + "---:|" * (len(APP_VERSIONS) + 1))
    for n in counts:
        vals = [effective_fps(cell(k, n)) for k, _ in APP_VERSIONS]
        speedup = f"{vals[0] / vals[1]:.1f}x" if vals[0] and vals[1] else ""
        w(f"| {n} | {n * tris_per_drone:,} |" + "".join(f" {fmt(v)} |" for v in vals)
          + f" {speedup} |")
    w("")

    # --- renderer cost --------------------------------------------------------------------

    w("## Renderer cost\n")
    w("Median total per-frame cost in milliseconds — surface acquisition + draw + GPU +\n"
      "present — with the application overheads that dominate the table above removed from\n"
      "both sides. Lower is better.\n")
    w("The wgpu column renders offscreen and waits for GPU completion; onscreen on macOS the\n"
      "window server gates every frame, so an onscreen number measures the compositor. The\n"
      "glium column has its scene-update thread suppressed, which is what was starving its\n"
      "renderer. Rows marked *capped* are sitting on the display refresh rate rather than\n"
      "working, and are not a measurement of anything.\n")
    w("| drones |" + "".join(f" {label} |" for _, label in RENDER_VERSIONS) + " ratio | |")
    w("|---:|" + "---:|" * (len(RENDER_VERSIONS) + 2))
    for n in counts:
        mo, bn = cell("masteroffscreen", n), cell("baselinenoupd", n)
        mo_c = st.median(mo["cost"]) if mo else None
        bn_c = st.median(bn["cost"]) if bn else None
        if mo_c and bn_c:
            ratio = f"{bn_c / mo_c:.2f}x" if mo_c < bn_c else f"{mo_c / bn_c:.2f}x slower"
        else:
            ratio = ""
        note = "" if n in uncapped else "capped"
        w(f"| {n} | {fmt(mo_c, 2) if mo_c else '—'} | {fmt(bn_c, 2) if bn_c else '—'} | "
          f"{ratio} | {note} |")
    w("")

    # --- where the time goes --------------------------------------------------------------

    w("## Where each frame goes\n")
    w("Median milliseconds per span. The two renderers put GPU time in different places,\n"
      "which is why only the total is comparable: wgpu's lands in `gpu_ms` offscreen or in\n"
      "`acquire_ms` onscreen, glium's in `present_ms` until its driver queue fills and in\n"
      "`render_ms` after that.\n")
    w("| version | drones | acquire | render | gpu | present | total | scene update |")
    w("|---|---:|---:|---:|---:|---:|---:|---:|")
    for key, label in VERSIONS:
        for n in counts:
            c = cell(key, n)
            if not c:
                continue
            # Every column is the median of its own span. The total is the median of the
            # per-frame sum, not the sum of these medians — medians do not add, and deriving
            # one column by subtracting the others produces negative times.
            w(f"| {label} | {n} | {fmt(st.median(c['acquire']), 2)} | "
              f"{fmt(st.median(c['render']), 2)} | {fmt(st.median(c['gpu']), 2)} | "
              f"{fmt(st.median(c['present']), 2)} | "
              f"{fmt(st.median(c['cost']), 2)} | {fmt(st.median(c['update']), 2)} |")
    w("")
    w("Columns are independent medians, so they do not add up to the total, which is the\n"
      "median of the per-frame sum.\n")

    # --- tails ----------------------------------------------------------------------------

    w("## Frame-time distribution\n")
    w("Frame interval in milliseconds. Lower is better. The spread matters as much as the\n"
      "median: a p99 an order of magnitude above p50 is a visualization that visibly hitches.\n")
    w("| version | drones | frames | p50 | p95 | p99 | max |")
    w("|---|---:|---:|---:|---:|---:|---:|")
    for key, label in VERSIONS:
        for n in counts:
            c = cell(key, n)
            if not c:
                continue
            iv = c["intervals"]
            w(f"| {label} | {n} | {c['frames']:,} | {fmt(st.median(iv), 2)} | "
              f"{fmt(percentile(iv, 95), 2)} | {fmt(percentile(iv, 99), 2)} | {fmt(max(iv), 2)} |")
    w("")

    # --- startup --------------------------------------------------------------------------

    if startup:
        w("## Time to first frame\n")
        w("Process launch to first presented frame: GPU context creation, shader\n"
          "compilation, `.obj` parsing and one frame. Dominated by scene loading.\n")
        w("| drones | wgpu (master) | glium (bd12cb9) | ratio |")
        w("|---:|---:|---:|---:|")
        for n in counts:
            ms = startup.get("master", {}).get(n)
            bs = startup.get("baseline", {}).get(n)
            ratio = f"{st.median(bs) / st.median(ms):.1f}x" if ms and bs else ""
            w(f"| {n} | {fmt(st.median(ms)) if ms else '—'} | "
              f"{fmt(st.median(bs)) if bs else '—'} | {ratio} |")
        w("")

    # --- caveats --------------------------------------------------------------------------

    w("## Reading these numbers\n")

    b_lo, nb_lo = cell("baseline", base_n), cell("baselinenoupd", base_n)
    if b_lo and nb_lo:
        w(f"- **The end-to-end win is the application, not OpenGL.** The historical version "
          f"evaluates its scene on a thread that spins with no sleep and holds the `RwLock` "
          f"every draw needs. Suppressing that one thread takes it from "
          f"{fmt(effective_fps(b_lo))} to {fmt(effective_fps(nb_lo))} FPS at {base_n} drone "
          "— with nothing about the renderer changed. That is where the headline speedup "
          "comes from.")

    if uncapped:
        u = uncapped[-1]
        mo_c = st.median(cell("masteroffscreen", u)["cost"])
        bn_c = st.median(cell("baselinenoupd", u)["cost"])
        pct = abs(mo_c - bn_c) / min(mo_c, bn_c) * 100
        lead = "ahead" if mo_c < bn_c else "behind"
        w(f"- **Renderer to renderer, the two are close.** At {u} drones, with both stripped "
          f"of application overhead, wgpu costs {fmt(mo_c, 2)} ms per frame against glium's "
          f"{fmt(bn_c, 2)} ms — {pct:.0f}% {lead}. Any claim of an order-of-magnitude "
          "*rendering* improvement is not supported by this data.")
        capped_pts = [n for n in counts if n not in uncapped]
        if capped_pts:
            w(f"- **Small scenes cannot be compared at all.** At {', '.join(str(c) for c in capped_pts)} "
              f"drones at least one side sits on the display refresh interval rather than "
              "working, so those rows measure the display. Only the uncapped rows "
              f"({', '.join(str(c) for c in uncapped)} drones) support a conclusion.")

    w("- **The onscreen wgpu numbers are compositor-dependent and vary between runs.** macOS "
      "gates drawable acquisition for a window, and whether a given run gets the full rate or "
      "is pinned to 60 Hz is not under the benchmark's control — `ControlFlow::Poll` and "
      "`PresentMode::Immediate` do not lift it. This affects the end-to-end table, which is "
      "why the renderer table renders offscreen instead.")
    w("- **Tail latency is a clear and unambiguous win for the current version**, and unlike "
      "the throughput figures it is not close. See the distribution table.")
    w("- **Effective FPS is total frames over total wall time**, not `1000 / median`. The "
      "baseline's frame times are bimodal — its render thread alternates between winning and "
      "losing the scene lock — and a median over that swings by two orders of magnitude "
      "between adjacent points.")
    w("- Both versions load byte-identical `.obj` files and draw the same 9 meshes per drone, "
      "from the same camera positions, at the same physical window size, with logging off. "
      "Scene pairs are generated from one definition by `bench/gen_scenes.py`.")

    report = "\n".join(out)
    print(report)
    out_path = os.path.join(args.results, "report.md")
    with open(out_path, "w") as f:
        f.write(report + "\n")
    print(f"\n[wrote {out_path}]")


if __name__ == "__main__":
    main()
