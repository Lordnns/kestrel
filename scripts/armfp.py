"""Arm floating-point semantics for half, single and double precision, in Python.

Shared by the test generators (gen-expected.py, gen-neon-vectors.py). Values
are raw bit patterns; arithmetic is exact (fractions) and rounded here
(FPRound), following the pseudocode of the Arm Architecture Reference Manual
(DDI 0487). Nothing here comes from kestrel.

Set FZ, FZ16 and DN (FPCR.FZ, FPCR.FZ16, FPCR.DN) at module level before
calling; FZ16 governs half precision, FZ the other formats. Rounding modes
are passed explicitly: "ne", "p", "m", "z", "a" (ties away), "odd".
Conversions to and from half precision ignore FZ16 (FPUnpackCV and
FPRoundCV clear it).
"""

import math
import struct
from fractions import Fraction as Fr

FZ = False
FZ16 = False
DN = False


def M(es):
    return (1 << es) - 1


def sx(x, es):
    x &= M(es)
    return x - (1 << es) if x >> (es - 1) else x


def val(x, es, signed):
    return sx(x, es) if signed else x & M(es)


def f32(x):
    """Bits of a host float rounded to single precision."""
    return struct.unpack("<I", struct.pack("<f", x))[0]


def f64(x):
    return struct.unpack("<Q", struct.pack("<d", x))[0]


def f16(x):
    """Bits of a host float rounded to half precision."""
    return struct.unpack("<H", struct.pack("<e", x))[0]



class Fmt:
    def __init__(self, w, e, f):
        self.w, self.e, self.f = w, e, f
        self.bias = (1 << (e - 1)) - 1
        self.emaxf = (1 << e) - 1
        self.sign = 1 << (w - 1)
        self.dnan = (self.emaxf << f) | (1 << (f - 1))
        self.quiet = 1 << (f - 1)


FH, FS, FD = Fmt(16, 5, 10), Fmt(32, 8, 23), Fmt(64, 11, 52)
FMT = {16: FH, 32: FS, 64: FD}


def flushes(f, cv=False):
    """Whether subnormals of format f flush to zero: FZ16 for half precision
    (never in conversions), FZ otherwise."""
    if f is FH:
        return FZ16 and not cv
    return FZ


def cls(b, f, cv=False):
    """(kind, sign, exact value), flushing subnormal inputs under FZ (FPUnpack)."""
    sign = (b >> (f.w - 1)) & 1
    ex, fr = (b >> f.f) & f.emaxf, b & ((1 << f.f) - 1)
    if flushes(f, cv) and ex == 0:
        fr = 0
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


def flush(b, f):
    """The operand as FPUnpack sees it: subnormals become signed zero under FZ."""
    if flushes(f) and (b >> f.f) & f.emaxf == 0:
        return b & f.sign
    return b


def qnan(x, f):
    """FPProcessNaN: quieten, or the default NaN under DN."""
    return f.dnan if DN else x | f.quiet


def inf(f, sign):
    return (sign << (f.w - 1)) | (f.emaxf << f.f)


def enc(v, f, mode="ne", zsign=0, cv=False):
    """Round the exact value v to format f (FPRound)."""
    if v == 0:
        return zsign << (f.w - 1)
    sign = 1 if v < 0 else 0
    m = abs(v)
    e = m.numerator.bit_length() - m.denominator.bit_length()
    if Fr(2) ** e > m:
        e -= 1
    emin = 1 - f.bias
    if flushes(f, cv) and e < emin:
        # Flush-to-zero judges the exact value, before rounding.
        return sign << (f.w - 1)
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


def nans(f, *ops):
    kinds = [cls(b, f)[0] for b in ops]
    for want in ("snan", "qnan"):
        for k, b in zip(kinds, ops):
            if k == want:
                return qnan(b, f)
    return None


def zero_sign(mode):
    """The sign of an exact zero sum: negative only when rounding to -inf."""
    return 1 if mode == "m" else 0


def f_add(a, b, f, mode="ne"):
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
    if ka == "zero" and kb == "zero" and sa == sb:
        return enc(0, f, zsign=sa)
    return enc(va + vb, f, mode, zsign=zero_sign(mode))


def f_neg(x, f):
    return x ^ f.sign


def f_sub(a, b, f, mode="ne"):
    if nans(f, a, b) is not None:
        return nans(f, a, b)
    return f_add(a, f_neg(b, f), f, mode)


