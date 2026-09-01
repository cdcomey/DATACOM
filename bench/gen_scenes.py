#!/usr/bin/env python3
"""Generate paired benchmark scenes for the current (wgpu) and historical (glium) renderers.

Both renderers get the *same* geometry, the same number of meshes, the same spin behaviors
and the same camera placement — expressed in whichever schema each one parses. Writing them
from one definition is the point of this script: the two scene formats diverged completely
between the two versions, and hand-maintaining a matched pair is how a benchmark quietly
ends up comparing two different workloads.

Outputs, under ``bench/scenes/``:

``modern/drones_<N>.json``
    Current schema. Nested ``Children``, quaternion rotations, per-viewport cameras with
    position/rotation behaviors. Loaded via ``cargo run -- <path>``, which resolves it
    beneath ``data/scene_loading/`` — so the generated tree is symlinked into place by
    ``run.sh`` rather than copied.

``legacy/drones_<N>.json``
    Historical schema at commit ``bd12cb9``. Flat ``Models`` list, axis-angle rotations,
    entity-level ``Behaviors`` addressing models by index. No viewports: that version
    hardcodes them in ``main.rs``, which is why the camera layout ships separately.

``legacy/drones_<N>.viewports.json``
    The camera layout for the run above, in the historical renderer's normalized-device
    viewport coordinates. ``bench/patch_baseline.py`` teaches that version to read this file
    instead of its hardcoded viewport list, which is the only way the two renderers can be
    made to frame the same thing.

# Why the camera pulls back as N grows

Drones are laid out on a square grid and the cameras retreat to keep the whole grid framed.
Holding the camera still instead would keep the triangle count rising while the *covered*
pixel count stayed flat, so the sweep would measure fragment cost falling per drone at the
same time as vertex cost rose, and the curve would mean nothing in particular. Framing the
grid holds screen coverage roughly constant, which leaves geometry throughput and draw-call
count as the thing that actually varies.
"""

import argparse
import json
import math
import os
import shutil

# --- The drone -------------------------------------------------------------------------
#
# Offsets and colors are transcribed from the two versions' respective `test_scene.json`
# files, which describe the same aircraft. The .obj files are byte-identical across the two
# commits (verified: same md5), so one drone is 46,896 + 8x1,392 = 58,032 triangles in both.

HULL_OBJ_MODERN = "test_data/blizzard.obj"
PROP_OBJ_MODERN = "test_data/prop.obj"
HULL_OBJ_LEGACY = "data/blizzard.obj"
PROP_OBJ_LEGACY = "data/prop.obj"

RED = [1.0, 0.0, 0.0]
BLUE = [0.0, 0.0, 1.0]
GREEN = [0.0, 1.0, 0.0]

# (name, position, color, spin sign). Order matters: the historical schema addresses these
# by index from its entity-level `Behaviors` list, so reordering silently re-targets spins.
PROPELLERS = [
    ("propeller FLT", [-0.72, -2.928, 1.191], RED, 1.0),
    ("propeller FLB", [-0.72, -2.928, 0.891], BLUE, -1.0),
    ("propeller FRT", [-0.72, 2.928, 1.191], BLUE, 1.0),
    ("propeller FRB", [-0.72, 2.928, 0.891], RED, -1.0),
    ("propeller RLT", [4.22, -2.928, 1.191], BLUE, 1.0),
    ("propeller RLB", [4.22, -2.928, 0.891], RED, -1.0),
    ("propeller RRT", [4.22, 2.928, 1.191], RED, 1.0),
    ("propeller RRB", [4.22, 2.928, 0.891], BLUE, -1.0),
]

TRIS_HULL = 46896
TRIS_PROP = 1392
TRIS_PER_DRONE = TRIS_HULL + len(PROPELLERS) * TRIS_PROP

GRID_SPACING = 14.0  # A drone is ~10 units nose to tail; this keeps them clear of each other.

