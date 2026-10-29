#!/usr/bin/env bash
# Bundle a built kestrel binary into a release archive.
#
#   scripts/package.sh <binary> <package-name>
#
# Produces dist/<package-name>.tar.gz (or .zip when the binary ends in .exe)
# containing the binary, the example ARM64 programs, TESTING.md, README and
# LICENSE. tar keeps the executable bit, which plain zip downloads lose.
set -euo pipefail

bin=$1
name=$2
root=$(cd "$(dirname "$0")/.." && pwd)
stage="$root/dist/$name"

rm -rf "$stage"
mkdir -p "$stage/examples"
cp "$bin" "$stage/"
cp "$root"/guest/bin/*.elf "$stage/examples/"
cp "$root/packaging/TESTING.md" "$root/README.md" "$root/LICENSE" "$stage/"

case "$name" in
macos-*)
    cp "$root/packaging/macos/Run self-test.command" "$stage/"
    chmod +x "$stage/Run self-test.command"
    ;;
esac

cd "$root/dist"
if [[ "$bin" == *.exe ]]; then
    if command -v 7z >/dev/null; then
        7z a -tzip "$name.zip" "$name" >/dev/null
    else
        zip -qr "$name.zip" "$name"
    fi
    echo "dist/$name.zip"
else
    chmod +x "$stage/$(basename "$bin")"
    tar czf "$name.tar.gz" "$name"
    echo "dist/$name.tar.gz"
fi
