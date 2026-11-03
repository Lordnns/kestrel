#!/usr/bin/env bash
# Phase 2 and phase 5A acceptance (PLAN.md): run the standard homebrew
# examples that scripts/build-examples.sh built, each on an empty SD card
# with scripted presses, save each one's last frame, and check what they
# show with scripts/check-examples.py; then the same with each guest core
# on its own host thread (--threaded).
#
# Usage: run-examples.sh [path to kestrel] (default: target/release/kestrel)
# THREADED_RUNS (default 5): how many threaded runs of the barrier example.
# Needs Python 3 and curl.
set -euo pipefail

# libnx's console font, from the libnx the examples link (4.12.0, in the
# pinned container image; ISC licence).
LIBNX_TAG_COMMIT=7644c9b26099aa2d2145bc72a21ee24190e92085

root=$(cd "$(dirname "$0")/.." && pwd)
kestrel=${1:-$root/target/release/kestrel}
work=$root/target/examples
frames=$work/frames
mkdir -p "$frames"

font=$work/default_font.bin
if [ ! -f "$font" ]; then
    curl -fsSL -o "$font" \
        "https://raw.githubusercontent.com/switchbrew/libnx/$LIBNX_TAG_COMMIT/nx/data/default_font.bin"
fi

# python3, or python where that is its name (Windows).
py=$(command -v python3 || command -v python)

# run_as <frames folder> <program> <frame name> <options>: run one example
# on an empty card and save its last frame as <frame name>.png; kestrel's
# exit status. Presses are held 1 s each: Plus ends each example;
# read-controls first gets A, and Plus while A is still held, so its last
# frame shows A.
run_as() {
    out=$1
    name=$2
    frame=$3
    shift 3
    sd=$(mktemp -d)
    echo "running $frame $*"
    status=0
    "$kestrel" run "$work/bin/$name.nro" --headless --sd "$sd" "$@" \
        --save-frame "$out/$frame.png" || status=$?
    rm -rf "$sd"
    return $status
}
run() { # run <frames folder> <program> <options>
    run_as "$1" "$2" "$2" "${@:3}"
}
all() { # all <frames folder> <options>
    run "$1" hello-world "${@:2}" --press Plus@2
    run "$1" read-controls "${@:2}" --press A@2 --press Plus@2.5
    run "$1" sdmc "${@:2}" --press Plus@2
    run "$1" barrier "${@:2}" --press Plus@2
    run "$1" vt52-demo "${@:2}" --press Plus@2
    run "$1" simplegfx "${@:2}" --press Plus@2
}
check() { # check <frames folder> [--skip <example>...]
    "$py" "$root/scripts/check-examples.py" "$font" "$@"
}

all "$frames"
# The deko3d examples (phase 5B). deko_examples' menu takes Down and A to
# start an example, which draws until Plus; Plus would bring the menu back,
# so each run stops after a number of frames instead (--max-frames), a
# little after the press (the clock is counted here, so the frames are the
# same each run). Not in the threaded runs below, whose clock is real time.
# Its first example only clears; the others, and deko_basic, draw with the
# host's GPU (Vulkan): where kestrel has none (the frozen platforms, a
# machine without a Vulkan driver), they are skipped with a note, unless
# KESTREL_REQUIRE_VULKAN says drawing must work (CI's Linux x86_64 job).
run_as "$frames" deko_examples deko_examples --press A@1 --max-frames 80
skip=()
probe=$(mktemp)
if ! "$kestrel" run "$work/bin/deko_basic.nro" --headless --max-frames 1 >/dev/null 2>"$probe"; then
    if grep -q "with no renderer here\|no Vulkan GPU to draw with" "$probe" \
        && [ -z "${KESTREL_REQUIRE_VULKAN:-}" ]; then
        echo "note: kestrel cannot draw here (no Vulkan); the drawing examples are skipped"
        skip=(--skip deko_basic --skip deko_triangle --skip deko_cube --skip deko_textured_cube)
    fi
fi
rm -f "$probe"
if [ ${#skip[@]} -eq 0 ]; then
    run "$frames" deko_basic --max-frames 60
    run_as "$frames" deko_examples deko_triangle --press Down@0.2 --press A@1.4 --max-frames 110
    run_as "$frames" deko_examples deko_cube --press Down@0.2 --press Down@1.4 \
        --press A@2.6 --max-frames 200
    run_as "$frames" deko_examples deko_textured_cube --press Down@0.2 --press Down@1.4 \
        --press Down@2.6 --press A@3.8 --max-frames 270
fi
check "$frames" ${skip[@]+"${skip[@]}"}

# Again with each guest core on its own host thread, in real time
# (--threaded): the frames must not change. The barrier example's threads
# wait for each other, so it runs THREADED_RUNS times (default 5) to catch
# races between the cores.
threaded=$work/frames-threaded
mkdir -p "$threaded"
all "$threaded" --threaded
deko=(--skip deko_examples --skip deko_basic --skip deko_triangle --skip deko_cube
    --skip deko_textured_cube)
check "$threaded" "${deko[@]}"
for _ in $(seq 2 "${THREADED_RUNS:-5}"); do
    run "$threaded" barrier --threaded --press Plus@2
    check "$threaded" "${deko[@]}"
done
