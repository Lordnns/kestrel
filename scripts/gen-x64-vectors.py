#!/usr/bin/env python3
"""Generate the x86_64 encoder's test vectors.

Writes crates/kestrel-cpu/src/jit/x64/asm/vectors.rs. Each case is a call
of kestrel's x86_64 encoder (jit/x64/asm.rs) paired with the instruction
it must produce, written in Intel syntax; the expected bytes come from
llvm-mc assembling that text, never from kestrel (the owner's rule of
2026-10-31: every encoding is checked against llvm-mc). Each case is put
in its own section and the bytes are read back with llvm-objdump.

    python3 scripts/gen-x64-vectors.py            # regenerate
    python3 scripts/gen-x64-vectors.py --check    # fail if out of date

Needs llvm-mc and llvm-objdump (override with $LLVM_MC and $OBJDUMP).
"""

import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "kestrel-cpu", "src", "jit", "x64", "asm", "vectors.rs")

R64 = "rax rcx rdx rbx rsp rbp rsi rdi r8 r9 r10 r11 r12 r13 r14 r15".split()
R32 = "eax ecx edx ebx esp ebp esi edi r8d r9d r10d r11d r12d r13d r14d r15d".split()
R16 = "ax cx dx bx sp bp si di r8w r9w r10w r11w r12w r13w r14w r15w".split()
R8 = "al cl dl bl spl bpl sil dil r8b r9b r10b r11b r12b r13b r14b r15b".split()
RUST = [r.upper() for r in R64]
CCS = "o no b ae e ne be a s ns p np l ge le g".split()

# A subset of registers that covers every encoding case: the low eight
# and the REX ones, RSP/R12 (SIB) and RBP/R13 (displacement).
SOME = [0, 1, 3, 4, 5, 6, 8, 12, 13, 15]
BASES = [0, 3, 4, 5, 8, 12, 13, 15]
DISPS = [0, 8, -8, 127, -128, 128, -129, 0x1234, -0x10000]

CASES = []  # (rust statement(s) using `a`, assembly text)


def case(rust, asm):
    CASES.append((rust, asm))


def w(width):
    return "W64" if width == 64 else "W32"


def reg(n, width):
    return (R64 if width == 64 else R32)[n]


def mem(size, b, d):
    sign = "+" if d >= 0 else "-"
    ptr = f"{size} ptr " if size else ""
    return f"{ptr}[{R64[b]} {sign} {abs(d)}]"


def m(b, d):
    return f"M({RUST[b]}, {d})"


