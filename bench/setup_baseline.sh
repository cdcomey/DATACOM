#!/usr/bin/env bash
#
# Materialize and build the historical glium renderer for benchmarking.
#
# Creates a detached git worktree at BASELINE_COMMIT, applies bench/patch_baseline.py, and
# builds it in release. Idempotent: re-running against an existing worktree re-applies
# nothing and just rebuilds.
#
# Usage: bench/setup_baseline.sh [worktree-path]

set -euo pipefail

# The last commit on master's own ancestry that still compiles with the OpenGL renderer.
#
# The glium branch tip (6a0e95f, "start of scope implementation") is a broken work-in-progress
# — twelve compile errors in plt.rs and dc.rs — so it cannot be the baseline. bd12cb9 is its
# parent, dated 2025-03-12 and 223 commits behind current master. Anything earlier would be
# comparing against a less complete version of the OpenGL renderer than actually shipped.
BASELINE_COMMIT="bd12cb9"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKTREE="${1:-${REPO_ROOT}/bench/.baseline}"

cd "${REPO_ROOT}"

if ! git rev-parse --verify --quiet "${BASELINE_COMMIT}^{commit}" >/dev/null; then
    echo "error: commit ${BASELINE_COMMIT} not found. Fetch full history first:" >&2
    echo "       git fetch --unshallow" >&2
    exit 1
fi

if [[ -d "${WORKTREE}" ]]; then
    echo "==> reusing worktree ${WORKTREE}"
else
    echo "==> creating worktree ${WORKTREE} at ${BASELINE_COMMIT}"
    # A worktree directory deleted by hand stays registered, and `add` then refuses. Prune
    # first so removing bench/.baseline is enough to force a clean rebuild.
    git worktree prune
    git worktree add --detach "${WORKTREE}" "${BASELINE_COMMIT}"
fi

echo "==> applying benchmark instrumentation"
python3 "${REPO_ROOT}/bench/patch_baseline.py" "${WORKTREE}"

echo "==> building baseline (release)"
# The baseline's own cargo/config.toml pins RUST_LOG=trace for `cargo run`; irrelevant here
# because the runner invokes the built binary directly with RUST_LOG unset.
( cd "${WORKTREE}" && cargo build --release )

BIN="${WORKTREE}/target/release/DATACOM"
if [[ ! -x "${BIN}" ]]; then
    echo "error: expected binary at ${BIN}" >&2
    exit 1
fi

echo
echo "baseline ready: ${BIN}"