# --- Viewport layout -------------------------------------------------------------------
#
# Three viewports, matching the shape of both versions' stock scenes: one full-screen, two
# stacked on the right. Expressed as fractions of the window so the same numbers can be
# turned into the modern renderer's pixel rects and the historical one's [-1, 1] NDC rects.
#
# (name, frac_x, frac_y, frac_w, frac_h, alignment, border_color, camera_direction)
VIEWPORTS = [
    ("main", 0.00, 0.00, 1.00, 1.00, "FullScreen", [0.0, 255.0, 0.0], [-0.80, -0.45, 0.40]),
    ("side", 0.75, 0.00, 0.25, 0.50, "TopRight", [0.0, 0.0, 255.0], [1.00, 0.00, 0.00]),
    ("rear", 0.75, 0.50, 0.25, 0.50, "BottomRight", [255.0, 0.0, 0.0], [0.00, 1.00, 0.25]),
]


def grid_positions(n):
    """Place `n` drones on the smallest centred square grid that holds them."""
    side = math.ceil(math.sqrt(n))
    span = (side - 1) * GRID_SPACING
    out = []
    for i in range(n):
        row, col = divmod(i, side)
        out.append([col * GRID_SPACING - span / 2.0, row * GRID_SPACING - span / 2.0, 0.0])
    return out


def camera_distance(n):
    """Far enough back that the whole grid stays in frame, so screen coverage holds steady."""
    side = math.ceil(math.sqrt(n))
    span = (side - 1) * GRID_SPACING + 12.0
    return max(28.0, span * 1.15)


def camera_position(direction, distance):
    mag = math.sqrt(sum(c * c for c in direction))
    return [round(c / mag * distance, 4) for c in direction]


# --- Current (wgpu) schema --------------------------------------------------------------


def modern_drone(index, position):
    children = [
        {
            "Name": "Blizzard",
            "Position": [0.0, 0.0, 0.0],
            "Rotation": [1.0, 0.0, 0.0, 0.0],
            "ObjectFilePath": HULL_OBJ_MODERN,
            "Color": GREEN,
        }
    ]
    for name, offset, color, sign in PROPELLERS:
        children.append(
            {
                "Name": name,
                "Position": offset,
                "Rotation": [1.0, 0.0, 0.0, 0.0],
                "ObjectFilePath": PROP_OBJ_MODERN,
                "Color": color,
                "Behavior": {
                    "behaviorType": "RotateConstantSpeed",
                    "data": [sign * 0.2, 0.0, 0.2, 0.0],
                },
            }
        )
    return {
        "Name": f"Blizzard {index:03d}",
        "Position": position,
        "Rotation": [1.0, 0.0, 0.0, 0.0],
        "Scale": [1.0, 1.0, 1.0],
        "Children": children,
    }


def modern_scene(n, width, height):
    distance = camera_distance(n)
    viewports = []
    for name, fx, fy, fw, fh, alignment, border, direction in VIEWPORTS:
        viewports.append(
            {
                "x": round(fx * width, 1),
                "y": round(fy * height, 1),
                "w": round(fw * width, 1),
                "h": round(fh * height, 1),
                "camera": {
                    "name": name,
                    "position": camera_position(direction, distance),
                    "rotation": [1.0, 0.0, 0.0, 0.0],
                    # FreeRoam holds still with no keyboard input, and FocusedOnPoint is an
                    # exact look-at — together they reproduce the historical version's fixed
                    # look_at_rh cameras, so neither run drifts and both frame the same grid.
                    "position_behavior": {"type": "FreeRoam", "speed": 8.0},
                    "rotation_behavior": {"type": "FocusedOnPoint", "point": [0.0, 0.0, 0.0]},
                },
                "border color": border,
                "alignment": alignment,
            }
        )

    return {
        "viewports": viewports,
        "terrain": {},
        "entities": [modern_drone(i, p) for i, p in enumerate(grid_positions(n))],
    }


# --- Historical (glium) schema ----------------------------------------------------------


