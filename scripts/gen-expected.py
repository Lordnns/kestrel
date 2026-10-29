#!/usr/bin/env python3
"""Compute guest/expected/*.txt independently of kestrel.

Each function models what one program in guest/src must print, using Python
arithmetic, hashlib and zlib. Nothing here runs kestrel or reads its output,
so a match in `kestrel selftest` is checked against an outside source
(PLAN.md, "Every expected value comes from an independent source").

    python3 scripts/gen-expected.py          # rewrite guest/expected
    python3 scripts/gen-expected.py --check  # fail if any file differs
"""

import hashlib
import math
import re
import sys
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "guest" / "src"
OUT = ROOT / "guest" / "expected"
M64 = (1 << 64) - 1


def hex64(v):
    """rt.h put_hex: 0x plus 16 lowercase digits."""
    return f"0x{v & M64:016x}"


def c_div(a, b):
    """C99 signed division truncates toward zero."""
    q = abs(a) // abs(b)
    return q if (a < 0) == (b < 0) else -q


def hello():
    return "Hello from guest ARM64 code!\n"


def algos():
    fib = [0, 1]
    while len(fib) <= 25:
        fib.append(fib[-1] + fib[-2])

    crc = zlib.crc32(b"The quick brown fox jumps over the lazy dog")

    limit = 100000
    is_p = bytearray([1]) * (limit + 1)
    is_p[0] = is_p[1] = 0
    for i in range(2, math.isqrt(limit) + 1):
        if is_p[i]:
            is_p[i * i :: i] = bytearray(len(is_p[i * i :: i]))
    primes = sum(is_p)

    a, b = -7, 2
    q = c_div(a, b)
    r = a - q * b
    return (
        f"fib(25) = {fib[25]}\n"
        f"crc32 = {hex64(crc)}\n"
        f"primes <= 100000 = {primes}\n"
        # The program sorts 2000 values and checks the order; a correct
        # sort always reports success.
        "sort ok\n"
        f"signed div/mod = {q + 100} {r + 100}\n"
    )


def is_prime(n):
    """Deterministic Miller-Rabin for n < 2^64 (bases 2..37), on Python ints."""
    if n < 2:
        return False
    bases = [2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37]
    for p in bases:
        if n % p == 0:
            return n == p
    d, s = n - 1, 0
    while d % 2 == 0:
        d, s = d // 2, s + 1
    for a in bases:
        x = pow(a, d, n)
        if x in (1, n - 1):
            continue
        for _ in range(s - 1):
            x = x * x % n
            if x == n - 1:
                break
        else:
            return False
    return True


def bigint():
    f = math.factorial(30) & ((1 << 128) - 1)
    p = 0xFEDCBA9876543210 * 0x0123456789ABCDEF
    lines = [
        f"30! = {hex64(f >> 64)}:{hex64(f)}",
        f"128-bit product = {hex64(p >> 64)}:{hex64(p)}",
    ]
    for label, n in [
        ("2^61-1       : ", (1 << 61) - 1),
        ("2^64-59      : ", 2**64 - 59),
        ("2^64-1       : ", 2**64 - 1),
        ("561          : ", 561),
        ("1e18+9       : ", 10**18 + 9),
    ]:
        lines.append(label + ("prime" if is_prime(n) else "composite"))
    lines.append(f"powmod(3, 2^63, 2^64-59) = {pow(3, 2**63, 2**64 - 59)}")
    lines.append(f"popcount(0xdeadbeefcafebabe) = {bin(0xDEADBEEFCAFEBABE).count('1')}")
    return "\n".join(lines) + "\n"


def vm():
    n = 1000
    sum_squares = n * (n + 1) * (2 * n + 1) // 6
    x, steps = 27, 0
    while x != 1:
        x = x // 2 if x % 2 == 0 else 3 * x + 1
        steps += 1
    agree = " (both dispatchers agree)\n"
    return (
        f"vm sum_squares = {sum_squares}{agree}"
        f"vm collatz(27) = {steps}{agree}"
        f"vm 20!         = {math.factorial(20)}{agree}"
    )


def sha256():
    def line(label, data):
        return f"{label}{hashlib.sha256(data).hexdigest()}\n"

    return (
        line("sha256('')    = ", b"")
        + line("sha256('abc') = ", b"abc")
        + line("sha256(a*1e5) = ", b"a" * 100000)
    )


def asm_checks():
    # Every `expect` holds a value worked out by hand from the Arm rules, so
    # a correct CPU passes all of them. Count them from the source.
    text = (SRC / "asm_checks.S").read_text()
    n = len(re.findall(r"^\s*(?:\d+:)?\s*expect\s", text, re.M))
    return f"asm checks: {n} passed, 0 failed\n"


PROGRAMS = {
    "hello": hello,
    "asm_checks": asm_checks,
    "vm": vm,
    "bigint": bigint,
    "algos": algos,
    "sha256": sha256,
}


def main():
    check = "--check" in sys.argv[1:]
    sources = {p.stem for p in SRC.glob("*.c") if p.stem != "start"}
    sources |= {p.stem for p in SRC.glob("*.S")}
    if sources != set(PROGRAMS):
        missing = sorted(sources - set(PROGRAMS))
        extra = sorted(set(PROGRAMS) - sources)
        sys.exit(f"models out of sync with guest/src: missing {missing}, extra {extra}")
    stale = []
    for name, model in PROGRAMS.items():
        path = OUT / f"{name}.txt"
        want = model().encode()
        if check:
            if not path.exists() or path.read_bytes() != want:
                stale.append(name)
        else:
            path.write_bytes(want)
            print(f"wrote {path.relative_to(ROOT)}")
    if stale:
        print("expected output differs from the independent model: " + ", ".join(stale))
        sys.exit(1)
    if check:
        print(f"all {len(PROGRAMS)} expected files match the independent model")


if __name__ == "__main__":
    main()
