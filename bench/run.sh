#!/usr/bin/env bash
#
# Run the renderer benchmark matrix: current wgpu master vs the historical glium renderer,
# over a sweep of drone counts, writing one CSV per (version, scene, repeat) into
# bench/results/.
#
#   bench/run.sh                    # full sweep, both versions
#   bench/run.sh --counts 1 8 64    # subset
#   bench/run.sh --repeats 5        # more repeats, tighter medians
#   bench/run.sh --only master      # skip the baseline
#
# Both versions open a real window: the historical renderer has no offscreen path, so there
# is nothing to run headless against. Leave the machine alone while the sweep runs —
# compositing another window over these will show up in the numbers.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BENCH="${REPO_ROOT}/bench"
RESULTS="${BENCH}/results"
SCENES="${BENCH}/scenes"
WORKTREE="${BENCH}/.baseline"

# --- knobs ------------------------------------------------------------------------------

COUNTS=(1 2 4 8 16 32 64 128)
REPEATS=3
FRAMES=600
WARMUP=60
MAX_SECONDS=20
WIDTH=1280
HEIGHT=720
PRESENT=immediate
ONLY=both
STARTUP_ONLY=0
# Hard ceiling per run, covering scene load as well as measurement. The baseline re-parses
# every .obj per model, so a 64-drone load is minutes of tobj before a frame is drawn.
RUN_TIMEOUT=600

while [[ $# -gt 0 ]]; do
    case "$1" in
        --counts) shift; COUNTS=(); while [[ $# -gt 0 && "$1" != --* ]]; do COUNTS+=("$1"); shift; done ;;
        --repeats) REPEATS="$2"; shift 2 ;;
        --frames) FRAMES="$2"; shift 2 ;;
        --warmup) WARMUP="$2"; shift 2 ;;
        --max-seconds) MAX_SECONDS="$2"; shift 2 ;;
        --size) WIDTH="${2%x*}"; HEIGHT="${2#*x}"; shift 2 ;;
        --present) PRESENT="$2"; shift 2 ;;
        --only) ONLY="$2"; shift 2 ;;
        --run-timeout) RUN_TIMEOUT="$2"; shift 2 ;;
        --startup-only) STARTUP_ONLY=1; shift ;;
        -h|--help) sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown flag: $1" >&2; exit 1 ;;
    esac
done

# --- helpers ----------------------------------------------------------------------------

# macOS ships no coreutils `timeout`. Run in the background, poll, and kill if it overruns.
# A run that has to be killed leaves no CSV, which the analyzer reports as a missing point
# rather than silently averaging over a shorter sample.
run_with_timeout() {
    local secs="$1"; shift
    "$@" >/dev/null 2>&1 &
    local pid=$!
    # 50ms ticks rather than 1s: the timeout does not need the resolution, but time_startup
    # measures around this loop and whole-second granularity quantized every startup figure.
    local ticks=0
    local limit=$(( secs * 20 ))
    while kill -0 "${pid}" 2>/dev/null; do
        if (( ticks >= limit )); then
            echo "    ! timed out after ${secs}s, killing"
            kill -9 "${pid}" 2>/dev/null || true
            wait "${pid}" 2>/dev/null || true
            return 124
        fi
        sleep 0.05
        ticks=$(( ticks + 1 ))
    done
    wait "${pid}" 2>/dev/null || true
    return 0
}

# Wall time for a process to reach its first presented frame: launch, GPU/GL context
# creation, shader compilation, .obj parsing and one frame. Recorded by asking for a single
# measured frame with no warmup, then timing the whole process. Reported separately from the
# steady-state numbers because it is dominated by scene loading, not by the renderer.
time_startup() {
    local start end
    start=$(python3 -c 'import time; print(time.time())')
    DATACOM_BENCH_FRAMES=1 DATACOM_BENCH_WARMUP=0 \
        run_with_timeout "${RUN_TIMEOUT}" "$@" || { echo "null"; return; }
    end=$(python3 -c 'import time; print(time.time())')
    python3 -c "print(round((${end} - ${start}) * 1000, 1))"
}

# --- preflight --------------------------------------------------------------------------

echo "==> generating scenes"
python3 "${BENCH}/gen_scenes.py" --counts "${COUNTS[@]}" --width "${WIDTH}" --height "${HEIGHT}"

echo "==> building master (release)"
( cd "${REPO_ROOT}" && cargo build --release )
MASTER_BIN="${REPO_ROOT}/target/release/datacom"

# `run_scene_from_json` resolves argv[1] beneath data/scene_loading/, so the generated tree
# is linked into place rather than copied — a copy would drift the moment gen_scenes.py runs.
ln -sfn ../../bench/scenes/modern "${REPO_ROOT}/data/scene_loading/bench"

if [[ "${ONLY}" != "master" ]]; then
    if [[ ! -x "${WORKTREE}/target/release/DATACOM" ]]; then
        echo "==> baseline not built; running setup_baseline.sh"
        "${BENCH}/setup_baseline.sh" "${WORKTREE}"
    fi
    BASELINE_BIN="${WORKTREE}/target/release/DATACOM"
fi

if [[ "${STARTUP_ONLY}" == 1 ]]; then
    mkdir -p "${RESULTS}"
else
    rm -rf "${RESULTS}"
    mkdir -p "${RESULTS}"
fi