def legacy_drone(index, position):
    models = [
        {
            "Name": "Blizzard",
            "ObjectFilePath": HULL_OBJ_LEGACY,
            "Position": [0.0, 0.0, 0.0],
            "Orientation": [0.0, 0.0, 0.0],
            "Rotation": [0.0, 0.0, 0.0],
            "Color": GREEN + [1.0],
        }
    ]
    behaviors = []
    for i, (name, offset, color, sign) in enumerate(PROPELLERS, start=1):
        models.append(
            {
                "Name": name,
                "ObjectFilePath": PROP_OBJ_LEGACY,
                "Position": offset,
                "Orientation": [0.0, 0.0, 0.0],
                "Rotation": [0.0, 0.0, 0.0],
                "Color": color + [1.0],
            }
        )
        # data = [model index, axis x, axis y, axis z, phase]; the historical
        # `ComponentRotateConstantSpeed` folds rate into the axis magnitude.
        behaviors.append(
            {
                "commandType": "ComponentRotateConstantSpeed",
                "data": [float(i), sign * 1.0, 0.0, 1.0, 0.0],
            }
        )

    return {
        "Name": f"Blizzard {index:03d}",
        "Position": position,
        "Rotation": [0.0, 0.0, 0.0],
        "Scale": [1.0, 1.0, 1.0],
        "Models": models,
        "Behaviors": behaviors,
    }


def legacy_scene(n):
    return {
        "viewports": [],
        "entities": [legacy_drone(i, p) for i, p in enumerate(grid_positions(n))],
    }


def legacy_viewports(n):
    """The same layout in the historical renderer's coordinates.

    Its `Viewport::new_with_camera(root, height, width, ..)` takes a top-left origin in NDC
    — x rightward from -1, y *downward* from +1 — with extents in NDC units, so a full-screen
    viewport is 2.0 x 2.0. Note height precedes width in that signature; the patched baseline
    reads the named fields below rather than a positional list, so the order cannot drift.
    """
    distance = camera_distance(n)
    out = []
    for name, fx, fy, fw, fh, _alignment, _border, direction in VIEWPORTS:
        out.append(
            {
                "name": name,
                "root": [round(-1.0 + 2.0 * fx, 6), round(1.0 - 2.0 * fy, 6)],
                "width": round(2.0 * fw, 6),
                "height": round(2.0 * fh, 6),
                "camera_position": camera_position(direction, distance),
                "camera_target": [0.0, 0.0, 0.0],
            }
        )
    return {"viewports": out}


# --- Driver ------------------------------------------------------------------------------


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--counts",
        type=int,
        nargs="+",
        default=[1, 2, 4, 8, 16, 32, 64],
        help="drone counts to emit (default: 1 2 4 8 16 32 64)",
    )
    ap.add_argument("--width", type=int, default=1280, help="bench window width")
    ap.add_argument("--height", type=int, default=720, help="bench window height")
    ap.add_argument(
        "--out",
        default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "scenes"),
        help="output directory",
    )
    args = ap.parse_args()

    modern_dir = os.path.join(args.out, "modern")
    legacy_dir = os.path.join(args.out, "legacy")
    # Regenerate from scratch: a stale scene left over from a previous --counts is
    # indistinguishable from a current one once run.sh starts globbing the directory.
    shutil.rmtree(args.out, ignore_errors=True)
    os.makedirs(modern_dir)
    os.makedirs(legacy_dir)

    print(f"{'drones':>7}  {'triangles':>10}  {'cam dist':>8}")
    for n in args.counts:
        write(os.path.join(modern_dir, f"drones_{n}.json"), modern_scene(n, args.width, args.height))
        write(os.path.join(legacy_dir, f"drones_{n}.json"), legacy_scene(n))
        write(os.path.join(legacy_dir, f"drones_{n}.viewports.json"), legacy_viewports(n))
        print(f"{n:>7}  {n * TRIS_PER_DRONE:>10,}  {camera_distance(n):>8.1f}")

    manifest = {
        "counts": args.counts,
        "window": [args.width, args.height],
        "meshes_per_drone": 1 + len(PROPELLERS),
        "triangles_per_drone": TRIS_PER_DRONE,
    }
    write(os.path.join(args.out, "manifest.json"), manifest)
    print(f"\nwrote {len(args.counts)} scene pairs to {args.out}")


def write(path, obj):
    with open(path, "w") as f:
        json.dump(obj, f, indent=2)
        f.write("\n")


if __name__ == "__main__":
    main()
