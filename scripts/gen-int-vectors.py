#!/usr/bin/env python3
"""Generate the general-purpose (integer) instruction test vectors.

Writes crates/kestrel-cpu/src/interp/int_vectors.rs. Each case is one
instruction, assembled by clang (--target=aarch64) and disassembled by
llvm-objdump to get its encoding. The expected registers, flags, program
counter, exclusive monitor and memory come from the models in this file,
written from the instruction descriptions and pseudocode of the Arm
Architecture Reference Manual (DDI 0487): AddWithCarry, ConditionHolds,
ShiftReg, ExtendReg, the bitfield and load/store operations. The models read
the assembly text, not the encoding. Nothing here runs kestrel.

    python3 scripts/gen-int-vectors.py            # regenerate
    python3 scripts/gen-int-vectors.py --check    # fail if out of date

Needs clang and llvm-objdump (override with $CLANG and $OBJDUMP).
"""

import os
import random
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "kestrel-cpu", "src", "interp", "int_vectors.rs")

# The test harness (interp/tests.rs) maps one code page at CODE and a
# read-write region at 0x20_0000. Each case runs at CODE + 0x80, with a
# known pattern around it in the code page for the literal loads.
CODE = 0x1_0000
PC = CODE + 0x80
CODE_LEN = 0x100
CODE0 = bytes((k * 29 + 3) & 0xFF for k in range(CODE_LEN))
MEM_BASE = 0x20_0100
MEM_LEN = 0x300
MEM0 = bytes((k * 37 + 11) & 0xFF for k in range(MEM_LEN))


def M(n):
    return (1 << n) - 1


def sx(x, n):
    x &= M(n)
    return x - (1 << n) if x >> (n - 1) else x


def le(b):
    return int.from_bytes(b, "little")


# ---- initial state -----------------------------------------------------------

rng = random.Random(20261030)
X0 = [rng.getrandbits(64) for _ in range(31)]
X0[0:16] = [
    0x0123_4567_89AB_CDEF,
    0xFEDC_BA98_7654_3210,
    0x8000_0000_0000_0000,
    0x7FFF_FFFF_FFFF_FFFF,
    0xFFFF_FFFF_FFFF_FFFF,
    0x0000_0000_8000_0000,
    0x0000_0000_7FFF_FFFF,
    0x0000_0000_FFFF_FFFF,
    5,
    (-5) & M(64),
    0x1234_5678_9ABC_DEF0,
    0x0F0F_0F0F_F0F0_F0F0,
    0x43,  # shift counts: 67 is 3 modulo 32 and 64
    0,
    0x00FF_00FF_8001_7FFE,
    0xDEAD_BEEF_CAFE_F00D,
]
X0[16] = MEM_BASE + 0x140  # pointers into the memory window
X0[17] = 8
X0[18] = 0x0000_0001_FFFF_FFF8  # W18 is -8 (only used sign-extended)
X0[19] = (-16) & M(64)
X0[20] = MEM_BASE + 0x180
X0[21] = 3
X0[22] = 0x03C0_0000  # a valid FPCR: DN, FZ, RMode = RZ
X0[23] = 0x0800_009F  # a valid FPSR: QC, IDC and the five cumulative flags
X0[24] = MEM_BASE + 0x19C  # not 64-byte aligned, for DC ZVA
X0[25] = le(MEM0[0x140:0x148])  # what X16 points at, for a CAS that matches
SP0 = MEM_BASE + 0x180
V0 = [rng.getrandbits(128) for _ in range(32)]
FPCR0 = 0x0200_0000  # DN
FPSR0 = 0x0800_0000  # QC
TPIDR0 = 0x7777_0000_1234_5678
TPIDRRO0 = 0x6666_0000_8765_4321


class State:
    def __init__(self, nzcv, excl):
        self.x = list(X0)
        self.sp = SP0
        self.v = list(V0)
        self.nzcv = nzcv
        self.excl = excl
        self.pc = PC + 4
        self.fpcr, self.fpsr, self.tpidr = FPCR0, FPSR0, TPIDR0
        self.code = bytearray(CODE0)
        self.mem = bytearray(MEM0)


ST = None  # the state of the case being modelled

# ---- registers and operands ----------------------------------------------------


def split_ops(rest):
    """Operands separated by commas outside brackets and braces."""
    ops, depth, cur = [], 0, ""
    for ch in rest:
        if ch in "[{":
            depth += 1
        elif ch in "]}":
            depth -= 1
        if ch == "," and depth == 0:
            ops.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        ops.append(cur.strip())
    return ops


def width(reg):
    return 32 if reg[0] == "w" else 64


def rd(reg):
    """Read a general register: xN, wN, xzr, wzr, sp, wsp."""
    if reg in ("xzr", "wzr"):
        return 0
    if reg in ("sp", "wsp"):
        v = ST.sp
    else:
        v = ST.x[int(reg[1:])]
    return v & M(width(reg))


def wr(reg, v):
    """Write a general register; a W register write zeroes the top half."""
    v &= M(width(reg))
    if reg in ("xzr", "wzr"):
        return
    if reg in ("sp", "wsp"):
        ST.sp = v
    else:
        ST.x[int(reg[1:])] = v


def imm(op):
    return int(op.lstrip("#"), 0)


def flags_of(n, z, c, v):
    ST.nzcv = (n << 3) | (z << 2) | (c << 1) | v


def add_with_carry(x, y, carry, n):
    """AddWithCarry() from the Arm ARM: result and NZCV."""
    usum = (x & M(n)) + (y & M(n)) + carry
    ssum = sx(x, n) + sx(y, n) + carry
    r = usum & M(n)
    return r, ((r >> (n - 1)) << 3) | ((r == 0) << 2) | ((r != usum) << 1) | (sx(r, n) != ssum)


def cond_holds(cond):
    """ConditionHolds() from the Arm ARM."""
    codes = ["eq", "ne", "hs", "lo", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "al", "nv"]
    alias = {"cs": "hs", "cc": "lo"}
    c = codes.index(alias.get(cond, cond))
    n, z, cy, v = (ST.nzcv >> 3) & 1, (ST.nzcv >> 2) & 1, (ST.nzcv >> 1) & 1, ST.nzcv & 1
    r = [z, cy, n, v, cy and not z, n == v, n == v and not z, True][c >> 1]
    return (not r) if (c & 1 and c != 15) else bool(r)


def shift_op(x, kind, amount, n):
    """ShiftReg(): LSL, LSR, ASR, ROR on an n-bit value."""
    x &= M(n)
    if kind == "lsl":
        return (x << amount) & M(n)
    if kind == "lsr":
        return x >> amount
    if kind == "asr":
        return (sx(x, n) >> amount) & M(n)
    if kind == "ror":
        return ((x >> amount) | (x << (n - amount))) & M(n) if amount else x
    raise ValueError(kind)


