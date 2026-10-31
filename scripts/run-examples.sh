#!/usr/bin/env bash
# Phase 2 acceptance (PLAN.md): run the standard homebrew examples that
# scripts/build-examples.sh built, each on an empty SD card with scripted
# presses, save each one's last frame, and check what they show with
# scripts/check-examples.py; then the same with each guest core on its own
# host thread (--threaded).
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

# run <frames folder> <name> <options>: run one example on an empty card
# and save its last frame. Presses are held 1 s each: Plus ends each
# example; read-controls first gets A, and Plus while A is still held, so
# its last frame shows A.
run() {
    out=$1
    name=$2
    shift 2
    sd=$(mktemp -d)
    echo "running $name $*"
    "$kestrel" run "$work/bin/$name.nro" --headless --sd "$sd" "$@" \
        --save-frame "$out/$name.png"
    rm -rf "$sd"
}
all() { # all <frames folder> <options>
    run "$1" hello-world "${@:2}" --press Plus@2
    run "$1" read-controls "${@:2}" --press A@2 --press Plus@2.5
    run "$1" sdmc "${@:2}" --press Plus@2
    run "$1" barrier "${@:2}" --press Plus@2
}
check() {
    "$py" "$root/scripts/check-examples.py" "$font" "$1"
}

all "$frames"
check "$frames"

# Again with each guest core on its own host thread, in real time
# (--threaded): the frames must not change. The barrier example's threads
# wait for each other, so it runs THREADED_RUNS times (default 5) to catch
# races between the cores.
threaded=$work/frames-threaded
mkdir -p "$threaded"
all "$threaded" --threaded
check "$threaded"
for _ in $(seq 2 "${THREADED_RUNS:-5}"); do
    run "$threaded" barrier --threaded --press Plus@2
    check "$threaded"
done
