#!/usr/bin/env bash
# Phase 2 acceptance (PLAN.md): run the standard homebrew examples that
# scripts/build-examples.sh built, each on an empty SD card with scripted
# presses, save each one's last frame, and check what they show with
# scripts/check-examples.py.
#
# Usage: run-examples.sh [path to kestrel] (default: target/release/kestrel)
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

# name and presses (each held 1 s): Plus ends each example; read-controls
# first gets A.
run() {
    name=$1
    shift
    sd=$(mktemp -d)
    echo "running $name"
    "$kestrel" run "$work/bin/$name.nro" --headless --sd "$sd" "$@" \
        --save-frame "$frames/$name.png"
    rm -rf "$sd"
}
run hello-world --press Plus@2
run read-controls --press A@2 --press Plus@3.5
run sdmc --press Plus@2
run barrier --press Plus@2

# python3, or python where that is its name (Windows).
py=$(command -v python3 || command -v python)
"$py" "$root/scripts/check-examples.py" "$font" "$frames"