EXTEND = {"uxtb": (8, False), "uxth": (16, False), "uxtw": (32, False), "uxtx": (64, False),
          "sxtb": (8, True), "sxth": (16, True), "sxtw": (32, True), "sxtx": (64, True)}


def extend_op(x, kind, amount, n):
    """ExtendReg(): extend the low bits, then shift left, n bits wide."""
    bits, signed = EXTEND[kind]
    v = sx(x, bits) if signed else x & M(bits)
    return (v << amount) & M(n)


def operand2(ops, n):
    """The second source of ADD/SUB/logical: immediate, shifted or extended register."""
    src = ops[0]
    if src.startswith("#"):
        v = imm(src)
        if len(ops) > 1:
            v <<= imm(ops[1].split()[1])
        return v & M(n)
    v = rd(src)
    if len(ops) > 1:
        parts = ops[1].split()
        kind = parts[0]
        amount = imm(parts[1]) if len(parts) > 1 else 0
        if kind in EXTEND:
            return extend_op(v, kind, amount, n)
        return shift_op(v, kind, amount, n)
    return v & M(n)


# ---- data processing ------------------------------------------------------------


def m_addsub(mn, ops):
    if mn in ("cmp", "cmn", "tst"):
        ops = [("wzr" if ops[0][0] == "w" or ops[0] == "wsp" else "xzr")] + ops
        mn = {"cmp": "subs", "cmn": "adds", "tst": "ands"}[mn]
    if mn in ("neg", "negs"):
        ops = [ops[0], "wzr" if ops[0][0] == "w" else "xzr"] + ops[1:]
        mn = "sub" if mn == "neg" else "subs"
    d, a = ops[0], ops[1]
    n = width(d)
    x = rd(a)
    y = operand2(ops[2:], n)
    sub = mn.startswith("sub")
    r, nzcv = add_with_carry(x, (~y) & M(n) if sub else y, 1 if sub else 0, n)
    if mn.endswith("s"):
        ST.nzcv = nzcv
    wr(d, r)


def m_logic(mn, ops):
    if mn == "tst":
        ops = ["wzr" if ops[0][0] == "w" else "xzr"] + ops
        mn = "ands"
    if mn == "mvn":
        ops = [ops[0], "wzr" if ops[0][0] == "w" else "xzr"] + ops[1:]
        mn = "orn"
    d, a = ops[0], ops[1]
    n = width(d)
    x = rd(a)
    y = operand2(ops[2:], n)
    if mn in ("bic", "bics", "orn", "eon"):
        y = ~y & M(n)
    r = {"and": x & y, "ands": x & y, "bic": x & y, "bics": x & y, "orr": x | y, "orn": x | y,
         "eor": x ^ y, "eon": x ^ y}[mn]
    if mn in ("ands", "bics"):
        flags_of(r >> (n - 1), int(r == 0), 0, 0)
    wr(d, r)


def m_mov(mn, ops):
    d = ops[0]
    n = width(d)
    if ops[1].startswith("#"):
        wr(d, imm(ops[1]) & M(n))
    else:
        wr(d, rd(ops[1]))


def m_movwide(mn, ops):
    d = ops[0]
    n = width(d)
    sh = imm(ops[2].split()[1]) if len(ops) > 2 else 0
    v = imm(ops[1]) << sh
    if mn == "movz":
        wr(d, v)
    elif mn == "movn":
        wr(d, ~v & M(n))
    else:
        wr(d, (rd(d) & ~(0xFFFF << sh)) | v)


def m_adr(mn, ops):
    off = target_off(ops[1])
    if mn == "adr":
        wr(ops[0], PC + off)
    else:
        # The immediate is the distance between 4 KiB pages.
        wr(ops[0], (PC & ~0xFFF) + off)


def ins_field(dst, val, lsb, w):
    return (dst & ~(M(w) << lsb)) | ((val & M(w)) << lsb)


def m_bitfield(mn, ops):
    d = ops[0]
    n = width(d)
    if mn == "bfc":
        lsb, w = imm(ops[1]), imm(ops[2])
        wr(d, ins_field(rd(d), 0, lsb, w))
        return
    s = rd(ops[1])
    if mn in ("sxtb", "sxth", "sxtw"):
        wr(d, sx(s, {"b": 8, "h": 16, "w": 32}[mn[-1]]))
        return
    if mn in ("uxtb", "uxth"):
        wr(d, s & M(8 if mn[-1] == "b" else 16))
        return
    if mn in ("asr", "lsr", "lsl"):
        wr(d, shift_op(s, mn, imm(ops[2]), n))
        return
    if mn in ("sbfm", "ubfm", "bfm"):
        # The pseudocode's two cases: imms >= immr extracts bits immr..imms to
        # the bottom; otherwise the low imms+1 bits move up to bit n - immr.
        r, si = imm(ops[2]), imm(ops[3])
        if si >= r:
            mn, lsb, w = {"sbfm": "sbfx", "ubfm": "ubfx", "bfm": "bfxil"}[mn], r, si - r + 1
        else:
            mn, lsb, w = {"sbfm": "sbfiz", "ubfm": "ubfiz", "bfm": "bfi"}[mn], n - r, si + 1
    else:
        lsb, w = imm(ops[2]), imm(ops[3])
    if mn == "sbfx":
        wr(d, sx((s >> lsb) & M(w), w))
    elif mn == "ubfx":
        wr(d, (s >> lsb) & M(w))
    elif mn == "sbfiz":
        wr(d, sx(s & M(w), w) << lsb)
    elif mn == "ubfiz":
        wr(d, (s & M(w)) << lsb)
    elif mn == "bfi":
        wr(d, ins_field(rd(d), s, lsb, w))
    elif mn == "bfxil":
        wr(d, ins_field(rd(d), s >> lsb, 0, w))
    else:
        raise ValueError(mn)


def m_extr(mn, ops):
    d = ops[0]
    n = width(d)
    if mn == "ror":
        hi = lo = rd(ops[1])
        lsb = imm(ops[2])
    else:
        hi, lo, lsb = rd(ops[1]), rd(ops[2]), imm(ops[3])
    wr(d, ((hi << n | lo) >> lsb) & M(n))


def m_adc(mn, ops):
    if mn in ("ngc", "ngcs"):
        ops = [ops[0], "wzr" if ops[0][0] == "w" else "xzr", ops[1]]
        mn = "sbc" + mn[3:]
    d = ops[0]
    n = width(d)
    y = rd(ops[2])
    if mn.startswith("sbc"):
        y = ~y & M(n)
    r, nzcv = add_with_carry(rd(ops[1]), y, (ST.nzcv >> 1) & 1, n)
    if mn.endswith("s"):
        ST.nzcv = nzcv
    wr(d, r)


def m_ccmp(mn, ops):
    a = ops[0]
    n = width(a)
    if cond_holds(ops[3]):
        y = imm(ops[1]) if ops[1].startswith("#") else rd(ops[1])
        if mn == "ccmp":
            ST.nzcv = add_with_carry(rd(a), ~y & M(n), 1, n)[1]
        else:
            ST.nzcv = add_with_carry(rd(a), y, 0, n)[1]
    else:
        ST.nzcv = imm(ops[2])


