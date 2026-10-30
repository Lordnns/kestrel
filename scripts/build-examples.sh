#!/usr/bin/env bash
# Build the standard homebrew examples phase 2 acceptance runs (PLAN.md):
# switchbrew/switch-examples (public domain mark) at a pinned commit, built
# with libnx in the public devkitPro toolchain container at a pinned image.
# The built NROs go to target/examples/bin and are not committed.
#
# Needs Docker and curl. On Windows (Git Bash) Docker Desktop must be
# running.
set -euo pipefail

# The pins. switch-examples as of 2026-10-30; the image's libnx is what
# the examples link (its version is printed below).
EXAMPLES_COMMIT=669786898205b7beb25ff1731e72982e6d0397d3
IMAGE=devkitpro/devkita64:20260219@sha256:1fc388c3a0d34bd2045a6dadcb1020e069d5f876a187fd705de14b4440c00282

# name:folder for each example.
EXAMPLES=(
    hello-world:graphics/printing/hello-world
    read-controls:hid/read-controls
    sdmc:fs/sdmc
    barrier:misc/barrier
)

root=$(cd "$(dirname "$0")/.." && pwd)
work=$root/target/examples
src=$work/switch-examples-$EXAMPLES_COMMIT
mkdir -p "$work/bin"

if [ ! -d "$src" ]; then
    echo "fetching switch-examples $EXAMPLES_COMMIT"
    curl -fsSL "https://codeload.github.com/switchbrew/switch-examples/tar.gz/$EXAMPLES_COMMIT" \
        | tar -xz -C "$work"
fi

# Docker Desktop on Windows takes Windows paths; stop Git Bash rewriting
# the container's paths.
mount=$src
if command -v cygpath >/dev/null; then
    mount=$(cygpath -w "$src")
    export MSYS_NO_PATHCONV=1
fi

folders=""
for e in "${EXAMPLES[@]}"; do folders="$folders ${e#*:}"; done
docker run --rm -v "$mount:/src" -w /src "$IMAGE" bash -c "
    set -e
    dkp-pacman -Q libnx devkitA64 switch-tools
    for f in $folders; do make -C \$f -j\$(nproc) >/dev/null; echo built \$f; done
"

for e in "${EXAMPLES[@]}"; do
    name=${e%%:*}
    folder=${e#*:}
    cp "$src/$folder/$(basename "$folder").nro" "$work/bin/$name.nro"
    echo "wrote target/examples/bin/$name.nro"
done