def gen():
    for width in (32, 64):
        size = "qword" if width == 64 else "dword"
        for d in SOME:
            for s in SOME:
                case(f"a.mov_rr({w(width)}, {RUST[d]}, {RUST[s]})", f"mov {reg(d, width)}, {reg(s, width)}")
                case(f"a.test_rr({w(width)}, {RUST[d]}, {RUST[s]})", f"test {reg(d, width)}, {reg(s, width)}")
                case(f"a.imul_rr({w(width)}, {RUST[d]}, {RUST[s]})", f"imul {reg(d, width)}, {reg(s, width)}")
        for b in BASES:
            for disp in DISPS:
                for r in (0, 5, 9):
                    case(f"a.load({w(width)}, {RUST[r]}, {m(b, disp)})", f"mov {reg(r, width)}, {mem(size, b, disp)}")
                    case(f"a.store({w(width)}, {m(b, disp)}, {RUST[r]})", f"mov {mem(size, b, disp)}, {reg(r, width)}")
        for op in "add or adc sbb and sub xor cmp".split():
            alu = op.capitalize()
            for d in SOME:
                for s in (0, 2, 4, 9, 13):
                    case(f"a.alu_rr(Alu::{alu}, {w(width)}, {RUST[d]}, {RUST[s]})", f"{op} {reg(d, width)}, {reg(s, width)}")
                for imm in (0, 1, -1, 127, -128, 128, -129, 0x12345678, -0x80000000):
                    case(f"a.alu_ri(Alu::{alu}, {w(width)}, {RUST[d]}, {imm})", f"{op} {reg(d, width)}, {imm}")
            for b in (3, 4, 5, 13):
                case(f"a.alu_rm(Alu::{alu}, {w(width)}, RCX, {m(b, 16)})", f"{op} {reg(1, width)}, {mem(size, b, 16)}")
                case(f"a.alu_rm(Alu::{alu}, {w(width)}, R10, {m(b, 0)})", f"{op} {reg(10, width)}, {mem(size, b, 0)}")
        for op in "rol ror shl shr sar".split():
            sh = op.capitalize()
            for d in SOME:
                for imm in (1, 2, 31) + ((63,) if width == 64 else ()):
                    case(f"a.shift_ri(Shift::{sh}, {w(width)}, {RUST[d]}, {imm})", f"{op} {reg(d, width)}, {imm}")
                case(f"a.shift_cl(Shift::{sh}, {w(width)}, {RUST[d]})", f"{op} {reg(d, width)}, cl")
        for op in "not neg mul imul div idiv".split():
            for r in SOME:
                case(f"a.unary(Unary::{op.capitalize()}, {w(width)}, {RUST[r]})", f"{op} {reg(r, width)}")
        for cc in range(16):
            for d, s in ((0, 1), (8, 15), (5, 12)):
                case(f"a.cmov(Cc::{CCS[cc].capitalize()}, {w(width)}, {RUST[d]}, {RUST[s]})",
                     f"cmov{CCS[cc]} {reg(d, width)}, {reg(s, width)}")
        for r in SOME:
            case(f"a.bt_ri({w(width)}, {RUST[r]}, 5)", f"bt {reg(r, width)}, 5")
            case(f"a.bswap({w(width)}, {RUST[r]})", f"bswap {reg(r, width)}")
    case("a.cqo(W64)", "cqo")
    case("a.cqo(W32)", "cdq")
    for imm in (0, 1, 0x7FFFFFFF, 0x80000000, 0xFFFFFFFF, 0x100000000, -1, -0x80000000, -0x80000001,
                0x123456789ABCDEF0):
        for r in (0, 5, 8, 15):
            u = imm & (2**64 - 1)
            if u <= 0xFFFFFFFF:
                text = f"mov {R32[r]}, {u}"
            elif -2**31 <= imm < 2**31:
                text = f"mov {R64[r]}, {imm}"
            else:
                text = f"movabs {R64[r]}, {u}"
            case(f"a.mov_ri({RUST[r]}, {u:#x})", text)
    for b in BASES:
        for disp in (0, 8, -129, 0x1234):
            for s in (0, 1, 2, 3, 6, 9):
                case(f"a.store8({m(b, disp)}, {RUST[s]})", f"mov {mem('byte', b, disp)}, {R8[s]}")
            case(f"a.store8_imm({m(b, disp)}, 1)", f"mov {mem('byte', b, disp)}, 1")
            case(f"a.load8_zx(R11, {m(b, disp)})", f"movzx r11d, {mem('byte', b, disp)}")
            case(f"a.load8_zx(RAX, {m(b, disp)})", f"movzx eax, {mem('byte', b, disp)}")
            case(f"a.lea(RCX, {m(b, disp)})", f"lea rcx, {mem('', b, disp)}")
            case(f"a.lea(R9, {m(b, disp)})", f"lea r9, {mem('', b, disp)}")
            for cc in (4, 2, 8, 0):
                case(f"a.setcc_m(Cc::{CCS[cc].capitalize()}, {m(b, disp)})", f"set{CCS[cc]} {mem('byte', b, disp)}")
    for d in (0, 1, 9):
        for s in SOME:
            case(f"a.movzx8({RUST[d]}, {RUST[s]})", f"movzx {R32[d]}, {R8[s]}")
            case(f"a.movzx16({RUST[d]}, {RUST[s]})", f"movzx {R32[d]}, {R16[s]}")
            case(f"a.movsx8({RUST[d]}, {RUST[s]})", f"movsx {R64[d]}, {R8[s]}")
            case(f"a.movsx16({RUST[d]}, {RUST[s]})", f"movsx {R64[d]}, {R16[s]}")
            case(f"a.movsxd({RUST[d]}, {RUST[s]})", f"movsxd {R64[d]}, {R32[s]}")
    for cc in range(16):
        for r in (0, 1, 6, 9):
            case(f"a.setcc_r(Cc::{CCS[cc].capitalize()}, {RUST[r]})", f"set{CCS[cc]} {R8[r]}")
    for r in range(16):
        case(f"a.push({RUST[r]})", f"push {R64[r]}")
        case(f"a.pop({RUST[r]})", f"pop {R64[r]}")
        case(f"a.call_r({RUST[r]})", f"call {R64[r]}")
    case("a.ret()", "ret")
    case("a.stc()", "stc")
    case("a.clc()", "clc")
    # Jumps: far enough (300 bytes of NOPs) that llvm-mc uses rel32 too.
    for cc in range(16):
        c = CCS[cc].capitalize()
        case(f"let f = a.jcc(Cc::{c}); a.code.extend([0x90; 300]); a.bind(f)",
             f"j{CCS[cc]} .Lf{{n}}\n.fill 300, 1, 0x90\n.Lf{{n}}:")
        case(f"a.code.extend([0x90; 300]); let f = a.jcc(Cc::{c}); a.bind_to(f, 0)",
             f".Lb{{n}}:\n.fill 300, 1, 0x90\nj{CCS[cc]} .Lb{{n}}")
    case("let f = a.jmp(); a.code.extend([0x90; 300]); a.bind(f)", "jmp .Lf{n}\n.fill 300, 1, 0x90\n.Lf{n}:")
    case("a.code.extend([0x90; 300]); let f = a.jmp(); a.bind_to(f, 0)", ".Lb{n}:\n.fill 300, 1, 0x90\njmp .Lb{n}")