INVERT = {"eq": "ne", "ne": "eq", "hs": "lo", "lo": "hs", "mi": "pl", "pl": "mi", "vs": "vc", "vc": "vs",
          "hi": "ls", "ls": "hi", "ge": "lt", "lt": "ge", "gt": "le", "le": "gt"}


def m_csel(mn, ops):
    d = ops[0]
    n = width(d)
    z = "wzr" if n == 32 else "xzr"
    # The aliases, as the Arm ARM defines them.
    if mn == "cset":
        mn, ops = "csinc", [d, z, z, INVERT[ops[1]]]
    elif mn == "csetm":
        mn, ops = "csinv", [d, z, z, INVERT[ops[1]]]
    elif mn in ("cinc", "cinv", "cneg"):
        mn, ops = "cs" + mn[1:], [d, ops[1], ops[1], INVERT[ops[2]]]
    a, b = rd(ops[1]), rd(ops[2])
    if cond_holds(ops[3]):
        r = a
    else:
        r = {"csel": b, "csinc": b + 1, "csinv": ~b, "csneg": -b}[mn]
    wr(d, r & M(n))


def m_div(mn, ops):
    d = ops[0]
    n = width(d)
    a, b = rd(ops[1]), rd(ops[2])
    if b == 0:
        r = 0
    elif mn == "udiv":
        r = a // b
    else:
        sa, sb = sx(a, n), sx(b, n)
        q = abs(sa) // abs(sb)  # rounds towards zero
        r = q if (sa < 0) == (sb < 0) else -q
    wr(d, r & M(n))


def m_shiftv(mn, ops):
    d = ops[0]
    n = width(d)
    wr(d, shift_op(rd(ops[1]), mn[:3], rd(ops[2]) % n, n))