def f_mul(a, b, f, mode="ne"):
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
    return enc(va * vb, f, mode)


def f_mulx(a, b, f, mode="ne"):
    n = nans(f, a, b)
    if n is not None:
        return n
    (ka, sa, _), (kb, sb, _) = cls(a, f), cls(b, f)
    if (ka, kb) in (("inf", "zero"), ("zero", "inf")):
        return ((sa ^ sb) << (f.w - 1)) | enc(Fr(2), f)
    return f_mul(a, b, f, mode)


def f_div(a, b, f, mode="ne"):
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
    return enc(va / vb, f, mode)


def f_fma(acc, a, b, f, mode="ne"):
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
    if kc == "zero" and pzero and sc == sp:
        return enc(0, f, zsign=sc)
    return enc(vc + va * vb, f, mode, zsign=zero_sign(mode))


def exact_sqrt(v):
    """sqrt(v) exactly when it is a dyadic rational, otherwise a value strictly
    between the same neighbours 2^-60 apart, which rounds the same way."""
    k = 60
    t = v * Fr(4) ** k
    s = math.isqrt(t.numerator // t.denominator)
    if Fr(s * s) == t:
        return Fr(s, 2**k)
    return Fr(2 * s + 1, 2 ** (k + 1))


def f_sqrt(a, f, mode="ne"):
    k, s, v = cls(a, f)
    if k in ("snan", "qnan"):
        return qnan(a, f)
    if k == "zero":
        return s << (f.w - 1)
    if s:
        return f.dnan
    if k == "inf":
        return a
    if f is FH:
        return enc(exact_sqrt(v), f, mode)
    # math.sqrt is correctly rounded for doubles; for singles the double
    # result rounds correctly again (53 >= 2 * 24 + 2).
    r = math.sqrt(float(v))
    return f64(r) if f is FD else f32(r)


def f_maxmin(a, b, f, is_max, num):
    a, b = flush(a, f), flush(b, f)
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


def f_recip_step(a, b, f, rsqrt, mode="ne"):
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
        return enc((3 + va * vb) / 2, f, mode, zsign=zero_sign(mode))
    return enc(2 + va * vb, f, mode, zsign=zero_sign(mode))


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
        return qnan(x, f)
    if k in ("zero", "inf"):
        return flush(x, f)
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
    """FPConvert (FPUnpackCV, FPRoundCV), with FPCR.AHP clear."""
    k, s, v = cls(x, src, cv=True)
    if k in ("snan", "qnan") and DN:
        return dst.dnan
    if k in ("snan", "qnan"):
        payload = x & (src.quiet - 1)
        payload = payload << (dst.f - src.f) if dst.f > src.f else payload >> (src.f - dst.f)
        return inf(dst, s) | dst.quiet | payload
    if k == "inf":
        return inf(dst, s)
    if k == "zero":
        return s << (dst.w - 1)
    return enc(v, dst, mode, cv=True)


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


def f_recpe(x, f, mode="ne"):
    k, s, v = cls(x, f)
    if k in ("snan", "qnan"):
        return qnan(x, f)
    if k == "inf":
        return s << (f.w - 1)
    if k == "zero":
        return inf(f, s)
    if abs(v) < Fr(2) ** -(f.bias + 1):
        # The reciprocal overflows: infinity or the largest normal number,
        # as the rounding mode says.
        to_inf = {"ne": True, "a": True, "z": False, "p": not s, "m": bool(s)}[mode]
        return inf(f, s) if to_inf else (s << (f.w - 1)) | ((f.emaxf - 1) << f.f) | ((1 << f.f) - 1)
    if flushes(f) and abs(v) >= Fr(2) ** (f.bias - 1):
        return s << (f.w - 1)
    frac = (x & ((1 << f.f) - 1)) << (52 - f.f)
    exp = (x >> f.f) & f.emaxf
    if exp == 0:
        if not (frac >> 51) & 1:
            exp = -1
            frac = (frac << 2) & M(52)
        else:
            frac = (frac << 1) & M(52)
    scaled = 256 | (frac >> 44)
    rexp = 2 * f.bias - 1 - exp
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
        return qnan(x, f)
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
    rexp = (3 * f.bias - 1 - exp) // 2
    est = rsqrt_estimate_int(scaled) & 0xFF
    return (rexp << f.f) | (est << (f.f - 8))


def f_recpx(x, f):
    k = cls(x, f)[0]
    if k in ("snan", "qnan"):
        return qnan(x, f)
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