def assemble():
    src = [".intel_syntax noprefix"]
    for n, (_, text) in enumerate(CASES):
        src.append(f'.section .text.c{n},"ax",@progbits')
        src.append(text.replace("{n}", str(n)))
    with tempfile.TemporaryDirectory() as tmp:
        s = os.path.join(tmp, "x.s")
        o = os.path.join(tmp, "x.o")
        with open(s, "w") as fh:
            fh.write("\n".join(src) + "\n")
        subprocess.run([os.environ.get("LLVM_MC", "llvm-mc"), "-triple=x86_64-unknown-linux-gnu",
                        "-filetype=obj", "-o", o, s], check=True)
        dump = subprocess.run([os.environ.get("OBJDUMP", "llvm-objdump"), "-s", o], check=True,
                              capture_output=True, text=True).stdout
    found = {}
    current = None
    for line in dump.splitlines():
        hdr = re.match(r"Contents of section \.text\.c(\d+):", line)
        if hdr:
            current = int(hdr.group(1))
            found[current] = bytearray()
            continue
        if line.startswith("Contents of section"):
            current = None
            continue
        if current is not None:
            words = line.split()[1:5]
            for word in words:
                if not re.fullmatch(r"[0-9a-f]+", word):
                    break
                found[current] += bytes.fromhex(word)
    return [bytes(found[n]) for n in range(len(CASES))]


def main():
    gen()
    encodings = assemble()
    out = [
        "// Generated by scripts/gen-x64-vectors.py: do not edit.",
        "// Expected bytes: llvm-mc assembling the Intel-syntax text of each case.",
        "",
        "use super::*;",
        "",
        "fn t(asm: &str, want: &[u8], f: impl FnOnce(&mut Asm)) {",
        "    let mut a = Asm::new();",
        "    f(&mut a);",
        "    assert_eq!(a.code, want, \"{asm}\");",
        "}",
        "",
        "#[test]",
        "fn encodings_match_llvm_mc() {",
    ]
    for (rust, text), enc in zip(CASES, encodings):
        shown = text.replace("\n", "; ").replace("{n}", "")
        want = ", ".join(f"{b:#04x}" for b in enc)
        out.append(f'    t("{shown}", &[{want}], |a| {{ {rust}; }});')
    out.append("}")
    text = "\n".join(out) + "\n"
    if "--check" in sys.argv:
        with open(OUT) as fh:
            if fh.read() != text:
                sys.exit("vectors.rs is out of date: run scripts/gen-x64-vectors.py")
        print(f"x64 vectors.rs up to date ({len(CASES)} cases)")
        return
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", newline="\n") as fh:
        fh.write(text)
    print(f"wrote {len(CASES)} cases to {os.path.relpath(OUT, ROOT)}")


if __name__ == "__main__":
    main()