def bytes_rev(x, group, n):
    """Reverse the bytes inside each group of `group` bits."""
    r = 0
    for base in range(0, n, group):
        part = (x >> base) & M(group)
        b = part.to_bytes(group // 8, "little")
        r |= int.from_bytes(b, "big") << base
    return r


def m_onesrc(mn, ops):
    d = ops[0]
    n = width(d)
    a = rd(ops[1])
    if mn == "rbit":
        r = int(format(a, f"0{n}b")[::-1], 2)
    elif mn == "rev16":
        r = bytes_rev(a, 16, n)
    elif mn == "rev32":
        r = bytes_rev(a, 32, n)
    elif mn in ("rev", "rev64"):
        r = bytes_rev(a, n, n)
    elif mn == "clz":
        r = n - a.bit_length()
    elif mn == "cls":
        sign = a >> (n - 1)
        r = 0
        for k in range(n - 2, -1, -1):
            if (a >> k) & 1 != sign:
                break
            r += 1
    else:
        raise ValueError(mn)
    wr(d, r)


def m_mul(mn, ops):
    d = ops[0]
    n = width(d)
    if mn in ("mul", "mneg"):
        mn, ops = ("madd" if mn == "mul" else "msub"), ops + ["xzr"]
    elif mn in ("smull", "umull", "smnegl", "umnegl"):
        mn, ops = {"smull": "smaddl", "umull": "umaddl", "smnegl": "smsubl", "umnegl": "umsubl"}[mn], ops + ["xzr"]
    if mn in ("smulh", "umulh"):
        a, b = rd(ops[1]), rd(ops[2])
        if mn == "smulh":
            a, b = sx(a, 64), sx(b, 64)
        wr(d, ((a * b) >> 64) & M(64))
        return
    acc = rd(ops[3])
    if mn in ("madd", "msub"):
        p = rd(ops[1]) * rd(ops[2])
    elif mn in ("smaddl", "smsubl"):
        p = sx(rd(ops[1]), 32) * sx(rd(ops[2]), 32)
    else:
        p = rd(ops[1]) * rd(ops[2])
    r = acc - p if mn in ("msub", "smsubl", "umsubl") else acc + p
    wr(d, r & M(n))


# ---- branches and system ------------------------------------------------------


def target_off(op):
    if op.startswith("#"):
        return imm(op)
    m = re.fullmatch(r"\.([+-])(0x[0-9a-f]+|\d+)", op)
    return int(m.group(2), 0) * (1 if m.group(1) == "+" else -1)


def m_branch(mn, ops):
    if mn in ("b", "bl"):
        if mn == "bl":
            ST.x[30] = PC + 4
        ST.pc = PC + target_off(ops[0])
        return
    if mn.startswith("b."):
        if cond_holds(mn[2:]):
            ST.pc = PC + target_off(ops[0])
        return
    if mn in ("cbz", "cbnz"):
        if (rd(ops[0]) == 0) == (mn == "cbz"):
            ST.pc = PC + target_off(ops[1])
        return
    if mn in ("tbz", "tbnz"):
        bit = (rd(ops[0]) >> imm(ops[1])) & 1
        if bit == (mn == "tbnz"):
            ST.pc = PC + target_off(ops[2])
        return
    if mn in ("br", "blr", "ret"):
        t = rd(ops[0]) if ops else ST.x[30]
        if mn == "blr":
            ST.x[30] = PC + 4
        ST.pc = t
        return
    raise ValueError(mn)


# kestrel's identification registers, as documented in interp/system.rs:
# 64-byte cache lines (log2 of 16 words) and DC ZVA on 64-byte blocks.
CTR = (1 << 31) | (4 << 24) | (4 << 20) | (4 << 16) | (0b11 << 14) | 4
DCZID = 4  # log2 of the block size in words, DC ZVA allowed (DZP = 0)
CNTFRQ = 19_200_000


def m_sys(mn, ops):
    if mn == "mrs":
        v = {"nzcv": ST.nzcv << 28, "fpcr": ST.fpcr, "fpsr": ST.fpsr, "tpidr_el0": ST.tpidr,
             "tpidrro_el0": TPIDRRO0, "ctr_el0": CTR, "dczid_el0": DCZID, "cntfrq_el0": CNTFRQ}[ops[1]]
        wr(ops[0], v)
    elif mn == "msr":
        v = rd(ops[1])
        if ops[0] == "nzcv":
            ST.nzcv = (v >> 28) & 0xF
        elif ops[0] == "fpcr":
            ST.fpcr = v
        elif ops[0] == "fpsr":
            ST.fpsr = v
        elif ops[0] == "tpidr_el0":
            ST.tpidr = v
        else:
            raise ValueError(ops[0])
    elif mn == "dc" and ops[0] == "zva":
        a = rd(ops[1]) & ~63
        mem_write(a, 64, 0)
    elif mn == "clrex":
        ST.excl = None
    elif mn in ("nop", "yield", "hint", "dmb", "dsb", "isb", "ic", "dc", "csdb", "esb", "psb"):
        pass  # hints and barriers execute as NOP; no caches are modelled
    else:
        raise ValueError(mn)


# ---- loads and stores -----------------------------------------------------------


def mem_read(a, n):
    if CODE <= a and a + n <= CODE + CODE_LEN:
        return le(ST.code[a - CODE:a - CODE + n])
    k = a - MEM_BASE
    assert 0 <= k and k + n <= MEM_LEN, hex(a)
    return le(ST.mem[k:k + n])


def mem_write(a, n, v):
    k = a - MEM_BASE
    assert 0 <= k and k + n <= MEM_LEN, hex(a)
    ST.mem[k:k + n] = (v & M(8 * n)).to_bytes(n, "little")


FPREG = {"b": 1, "h": 2, "s": 4, "d": 8, "q": 16}


def reg_size(reg):
    return FPREG[reg[0]] if reg[0] in FPREG else width(reg) // 8


def rd_any(reg):
    if reg[0] in FPREG:
        return ST.v[int(reg[1:])]
    return rd(reg)


def wr_any(reg, v):
    if reg[0] in FPREG:
        ST.v[int(reg[1:])] = v & M(8 * FPREG[reg[0]])  # the rest of the register is zeroed
    else:
        wr(reg, v)


def address(ops):
    """Address of a memory operand and the base register's write-back, if any."""
    mem = ops[0]
    if not mem.startswith("["):
        return PC + target_off(mem), None
    pre = mem.endswith("!")
    inner = [p.strip() for p in mem.strip("[]!").split(",")]
    base = rd(inner[0])
    off = 0
    if len(inner) > 1:
        if inner[1].startswith("#"):
            off = imm(inner[1])
        else:
            idx = rd(inner[1])
            if len(inner) > 2:
                parts = inner[2].split()
                kind = parts[0] if parts[0] != "lsl" else "uxtx"
                off = sx(extend_op(idx, kind, imm(parts[1]) if len(parts) > 1 else 0, 64), 64)
            else:
                off = idx
    if len(ops) > 1:  # post-index
        return base, (inner[0], (base + imm(ops[1])) & M(64))
    a = (base + off) & M(64)
    return a, ((inner[0], a) if pre else None)


def access(mn):
    """(bytes, signed) of a load/store mnemonic, or None for the register's size."""
    for suffix, size in (("sb", 1), ("sh", 2), ("sw", 4)):
        if mn.endswith(suffix) and mn.startswith("ld"):
            return size, True
    if mn.endswith("b"):
        return 1, False
    if mn.endswith("h"):
        return 2, False
    return None, False


def m_ldst(mn, ops):
    if mn.startswith("prf"):
        return
    rt = ops[0]
    size, signed = access(mn)
    size = size or reg_size(rt)
    a, wb = address(ops[1:])
    if mn.startswith("ld"):
        v = mem_read(a, size)
        if signed:
            v = sx(v, 8 * size)
        wr_any(rt, v)
    else:
        mem_write(a, size, rd_any(rt))
    if wb:
        wr(wb[0], wb[1])


def m_pair(mn, ops):
    r1, r2 = ops[0], ops[1]
    size = 4 if mn == "ldpsw" else reg_size(r1)
    mem = ops[2]
    inner = [p.strip() for p in mem.strip("[]!").split(",")]
    base = rd(inner[0])
    off = imm(inner[1]) if len(inner) > 1 else 0
    if len(ops) > 3:
        a, wb = base, base + imm(ops[3])
    else:
        a = base + off
        wb = a if mem.endswith("!") else None
    if mn.startswith("ld"):
        v1, v2 = mem_read(a, size), mem_read(a + size, size)
        if mn == "ldpsw":
            v1, v2 = sx(v1, 32), sx(v2, 32)
        wr_any(r1, v1)
        wr_any(r2, v2)
    else:
        mem_write(a, size, rd_any(r1))
        mem_write(a + size, size, rd_any(r2))
    if wb is not None:
        wr(inner[0], wb)


def m_excl(mn, ops):
    """Load/store exclusive, acquire/release and LORegion forms.

    One processing element: the local monitor records the address a
    load-exclusive marked; a store-exclusive succeeds only at that address,
    and leaves the monitor open whether or not it succeeded (Arm ARM,
    "The local monitor"). Plain acquire/release accesses leave it alone."""
    size, _ = access(mn)
    pair = mn in ("ldxp", "ldaxp", "stxp", "stlxp")
    store_ex = mn.startswith("st") and "x" in mn
    if store_ex:
        status, regs, mem = ops[0], ops[1:-1], ops[-1]
    else:
        status, regs, mem = None, ops[:-1], ops[-1]
    size = size or reg_size(regs[0])
    a = rd(mem.strip("[]").split(",")[0].strip())
    if mn.startswith("ld"):
        for k, r in enumerate(regs):
            wr(r, mem_read(a + k * size, size))
        if "x" in mn:
            ST.excl = a
    elif store_ex:
        ok = ST.excl == a
        if ok:
            for k, r in enumerate(regs):
                mem_write(a + k * size, size, rd(r))
        ST.excl = None
        wr(status, 0 if ok else 1)
    else:
        mem_write(a, size, rd(regs[0]))
    assert pair == (len(regs) == 2)


def m_cas(mn, ops):
    s, t, mem = ops
    size, _ = access(mn.rstrip("al") if mn not in ("casa", "casl", "casal") else mn)
    size = size or reg_size(s)
    a = rd(mem.strip("[]"))
    old = mem_read(a, size)
    if old == rd(s) & M(8 * size):
        mem_write(a, size, rd(t))
    wr(s, old)


ATOMIC = {
    "add": lambda o, v, n: o + v,
    "clr": lambda o, v, n: o & ~v,
    "eor": lambda o, v, n: o ^ v,
    "set": lambda o, v, n: o | v,
    "smax": lambda o, v, n: o if sx(o, n) >= sx(v, n) else v,
    "smin": lambda o, v, n: o if sx(o, n) <= sx(v, n) else v,
    "umax": lambda o, v, n: max(o, v),
    "umin": lambda o, v, n: min(o, v),
}


def split_atomic(mn):
    """ld<op>[a][l][b|h] / st<op>[l][b|h] / swp[a][l][b|h] -> (op, bytes, load)."""
    m = re.fullmatch(r"(ld|st)(add|clr|eor|set|smax|smin|umax|umin)(a?l?)([bh]?)", mn)
    if m:
        return m.group(2), {"b": 1, "h": 2, "": None}[m.group(4)], m.group(1) == "ld"
    m = re.fullmatch(r"swp(a?l?)([bh]?)", mn)
    return "swp", {"b": 1, "h": 2, "": None}[m.group(2)], True


def m_atomic(mn, ops):
    op, size, load = split_atomic(mn)
    if load:
        s, t, mem = ops
    else:
        s, mem = ops
        t = "xzr"
    size = size or reg_size(s)
    n = 8 * size
    a = rd(mem.strip("[]"))
    old = mem_read(a, size)
    v = rd(s) & M(n)
    new = v if op == "swp" else ATOMIC[op](old, v, n)
    mem_write(a, size, new)
    wr(t, old)


def m_fmov(mn, ops):
    d, s = ops
    if d.startswith("v"):  # fmov v0.d[1], xN: the top half only
        k = int(d[1:].split(".")[0])
        ST.v[k] = (ST.v[k] & M(64)) | (rd(s) << 64)
    elif s.startswith("v"):
        wr(d, ST.v[int(s[1:].split(".")[0])] >> 64)
    elif d[0] in "sd":
        wr_any(d, rd(s))
    else:
        wr(d, ST.v[int(s[1:])] & M(width(d)))


# ---- dispatch -------------------------------------------------------------------


def model(asm):
    mn, _, rest = asm.partition(" ")
    ops = split_ops(rest)
    if mn in ("add", "adds", "sub", "subs", "cmp", "cmn", "neg", "negs"):
        return m_addsub(mn, ops)
    if mn in ("and", "ands", "bic", "bics", "orr", "orn", "eor", "eon", "tst", "mvn"):
        return m_logic(mn, ops)
    if mn == "mov":
        return m_mov(mn, ops)
    if mn in ("movz", "movn", "movk"):
        return m_movwide(mn, ops)
    if mn in ("adr", "adrp"):
        return m_adr(mn, ops)
    if mn in ("sbfm", "ubfm", "bfm", "asr", "lsr", "lsl", "sbfx", "ubfx", "sbfiz", "ubfiz", "bfi", "bfxil",
              "bfc", "sxtb", "sxth", "sxtw", "uxtb", "uxth") and (len(ops) < 3 or ops[2].startswith("#")):
        return m_bitfield(mn, ops)
    if mn in ("asr", "lsr", "lsl", "ror") and not ops[2].startswith("#"):
        return m_shiftv(mn, ops)
    if mn in ("extr", "ror"):
        return m_extr(mn, ops)
    if mn in ("adc", "adcs", "sbc", "sbcs", "ngc", "ngcs"):
        return m_adc(mn, ops)
    if mn in ("ccmp", "ccmn"):
        return m_ccmp(mn, ops)
    if mn in ("csel", "csinc", "csinv", "csneg", "cset", "csetm", "cinc", "cinv", "cneg"):
        return m_csel(mn, ops)
    if mn in ("udiv", "sdiv"):
        return m_div(mn, ops)
    if mn in ("rbit", "rev16", "rev32", "rev", "rev64", "clz", "cls"):
        return m_onesrc(mn, ops)
    if mn in ("madd", "msub", "mul", "mneg", "smaddl", "smsubl", "umaddl", "umsubl", "smull", "umull",
              "smnegl", "umnegl", "smulh", "umulh"):
        return m_mul(mn, ops)
    if mn in ("b", "bl", "cbz", "cbnz", "tbz", "tbnz", "br", "blr", "ret") or mn.startswith("b."):
        return m_branch(mn, ops)
    if mn in ("mrs", "msr", "dc", "ic", "clrex", "nop", "yield", "hint", "dmb", "dsb", "isb", "csdb"):
        return m_sys(mn, ops)
    if mn == "fmov":
        return m_fmov(mn, ops)
    if re.fullmatch(r"cas[al]*[bh]?", mn):
        return m_cas(mn, ops)
    if re.fullmatch(r"(ld|st)(add|clr|eor|set|smax|smin|umax|umin)a?l?[bh]?|swpa?l?[bh]?", mn):
        return m_atomic(mn, ops)
    if re.fullmatch(r"(ld[a]?xr|st[l]?xr|ld[a]?xp|st[l]?xp|ldar|stlr|ldlar|stllr)[bh]?", mn):
        return m_excl(mn, ops)
    if mn in ("ldp", "stp", "ldnp", "stnp", "ldpsw"):
        return m_pair(mn, ops)
    if re.fullmatch(r"(ld|st)(r|ur|tr)(s?[bhw])?|prfu?m", mn):
        return m_ldst(mn, ops)
    raise ValueError(f"no model for {asm!r}")


# ---- the cases ------------------------------------------------------------------

N, Z, C, V = 8, 4, 2, 1


def c(asm, nzcv=0, excl=None):
    return (asm, nzcv, excl)


P = MEM_BASE + 0x140  # where X16 points

GROUPS = {
    "dp_imm": [
        "adr x0, .+0x1234", "adr x5, .-0x80", "adrp x0, #0x2000", "adrp x1, #-0x1000",
        "add x0, x1, #0x123", "add w0, w1, #0xfff", "add x0, sp, #16", "add sp, x16, #0x10",
        "add x0, x1, #1, lsl #12", "adds x0, x3, #1", "adds w0, w7, #1", "sub x0, x1, #5",
        "subs x0, x8, #5", "subs w0, w13, #1", "sub w0, wsp, #0x10, lsl #12", "cmp x8, #6",
        "cmn w7, #1", "mov x0, sp", "mov sp, x16",
        "and x0, x1, #0xff00ff00ff00ff00", "and w0, w15, #0xf0f0f0f0", "orr x0, xzr, #0x5555555555555555",
        "orr w0, w1, #0x1", "eor x0, x4, #0x7fffffffffffffff", "eor w0, w0, #0xffff0000",
        "ands x0, x2, #0x8000000000000000", "ands w0, w11, #0x0f0f0f0f", "tst x1, #0x3",
        "and sp, x16, #0xfffffffffffffff0", "mov w0, #0x00ff00ff",
        "movz x0, #0x1234, lsl #48", "movz w0, #0xffff, lsl #16", "movn x0, #0", "movn w0, #0x1234",
        "movn x0, #0xabcd, lsl #32", "movk x0, #0xbeef, lsl #16", "movk w1, #0x5555",
        "movk x15, #0, lsl #48",
        "asr x0, x2, #13", "asr w0, w5, #31", "lsr x0, x1, #60", "lsr w0, w14, #4", "lsl x0, x1, #63",
        "lsl w0, w1, #31", "sbfx x0, x1, #4, #12", "ubfx w0, w15, #8, #8", "sbfiz x0, x9, #3, #5",
        "ubfiz w0, w4, #24, #8", "bfi x0, x1, #32, #16", "bfxil w0, w15, #20, #12", "bfc x1, #4, #8",
        "sxtb x0, w1", "sxth w0, w15", "sxtw x0, w5", "uxtb w0, w15", "uxth w0, w1",
        "sbfm x0, x1, #60, #3", "ubfm w0, w15, #31, #30", "bfm x0, x15, #8, #23",
        "extr x0, x1, x2, #12", "extr w0, w1, w15, #31", "ror x0, x1, #8", "ror w0, w15, #16",
        "extr x0, x1, x2, #0",
    ],
    "dp_reg": [
        "and x0, x1, x2, lsl #3", "and w0, w15, w11, lsr #4", "bic x0, x4, x0, asr #8",
        "bic w0, w4, w14, ror #12", "orr x0, x1, x15, ror #63", "orr w0, wzr, w15", "mvn x0, x15",
        "orn w0, w11, w14, lsl #31", "eor x0, x0, x1, lsr #32", "eon x0, x1, x4", "eon w0, w15, w1, asr #3",
        "ands x0, x2, x4", "ands w0, w11, w14, lsl #4", "bics x0, x4, x4", "bics w0, w15, w1",
        c("tst x1, x2, lsr #1", Z | C | V),
        "add x0, x1, x2, lsr #4", "add w0, w15, w6, asr #2", "add x0, x1, x15, lsl #63",
        "adds x0, x3, x8", "adds w0, w7, w7", "sub x0, x13, x1, asr #63", "sub w0, w8, w9, lsl #1",
        "subs x0, x8, x8", "subs w0, w5, w6", "neg x0, x2", "negs w0, w5", "cmp x9, x8, lsl #1",
        "cmn x3, x4",
        "add x0, x1, w15, uxtb", "add x0, sp, x17, lsl #2", "add sp, sp, x17", "add w0, w1, w15, sxth #1",
        "add x0, x1, w9, sxtw #4", "sub x0, x1, w15, uxth #3", "sub sp, x20, w14, uxtw",
        "sub x0, x16, x19, sxtx #1", "adds x0, x3, w15, sxtb", "subs w0, w1, w2, uxtw", "cmp sp, x17",
        c("cmn w1, w15, sxtb", N | Z | C | V), "add x0, x1, x2, uxtx #4", "add w0, wsp, w1, uxtb #2",
        c("adc x0, x1, x2"), c("adc x0, x4, x13", C), c("adcs x0, x4, x13", C), c("adcs w0, w6, w13", C),
        c("adcs x0, x3, x3"), c("sbc x0, x8, x8"), c("sbc w0, w1, w15", C), c("sbcs x0, x8, x8", C),
        c("sbcs w0, w5, w13"), c("ngc x0, x15"), c("ngcs w0, w13", C),
        c("ccmp x8, x9, #0b0101, eq", Z), c("ccmp x8, x9, #0b0101, eq"), c("ccmp w8, #5, #0, ne"),
        c("ccmn x9, #5, #0b1111, ge"), c("ccmn x9, #5, #0b0011, ge", N), c("ccmn w5, w5, #2, al"),
        c("ccmp x3, #31, #0b1000, hs", N | C), c("ccmn x4, x17, #0, mi", N | V),
        c("ccmp w2, w13, #0b0110, lo", C),
        c("csel x0, x1, x2, eq", Z), c("csel x0, x1, x2, eq"), c("csel w0, w1, w15, lt", N),
        c("csinc x0, x1, x4, ne", Z), c("csinc w0, w1, w4, hi", C), c("csinv x0, x1, x13, gt", Z),
        c("csinv w0, w1, w15, le"), c("csneg x0, x1, x8, vs"), c("csneg w0, w1, w5, vc", V),
        c("cset x0, hs", C), c("csetm w0, ls", C), c("cinc x0, x4, pl"), c("cneg w0, w8, mi"),
        c("cinv x0, x1, ge", N),
        "udiv x0, x1, x8", "udiv w0, w1, w13", "sdiv x0, x2, x4", "sdiv w0, w9, w8", "sdiv x0, x9, x21",
        "sdiv x0, x1, x13", "sdiv w0, w5, w4",
        "lsl x0, x1, x12", "lsl w0, w1, w12", "lsr x0, x2, x12", "lsr w0, w5, w17", "asr x0, x2, x12",
        "asr w0, w5, w12", "ror x0, x1, x12", "ror w0, w15, w17",
        "rbit x0, x1", "rbit w0, w15", "rev16 x0, x1", "rev16 w0, w15", "rev32 x0, x1", "rev w0, w15",
        "rev x0, x1", "clz x0, x6", "clz w0, w13", "clz x0, x2", "cls x0, x9", "cls w0, w14",
        "cls x0, x13", "cls w0, w4", "cls x0, x3",
        "madd x0, x1, x2, x15", "madd w0, w1, w15, w9", "msub x0, x1, x9, x4", "msub w0, w5, w5, w13",
        "mul x0, x1, x15", "mneg w0, w1, w9", "smaddl x0, w9, w15, x1", "smsubl x0, w5, w5, x13",
        "smull x0, w9, w9", "umaddl x0, w9, w15, x1", "umsubl x0, w7, w7, x13", "umull x0, w4, w4",
        "smnegl x0, w9, w1", "umnegl x0, w9, w1", "smulh x0, x2, x2", "smulh x0, x9, x1",
        "umulh x0, x4, x4", "umulh x0, x1, x15",
    ],
    "branch": [
        "b .+0x100", "b .-0x80", "bl .+0x40", "bl .-4",
        "cbz x13, .+8", "cbz x8, .+8", "cbz w2, .+12", "cbnz w5, .-16", "cbnz x13, .+8",
        "tbz x2, #63, .+16", "tbz w8, #1, .+16", "tbnz x5, #31, .+20", "tbnz x1, #40, .+8",
        "tbnz x2, #63, .-0x40",
        c("b.eq .+8", Z), c("b.eq .+8"), c("b.ne .+8"), c("b.ne .+8", Z), c("b.hs .+8", C), c("b.hs .+8"),
        c("b.lo .+8"), c("b.lo .+8", C), c("b.mi .+8", N), c("b.mi .+8"), c("b.pl .+8"), c("b.pl .+8", N),
        c("b.vs .+8", V), c("b.vs .+8"), c("b.vc .+8"), c("b.vc .+8", V), c("b.hi .+8", C),
        c("b.hi .+8", C | Z), c("b.ls .+8", C | Z), c("b.ls .+8", C), c("b.ge .+8", N | V),
        c("b.ge .+8", N), c("b.lt .+8", V), c("b.lt .+8"), c("b.gt .+8"), c("b.gt .+8", Z),
        c("b.le .+8", Z), c("b.le .+8"), c("b.al .-0x100"), c("b.nv .+8"),
        "br x16", "blr x20", "ret", "ret x0",
    ],
    "system": [
        c("mrs x0, nzcv", N | C), "msr nzcv, x7", c("msr nzcv, x2", N | Z | C | V), "mrs x0, fpcr", "msr fpcr, x22",
        "mrs x0, fpsr", "msr fpsr, x23", "msr tpidr_el0, x15", "mrs x0, tpidr_el0",
        "mrs x0, tpidrro_el0", "mrs x0, ctr_el0", "mrs x0, dczid_el0", "mrs x0, cntfrq_el0",
        "dc zva, x16", "dc zva, x24", "nop", "yield", "hint #34", "csdb", c("clrex", 0, P),
        c("dmb ish", 0, P), "dsb sy", "isb", "ic ivau, x16", "dc civac, x16", "dc cvau, x16",
        "dc cvac, x16", "dc cvap, x16",
    ],
    "ldst_single": [
        "ldr x0, [x16]", "ldr x0, [x16, #8]", "ldr w0, [x16, #4]", "ldrh w0, [x16, #2]",
        "ldrb w0, [x16, #63]", "ldrsb x0, [x16, #2]", "ldrsb w0, [x16, #3]", "ldrsh x0, [x16, #2]",
        "ldrsh w0, [x16, #6]", "ldrsw x0, [x16, #0]", "ldrsw x0, [x16, #4]", "str x15, [x16, #16]",
        "str w15, [sp, #4]", "strh w15, [x16, #6]", "strb w15, [x16, #1]", "prfm pldl1keep, [x16, #8]",
        "ldr x0, [sp, #32]", "str xzr, [x16]", "ldr x0, [x20, #0x78]",
        "ldur x0, [x16, #-3]", "ldur w0, [x16, #1]", "ldurh w0, [x16, #-1]", "ldursb x0, [x16, #-1]",
        "ldursh w0, [x16, #5]", "ldursw x0, [x16, #-5]", "stur x15, [x16, #-7]", "sturb w15, [x16, #-1]",
        "sturh w15, [x16, #3]", "stur w15, [x16, #255]", "ldurb w0, [x16, #-256]", "prfum pldl1keep, [x16, #-1]",
        "ldtr x0, [x16, #8]", "sttrb w15, [x16, #2]", "ldtrsh x0, [x16, #2]",
        "ldr x0, [x16, #8]!", "ldr w0, [x16], #-4", "str x15, [x16, #-16]!", "strb w15, [x16], #1",
        "ldrsh x0, [x16, #2]!", "ldr x0, [sp], #16", "str x1, [sp, #-16]!", "ldrsw x0, [x16], #4",
        "ldrb w0, [x16, #-1]!", "strh w15, [x16], #-2",
        "ldr x0, [x16, x17]", "ldr x0, [x16, x17, lsl #3]", "ldr w0, [x20, w18, sxtw]",
        "ldr w0, [x20, w18, sxtw #2]", "ldrh w0, [x16, x17, lsl #1]", "ldrb w0, [x16, x17]",
        "ldrb w0, [x16, x17, lsl #0]", "ldrsb x0, [x16, w17, uxtw]", "ldrsw x0, [x20, x19, sxtx #2]",
        "str x15, [x16, x17, lsl #3]", "strh w15, [x20, x19]", "prfm pldl1keep, [x16, x17]",
        "ldrsh w0, [x20, x19, sxtx]", "ldr x0, [sp, x17]",
        "ldr x0, .+8", "ldr w0, .-16", "ldrsw x0, .+4", "ldrsw x0, .+20", "ldr s0, .+12", "ldr d0, .-8",
        "ldr q0, .+16", "prfm pldl1keep, .+8",
    ],
    "ldst_simd": [
        "ldr b0, [x16, #1]", "ldr h0, [x16, #2]", "ldr s0, [x16, #4]", "ldr d0, [x16, #8]",
        "ldr q0, [x16, #16]", "str b1, [x16, #3]", "str h1, [x16, #2]", "str s1, [x16, #4]",
        "str d1, [x16, #8]", "str q1, [x16, #32]", "ldur q0, [x16, #-1]", "stur d1, [x16, #-5]",
        "ldur h0, [x16, #3]", "stur b1, [x16, #-9]", "ldr q0, [x16, #16]!", "str q1, [x16], #-16",
        "ldr s0, [x16], #4", "str h1, [x16, #-2]!", "ldr d0, [x16, x17, lsl #3]",
        "ldr q0, [x16, x17, lsl #4]", "str s1, [x20, w18, sxtw #2]", "ldr b0, [x16, x17]",
        "str q1, [x20, x19]", "ldr h0, [x16, w17, uxtw #1]",
        "fmov w0, s1", "fmov s0, w15", "fmov x0, d1", "fmov d0, x15", "fmov x0, v1.d[1]",
        "fmov v0.d[1], x15",
    ],
    "ldst_pair": [
        "ldp x0, x1, [x16]", "ldp w0, w1, [x16, #-8]", "ldp x0, x1, [x16, #16]!", "ldp x0, x1, [x16], #-16",
        "ldpsw x0, x1, [x16, #8]", "ldpsw x0, x1, [x20], #8", "stp x15, x1, [x16, #-16]!",
        "stp w15, w14, [x16], #8", "stp x0, x1, [sp, #-16]!", "ldp x29, x30, [sp], #16",
        "ldnp x0, x1, [x16, #8]", "stnp w15, w1, [x16]", "ldp s0, s1, [x16]", "ldp d0, d1, [x16, #8]!",
        "ldp q0, q1, [x16, #-32]", "stp q1, q2, [x16], #32", "stp d1, d2, [x16, #-16]",
        "stp s1, s2, [x16, #4]!", "ldnp q0, q1, [x16]", "stnp d1, d2, [x16, #8]", "ldnp s0, s1, [x16, #-4]",
        "stp xzr, x15, [x16, #8]",
    ],
    "ldst_excl": [
        "ldxr x0, [x16]", "ldxrb w0, [x16]", "ldxrh w0, [x16]", "ldxr w0, [x16]", "ldxr x0, [sp]",
        c("stxr w0, x15, [x16]", 0, P), "stxr w0, x15, [x16]", c("stxr w0, x15, [x16]", 0, P + 8),
        c("stxrb w0, w15, [x16]", 0, P), c("stxrh w0, w15, [x16]", 0, P), c("stxr w0, w15, [x20]", 0, SP0),
        c("stxrh w1, w15, [x16]", 0, SP0),
        "ldaxr x0, [x16]", "ldaxrb w0, [x16]", c("stlxr w0, x15, [x16]", 0, P), c("stlxrb w0, w15, [x16]"),
        "ldxp x0, x1, [x16]", "ldxp w0, w1, [x16]", c("stxp w0, x15, x14, [x16]", 0, P),
        "stxp w0, w15, w14, [x16]", "ldaxp x0, x1, [x16]", c("stlxp w2, w15, w14, [x16]", 0, P),
        "ldar x0, [x16]", "ldarb w0, [x16]", "ldarh w0, [x16]", "ldar w0, [x16]", c("ldar x0, [x16]", 0, SP0),
        "stlr x15, [x16]", "stlrb w15, [x16]", "stlrh w15, [x16]", c("stlr w15, [x16]", 0, P),
        "ldlar x0, [x16]", "stllrh w15, [x16]",
        "cas x8, x15, [x16]", "cas x25, x15, [x16]", "casa w25, w15, [x16]", "casl w8, w15, [x16]",
        "casal x25, x15, [x16]", "casb w25, w15, [x16]", "cash w25, w15, [x16]", "casab w8, w15, [x16]",
        "caslh w8, w15, [x16]", "casalb w25, w15, [x16]",
    ],
    "ldst_atomic": [
        "ldadd x15, x0, [x16]", "ldaddb w15, w0, [x16]", "ldaddh w15, w0, [x16]", "ldadd w15, w0, [x16]",
        "ldadda x4, x0, [x16]", "ldaddl w4, w0, [x20]", "ldaddal x15, x0, [x16]",
        "ldclr x15, x0, [x16]", "ldclrb w4, w0, [x20]", "ldclral w15, w0, [x16]",
        "ldeor x15, x0, [x16]", "ldeorh w15, w0, [x16]", "ldeoral x15, x0, [x16]",
        "ldset x15, x0, [x16]", "ldseth w15, w0, [x20]", "ldseta w15, w0, [x16]",
        "ldsmax x9, x0, [x16]", "ldsmax x2, x0, [x16]", "ldsmaxb w9, w0, [x20]", "ldsmaxh w15, w0, [x16]",
        "ldsmax w9, w0, [x16]", "ldsmaxal w6, w0, [x16]",
        "ldsmin x9, x0, [x16]", "ldsminb w8, w0, [x20]", "ldsminh w15, w0, [x16]", "ldsmin w5, w0, [x16]",
        "ldsminl x3, x0, [x16]",
        "ldumax x9, x0, [x16]", "ldumaxb w8, w0, [x20]", "ldumaxh w15, w0, [x16]", "ldumax w13, w0, [x16]",
        "ldumaxa x13, x0, [x16]",
        "ldumin x9, x0, [x16]", "lduminb w8, w0, [x20]", "lduminh w15, w0, [x16]", "ldumin w4, w0, [x16]",
        "lduminal x4, x0, [x16]",
        "swp x15, x0, [x16]", "swpb w15, w0, [x16]", "swph w15, w0, [x20]", "swp w15, w0, [x16]",
        "swpal x15, x0, [x16]", "swpa w15, w0, [x16]",
        "stadd x15, [x16]", "staddlb w15, [x16]", "stclrh w15, [x16]", "steor w15, [x16]",
        "stset x15, [x16]", "stsmax w9, [x16]", "stsminl x9, [x16]", "stumaxb w4, [x20]", "stumin x13, [x16]",
    ],
}

CASES = [
    (g, *(item if isinstance(item, tuple) else (item, 0, None)))
    for g, items in GROUPS.items()
    for item in items
]  # (group, asm, nzcv, excl)

# ---- assembly and output -------------------------------------------------------


def assemble(lines):
    with tempfile.TemporaryDirectory() as tmp:
        src, obj = os.path.join(tmp, "v.s"), os.path.join(tmp, "v.o")
        with open(src, "w") as fh:
            fh.write("".join(line + "\n" for line in lines))
        subprocess.run([os.environ.get("CLANG", "clang"), "--target=aarch64-none-elf", "-march=armv8.2-a", "-c",
                        src, "-o", obj], check=True)
        dis = subprocess.run([os.environ.get("OBJDUMP", "llvm-objdump"), "-dr", obj], check=True,
                             capture_output=True, text=True).stdout
    if "R_AARCH64" in dis:
        sys.exit("an encoding was left to a relocation: write the offset as an immediate")
    words = re.findall(r"^\s*[0-9a-f]+:\s+([0-9a-f]{8})\s", dis, re.M)
    if len(words) != len(lines):
        sys.exit(f"assembled {len(words)} words for {len(lines)} lines")
    return [int(w, 16) for w in words]


def opt(v):
    return "None" if v is None else f"Some({v:#x})"


def main():
    global ST
    words = assemble([asm for _, asm, _, _ in CASES])
    out = []
    w = out.append
    w("// Generated by scripts/gen-int-vectors.py. Do not edit.")
    w("//")
    w("// Encodings: clang --target=aarch64 and llvm-objdump. Expected values: the")
    w("// Python models in the script, written from the Arm ARM.")
    w("")
    w("use super::int_tests::Case;")
    w("")
    w(f"pub const PC: u64 = {PC:#x};")
    w("pub const CODE: [u8; %d] = [%s];" % (CODE_LEN, ", ".join(str(b) for b in CODE0)))
    w(f"pub const MEM_BASE: u64 = {MEM_BASE:#x};")
    w("pub const MEM: [u8; %d] = [%s];" % (MEM_LEN, ", ".join(str(b) for b in MEM0)))
    w(f"pub const SP: u64 = {SP0:#x};")
    w(f"pub const FPCR: u64 = {FPCR0:#x};")
    w(f"pub const FPSR: u64 = {FPSR0:#x};")
    w(f"pub const TPIDR: u64 = {TPIDR0:#x};")
    w(f"pub const TPIDRRO: u64 = {TPIDRRO0:#x};")
    w("pub const X: [u64; 31] = [")
    for x in X0:
        w(f"    {x:#018x},")
    w("];")
    w("pub const V: [u128; 32] = [")
    for x in V0:
        w(f"    {x:#034x},")
    w("];")
    w("")
    w("pub const CASES: &[Case] = &[")
    for (group, asm, nzcv, excl), insn in zip(CASES, words):
        ST = State(nzcv, excl)
        model(asm)
        xs = [(k, x) for k, x in enumerate(ST.x) if x != X0[k]]
        if ST.sp != SP0:
            xs.append((31, ST.sp))
        v = [(k, x) for k, x in enumerate(ST.v) if x != V0[k]]
        sysr = [(k, a) for k, (a, b) in enumerate([(ST.fpcr, FPCR0), (ST.fpsr, FPSR0), (ST.tpidr, TPIDR0)])
                if a != b]
        mem = [(k, b) for k, b in enumerate(ST.mem) if b != MEM0[k]]
        w("    Case {")
        w(f"        group: {group!r},".replace("'", '"'))
        w(f"        asm: {asm!r},".replace("'", '"'))
        w(f"        insn: {insn:#010x},")
        w(f"        nzcv_in: {nzcv:#06b},")
        w(f"        excl_in: {opt(excl)},")
        w("        x: &[%s]," % ", ".join(f"({k}, {x:#x})" for k, x in xs))
        w("        v: &[%s]," % ", ".join(f"({k}, {x:#x})" for k, x in v))
        w(f"        nzcv: {ST.nzcv:#06b},")
        w(f"        pc: {ST.pc & M(64):#x},")
        w(f"        excl: {opt(ST.excl)},")
        w("        sys: &[%s]," % ", ".join(f"({k}, {x:#x})" for k, x in sysr))
        w("        mem: &[%s]," % ", ".join(f"({k}, {b:#x})" for k, b in mem))
        w("    },")
    w("];")
    text = "\n".join(out) + "\n"
    if "--check" in sys.argv:
        with open(OUT) as fh:
            if fh.read() != text:
                sys.exit("int_vectors.rs is out of date: run scripts/gen-int-vectors.py")
        print(f"int_vectors.rs up to date ({len(CASES)} cases)")
        return
    with open(OUT, "w", newline="\n") as fh:
        fh.write(text)
    print(f"wrote {len(CASES)} cases to {os.path.relpath(OUT, ROOT)}")


if __name__ == "__main__":
    main()
