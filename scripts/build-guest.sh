#!/usr/bin/env sh
# Build the ARM64 test programs in guest/src into guest/bin.
# Needs clang and ld.lld (any host: they cross-compile to aarch64).
#
# Every C program is built twice: integer-only (name.elf) and with floating
# point and NEON allowed (name-neon.elf), so auto-vectorised code runs
# against the same expected output. Programs that need floating point or
# NEON themselves say so with a "// build: simd-only" line and are built
# once, as name.elf, with floating point allowed.
#
# Homebrew-format programs in guest/src/nro bring their own startup code
# and are converted to NRO (name.nro) by scripts/make-nro.py.
#
# GUEST_OPT (default -O2) and GUEST_OUT (default guest/bin) let
# scripts/check-optimised.sh build other variants elsewhere.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
src="$root/guest/src"
out=${GUEST_OUT:-$root/guest/bin}
mkdir -p "$out"

CC=${CC:-clang}
LD=${LD:-ld.lld}
BASE="--target=aarch64-none-elf -ffreestanding -nostdlib \
      -fno-stack-protector -fno-pic ${GUEST_OPT:--O2} -Wall -Werror"
INT="$BASE -mgeneral-regs-only"
# No FMA contraction and no errno, so floating-point results are reproducible.
SIMD="$BASE -ffp-contract=off -fno-math-errno"

build() { # build <name> <suffix> <flags>
    $CC $3 -c "$src/start.c" -o "$out/start$2.o"
    $CC $3 -c "$src/$1.c" -o "$out/$1$2.o"
    $LD -static -e _start "$out/start$2.o" "$out/$1$2.o" -o "$out/$1$2.elf"
    echo "built $out/$1$2.elf"
}

for f in "$src"/*.c; do
    name=$(basename "$f" .c)
    [ "$name" = start ] && continue
    if grep -q '^// build: simd-only' "$f"; then
        build "$name" "" "$SIMD"
    else
        build "$name" "" "$INT"
        build "$name" -neon "$SIMD"
    fi
done
# Hand-written assembly programs provide their own _start.
for f in "$src"/*.S; do
    name=$(basename "$f" .S)
    $CC --target=aarch64-none-elf -march=armv8.2-a+lse -c "$f" -o "$out/$name.o"
    $LD -static -e _start "$out/$name.o" -o "$out/$name.elf"
    echo "built $out/$name.elf"
done
# Homebrew-format programs: own startup code, linked position-independent
# and converted to NRO by scripts/make-nro.py (needs python3).
PIE="--target=aarch64-none-elf -ffreestanding -nostdlib -fno-stack-protector \
     -fPIE -fvisibility=hidden ${GUEST_OPT:--O2} -Wall -Werror -mgeneral-regs-only"
$CC --target=aarch64-none-elf -c "$src/nro/crt0.S" -o "$out/crt0.o"
for f in "$src"/nro/*.c; do
    name=$(basename "$f" .c)
    $CC $PIE -c "$f" -o "$out/$name.o"
    $LD -pie --no-dynamic-linker -z text -T "$src/nro/nro.ld" \
        "$out/crt0.o" "$out/$name.o" -o "$out/$name.nro.elf"
    python3 "$root/scripts/make-nro.py" "$out/$name.nro.elf" "$out/$name.nro"
    rm -f "$out/$name.nro.elf"
    echo "built $out/$name.nro"
done
rm -f "$out"/*.o