cat > "${RESULTS}/config.json" <<EOF
{
  "counts": [$(IFS=,; echo "${COUNTS[*]}")],
  "repeats": ${REPEATS},
  "frames": ${FRAMES},
  "warmup": ${WARMUP},
  "max_seconds": ${MAX_SECONDS},
  "window": [${WIDTH}, ${HEIGHT}],
  "present_mode": "${PRESENT}",
  "master_commit": "$(git -C "${REPO_ROOT}" rev-parse --short HEAD)",
  "baseline_commit": "$(git -C "${WORKTREE}" rev-parse --short HEAD 2>/dev/null || echo null)",
  "host": "$(uname -sm)",
  "generated": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

export RUST_LOG=off
export DATACOM_BENCH_FRAMES="${FRAMES}"
export DATACOM_BENCH_WARMUP="${WARMUP}"
export DATACOM_BENCH_MAX_SECONDS="${MAX_SECONDS}"
export DATACOM_BENCH_SIZE="${WIDTH}x${HEIGHT}"

# --- the matrix -------------------------------------------------------------------------

for n in "${COUNTS[@]}"; do
    for rep in $(seq 1 "${REPEATS}"); do
        [[ "${STARTUP_ONLY}" == 1 ]] && break 2

        if [[ "${ONLY}" != "baseline" ]]; then
            echo "==> master   drones=${n} rep=${rep}"
            DATACOM_BENCH_CSV="${RESULTS}/master_n${n}_r${rep}.csv" \
            DATACOM_BENCH_PRESENT="${PRESENT}" \
                run_with_timeout "${RUN_TIMEOUT}" \
                    env -C "${REPO_ROOT}" "${MASTER_BIN}" "bench/drones_${n}.json" n || true

            # Offscreen variant: no swapchain, no present, and a post-submit wait for GPU
            # completion. Onscreen on macOS the window server gates the drawable, so every
            # frame measures the compositor instead of the renderer — see src/bench.rs.
            echo "==> master   drones=${n} rep=${rep} (offscreen)"
            DATACOM_BENCH_CSV="${RESULTS}/masteroffscreen_n${n}_r${rep}.csv" \
            DATACOM_BENCH_OFFSCREEN=1 \
                run_with_timeout "${RUN_TIMEOUT}" \
                    env -C "${REPO_ROOT}" "${MASTER_BIN}" "bench/drones_${n}.json" n || true
        fi

        if [[ "${ONLY}" != "master" ]]; then
            echo "==> baseline drones=${n} rep=${rep}"
            DATACOM_BENCH_CSV="${RESULTS}/baseline_n${n}_r${rep}.csv" \
            DATACOM_BENCH_VIEWPORTS="${SCENES}/legacy/drones_${n}.viewports.json" \
                run_with_timeout "${RUN_TIMEOUT}" \
                    env -C "${WORKTREE}" "${BASELINE_BIN}" "${SCENES}/legacy/drones_${n}.json" || true

            # Second baseline variant with the spinning update thread suppressed. The default
            # run above is the historical version as it actually behaved; this one separates
            # "OpenGL drew slowly" from "the update thread starved the renderer of the scene
            # lock", which the headline ratio on its own cannot distinguish.
            echo "==> baseline drones=${n} rep=${rep} (no update thread)"
            DATACOM_BENCH_CSV="${RESULTS}/baselinenoupd_n${n}_r${rep}.csv" \
            DATACOM_BENCH_VIEWPORTS="${SCENES}/legacy/drones_${n}.viewports.json" \
            DATACOM_BENCH_NO_UPDATE_THREAD=1 \
                run_with_timeout "${RUN_TIMEOUT}" \
                    env -C "${WORKTREE}" "${BASELINE_BIN}" "${SCENES}/legacy/drones_${n}.json" || true
        fi

    done
done

# --- time to first frame ------------------------------------------------------------------

echo "==> time to first frame"
STARTUP="${RESULTS}/startup.csv"
echo "version,drones,rep,startup_ms" > "${STARTUP}"
for n in "${COUNTS[@]}"; do
    for rep in $(seq 1 "${REPEATS}"); do
        if [[ "${ONLY}" != "baseline" ]]; then
            ms=$(DATACOM_BENCH_CSV="${RESULTS}/.startup_master.csv" DATACOM_BENCH_PRESENT="${PRESENT}" \
                time_startup env -C "${REPO_ROOT}" "${MASTER_BIN}" "bench/drones_${n}.json" n)
            echo "master,${n},${rep},${ms}" >> "${STARTUP}"
            echo "    master   n=${n} rep=${rep}: ${ms} ms"
        fi
        if [[ "${ONLY}" != "master" ]]; then
            ms=$(DATACOM_BENCH_CSV="${RESULTS}/.startup_baseline.csv" \
                 DATACOM_BENCH_VIEWPORTS="${SCENES}/legacy/drones_${n}.viewports.json" \
                time_startup env -C "${WORKTREE}" "${BASELINE_BIN}" "${SCENES}/legacy/drones_${n}.json")
            echo "baseline,${n},${rep},${ms}" >> "${STARTUP}"
            echo "    baseline n=${n} rep=${rep}: ${ms} ms"
        fi
    done
done
rm -f "${RESULTS}/.startup_"*.csv

echo
echo "==> analyzing"
python3 "${BENCH}/analyze.py" --results "${RESULTS}"
