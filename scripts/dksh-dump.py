#!/usr/bin/env python3
"""Dump the shader programs in a deko3d .dksh file, naming each Maxwell
instruction from envytools' disassembler tables, to see what a program
needs before kestrel translates it (a development tool, not a test).

The .dksh layout is deko3d's (source/dksh.h, zlib): a control section
(header, then 64-byte program headers: type, entry point, register count,
constant buffer 1's offset and size) and a code section. At each entry
point: the 80-byte shader program header, then instructions in groups of
four 64-bit words (a scheduling word, three instructions). Instruction
names come from envytools' envydis/gm107.c root table (MIT), which
scripts/gen-gpu-methods.py fetches into target/envytools-<commit>.

    python3 scripts/dksh-dump.py <file.dksh>...

deko3d's examples, once built by scripts/build-examples.sh, keep their
shaders in target/examples/switch-examples-*/graphics/deko3d/*/romfs/shaders.
"""

import glob
import os
import re
import struct
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def table():
    found = glob.glob(os.path.join(ROOT, "target", "envytools-*", "envydis", "gm107.c"))
    if not found:
        sys.exit("envytools is not in target/: run scripts/gen-gpu-methods.py once")
    src = open(found[0], encoding="utf-8").read()
    root = src[src.index("static struct insn tabroot[]"):]
    root = root[:root.index("\n};")]
    entries = re.finditer(r'\{\s*(0x[0-9a-f]+)ull,\s*(0x[0-9a-f]+)ull,.*?N\(\s*"([^"]+)"\)', root)
    return [(int(m.group(1), 16), int(m.group(2), 16), m.group(3)) for m in entries]


def dump(path, tab):
    d = open(path, "rb").read()
    _magic, _hsz, csz, codesz, poff, n = struct.unpack_from("<6I", d, 0)
    code = d[csz:csz + codesz]
    for i in range(n):
        kind, entry, gprs, cb1off, cb1sz = struct.unpack_from("<5I", d, poff + 64 * i)
        print(f"== {os.path.basename(path)}: program type {kind} (0 vertex, 1 pixel), "
              f"entry {entry:#x}, {gprs} registers, constant buffer 1 {cb1off:#x}+{cb1sz:#x}")
        at, k = entry + 0x50, 0
        while at + 8 <= len(code):
            w = struct.unpack_from("<Q", code, at)[0]
            if k % 4:
                name = next((nm for v, mk, nm in tab if w & mk == v), "?")
                print(f"  {at:#06x} {w:016x} {name}")
                if name == "exit" and (w >> 16) & 0xf == 7:
                    break
            at, k = at + 8, k + 1


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    t = table()
    for p in sys.argv[1:]:
        dump(p, t)
