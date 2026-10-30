#!/usr/bin/env sh
# Build the ARM64 test programs in guest/src into guest/bin.
# Needs clang and ld.lld (any host: they cross-compile to aarch64).
#
# Every C program is built twice: integer-only (name.elf) and with floating
# point and NEON allowed (name-neon.elf), so auto-vectorised code runs
# against the same expected output.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
src="$root/guest/src"
out="$root/guest/bin"
mkdir -p "$out"

CC=${CC:-clang}
LD=${LD:-ld.lld}
BASE="--target=aarch64-none-elf -ffreestanding -nostdlib \
      -fno-stack-protector -fno-pic -O2 -Wall -Werror"
INT="$BASE -mgeneral-regs-only"
# No FMA contraction and no errno, so floating-point results are reproducible.
SIMD="$BASE -ffp-contract=off -fno-math-errno"

build() { # build <name> <suffix> <flags>
    $CC $3 -c "$src/start.c" -o "$out/start$2.o"
    $CC $3 -c "$src/$1.c" -o "$out/$1$2.o"
    $LD -static -e _start "$out/start$2.o" "$out/$1$2.o" -o "$out/$1$2.elf"
    echo "built guest/bin/$1$2.elf"
}

for f in "$src"/*.c; do
    name=$(basename "$f" .c)
    [ "$name" = start ] && continue
    build "$name" "" "$INT"
    build "$name" -neon "$SIMD"
done
# Hand-written assembly programs provide their own _start.
for f in "$src"/*.S; do
    name=$(basename "$f" .S)
    $CC --target=aarch64-none-elf -march=armv8.2-a+lse -c "$f" -o "$out/$name.o"
    $LD -static -e _start "$out/$name.o" -o "$out/$name.elf"
    echo "built guest/bin/$name.elf"
done
rm -f "$out"/*.o
