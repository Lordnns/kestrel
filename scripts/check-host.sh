#!/usr/bin/env sh
# Real-hardware oracle: build the C test programs in guest/src natively and
# run them on this machine's ARM CPU, comparing each output with
# guest/expected. A pass means a real ARM CPU agrees with every expected
# answer, independently of kestrel and of the Python models in scripts/.
#
# Runs on an AArch64 Linux or macOS host with a C compiler (CC, default
# clang). The builds mirror scripts/build-guest.sh: each program integer-only
# and with NEON, or once with NEON for "// build: simd-only" programs, with
# rt.h's KESTREL_HOST mode turning the console calls into C library calls.
# The hand-written asm_checks.S program is console-only and not run here.
set -eu

case $(uname -m) in
    aarch64 | arm64) ;;
    *)
        echo "check-host.sh needs an AArch64 host (this one is $(uname -m))" >&2
        exit 1
        ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
src="$root/guest/src"
expected="$root/guest/expected"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

CC=${CC:-clang}
BASE="-DKESTREL_HOST -ffreestanding -fno-stack-protector -O2 -Wall -Werror"
INT="$BASE -mgeneral-regs-only"
# No FMA contraction and no errno, so floating-point results are reproducible.
SIMD="$BASE -ffp-contract=off -fno-math-errno"

failed=0
check() { # check <name> <suffix> <flags>
    exe="$work/$1$2"
    # shellcheck disable=SC2086 # the flags are meant to split
    $CC $3 "$src/$1.c" -o "$exe"
    if "$exe" >"$exe.out" && cmp -s "$exe.out" "$expected/$1.txt"; then
        echo "ok   $1$2"
    else
        echo "FAIL $1$2"
        diff "$expected/$1.txt" "$exe.out" || true
        failed=$((failed + 1))
    fi
}

for f in "$src"/*.c; do
    name=$(basename "$f" .c)
    [ "$name" = start ] && continue
    if grep -q '^// build: simd-only' "$f"; then
        check "$name" "" "$SIMD"
    else
        check "$name" "" "$INT"
        check "$name" -neon "$SIMD"
    fi
done

if [ "$failed" -ne 0 ]; then
    echo "$failed program(s) disagree with guest/expected on this CPU" >&2
    exit 1
fi
echo "every program matches guest/expected on this CPU ($(uname -sm))"
