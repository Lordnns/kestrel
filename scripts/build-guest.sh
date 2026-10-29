#!/usr/bin/env sh
# Build the ARM64 test programs in guest/src into guest/bin.
# Needs clang and ld.lld (any host: they cross-compile to aarch64).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
src="$root/guest/src"
out="$root/guest/bin"
mkdir -p "$out"

CC=${CC:-clang}
LD=${LD:-ld.lld}
# -mgeneral-regs-only keeps the code integer-only until FP/NEON land.
CFLAGS="--target=aarch64-none-elf -ffreestanding -nostdlib -fno-stack-protector \
        -fno-pic -O2 -mgeneral-regs-only -Wall -Werror"

$CC $CFLAGS -c "$src/start.c" -o "$out/start.o"
for f in "$src"/*.c; do
    name=$(basename "$f" .c)
    [ "$name" = start ] && continue
    $CC $CFLAGS -c "$f" -o "$out/$name.o"
    $LD -static -e _start "$out/start.o" "$out/$name.o" -o "$out/$name.elf"
    echo "built guest/bin/$name.elf"
done
# Hand-written assembly programs provide their own _start.
for f in "$src"/*.S; do
    name=$(basename "$f" .S)
    $CC --target=aarch64-none-elf -march=armv8.2-a+lse -c "$f" -o "$out/$name.o"
    $LD -static -e _start "$out/$name.o" -o "$out/$name.elf"
    echo "built guest/bin/$name.elf"
done
rm -f "$out"/*.o
