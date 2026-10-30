#!/usr/bin/env python3
"""Generate the Advanced SIMD (NEON) test vectors for the interpreter.

Writes crates/kestrel-cpu/src/interp/neon_vectors.rs. Each case is one
instruction, assembled by clang (--target=aarch64) and disassembled by
llvm-objdump to get its encoding. The expected registers, FPSR.QC and memory
come from the per-lane models in this file, written from the instruction
descriptions in the Arm Architecture Reference Manual (DDI 0487). Floating
point is computed exactly with fractions and rounded here. Nothing here runs
kestrel.

    python3 scripts/gen-neon-vectors.py            # regenerate
    python3 scripts/gen-neon-vectors.py --check    # fail if out of date

Needs clang and llvm-objdump (override with $CLANG and $OBJDUMP).
"""

import math
import os
import random
import re
import struct
import subprocess
import sys
import tempfile
from fractions import Fraction as Fr

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "kestrel-cpu", "src", "interp", "neon_vectors.rs")

MEM_BASE = 0x20_0100  # inside the test harness's read-write stack region
MEM_LEN = 256


def M(es):
    return (1 << es) - 1


def lanes(v, es, n=None):
    return [(v >> (es * k)) & M(es) for k in range(n or 128 // es)]


def pack(ls, es):
    r = 0
    for k, x in enumerate(ls):
        r |= (x & M(es)) << (es * k)
    return r


def sx(x, es):
    x &= M(es)
    return x - (1 << es) if x >> (es - 1) else x


def val(x, es, signed):
    return sx(x, es) if signed else x & M(es)


ARR = {
    "8b": (8, 64), "16b": (8, 128), "4h": (16, 64), "8h": (16, 128),
    "2s": (32, 64), "4s": (32, 128), "1d": (64, 64), "2d": (64, 128),
    "b": (8, 8), "h": (16, 16), "s": (32, 32), "d": (64, 64),
}
SCALAR = {"b": 8, "h": 16, "s": 32, "d": 64}

# ---- initial state -----------------------------------------------------------

rng = random.Random(20261029)
V0 = [rng.getrandbits(128) for _ in range(32)]
# Edge lanes for saturation, compares and sign handling.
V0[3] = 0x80000000_00000000_7FFFFFFF_FFFFFFFF
V0[4] = 0x807FFF00_807FFF00_8000_7FFF_FFFF_0001
V0[5] = 0x7F80_8001_7FFF_8000_0001_FFFF_0080_FF7F
# Small signed shift amounts in every byte (only the low byte of each element
# is the shift count).
V0[6] = pack([3, -2, 7, -9, 1, 0, -1, 5, -4, 2, 9, -5, 4, -7, 6, -15], 8)


def f32(x):
    return struct.unpack("<I", struct.pack("<f", x))[0]


def f64(x):
    return struct.unpack("<Q", struct.pack("<d", x))[0]


QNAN_S = 0x7FC12345
V0[16] = pack([f32(1.5), f32(-2.25), f32(1e10), f32(0.1)], 32)
V0[17] = pack([f32(-0.75), f32(3.5), f32(2e-3), QNAN_S], 32)
V0[18] = pack([f64(math.pi), f64(-1e300)], 64)
V0[19] = pack([f64(2.5), f64(1e-300)], 64)
V0[20] = pack([f32(-0.0), f32(math.inf), f32(-3.7), f32(100.5)], 32)
V0[21] = pack([f64(-1.5), f64(2.5e9)], 64)
V0[22] = pack([7, -100, 123456789, -1], 32)
V0[23] = pack([-(2**53) - 1, (1 << 62) + 1], 64)
V0[24] = pack([f32(2.5), f32(-0.5), f32(7.25), f32(-1e-3)], 32)
V0[25] = pack([f64(-2.5), f64(0.375)], 64)
X0 = [0xDEAD_BEEF_DEAD_BEEF, MEM_BASE, 48, 0x8899_AABB_CCDD_EEFF]
MEM0 = bytes((k * 37 + 11) & 0xFF for k in range(MEM_LEN))


class State:
    def __init__(self, fpcr):
        self.v = list(V0)
        self.x = list(X0)
        self.mem = bytearray(MEM0)
        self.qc = False
        self.fpcr = fpcr


ST = None  # the state of the case being modelled


def sat(x, es, unsigned):
    lo, hi = (0, M(es)) if unsigned else (-(1 << (es - 1)), (1 << (es - 1)) - 1)
    if x > hi or x < lo:
        ST.qc = True
        x = hi if x > hi else lo
    return x & M(es)


def wr(d, value, bits):
    ST.v[d] = value & ((1 << bits) - 1)


def wr_narrow(d, value, upper):
    if upper:
        ST.v[d] = (ST.v[d] & M(64)) | ((value & M(64)) << 64)
    else:
        ST.v[d] = value & M(64)


def shift(x, sh, rnd):
    """x * 2^sh in exact arithmetic, rounding right shifts if asked."""
    if sh >= 0:
        return x << sh
    sh = -sh
    return (x + (1 << (sh - 1))) >> sh if rnd else x >> sh


def sbyte(b):
    return sx(b & 0xFF, 8)


def clmul(a, b):
    r = 0
    for k in range(64):
        if (b >> k) & 1:
            r ^= a << k
    return r


# ---- floating point ----------------------------------------------------------


class Fmt:
    def __init__(self, w, e, f):
        self.w, self.e, self.f = w, e, f
        self.bias = (1 << (e - 1)) - 1
        self.emaxf = (1 << e) - 1
        self.sign = 1 << (w - 1)
        self.dnan = (self.emaxf << f) | (1 << (f - 1))
        self.quiet = 1 << (f - 1)


FS, FD = Fmt(32, 8, 23), Fmt(64, 11, 52)
FMT = {32: FS, 64: FD}


def cls(b, f):
    """(kind, sign, exact value)."""
    sign = (b >> (f.w - 1)) & 1
    ex, fr = (b >> f.f) & f.emaxf, b & ((1 << f.f) - 1)
    if ex == f.emaxf:
        if fr == 0:
            return "inf", sign, None
        return ("qnan" if fr & f.quiet else "snan"), sign, None
    if ex == 0 and fr == 0:
        return "zero", sign, Fr(0)
    if ex == 0:
        v = Fr(fr, 1 << f.f) * Fr(2) ** (1 - f.bias)
    else:
        v = (1 + Fr(fr, 1 << f.f)) * Fr(2) ** (ex - f.bias)
    return "num", sign, -v if sign else v


def inf(f, sign):
    return (sign << (f.w - 1)) | (f.emaxf << f.f)


def enc(v, f, mode="ne", zsign=0):
    """Round the exact value v to format f (FPRound)."""
    if v == 0:
        return zsign << (f.w - 1)
    sign = 1 if v < 0 else 0
    m = abs(v)
    e = m.numerator.bit_length() - m.denominator.bit_length()
    if Fr(2) ** e > m:
        e -= 1
    emin = 1 - f.bias
    q = max(e, emin) - f.f
    n = m / Fr(2) ** q
    i = n.numerator // n.denominator
    rem = n - i
    if mode == "odd":
        i |= 1 if rem else 0
    else:
        up = {
            "ne": rem > Fr(1, 2) or (rem == Fr(1, 2) and i & 1),
            "a": rem >= Fr(1, 2),
            "z": False,
            "p": rem > 0 and not sign,
            "m": rem > 0 and sign,
        }[mode]
        i += 1 if up else 0
    if i == 1 << (f.f + 1):
        i >>= 1
        q += 1
    s = sign << (f.w - 1)
    if i >= 1 << f.f:
        ex = q + f.f + f.bias
        if ex >= f.emaxf:
            to_inf = {"ne": True, "a": True, "z": False, "odd": False,
                      "p": not sign, "m": bool(sign)}[mode]
            return inf(f, sign) if to_inf else s | ((f.emaxf - 1) << f.f) | ((1 << f.f) - 1)
        return s | (ex << f.f) | (i - (1 << f.f))
    return s | i


RMODE = {0: "ne", 1: "p", 2: "m", 3: "z"}


def fpcr_mode():
    return RMODE[(ST.fpcr >> 22) & 3]


def nans(f, *ops):
    kinds = [cls(b, f)[0] for b in ops]
    for want in ("snan", "qnan"):
        for k, b in zip(kinds, ops):
            if k == want:
                return b | f.quiet
    return None


def f_add(a, b, f):
    n = nans(f, a, b)
    if n is not None:
        return n
    (ka, sa, va), (kb, sb, vb) = cls(a, f), cls(b, f)
    if ka == "inf" and kb == "inf":
        return inf(f, sa) if sa == sb else f.dnan
    if ka == "inf":
        return inf(f, sa)
    if kb == "inf":
        return inf(f, sb)
    if ka == "zero" and kb == "zero":
        return enc(0, f, zsign=sa & sb)
    return enc(va + vb, f)


def f_neg(x, f):
    return x ^ f.sign


def f_sub(a, b, f):
    if nans(f, a, b) is not None:
        return nans(f, a, b)
    return f_add(a, f_neg(b, f), f)


def f_mul(a, b, f):
    n = nans(f, a, b)
    if n is not None:
        return n
    (ka, sa, va), (kb, sb, vb) = cls(a, f), cls(b, f)
    s = sa ^ sb
    if (ka, kb) in (("inf", "zero"), ("zero", "inf")):
        return f.dnan
    if "inf" in (ka, kb):
        return inf(f, s)
    if "zero" in (ka, kb):
        return s << (f.w - 1)
    return enc(va * vb, f)


def f_mulx(a, b, f):
    n = nans(f, a, b)
    if n is not None:
        return n
    (ka, sa, _), (kb, sb, _) = cls(a, f), cls(b, f)
    if (ka, kb) in (("inf", "zero"), ("zero", "inf")):
        return ((sa ^ sb) << (f.w - 1)) | enc(Fr(2), f)
    return f_mul(a, b, f)


def f_div(a, b, f):
    n = nans(f, a, b)
    if n is not None:
        return n
    (ka, sa, va), (kb, sb, vb) = cls(a, f), cls(b, f)
    s = sa ^ sb
    if (ka, kb) in (("inf", "inf"), ("zero", "zero")):
        return f.dnan
    if ka == "inf" or kb == "zero":
        return inf(f, s)
    if ka == "zero" or kb == "inf":
        return s << (f.w - 1)
    return enc(va / vb, f)


def f_fma(acc, a, b, f):
    """FPMulAdd(acc, a, b) = acc + a*b, rounded once."""
    (kc, sc, vc), (ka, sa, va), (kb, sb, vb) = cls(acc, f), cls(a, f), cls(b, f)
    inv_prod = (ka, kb) in (("inf", "zero"), ("zero", "inf"))
    n = nans(f, acc, a, b)
    if n is not None:
        return f.dnan if kc == "qnan" and inv_prod else n
    sp = sa ^ sb
    if inv_prod:
        return f.dnan
    pinf = "inf" in (ka, kb)
    if pinf and kc == "inf" and sc != sp:
        return f.dnan
    if pinf:
        return inf(f, sp)
    if kc == "inf":
        return inf(f, sc)
    pzero = "zero" in (ka, kb)
    if kc == "zero" and pzero:
        return enc(0, f, zsign=sc & sp)
    return enc(vc + va * vb, f)


def f_sqrt(a, f):
    k, s, v = cls(a, f)
    if k in ("snan", "qnan"):
        return a | f.quiet
    if k == "zero":
        return a
    if s:
        return f.dnan
    if k == "inf":
        return a
    # math.sqrt is correctly rounded for doubles; for singles the double
    # result rounds correctly again (53 >= 2 * 24 + 2).
    r = math.sqrt(float(v))
    return f64(r) if f is FD else f32(r)


def f_maxmin(a, b, f, is_max, num):
    ka, kb = cls(a, f)[0], cls(b, f)[0]
    if num:
        fill = inf(f, 1 if is_max else 0)
        if ka == "qnan" and kb != "qnan":
            a = fill
        elif kb == "qnan" and ka != "qnan":
            b = fill
    n = nans(f, a, b)
    if n is not None:
        return n
    (ka, sa, va), (kb, sb, vb) = cls(a, f), cls(b, f)
    key = lambda k, s, v: (math.inf if not s else -math.inf) if k == "inf" else v
    xa, xb = key(ka, sa, va), key(kb, sb, vb)
    if ka == "zero" and kb == "zero":
        return (sa & sb if is_max else sa | sb) << (f.w - 1)
    if xa == xb:
        return b
    pick_a = xa > xb if is_max else xa < xb
    return a if pick_a else b


def f_cmp(a, b, f, op):
    ka, kb = cls(a, f)[0], cls(b, f)[0]
    if "qnan" in (ka, kb) or "snan" in (ka, kb):
        return False
    key = lambda x: (lambda k, s, v: (math.inf if not s else -math.inf) if k == "inf" else v)(*cls(x, f))
    xa, xb = key(a), key(b)
    return {"eq": xa == xb, "ge": xa >= xb, "gt": xa > xb}[op]


def f_abs(x, f):
    return x & ~f.sign & M(f.w)


def f_recip_step(a, b, f, rsqrt):
    a = f_neg(a, f)
    n = nans(f, a, b)
    if n is not None:
        return n
    (ka, sa, va), (kb, sb, vb) = cls(a, f), cls(b, f)
    if (ka, kb) in (("inf", "zero"), ("zero", "inf")):
        return enc(Fr(3, 2) if rsqrt else Fr(2), f)
    if "inf" in (ka, kb):
        return inf(f, sa ^ sb)
    if rsqrt:
        return enc((3 + va * vb) / 2, f)
    return enc(2 + va * vb, f)


def round_frac(v, mode):
    """Round an exact value to an integer."""
    i = v.numerator // v.denominator
    rem = v - i
    neg = v < 0
    if mode == "ne":
        up = rem > Fr(1, 2) or (rem == Fr(1, 2) and i & 1)
    elif mode == "a":
        up = rem > Fr(1, 2) or (rem == Fr(1, 2) and not neg)
    elif mode == "z":
        up = rem > 0 and neg
    elif mode == "p":
        up = rem > 0
    else:
        up = False
    return i + (1 if up else 0)


def f_rint(x, f, mode):
    k, s, v = cls(x, f)
    if k in ("snan", "qnan"):
        return x | f.quiet
    if k in ("zero", "inf"):
        return x
    r = round_frac(v, mode)
    return enc(Fr(r), f, zsign=s)


def f_to_int(x, f, mode, es, unsigned, fbits=0):
    k, s, v = cls(x, f)
    if k in ("snan", "qnan", "zero"):
        return 0
    lo, hi = (0, M(es)) if unsigned else (-(1 << (es - 1)), (1 << (es - 1)) - 1)
    if k == "inf":
        return (lo if s else hi) & M(es)
    r = round_frac(v * 2**fbits, mode)
    return max(lo, min(hi, r)) & M(es)


def f_from_int(i, es, unsigned, f, fbits=0, mode="ne"):
    return enc(Fr(val(i, es, not unsigned), 2**fbits), f, mode)


def f_convert(x, src, dst, mode="ne"):
    k, s, v = cls(x, src)
    if k in ("snan", "qnan"):
        payload = x & (src.quiet - 1)
        payload = payload << (dst.f - src.f) if dst.f > src.f else payload >> (src.f - dst.f)
        return inf(dst, s) | dst.quiet | payload
    if k == "inf":
        return inf(dst, s)
    if k == "zero":
        return s << (dst.w - 1)
    return enc(v, dst, mode)


def recip_estimate_int(a):
    a = a * 2 + 1
    b = (1 << 19) // a
    return (b + 1) // 2


def rsqrt_estimate_int(a):
    if a < 256:
        a = a * 2 + 1
    else:
        a = (a >> 1) << 1
        a = (a + 1) * 2
    b = 512
    while a * (b + 1) * (b + 1) < (1 << 28):
        b += 1
    return (b + 1) // 2


def f_recpe(x, f):
    k, s, v = cls(x, f)
    if k in ("snan", "qnan"):
        return x | f.quiet
    if k == "inf":
        return s << (f.w - 1)
    if k == "zero":
        return inf(f, s)
    tiny = Fr(2) ** (-128 if f is FS else -1024)
    if abs(v) < tiny:
        return inf(f, s)
    frac = (x & ((1 << f.f) - 1)) << (52 - f.f)
    exp = (x >> f.f) & f.emaxf
    if exp == 0:
        if not (frac >> 51) & 1:
            exp = -1
            frac = (frac << 2) & M(52)
        else:
            frac = (frac << 1) & M(52)
    scaled = 256 | (frac >> 44)
    rexp = (253 if f is FS else 2045) - exp
    est = recip_estimate_int(scaled)
    frac = (est & 0xFF) << 44
    if rexp == 0:
        frac = (1 << 51) | (frac >> 1)
    elif rexp == -1:
        frac = (1 << 50) | (frac >> 2)
        rexp = 0
    return (s << (f.w - 1)) | (rexp << f.f) | (frac >> (52 - f.f))


def f_rsqrte(x, f):
    k, s, v = cls(x, f)
    if k in ("snan", "qnan"):
        return x | f.quiet
    if k == "zero":
        return inf(f, s)
    if s:
        return f.dnan
    if k == "inf":
        return 0
    frac = (x & ((1 << f.f) - 1)) << (52 - f.f)
    exp = (x >> f.f) & f.emaxf
    if exp == 0:
        while not (frac >> 51) & 1:
            frac = (frac << 1) & M(52)
            exp -= 1
        frac = (frac << 1) & M(52)
    scaled = (256 | (frac >> 44)) if exp % 2 == 0 else (128 | (frac >> 45))
    rexp = ((380 if f is FS else 3068) - exp) // 2
    est = rsqrt_estimate_int(scaled) & 0xFF
    return (rexp << f.f) | (est << (f.f - 8))


def f_recpx(x, f):
    k = cls(x, f)[0]
    if k in ("snan", "qnan"):
        return x | f.quiet
    exp = (x >> f.f) & f.emaxf
    rexp = f.emaxf - 1 if exp == 0 else (~exp & f.emaxf)
    return (x & f.sign) | (rexp << f.f)


def urecpe(x):
    if not (x >> 31) & 1:
        return M(32)
    return (recip_estimate_int(x >> 23) & 0x1FF) << 23


def ursqrte(x):
    if x >> 30 == 0:
        return M(32)
    return (rsqrt_estimate_int(x >> 23) & 0x1FF) << 23


def vfp_expand(imm8, f):
    b7, b6 = imm8 >> 7, (imm8 >> 6) & 1
    e = f.e
    exp = ((b6 ^ 1) << (e - 1)) | ((((1 << (e - 3)) - 1) if b6 else 0) << 2) | ((imm8 >> 4) & 3)
    return (b7 << (f.w - 1)) | (exp << f.f) | ((imm8 & 0xF) << (f.f - 4))


# ---- operand parsing ---------------------------------------------------------


def parse(asm):
    mn, _, rest = asm.partition(" ")
    regs = [int(r) for r in re.findall(r"(?<![\w.#])[vbhsdqwx](\d+)\b", rest)]
    arrs = re.findall(r"\.(\d+[bhsd]|[bhsd])\b", rest)
    idx = [int(k) for k in re.findall(r"\[(\d+)\]", rest)]
    imms = re.findall(r"#(-?[0-9a-fA-Fx.e+-]+)", rest)
    scal = re.match(r"\s*([bhsd])\d+\b", rest)
    return mn, regs, arrs, idx, imms, scal.group(1) if scal else None


def shape(arrs, scal):
    if scal:
        return SCALAR[scal], SCALAR[scal]
    return ARR[arrs[0]]


# ---- integer models ----------------------------------------------------------


def U(f):
    return lambda a, b, d, e: f(a & M(e), b & M(e), d, e)


def Sg(f):
    return lambda a, b, d, e: f(sx(a, e), sx(b, e), d, e)


CM = lambda c, e: M(e) if c else 0
SAME = {
    "shadd": Sg(lambda a, b, d, e: (a + b) >> 1),
    "uhadd": U(lambda a, b, d, e: (a + b) >> 1),
    "sqadd": Sg(lambda a, b, d, e: sat(a + b, e, False)),
    "uqadd": U(lambda a, b, d, e: sat(a + b, e, True)),
    "srhadd": Sg(lambda a, b, d, e: (a + b + 1) >> 1),
    "urhadd": U(lambda a, b, d, e: (a + b + 1) >> 1),
    "shsub": Sg(lambda a, b, d, e: (a - b) >> 1),
    "uhsub": U(lambda a, b, d, e: (a - b) >> 1),
    "sqsub": Sg(lambda a, b, d, e: sat(a - b, e, False)),
    "uqsub": U(lambda a, b, d, e: sat(a - b, e, True)),
    "cmgt": Sg(lambda a, b, d, e: CM(a > b, e)),
    "cmhi": U(lambda a, b, d, e: CM(a > b, e)),
    "cmge": Sg(lambda a, b, d, e: CM(a >= b, e)),
    "cmhs": U(lambda a, b, d, e: CM(a >= b, e)),
    "sshl": lambda a, b, d, e: shift(sx(a, e), sbyte(b), False),
    "ushl": lambda a, b, d, e: shift(a, sbyte(b), False),
    "sqshl": lambda a, b, d, e: sat(shift(sx(a, e), sbyte(b), False), e, False),
    "uqshl": lambda a, b, d, e: sat(shift(a, sbyte(b), False), e, True),
    "srshl": lambda a, b, d, e: shift(sx(a, e), sbyte(b), True),
    "urshl": lambda a, b, d, e: shift(a, sbyte(b), True),
    "sqrshl": lambda a, b, d, e: sat(shift(sx(a, e), sbyte(b), True), e, False),
    "uqrshl": lambda a, b, d, e: sat(shift(a, sbyte(b), True), e, True),
    "smax": Sg(lambda a, b, d, e: max(a, b)),
    "umax": U(lambda a, b, d, e: max(a, b)),
    "smin": Sg(lambda a, b, d, e: min(a, b)),
    "umin": U(lambda a, b, d, e: min(a, b)),
    "sabd": Sg(lambda a, b, d, e: abs(a - b)),
    "uabd": U(lambda a, b, d, e: abs(a - b)),
    "saba": Sg(lambda a, b, d, e: d + abs(a - b)),
    "uaba": U(lambda a, b, d, e: d + abs(a - b)),
    "add": lambda a, b, d, e: a + b,
    "sub": lambda a, b, d, e: a - b,
    "cmtst": lambda a, b, d, e: CM(a & b, e),
    "cmeq": lambda a, b, d, e: CM(a == b, e),
    "mla": lambda a, b, d, e: d + a * b,
    "mls": lambda a, b, d, e: d - a * b,
    "mul": lambda a, b, d, e: a * b,
    "pmul": lambda a, b, d, e: clmul(a, b),
    "sqdmulh": Sg(lambda a, b, d, e: sat((2 * a * b) >> e, e, False)),
    "sqrdmulh": Sg(lambda a, b, d, e: sat((2 * a * b + (1 << (e - 1))) >> e, e, False)),
    "sqrdmlah": Sg(lambda a, b, d, e: sat(((sx(d, e) << e) + 2 * a * b + (1 << (e - 1))) >> e, e, False)),
    "sqrdmlsh": Sg(lambda a, b, d, e: sat(((sx(d, e) << e) - 2 * a * b + (1 << (e - 1))) >> e, e, False)),
    "and": lambda a, b, d, e: a & b,
    "bic": lambda a, b, d, e: a & ~b,
    "orr": lambda a, b, d, e: a | b,
    "orn": lambda a, b, d, e: a | ~b,
    "eor": lambda a, b, d, e: a ^ b,
    "bsl": lambda a, b, d, e: (d & a) | (~d & b),
    "bit": lambda a, b, d, e: (b & a) | (~b & d),
    "bif": lambda a, b, d, e: (~b & a) | (b & d),
}
PAIR = {
    "addp": lambda a, b, e: a + b,
    "smaxp": lambda a, b, e: max(sx(a, e), sx(b, e)),
    "umaxp": lambda a, b, e: max(a, b),
    "sminp": lambda a, b, e: min(sx(a, e), sx(b, e)),
    "uminp": lambda a, b, e: min(a, b),
}


def m_same(asm, fn):
    mn, (d, n, m), arrs, _, _, scal = parse(asm)
    es, bits = shape(arrs, scal)
    k = bits // es
    a, b, dd = lanes(ST.v[n], es, k), lanes(ST.v[m], es, k), lanes(ST.v[d], es, k)
    wr(d, pack([fn(a[i], b[i], dd[i], es) for i in range(k)], es), bits)


def m_pair(asm, fn):
    mn, (d, n, m), arrs, _, _, scal = parse(asm)
    es, bits = ARR[arrs[0]]
    k = bits // es
    cat = lanes(ST.v[n], es, k) + lanes(ST.v[m], es, k)
    wr(d, pack([fn(cat[2 * i], cat[2 * i + 1], es) for i in range(k)], es), bits)


def m_misc(asm, fn):
    mn, (d, n), arrs, _, _, scal = parse(asm)
    es, bits = shape(arrs, scal)
    k = bits // es
    a, dd = lanes(ST.v[n], es, k), lanes(ST.v[d], es, k)
    wr(d, pack([fn(a[i], dd[i], es) for i in range(k)], es), bits)


def clz(x, w):
    return w - x.bit_length()


MISC = {
    "suqadd": lambda a, d, e: sat(sx(d, e) + a, e, False),
    "usqadd": lambda a, d, e: sat(d + sx(a, e), e, True),
    "cls": lambda a, d, e: clz((a >> 1) ^ (a & M(e - 1)), e - 1),
    "clz": lambda a, d, e: clz(a, e),
    "cnt": lambda a, d, e: bin(a).count("1"),
    "not": lambda a, d, e: ~a,
    "rbit": lambda a, d, e: int(format(a, "08b")[::-1], 2),
    "sqabs": lambda a, d, e: sat(abs(sx(a, e)), e, False),
    "sqneg": lambda a, d, e: sat(-sx(a, e), e, False),
    "abs": lambda a, d, e: abs(sx(a, e)),
    "neg": lambda a, d, e: -sx(a, e),
}
CMP0 = {
    "cmgt": lambda x: x > 0, "cmge": lambda x: x >= 0, "cmeq": lambda x: x == 0,
    "cmle": lambda x: x <= 0, "cmlt": lambda x: x < 0,
}


def m_cmp0(asm, op):
    m_misc(asm, lambda a, d, e: CM(CMP0[op](sx(a, e)), e))


def m_rev(asm, container):
    mn, (d, n), arrs, _, _, _ = parse(asm)
    es, bits = ARR[arrs[0]]
    k = bits // es
    a = lanes(ST.v[n], es, k)
    g = container // es
    wr(d, pack([a[i ^ (g - 1)] for i in range(k)], es), bits)


def m_addlp(asm, signed, acc):
    mn, (d, n), arrs, _, _, _ = parse(asm)
    es2, bits = ARR[arrs[0]]
    es = es2 // 2
    a = lanes(ST.v[n], es)
    dd = lanes(ST.v[d], es2)
    out = [val(a[2 * i], es, signed) + val(a[2 * i + 1], es, signed) + (dd[i] if acc else 0)
           for i in range(bits // es2)]
    wr(d, pack(out, es2), bits)


def m_xtn(asm, kind):
    mn, (d, n), arrs, _, _, scal = parse(asm)
    upper = mn.endswith("2")
    if scal:
        es = SCALAR[scal]
        k = 1
    else:
        es = ARR[arrs[0]][0]
        k = 64 // es
    a = lanes(ST.v[n], 2 * es)
    f = {
        "xtn": lambda x: x,
        "sqxtn": lambda x: sat(sx(x, 2 * es), es, False),
        "uqxtn": lambda x: sat(x, es, True),
        "sqxtun": lambda x: sat(sx(x, 2 * es), es, True),
    }[kind]
    r = pack([f(a[i]) for i in range(k)], es)
    if scal:
        wr(d, r, es)
    else:
        wr_narrow(d, r, upper)


def m_shll(asm):
    mn, (d, n), arrs, _, _, _ = parse(asm)
    es2 = ARR[arrs[0]][0]
    es = es2 // 2
    k = 64 // es
    a = lanes(ST.v[n], es)[k:] if mn.endswith("2") else lanes(ST.v[n], es)[:k]
    wr(d, pack([x << es for x in a], es2), 128)


def m_long(asm, kind, signed):
    """Three different: long, wide, narrow-high and the doubling forms."""
    mn, (d, n, m), arrs, _, _, scal = parse(asm)
    upper = mn.endswith("2")
    if scal:
        es2 = SCALAR[scal]
        es, k = es2 // 2, 1
    elif kind in ("addhn", "subhn", "raddhn", "rsubhn"):
        es = ARR[arrs[0]][0]
        es2, k = 2 * es, 64 // es
    else:
        es2 = ARR[arrs[0]][0]
        es, k = es2 // 2, 64 // (es2 // 2)
    half = lambda r: lanes(ST.v[r], es)[k:] if upper else lanes(ST.v[r], es)[:k]
    if kind in ("addhn", "subhn", "raddhn", "rsubhn"):
        a, b = lanes(ST.v[n], es2, k), lanes(ST.v[m], es2, k)
        out = []
        for i in range(k):
            x = a[i] + b[i] if "add" in kind else a[i] - b[i]
            if kind[0] == "r":
                x += 1 << (es - 1)
            out.append((x >> es) & M(es))
        wr_narrow(d, pack(out, es), upper)
        return
    bm = half(m)
    dd = lanes(ST.v[d], es2)
    if kind in ("addw", "subw"):
        a = [val(x, es2, signed) for x in lanes(ST.v[n], es2, k)]
    else:
        a = [val(x, es, signed) for x in half(n)]
    b = [val(x, es, signed) for x in bm]
    out = []
    for i in range(k):
        x, y, acc = a[i], b[i], dd[i]
        r = {
            "addl": lambda: x + y, "addw": lambda: x + y,
            "subl": lambda: x - y, "subw": lambda: x - y,
            "abal": lambda: acc + abs(x - y), "abdl": lambda: abs(x - y),
            "mlal": lambda: acc + x * y, "mlsl": lambda: acc - x * y,
            "mull": lambda: x * y,
            "pmull": lambda: clmul(bm[i], half(n)[i]),
            "sqdmull": lambda: sat(2 * x * y, es2, False),
            "sqdmlal": lambda: sat(sx(acc, es2) + sx(sat(2 * x * y, es2, False), es2), es2, False),
            "sqdmlsl": lambda: sat(sx(acc, es2) - sx(sat(2 * x * y, es2, False), es2), es2, False),
        }[kind]()
        out.append(r)
    wr(d, pack(out, es2), es2 if scal else 128)


def m_across(asm, kind, signed):
    mn, (d, n), arrs, _, _, _ = parse(asm)
    es, bits = ARR[arrs[0]]
    xs = [val(x, es, signed) for x in lanes(ST.v[n], es, bits // es)]
    if kind == "addlv":
        wr(d, sum(xs), 2 * es)
    elif kind == "addv":
        wr(d, sum(xs), es)
    else:
        wr(d, (max if kind == "maxv" else min)(xs), es)


def m_shift(asm, kind, signed=True):
    mn, regs, arrs, _, imms, scal = parse(asm)
    d, n = regs[:2]
    sh = int(imms[0])
    upper = mn.endswith("2")
    if kind in ("shrn", "sqshrn", "sqshrun", "uqshrn"):
        rnd = "rshr" in mn
        if scal:
            es, k = SCALAR[scal], 1
        else:
            es = ARR[arrs[0]][0]
            k = 64 // es
        src = lanes(ST.v[n], 2 * es)
        out = []
        for i in range(k):
            x = val(src[i], 2 * es, kind in ("sqshrn", "sqshrun"))
            y = shift(x, -sh, rnd)
            out.append({
                "shrn": lambda: y & M(es),
                "sqshrn": lambda: sat(y, es, False),
                "sqshrun": lambda: sat(y, es, True),
                "uqshrn": lambda: sat(y, es, True),
            }[kind]())
        r = pack(out, es)
        if scal:
            wr(d, r, es)
        else:
            wr_narrow(d, r, upper)
        return
    if kind == "shll":
        es2 = ARR[arrs[0]][0]
        es, k = es2 // 2, 64 // (es2 // 2)
        a = lanes(ST.v[n], es)[k:] if upper else lanes(ST.v[n], es)[:k]
        wr(d, pack([val(x, es, signed) << sh for x in a], es2), 128)
        return
    es, bits = shape(arrs, scal)
    k = bits // es
    a, dd = lanes(ST.v[n], es, k), lanes(ST.v[d], es, k)
    out = []
    for i in range(k):
        x, acc = a[i], dd[i]
        out.append({
            "shr": lambda: shift(val(x, es, signed), -sh, False),
            "sra": lambda: acc + shift(val(x, es, signed), -sh, False),
            "rshr": lambda: shift(val(x, es, signed), -sh, True),
            "rsra": lambda: acc + shift(val(x, es, signed), -sh, True),
            "sri": lambda: (acc & ~(M(es) >> sh)) | (x >> sh),
            "shl": lambda: x << sh,
            "sli": lambda: (acc & M(sh)) | (x << sh),
            "sqshlu": lambda: sat(sx(x, es) << sh, es, True),
            "sqshl": lambda: sat(sx(x, es) << sh, es, False),
            "uqshl": lambda: sat(x << sh, es, True),
        }[kind]())
    wr(d, pack(out, es), bits)


def elem_operand(asm):
    """The by-element operand Vm.T[index]."""
    m = re.search(r"v(\d+)\.([bhsd])\[(\d+)\]", asm)
    return int(m.group(1)), SCALAR[m.group(2)], int(m.group(3))


def m_elem(asm, fn):
    mn, regs, arrs, _, _, scal = parse(asm)
    d, n = regs[:2]
    rm, es, index = elem_operand(asm)
    b = lanes(ST.v[rm], es)[index]
    bits = es if scal else ARR[arrs[0]][1]
    k = bits // es
    a, dd = lanes(ST.v[n], es, k), lanes(ST.v[d], es, k)
    wr(d, pack([fn(a[i], b, dd[i], es) for i in range(k)], es), bits)


def m_elem_long(asm, kind, signed):
    mn, regs, arrs, _, _, scal = parse(asm)
    d, n = regs[:2]
    rm, es, index = elem_operand(asm)
    y = val(lanes(ST.v[rm], es)[index], es, signed)
    es2 = 2 * es
    k = 1 if scal else 64 // es
    a = lanes(ST.v[n], es)
    a = a[k:2 * k] if mn.endswith("2") else a[:k]
    dd = lanes(ST.v[d], es2)
    out = []
    for i in range(k):
        x, acc = val(a[i], es, signed), dd[i]
        out.append({
            "mull": lambda: x * y, "mlal": lambda: acc + x * y, "mlsl": lambda: acc - x * y,
            "sqdmull": lambda: sat(2 * x * y, es2, False),
            "sqdmlal": lambda: sat(sx(acc, es2) + sx(sat(2 * x * y, es2, False), es2), es2, False),
            "sqdmlsl": lambda: sat(sx(acc, es2) - sx(sat(2 * x * y, es2, False), es2), es2, False),
        }[kind]())
    wr(d, pack(out, es2), es2 if scal else 128)


# ---- copies, permutes, immediates --------------------------------------------


def m_dup_elem(asm):
    mn, regs, arrs, idx, _, scal = parse(asm)
    d, n = regs[:2]
    rm, es, index = elem_operand(asm)
    x = lanes(ST.v[rm], es)[index]
    bits = es if scal else ARR[arrs[0]][1]
    wr(d, pack([x] * (bits // es), es), bits)


def m_dup_gen(asm):
    mn, (d, n), arrs, _, _, _ = parse(asm)
    es, bits = ARR[arrs[0]]
    wr(d, pack([ST.x[n]] * (bits // es), es), bits)


def m_ins_gen(asm):
    m = re.match(r"\w+ v(\d+)\.([bhsd])\[(\d+)\], [wx](\d+)", asm)
    d, es, i, n = int(m.group(1)), SCALAR[m.group(2)], int(m.group(3)), int(m.group(4))
    ls = lanes(ST.v[d], es)
    ls[i] = ST.x[n] & M(es)
    ST.v[d] = pack(ls, es)


def m_ins_elem(asm):
    m = re.match(r"\w+ v(\d+)\.([bhsd])\[(\d+)\], v(\d+)\.[bhsd]\[(\d+)\]", asm)
    d, es, i, n, j = int(m.group(1)), SCALAR[m.group(2)], int(m.group(3)), int(m.group(4)), int(m.group(5))
    ls = lanes(ST.v[d], es)
    ls[i] = lanes(ST.v[n], es)[j]
    ST.v[d] = pack(ls, es)


def m_mov_to_gen(asm, signed):
    m = re.match(r"\w+ ([wx])(\d+), v(\d+)\.([bhsd])\[(\d+)\]", asm)
    w = 32 if m.group(1) == "w" else 64
    d, n, es, i = int(m.group(2)), int(m.group(3)), SCALAR[m.group(4)], int(m.group(5))
    x = val(lanes(ST.v[n], es)[i], es, signed)
    ST.x[d] = x & M(w)


def m_permute(asm, kind, part):
    mn, (d, n, m), arrs, _, _, _ = parse(asm)
    es, bits = ARR[arrs[0]]
    k = bits // es
    a, b = lanes(ST.v[n], es, k), lanes(ST.v[m], es, k)
    if kind == "uzp":
        cat = a + b
        out = [cat[2 * i + part] for i in range(k)]
    elif kind == "trn":
        out = []
        for p in range(k // 2):
            out += [a[2 * p + part], b[2 * p + part]]
    else:
        base = part * k // 2
        out = []
        for p in range(k // 2):
            out += [a[base + p], b[base + p]]
    wr(d, pack(out, es), bits)


def m_ext(asm):
    mn, (d, n, m), arrs, _, imms, _ = parse(asm)
    bits = ARR[arrs[0]][1]
    pos = int(imms[0])
    cat = lanes(ST.v[n], 8, bits // 8) + lanes(ST.v[m], 8, bits // 8)
    wr(d, pack(cat[pos:pos + bits // 8], 8), bits)


def m_tbl(asm, tbx):
    m = re.match(r"\w+ v(\d+)\.(\d+b), \{([^}]*)\}, v(\d+)", asm)
    d, arr, tables, idx = int(m.group(1)), m.group(2), m.group(3), int(m.group(4))
    regs = [int(r) for r in re.findall(r"v(\d+)", tables)]
    table = sum((lanes(ST.v[r], 8) for r in regs), [])
    bits = ARR[arr][1]
    ix, dd = lanes(ST.v[idx], 8, bits // 8), lanes(ST.v[d], 8, bits // 8)
    out = [table[k] if k < len(table) else (dd[i] if tbx else 0) for i, k in enumerate(ix)]
    wr(d, pack(out, 8), bits)


def m_movi(asm):
    mn, regs, arrs, _, imms, scal = parse(asm)
    d = regs[0]
    m = re.search(r"(lsl|msl) #(\d+)", asm)
    amount = int(m.group(2)) if m else 0
    if scal == "d" or (arrs and arrs[0] == "2d" and mn == "movi"):
        imm = int(imms[0], 0)
        ST.v[d] = (imm | (imm << 64)) if arrs else imm
        return
    es, bits = ARR[arrs[0]]
    imm = int(imms[0], 0)
    if m and m.group(1) == "msl":
        x = (imm << amount) | M(amount)
    else:
        x = imm << amount
    if mn in ("mvni", "bic"):
        x = ~x
    k = bits // es
    if mn in ("orr", "bic"):
        cur = lanes(ST.v[d], es, k)
        out = [(c | x) if mn == "orr" else (c & x) for c in cur]
    else:
        out = [x] * k
    wr(d, pack(out, es), bits)


def m_fmov_imm(asm):
    mn, (d,), arrs, _, imms, _ = parse(asm)
    es, bits = ARR[arrs[0]]
    f = FMT[es]
    v = Fr(imms[0]) if "." in imms[0] else Fr(int(imms[0]))
    x = enc(v, f)
    wr(d, pack([x] * (bits // es), es), bits)


# ---- floating-point lane models -----------------------------------------------

FSAME = {
    "fadd": lambda a, b, d, f: f_add(a, b, f),
    "fsub": lambda a, b, d, f: f_sub(a, b, f),
    "fmul": lambda a, b, d, f: f_mul(a, b, f),
    "fdiv": lambda a, b, d, f: f_div(a, b, f),
    "fmulx": lambda a, b, d, f: f_mulx(a, b, f),
    "fmla": lambda a, b, d, f: f_fma(d, a, b, f),
    "fmls": lambda a, b, d, f: f_fma(d, f_neg(a, f), b, f),
    "fmax": lambda a, b, d, f: f_maxmin(a, b, f, True, False),
    "fmin": lambda a, b, d, f: f_maxmin(a, b, f, False, False),
    "fmaxnm": lambda a, b, d, f: f_maxmin(a, b, f, True, True),
    "fminnm": lambda a, b, d, f: f_maxmin(a, b, f, False, True),
    "fabd": lambda a, b, d, f: f_abs(f_sub(a, b, f), f),
    "fcmeq": lambda a, b, d, f: M(f.w) if f_cmp(a, b, f, "eq") else 0,
    "fcmge": lambda a, b, d, f: M(f.w) if f_cmp(a, b, f, "ge") else 0,
    "fcmgt": lambda a, b, d, f: M(f.w) if f_cmp(a, b, f, "gt") else 0,
    "facge": lambda a, b, d, f: M(f.w) if f_cmp(f_abs(a, f), f_abs(b, f), f, "ge") else 0,
    "facgt": lambda a, b, d, f: M(f.w) if f_cmp(f_abs(a, f), f_abs(b, f), f, "gt") else 0,
    "frecps": lambda a, b, d, f: f_recip_step(a, b, f, False),
    "frsqrts": lambda a, b, d, f: f_recip_step(a, b, f, True),
}
FPAIR = {
    "faddp": lambda a, b, f: f_add(a, b, f),
    "fmaxp": lambda a, b, f: f_maxmin(a, b, f, True, False),
    "fminp": lambda a, b, f: f_maxmin(a, b, f, False, False),
    "fmaxnmp": lambda a, b, f: f_maxmin(a, b, f, True, True),
    "fminnmp": lambda a, b, f: f_maxmin(a, b, f, False, True),
}


def m_fsame(asm, fn):
    m_same(asm, lambda a, b, d, e: fn(a, b, d, FMT[e]))


def m_fpair(asm, fn):
    mn, regs, arrs, _, _, scal = parse(asm)
    if scal:
        d, n = regs
        es = SCALAR[scal]
        a = lanes(ST.v[n], es)
        wr(d, fn(a[0], a[1], FMT[es]), es)
        return
    m_pair(asm, lambda a, b, e: fn(a, b, FMT[e]))


def m_fmisc(asm, fn):
    m_misc(asm, lambda a, d, e: fn(a, FMT[e]))


def m_fcvt_narrow(asm, mode):
    mn, (d, n), arrs, _, _, scal = parse(asm)
    src = lanes(ST.v[n], 64)
    out = [f_convert(x, FD, FS, mode) for x in src]
    if scal:
        wr(d, out[0], 32)
    else:
        wr_narrow(d, pack(out, 32), mn.endswith("2"))


def m_fcvtl(asm):
    mn, (d, n), _, _, _, _ = parse(asm)
    src = lanes(ST.v[n], 32)
    src = src[2:] if mn.endswith("2") else src[:2]
    wr(d, pack([f_convert(x, FS, FD) for x in src], 64), 128)


def m_facross(asm, fn):
    mn, (d, n), _, _, _, _ = parse(asm)
    e = lanes(ST.v[n], 32)
    wr(d, fn(fn(e[0], e[1], FS), fn(e[2], e[3], FS), FS), 32)


def m_felem(asm, fn):
    m_elem(asm, lambda a, b, d, e: fn(a, b, d, FMT[e]))


def m_fixed(asm, to_int, unsigned):
    mn, regs, arrs, _, imms, scal = parse(asm)
    d, n = regs[:2]
    fb = int(imms[0])
    es, bits = shape(arrs, scal)
    f = FMT[es]
    a = lanes(ST.v[n], es, bits // es)
    if to_int:
        out = [f_to_int(x, f, "z", es, unsigned, fb) for x in a]
    else:
        out = [f_from_int(x, es, unsigned, f, fb, fpcr_mode()) for x in a]
    wr(d, pack(out, es), bits)


# ---- loads and stores --------------------------------------------------------


def mem_read(addr, n):
    off = addr - MEM_BASE
    return int.from_bytes(ST.mem[off:off + n], "little")


def mem_write(addr, n, v):
    off = addr - MEM_BASE
    ST.mem[off:off + n] = (v & M(8 * n)).to_bytes(n, "little")


def ldst_regs(asm):
    m = re.search(r"\{([^}]*)\}", asm)
    regs = [int(r) for r in re.findall(r"v(\d+)", m.group(1))]
    arr = re.search(r"v\d+\.(\d*[bhsd])", m.group(1)).group(1)
    if "-" in m.group(1):
        regs = list(range(regs[0], regs[1] + 1))
    return regs, arr


def post_index(asm, n, total):
    m = re.search(r"\], (#(\d+)|x(\d+))$", asm)
    if m:
        ST.x[1] = (ST.x[1] + (total if m.group(2) else ST.x[int(m.group(3))])) & M(64)


def m_ldst_multi(asm, load, selem):
    regs, arr = ldst_regs(asm)
    es, bits = ARR[arr]
    eb = es // 8
    rpt = len(regs) // selem
    addr = ST.x[1]
    if load:
        for r in regs:
            ST.v[r] = 0
    for r in range(rpt):
        for e in range(bits // es):
            for s in range(selem):
                reg = regs[r + s]
                if load:
                    ls = lanes(ST.v[reg], es)
                    ls[e] = mem_read(addr, eb)
                    ST.v[reg] = pack(ls, es)
                else:
                    mem_write(addr, eb, lanes(ST.v[reg], es)[e])
                addr += eb
    post_index(asm, 1, addr - ST.x[1])


def m_ldst_lane(asm, load):
    regs, arr = ldst_regs(asm)
    es = SCALAR[arr]
    eb = es // 8
    index = int(re.search(r"\}\[(\d+)\]", asm).group(1))
    addr = ST.x[1]
    for reg in regs:
        ls = lanes(ST.v[reg], es)
        if load:
            ls[index] = mem_read(addr, eb)
            ST.v[reg] = pack(ls, es)
        else:
            mem_write(addr, eb, ls[index])
        addr += eb
    post_index(asm, 1, addr - ST.x[1])


def m_ldr(asm):
    regs, arr = ldst_regs(asm)
    es, bits = ARR[arr]
    eb = es // 8
    addr = ST.x[1]
    for reg in regs:
        x = mem_read(addr, eb)
        ST.v[reg] = pack([x] * (bits // es), es)
        addr += eb
    post_index(asm, 1, addr - ST.x[1])


# ---- the cases ----------------------------------------------------------------

CASES = []  # (group, asm, fpcr, model)


def case(group, asm, model, fpcr=0):
    CASES.append((group, asm, fpcr, model))


def same_cases():
    g = "int_same"
    for asm in [
        "shadd v0.16b, v1.16b, v2.16b", "uhadd v0.8h, v1.8h, v2.8h",
        "sqadd v0.4s, v3.4s, v3.4s", "uqadd v0.2d, v3.2d, v1.2d",
        "srhadd v0.8b, v1.8b, v2.8b", "urhadd v0.4h, v1.4h, v2.4h",
        "shsub v0.4s, v1.4s, v2.4s", "uhsub v0.16b, v1.16b, v2.16b",
        "sqsub v0.8h, v4.8h, v5.8h", "uqsub v0.4s, v1.4s, v2.4s",
        "cmgt v0.2d, v1.2d, v2.2d", "cmhi v0.4s, v1.4s, v2.4s",
        "cmge v0.8h, v4.8h, v5.8h", "cmhs v0.16b, v1.16b, v2.16b",
        "sshl v0.4s, v1.4s, v6.4s", "ushl v0.2d, v1.2d, v6.2d",
        "sqshl v0.8h, v1.8h, v6.8h", "uqshl v0.16b, v1.16b, v6.16b",
        "srshl v0.4s, v1.4s, v6.4s", "urshl v0.8h, v1.8h, v6.8h",
        "sqrshl v0.2d, v1.2d, v6.2d", "uqrshl v0.4s, v1.4s, v6.4s",
        "smax v0.4s, v1.4s, v2.4s", "umax v0.16b, v1.16b, v2.16b",
        "smin v0.8h, v1.8h, v2.8h", "umin v0.2s, v1.2s, v2.2s",
        "sabd v0.16b, v1.16b, v2.16b", "uabd v0.8h, v1.8h, v2.8h",
        "saba v0.4s, v1.4s, v2.4s", "uaba v0.16b, v1.16b, v2.16b",
        "add v0.2d, v1.2d, v2.2d", "add v0.8b, v1.8b, v2.8b", "sub v0.4s, v1.4s, v2.4s",
        "cmtst v0.8h, v1.8h, v2.8h", "cmeq v0.16b, v4.16b, v5.16b",
        "mla v0.4s, v1.4s, v2.4s", "mls v0.8h, v1.8h, v2.8h",
        "mul v0.16b, v1.16b, v2.16b", "pmul v0.16b, v1.16b, v2.16b",
        "sqdmulh v0.4s, v3.4s, v3.4s", "sqrdmulh v0.8h, v1.8h, v2.8h",
        "and v0.16b, v1.16b, v2.16b", "bic v0.8b, v1.8b, v2.8b",
        "orr v0.16b, v1.16b, v2.16b", "orn v0.16b, v1.16b, v2.16b",
        "eor v0.8b, v1.8b, v2.8b", "bsl v0.16b, v1.16b, v2.16b",
        "bit v0.16b, v1.16b, v2.16b", "bif v0.8b, v1.8b, v2.8b",
        # Scalar forms.
        "sqadd b0, b3, b4", "uqsub h0, h1, h2", "sqsub s0, s4, s5", "uqadd d0, d1, d2",
        "cmgt d0, d1, d2", "cmhs d0, d1, d2", "add d0, d1, d2", "sub d0, d1, d2",
        "cmtst d0, d1, d2", "cmeq d0, d1, d1", "sshl d0, d1, d6", "urshl d0, d1, d6",
        "sqshl s0, s1, s6", "uqrshl b0, b1, b6", "sqdmulh s0, s3, s3", "sqrdmulh h0, h1, h2",
        # Armv8.1 RDM.
        "sqrdmlah v0.4s, v1.4s, v2.4s", "sqrdmlsh v0.8h, v3.8h, v3.8h", "sqrdmlah h0, h3, h3",
        "sqrdmlsh s0, s1, s2",
    ]:
        case(g, asm, lambda asm=asm: m_same(asm, SAME[asm.split()[0]]))
    for asm in ["addp v0.2d, v1.2d, v2.2d", "addp v0.8b, v1.8b, v2.8b",
                "smaxp v0.4s, v1.4s, v2.4s", "umaxp v0.8b, v1.8b, v2.8b",
                "sminp v0.8h, v1.8h, v2.8h", "uminp v0.16b, v1.16b, v2.16b"]:
        case(g, asm, lambda asm=asm: m_pair(asm, PAIR[asm.split()[0]]))
    case(g, "addp d0, v1.2d", lambda: wr(0, sum(lanes(ST.v[1], 64)), 64))


def misc_cases():
    g = "int_misc"
    for asm in ["suqadd v0.4s, v1.4s", "usqadd v0.16b, v1.16b", "cls v0.8h, v1.8h",
                "clz v0.4s, v1.4s", "cnt v0.16b, v1.16b", "not v0.8b, v1.8b",
                "rbit v0.16b, v1.16b", "sqabs v0.16b, v4.16b", "sqneg v0.4s, v3.4s",
                "abs v0.2d, v1.2d", "neg v0.8h, v1.8h", "suqadd s0, s1",
                "usqadd d0, d1", "sqabs b0, b4", "sqneg d0, d3", "abs d0, d3", "neg d0, d1"]:
        case(g, asm, lambda asm=asm: m_misc(asm, MISC[asm.split()[0]]))
    for asm in ["cmgt v0.4s, v1.4s, #0", "cmge v0.8h, v4.8h, #0", "cmeq v0.16b, v4.16b, #0",
                "cmle v0.2d, v1.2d, #0", "cmlt v0.8b, v1.8b, #0", "cmgt d0, d1, #0",
                "cmeq d0, d4, #0", "cmlt d0, d3, #0"]:
        case(g, asm, lambda asm=asm: m_cmp0(asm, asm.split()[0]))
    case(g, "rev64 v0.16b, v1.16b", lambda: m_rev("rev64 v0.16b, v1.16b", 64))
    case(g, "rev64 v0.4s, v1.4s", lambda: m_rev("rev64 v0.4s, v1.4s", 64))
    case(g, "rev32 v0.8h, v1.8h", lambda: m_rev("rev32 v0.8h, v1.8h", 32))
    case(g, "rev16 v0.16b, v1.16b", lambda: m_rev("rev16 v0.16b, v1.16b", 16))
    case(g, "saddlp v0.8h, v1.16b", lambda: m_addlp("saddlp v0.8h, v1.16b", True, False))
    case(g, "uaddlp v0.2s, v1.4h", lambda: m_addlp("uaddlp v0.2s, v1.4h", False, False))
    case(g, "sadalp v0.2d, v1.4s", lambda: m_addlp("sadalp v0.2d, v1.4s", True, True))
    case(g, "uadalp v0.8h, v1.16b", lambda: m_addlp("uadalp v0.8h, v1.16b", False, True))
    for asm, kind in [("xtn v0.8b, v1.8h", "xtn"), ("xtn2 v0.4s, v1.2d", "xtn"),
                      ("sqxtn v0.4h, v3.4s", "sqxtn"), ("sqxtn2 v0.16b, v4.8h", "sqxtn"),
                      ("uqxtn v0.2s, v3.2d", "uqxtn"), ("sqxtun v0.8b, v4.8h", "sqxtun"),
                      ("sqxtun2 v0.8h, v3.4s", "sqxtun"), ("sqxtn s0, d3", "sqxtn"),
                      ("uqxtn b0, h4", "uqxtn"), ("sqxtun h0, s3", "sqxtun")]:
        case(g, asm, lambda asm=asm, kind=kind: m_xtn(asm, kind))
    case(g, "shll v0.8h, v1.8b, #8", lambda: m_shll("shll v0.8h, v1.8b, #8"))
    case(g, "shll2 v0.2d, v1.4s, #32", lambda: m_shll("shll2 v0.2d, v1.4s, #32"))


def diff_cases():
    g = "int_diff"
    for asm in [
        "saddl v0.8h, v1.8b, v2.8b", "uaddl2 v0.4s, v1.8h, v2.8h", "saddw v0.2d, v1.2d, v2.2s",
        "uaddw2 v0.8h, v1.8h, v2.16b", "ssubl v0.4s, v1.4h, v2.4h", "usubl2 v0.2d, v1.4s, v2.4s",
        "ssubw2 v0.4s, v1.4s, v2.8h", "usubw v0.8h, v1.8h, v2.8b",
        "addhn v0.8b, v1.8h, v2.8h", "addhn2 v0.8h, v1.4s, v2.4s", "raddhn v0.2s, v1.2d, v2.2d",
        "subhn v0.4h, v1.4s, v2.4s", "rsubhn2 v0.16b, v1.8h, v2.8h",
        "sabal v0.8h, v1.8b, v2.8b", "uabal2 v0.4s, v1.8h, v2.8h", "sabdl v0.2d, v1.2s, v2.2s",
        "uabdl2 v0.8h, v1.16b, v2.16b", "smlal v0.4s, v1.4h, v2.4h", "umlal2 v0.2d, v1.4s, v2.4s",
        "smlsl2 v0.8h, v1.16b, v2.16b", "umlsl v0.4s, v1.4h, v2.4h",
        "smull v0.2d, v1.2s, v2.2s", "umull2 v0.8h, v1.16b, v2.16b",
        "sqdmull v0.4s, v3.4h, v3.4h", "sqdmull2 v0.2d, v1.4s, v2.4s",
        "sqdmlal v0.4s, v1.4h, v2.4h", "sqdmlsl2 v0.2d, v3.4s, v3.4s",
        "pmull v0.8h, v1.8b, v2.8b", "pmull2 v0.8h, v1.16b, v2.16b",
        "sqdmull s0, h3, h3", "sqdmlal d0, s1, s2", "sqdmlsl s0, h1, h2",
    ]:
        mn = asm.split()[0].rstrip("2")
        signed = mn[0] == "s" and mn not in ("subhn",)
        kind = mn if mn in ("addhn", "subhn", "raddhn", "rsubhn", "pmull", "sqdmull",
                            "sqdmlal", "sqdmlsl") else mn[1:]
        case(g, asm, lambda asm=asm, kind=kind, signed=signed: m_long(asm, kind, signed))
    g = "int_across"
    for asm in ["addv b0, v1.16b", "addv s0, v1.4s", "addv h0, v1.4h", "saddlv h0, v1.16b",
                "uaddlv d0, v1.4s", "saddlv s0, v1.4h", "smaxv h0, v1.8h", "umaxv s0, v1.4s",
                "sminv b0, v4.16b", "uminv b0, v1.8b"]:
        mn = asm.split()[0]
        kind = "addv" if mn == "addv" else mn[1:]
        case(g, asm, lambda asm=asm, kind=kind, s=mn[0] == "s": m_across(asm, kind, s))


def shift_cases():
    g = "int_shift"
    for asm in [
        "sshr v0.4s, v1.4s, #5", "ushr v0.2d, v1.2d, #63", "ssra v0.8h, v1.8h, #16",
        "usra v0.16b, v1.16b, #3", "srshr v0.4s, v1.4s, #31", "urshr v0.8h, v1.8h, #1",
        "srsra v0.2d, v1.2d, #40", "ursra v0.8b, v1.8b, #8", "sri v0.4s, v1.4s, #7",
        "sri v0.2d, v1.2d, #64", "shl v0.8h, v1.8h, #3", "sli v0.2d, v1.2d, #13",
        "sqshlu v0.4s, v1.4s, #4", "sqshl v0.16b, v1.16b, #2", "uqshl v0.8h, v1.8h, #9",
        "sshr d0, d1, #64", "ushr d0, d1, #64", "srshr d0, d1, #1", "ursra d0, d1, #10",
        "shl d0, d1, #63", "sli d0, d1, #1", "sri d0, d1, #33",
        "sqshl b0, b1, #3", "uqshl s0, s1, #5", "sqshlu h0, h4, #2", "sqshlu d0, d3, #1",
    ]:
        mn = asm.split()[0]
        if mn in ("sri", "sli", "shl"):
            kind, signed = mn, False
        elif mn in ("sqshlu", "sqshl", "uqshl"):
            kind, signed = mn, mn[0] == "s"
        else:
            kind, signed = mn[1:], mn[0] == "s"
        case(g, asm, lambda asm=asm, kind=kind, signed=signed: m_shift(asm, kind, signed))
    for asm, kind in [
        ("shrn v0.8b, v1.8h, #3", "shrn"), ("shrn2 v0.4s, v1.2d, #32", "shrn"),
        ("rshrn v0.4h, v1.4s, #16", "shrn"), ("rshrn2 v0.16b, v1.8h, #1", "shrn"),
        ("sqshrn v0.8b, v3.8h, #2", "sqshrn"), ("uqshrn2 v0.8h, v1.4s, #4", "uqshrn"),
        ("sqrshrn v0.2s, v1.2d, #20", "sqshrn"), ("uqrshrn v0.4h, v1.4s, #3", "uqshrn"),
        ("sqshrun v0.8b, v4.8h, #1", "sqshrun"), ("sqrshrun2 v0.4s, v3.2d, #31", "sqshrun"),
        ("sqshrn s0, d3, #4", "sqshrn"), ("uqrshrn b0, h1, #2", "uqshrn"),
        ("sqrshrun h0, s3, #9", "sqshrun"),
    ]:
        case(g, asm, lambda asm=asm, kind=kind: m_shift(asm, kind))
    for asm in ["sshll v0.8h, v1.8b, #3", "ushll2 v0.2d, v1.4s, #31", "sxtl v0.4s, v1.4h",
                "uxtl2 v0.8h, v1.16b"]:
        a2 = asm if "#" in asm else asm + ", #0"
        signed = asm[0] == "s"
        case(g, asm, lambda a2=a2, signed=signed: m_shift(a2, "shll", signed))


def elem_cases():
    g = "int_elem"
    for asm, fn in [
        ("mul v0.4s, v1.4s, v2.s[3]", lambda a, b, d, e: a * b),
        ("mul v0.8h, v1.8h, v2.h[5]", lambda a, b, d, e: a * b),
        ("mla v0.4h, v1.4h, v15.h[7]", lambda a, b, d, e: d + a * b),
        ("mls v0.2s, v1.2s, v20.s[1]", lambda a, b, d, e: d - a * b),
        ("sqdmulh v0.4s, v3.4s, v3.s[3]", lambda a, b, d, e: sat((2 * sx(a, e) * sx(b, e)) >> e, e, False)),
        ("sqrdmulh v0.8h, v1.8h, v2.h[2]",
         lambda a, b, d, e: sat((2 * sx(a, e) * sx(b, e) + (1 << (e - 1))) >> e, e, False)),
        ("sqdmulh s0, s1, v2.s[1]", lambda a, b, d, e: sat((2 * sx(a, e) * sx(b, e)) >> e, e, False)),
        ("sqrdmulh h0, h1, v3.h[0]",
         lambda a, b, d, e: sat((2 * sx(a, e) * sx(b, e) + (1 << (e - 1))) >> e, e, False)),
        ("sqrdmlah v0.8h, v1.8h, v2.h[6]", SAME["sqrdmlah"]),
        ("sqrdmlsh v0.2s, v3.2s, v3.s[1]", SAME["sqrdmlsh"]),
        ("sqrdmlah s0, s1, v2.s[3]", SAME["sqrdmlah"]),
        ("sqrdmlsh h0, h4, v4.h[2]", SAME["sqrdmlsh"]),
    ]:
        case(g, asm, lambda asm=asm, fn=fn: m_elem(asm, fn))
    for asm in ["smull v0.4s, v1.4h, v2.h[3]", "umull2 v0.2d, v1.4s, v2.s[2]",
                "smlal2 v0.4s, v1.8h, v2.h[6]", "umlal v0.2d, v1.2s, v2.s[0]",
                "smlsl v0.2d, v1.2s, v2.s[1]", "umlsl2 v0.4s, v1.8h, v9.h[1]",
                "sqdmull v0.4s, v3.4h, v3.h[1]", "sqdmlal2 v0.2d, v1.4s, v2.s[3]",
                "sqdmlsl v0.4s, v1.4h, v2.h[4]", "sqdmull d0, s1, v2.s[2]",
                "sqdmlal s0, h1, v2.h[7]"]:
        mn = asm.split()[0].rstrip("2")
        kind = mn if mn.startswith("sqd") else mn[1:]
        signed = mn[0] == "s"
        case(g, asm, lambda asm=asm, kind=kind, signed=signed: m_elem_long(asm, kind, signed))


def perm_cases():
    g = "perm"
    case(g, "dup v0.16b, v1.b[13]", lambda: m_dup_elem("dup v0.16b, v1.b[13]"))
    case(g, "dup v0.2d, v1.d[1]", lambda: m_dup_elem("dup v0.2d, v1.d[1]"))
    case(g, "dup v0.4h, v1.h[6]", lambda: m_dup_elem("dup v0.4h, v1.h[6]"))
    case(g, "mov s0, v1.s[3]", lambda: m_dup_elem("mov s0, v1.s[3]"))
    case(g, "dup v0.4s, w3", lambda: m_dup_gen("dup v0.4s, w3"))
    case(g, "dup v0.8b, w3", lambda: m_dup_gen("dup v0.8b, w3"))
    case(g, "dup v0.2d, x3", lambda: m_dup_gen("dup v0.2d, x3"))
    case(g, "mov v0.h[5], w3", lambda: m_ins_gen("mov v0.h[5], w3"))
    case(g, "mov v0.d[1], x3", lambda: m_ins_gen("mov v0.d[1], x3"))
    case(g, "mov v0.b[2], v1.b[15]", lambda: m_ins_elem("mov v0.b[2], v1.b[15]"))
    case(g, "mov v0.s[3], v1.s[0]", lambda: m_ins_elem("mov v0.s[3], v1.s[0]"))
    case(g, "umov w0, v1.b[9]", lambda: m_mov_to_gen("umov w0, v1.b[9]", False))
    case(g, "umov w0, v1.h[3]", lambda: m_mov_to_gen("umov w0, v1.h[3]", False))
    case(g, "mov x0, v1.d[1]", lambda: m_mov_to_gen("mov x0, v1.d[1]", False))
    case(g, "smov w0, v4.b[3]", lambda: m_mov_to_gen("smov w0, v4.b[3]", True))
    case(g, "smov x0, v4.h[1]", lambda: m_mov_to_gen("smov x0, v4.h[1]", True))
    case(g, "smov x0, v3.s[1]", lambda: m_mov_to_gen("smov x0, v3.s[1]", True))
    for asm in ["uzp1 v0.16b, v1.16b, v2.16b", "uzp2 v0.4s, v1.4s, v2.4s",
                "uzp1 v0.4h, v1.4h, v2.4h", "trn1 v0.8h, v1.8h, v2.8h",
                "trn2 v0.2d, v1.2d, v2.2d", "trn2 v0.8b, v1.8b, v2.8b",
                "zip1 v0.4s, v1.4s, v2.4s", "zip2 v0.8b, v1.8b, v2.8b",
                "zip2 v0.16b, v1.16b, v2.16b"]:
        mn = asm.split()[0]
        case(g, asm, lambda asm=asm, mn=mn: m_permute(asm, mn[:3], int(mn[3]) - 1))
    case(g, "ext v0.16b, v1.16b, v2.16b, #3", lambda: m_ext("ext v0.16b, v1.16b, v2.16b, #3"))
    case(g, "ext v0.8b, v1.8b, v2.8b, #5", lambda: m_ext("ext v0.8b, v1.8b, v2.8b, #5"))
    # Index registers: v6 has small bytes (some in range), v1 random.
    for asm in ["tbl v0.16b, {v1.16b}, v6.16b", "tbl v0.8b, {v1.16b, v2.16b}, v5.8b",
                "tbl v0.16b, {v30.16b, v31.16b, v0.16b}, v6.16b",
                "tbl v0.16b, {v1.16b, v2.16b, v3.16b, v4.16b}, v5.16b",
                "tbx v0.16b, {v1.16b}, v6.16b", "tbx v0.8b, {v1.16b, v2.16b, v3.16b}, v5.8b"]:
        case(g, asm, lambda asm=asm: m_tbl(asm, asm.startswith("tbx")))
    g = "imm"
    for asm in ["movi v0.4s, #0x5a, lsl #16", "movi v0.2s, #0xff", "movi v0.8h, #0x81, lsl #8",
                "movi v0.4h, #0x7", "movi v0.4s, #0x12, msl #16", "movi v0.16b, #0xa5",
                "movi v0.2d, #0xff00ff0000ffff00", "movi d0, #0xff000000000000ff",
                "mvni v0.4s, #0x3c, lsl #24", "mvni v0.8h, #0x1", "mvni v0.2s, #0x40, msl #8",
                "orr v0.4s, #0x80, lsl #8", "orr v0.4h, #0xf0", "bic v0.8h, #0xff, lsl #8",
                "bic v0.2s, #0x1"]:
        case(g, asm, lambda asm=asm: m_movi(asm))
    for asm in ["fmov v0.4s, #1.5", "fmov v0.2s, #-31.0", "fmov v0.2d, #0.125"]:
        case(g, asm, lambda asm=asm: m_fmov_imm(asm))


def fp_cases():
    g = "fp_same"
    for asm in [
        "fadd v0.4s, v16.4s, v17.4s", "fadd v0.2d, v18.2d, v19.2d", "fsub v0.2s, v16.2s, v24.2s",
        "fsub v0.2d, v18.2d, v25.2d", "fmul v0.4s, v16.4s, v24.4s", "fmul v0.2d, v18.2d, v19.2d",
        "fdiv v0.4s, v16.4s, v24.4s", "fdiv v0.2d, v21.2d, v25.2d", "fmulx v0.4s, v20.4s, v16.4s",
        "fmla v0.4s, v16.4s, v24.4s", "fmla v24.2d, v18.2d, v19.2d", "fmls v16.4s, v24.4s, v17.4s",
        "fmax v0.4s, v16.4s, v17.4s", "fmin v0.4s, v20.4s, v24.4s", "fmaxnm v0.4s, v16.4s, v17.4s",
        "fminnm v0.2d, v18.2d, v25.2d", "fabd v0.4s, v16.4s, v24.4s", "fcmeq v0.4s, v16.4s, v16.4s",
        "fcmge v0.4s, v16.4s, v24.4s", "fcmgt v0.2d, v18.2d, v19.2d", "facge v0.4s, v16.4s, v17.4s",
        "facgt v0.2d, v21.2d, v25.2d", "frecps v0.4s, v16.4s, v24.4s", "frsqrts v0.2d, v19.2d, v25.2d",
        # Scalar forms.
        "fmulx s0, s16, s24", "fcmeq d0, d18, d18", "fcmge s0, s20, s24", "fcmgt d0, d19, d18",
        "facge s0, s24, s16", "facgt d0, d25, d21", "fabd d0, d18, d19", "frecps s0, s16, s24",
        "frsqrts d0, d21, d25",
    ]:
        case(g, asm, lambda asm=asm: m_fsame(asm, FSAME[asm.split()[0]]))
    for asm in ["faddp v0.4s, v16.4s, v24.4s", "faddp v0.2d, v18.2d, v19.2d",
                "fmaxp v0.4s, v16.4s, v17.4s", "fminp v0.2s, v20.2s, v24.2s",
                "fmaxnmp v0.4s, v24.4s, v17.4s", "fminnmp v0.2d, v18.2d, v25.2d",
                "faddp s0, v16.2s", "faddp d0, v18.2d", "fmaxp s0, v24.2s", "fminp d0, v21.2d",
                "fmaxnmp s0, v17.2s", "fminnmp d0, v25.2d"]:
        case(g, asm, lambda asm=asm: m_fpair(asm, FPAIR[asm.split()[0]]))
    g = "fp_misc"
    rint = lambda mode: (lambda a, f: f_rint(a, f, mode))
    cvt = lambda mode, u: (lambda a, f: f_to_int(a, f, mode, f.w, u))
    FMISC = {
        "fabs": lambda a, f: f_abs(a, f), "fneg": lambda a, f: f_neg(a, f),
        "fsqrt": f_sqrt, "frecpe": f_recpe, "frsqrte": f_rsqrte, "frecpx": f_recpx,
        "frintn": rint("ne"), "frintp": rint("p"), "frintm": rint("m"), "frintz": rint("z"),
        "frinta": rint("a"), "frintx": lambda a, f: f_rint(a, f, fpcr_mode()),
        "frinti": lambda a, f: f_rint(a, f, fpcr_mode()),
        "fcvtns": cvt("ne", False), "fcvtps": cvt("p", False), "fcvtms": cvt("m", False),
        "fcvtzs": cvt("z", False), "fcvtas": cvt("a", False), "fcvtnu": cvt("ne", True),
        "fcvtpu": cvt("p", True), "fcvtmu": cvt("m", True), "fcvtzu": cvt("z", True),
        "fcvtau": cvt("a", True),
        "scvtf": lambda a, f: f_from_int(a, f.w, False, f, 0, fpcr_mode()),
        "ucvtf": lambda a, f: f_from_int(a, f.w, True, f, 0, fpcr_mode()),
        "urecpe": lambda a, f: urecpe(a), "ursqrte": lambda a, f: ursqrte(a),
    }
    FCMP0 = {
        "fcmgt": lambda a, f: f_cmp(a, 0, f, "gt"), "fcmge": lambda a, f: f_cmp(a, 0, f, "ge"),
        "fcmeq": lambda a, f: f_cmp(a, 0, f, "eq"), "fcmle": lambda a, f: f_cmp(0, a, f, "ge"),
        "fcmlt": lambda a, f: f_cmp(0, a, f, "gt"),
    }
    RM = 2 << 22
    for asm, fpcr in [
        ("fabs v0.4s, v20.4s", 0), ("fneg v0.2d, v21.2d", 0), ("fsqrt v0.4s, v24.4s", 0),
        ("fsqrt v0.2d, v19.2d", 0), ("frecpe v0.4s, v16.4s", 0), ("frsqrte v0.2d, v19.2d", 0),
        ("frintn v0.4s, v24.4s", 0), ("frintp v0.4s, v20.4s", 0), ("frintm v0.2d, v21.2d", 0),
        ("frintz v0.4s, v20.4s", 0), ("frinta v0.4s, v24.4s", 0), ("frintx v0.2d, v25.2d", 0),
        ("frinti v0.4s, v24.4s", RM), ("fcvtns v0.4s, v24.4s", 0), ("fcvtps v0.4s, v20.4s", 0),
        ("fcvtms v0.2d, v21.2d", 0), ("fcvtzs v0.4s, v16.4s", 0), ("fcvtas v0.4s, v24.4s", 0),
        ("fcvtnu v0.4s, v24.4s", 0), ("fcvtpu v0.2d, v25.2d", 0), ("fcvtmu v0.4s, v20.4s", 0),
        ("fcvtzu v0.2d, v21.2d", 0), ("fcvtau v0.4s, v16.4s", 0), ("scvtf v0.4s, v22.4s", 0),
        ("ucvtf v0.4s, v22.4s", 0), ("scvtf v0.2d, v23.2d", 2 << 22), ("ucvtf v0.2d, v23.2d", 0),
        ("urecpe v0.4s, v1.4s", 0), ("ursqrte v0.2s, v2.2s", 0),
        # Scalar forms.
        ("frecpe d0, d18", 0), ("frsqrte s0, s24", 0), ("frecpx s0, s16", 0),
        ("frecpx d0, d19", 0), ("fcvtzs s0, s20", 0), ("fcvtau d0, d21", 0),
        ("scvtf d0, d23", 0), ("ucvtf s0, s1", 1 << 22),
    ]:
        case(g, asm, lambda asm=asm: m_fmisc(asm, FMISC[asm.split()[0]]), fpcr)
    for asm in ["fcmgt v0.4s, v24.4s, #0.0", "fcmge v0.2d, v21.2d, #0.0",
                "fcmeq v0.4s, v20.4s, #0.0", "fcmle v0.4s, v17.4s, #0.0",
                "fcmlt v0.2d, v25.2d, #0.0", "fcmeq d0, d18, #0.0", "fcmlt s0, s24, #0.0"]:
        fn = FCMP0[asm.split()[0]]
        case(g, asm, lambda asm=asm, fn=fn: m_fmisc(asm, lambda a, f: M(f.w) if fn(a, f) else 0))
    case(g, "fcvtn v0.2s, v18.2d", lambda: m_fcvt_narrow("fcvtn v0.2s, v18.2d", "ne"))
    case(g, "fcvtn2 v0.4s, v21.2d", lambda: m_fcvt_narrow("fcvtn2 v0.4s, v21.2d", "z"), 3 << 22)
    case(g, "fcvtxn v0.2s, v18.2d", lambda: m_fcvt_narrow("fcvtxn v0.2s, v18.2d", "odd"))
    case(g, "fcvtxn2 v0.4s, v19.2d", lambda: m_fcvt_narrow("fcvtxn2 v0.4s, v19.2d", "odd"))
    case(g, "fcvtxn s0, d18", lambda: m_fcvt_narrow("fcvtxn s0, d18", "odd"))
    case(g, "fcvtl v0.2d, v16.2s", lambda: m_fcvtl("fcvtl v0.2d, v16.2s"))
    case(g, "fcvtl2 v0.2d, v17.4s", lambda: m_fcvtl("fcvtl2 v0.2d, v17.4s"))
    g = "fp_across"
    for asm in ["fmaxv s0, v16.4s", "fminv s0, v20.4s", "fmaxnmv s0, v17.4s", "fminnmv s0, v17.4s"]:
        fn = {"fmaxv": FPAIR["fmaxp"], "fminv": FPAIR["fminp"], "fmaxnmv": FPAIR["fmaxnmp"],
              "fminnmv": FPAIR["fminnmp"]}[asm.split()[0]]
        case(g, asm, lambda asm=asm, fn=fn: m_facross(asm, fn))
    g = "fp_elem"
    for asm in ["fmla v0.4s, v16.4s, v24.s[2]", "fmla v24.2d, v18.2d, v19.d[1]",
                "fmls v16.2s, v24.2s, v17.s[1]", "fmul v0.4s, v16.4s, v24.s[3]",
                "fmul v0.2d, v21.2d, v25.d[0]", "fmulx v0.4s, v20.4s, v20.s[0]",
                "fmul s0, s16, v24.s[1]", "fmla d24, d18, v19.d[0]", "fmulx d0, d21, v25.d[1]",
                "fmls s16, s24, v17.s[2]"]:
        case(g, asm, lambda asm=asm: m_felem(asm, FSAME[asm.split()[0]]))
    g = "fp_fixed"
    for asm in ["scvtf v0.4s, v22.4s, #8", "ucvtf v0.2d, v23.2d, #16", "fcvtzs v0.4s, v24.4s, #4",
                "fcvtzu v0.2d, v25.2d, #10", "scvtf s0, s22, #3", "fcvtzu d0, d21, #5",
                "fcvtzs d0, d18, #60"]:
        mn = asm.split()[0]
        case(g, asm, lambda asm=asm, mn=mn: m_fixed(asm, mn.startswith("fcvt"), mn in ("ucvtf", "fcvtzu")))


def mem_cases():
    g = "mem"
    for asm, selem in [
        ("ld1 {v0.16b}, [x1]", 1), ("ld1 {v0.4h, v1.4h}, [x1]", 1),
        ("ld1 {v0.4s, v1.4s, v2.4s}, [x1], #48", 1), ("ld1 {v30.2d, v31.2d, v0.2d, v1.2d}, [x1]", 1),
        ("ld2 {v0.8h, v1.8h}, [x1], x2", 2), ("ld2 {v0.8b, v1.8b}, [x1]", 2),
        ("ld3 {v0.4s, v1.4s, v2.4s}, [x1]", 3), ("ld3 {v0.8b, v1.8b, v2.8b}, [x1], #24", 3),
        ("ld4 {v0.16b, v1.16b, v2.16b, v3.16b}, [x1]", 4), ("ld4 {v0.4h, v1.4h, v2.4h, v3.4h}, [x1], x2", 4),
        ("st1 {v1.16b}, [x1]", 1), ("st1 {v1.2s, v2.2s}, [x1], #16", 1),
        ("st1 {v1.8h, v2.8h, v3.8h}, [x1]", 1), ("st1 {v1.2d, v2.2d, v3.2d, v4.2d}, [x1], x2", 1),
        ("st2 {v1.4s, v2.4s}, [x1]", 2), ("st3 {v1.8h, v2.8h, v3.8h}, [x1], #48", 3),
        ("st4 {v1.8b, v2.8b, v3.8b, v4.8b}, [x1]", 4), ("st4 {v1.2d, v2.2d, v3.2d, v4.2d}, [x1]", 4),
    ]:
        case(g, asm, lambda asm=asm, selem=selem: m_ldst_multi(asm, asm.startswith("ld"), selem))
    for asm in ["ld1 {v0.b}[9], [x1]", "ld1 {v0.d}[1], [x1], #8", "ld2 {v0.h, v1.h}[5], [x1]",
                "ld3 {v0.s, v1.s, v2.s}[3], [x1], x2", "ld4 {v0.d, v1.d, v2.d, v3.d}[0], [x1]",
                "st1 {v1.s}[2], [x1]", "st2 {v1.b, v2.b}[15], [x1], #2",
                "st3 {v1.h, v2.h, v3.h}[7], [x1]", "st4 {v1.s, v2.s, v3.s, v4.s}[1], [x1], x2"]:
        case(g, asm, lambda asm=asm: m_ldst_lane(asm, asm.startswith("ld")))
    for asm in ["ld1r {v0.8b}, [x1]", "ld1r {v0.2d}, [x1], #8", "ld2r {v0.4s, v1.4s}, [x1]",
                "ld3r {v0.8h, v1.8h, v2.8h}, [x1], x2", "ld4r {v0.2s, v1.2s, v2.2s, v3.2s}, [x1]"]:
        case(g, asm, lambda asm=asm: m_ldr(asm))


# ---- assembly and output -------------------------------------------------------


def assemble(lines):
    with tempfile.TemporaryDirectory() as tmp:
        src, obj = os.path.join(tmp, "v.s"), os.path.join(tmp, "v.o")
        with open(src, "w") as fh:
            fh.write("".join(line + "\n" for line in lines))
        subprocess.run([os.environ.get("CLANG", "clang"), "--target=aarch64-none-elf", "-march=armv8.2-a", "-c", src,
                        "-o", obj], check=True)
        dis = subprocess.run([os.environ.get("OBJDUMP", "llvm-objdump"), "-d", obj], check=True, capture_output=True,
                             text=True).stdout
    words = re.findall(r"^\s*[0-9a-f]+:\s+([0-9a-f]{8})\s", dis, re.M)
    if len(words) != len(lines):
        sys.exit(f"assembled {len(words)} words for {len(lines)} lines")
    return [int(w, 16) for w in words]


def main():
    global ST
    for build in (same_cases, misc_cases, diff_cases, shift_cases, elem_cases, perm_cases,
                  fp_cases, mem_cases):
        build()
    words = assemble([asm for _, asm, _, _ in CASES])
    out = []
    w = out.append
    w("// Generated by scripts/gen-neon-vectors.py. Do not edit.")
    w("//")
    w("// Encodings: clang --target=aarch64 and llvm-objdump. Expected values: the")
    w("// per-lane Python models in the script, written from the Arm ARM.")
    w("")
    w("use super::neon_tests::Case;")
    w("")
    w(f"pub const MEM_BASE: u64 = {MEM_BASE:#x};")
    w("pub const MEM: [u8; %d] = [%s];" % (MEM_LEN, ", ".join(str(b) for b in MEM0)))
    w("pub const X: [u64; 4] = [%s];" % ", ".join(f"{x:#x}" for x in X0))
    w("pub const V: [u128; 32] = [")
    for x in V0:
        w(f"    {x:#034x},")
    w("];")
    w("")
    w("pub const CASES: &[Case] = &[")
    for (group, asm, fpcr, model), insn in zip(CASES, words):
        ST = State(fpcr)
        model()
        v = [(k, x) for k, x in enumerate(ST.v) if x != V0[k]]
        xs = [(k, x) for k, x in enumerate(ST.x) if x != X0[k]]
        mem = [(k, b) for k, b in enumerate(ST.mem) if b != MEM0[k]]
        w("    Case {")
        w(f"        group: {group!r},".replace("'", '"'))
        w(f"        asm: {asm!r},".replace("'", '"'))
        w(f"        insn: {insn:#010x},")
        if fpcr:
            w(f"        fpcr: {fpcr:#x},")
        else:
            w("        fpcr: 0,")
        w("        v: &[%s]," % ", ".join(f"({k}, {x:#x})" for k, x in v))
        w("        x: &[%s]," % ", ".join(f"({k}, {x:#x})" for k, x in xs))
        w(f"        qc: {'true' if ST.qc else 'false'},")
        w("        mem: &[%s]," % ", ".join(f"({k}, {b:#x})" for k, b in mem))
        w("    },")
    w("];")
    text = "\n".join(out) + "\n"
    if "--check" in sys.argv:
        with open(OUT) as fh:
            if fh.read() != text:
                sys.exit("neon_vectors.rs is out of date: run scripts/gen-neon-vectors.py")
        print(f"neon_vectors.rs up to date ({len(CASES)} cases)")
        return
    with open(OUT, "w", newline="\n") as fh:
        fh.write(text)
    print(f"wrote {len(CASES)} cases to {os.path.relpath(OUT, ROOT)}")


if __name__ == "__main__":
    main()
