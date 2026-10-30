#!/usr/bin/env sh
# Phase 1 acceptance: rebuild every test program aggressively optimised for
# the Armv8.2 target (-O3, so the compiler vectorises as much as it can) and
# run each one in kestrel. Every program must run to the end, hitting no
# unsupported or undefined instruction, and print exactly guest/expected.
#
# Needs clang and ld.lld (any host, as for build-guest.sh) and cargo, or
# KESTREL pointing at a kestrel binary. GUEST_OPT overrides the flags.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
expected="$root/guest/expected"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

GUEST_OPT=${GUEST_OPT:--O3 -march=armv8.2-a}
if [ -z "${KESTREL:-}" ]; then
    cargo build -q --release -p kestrel-cli --manifest-path "$root/Cargo.toml"
    KESTREL="$root/target/release/kestrel"
fi
GUEST_OUT="$work" GUEST_OPT="$GUEST_OPT" sh "$root/scripts/build-guest.sh" >/dev/null

failed=0
for elf in "$work"/*.elf; do
    name=$(basename "$elf" .elf)
    want="$expected/${name%-neon}.txt"
    if "$KESTREL" run "$elf" --backend interp >"$work/$name.out" && cmp -s "$work/$name.out" "$want"; then
        echo "ok   $name"
    else
        echo "FAIL $name"
        diff "$want" "$work/$name.out" || true
        failed=$((failed + 1))
    fi
done

if [ "$failed" -ne 0 ]; then
    echo "$failed program(s) failed when built with $GUEST_OPT" >&2
    exit 1
fi
echo "every program passes in kestrel when built with $GUEST_OPT"
