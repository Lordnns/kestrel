#!/usr/bin/env python3
"""Generate the GPU engines' method tables from NVIDIA's class headers.

Writes crates/kestrel-hle/src/gpu/methods.rs: for each Maxwell engine class
the Tegra X1's GM20B offers (GET_CHARACTERISTICS: 2D 0x902D, 3D 0xB197,
compute 0xB1C0, inline-to-memory 0xA140, DMA copy 0xB0B5), every method
NVIDIA's open-gpu-doc header defines (`classes/<engine>/cl<class>.h`, MIT):
its byte offset, name and, for arrays, its strides and counts.

A method is "state" when its name starts with SET_ (NVIDIA's naming: SET_
methods set a register), except those listed in TRIGGERS below, which act
when written; every other method acts (LAUNCH_DMA, DRAW_*, CLEAR_SURFACE,
WAIT_FOR_IDLE...). The command processor records state and must execute an
action or stop naming it.

The headers give no array sizes. An array runs from its base, by its
stride, up to the next method that is not part of it; where envytools'
register database (rnndb, MIT) gives the 3D class an array's length at
the same offset and stride, the smaller of the two is taken (kestrel's
reading; reserved gaps follow some arrays). The arrays envytools does not
confirm are listed in the generated file.

    python3 scripts/gen-gpu-methods.py            # regenerate
    python3 scripts/gen-gpu-methods.py --check    # fail if out of date

Needs git, to fetch open-gpu-doc and envytools at the pinned commits into
target/.
"""

import os
import re
import subprocess
import sys
import xml.etree.ElementTree as ET

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "kestrel-hle", "src", "gpu", "methods.rs")
COMMIT = "9fdf5c4062007929d9f4e6cbad9c9771fe61b880"
REPO = "https://github.com/NVIDIA/open-gpu-doc"
SRC = os.path.join(ROOT, "target", f"open-gpu-doc-{COMMIT}")
ENVY_COMMIT = "f102b82381f3f11cee113d16374c87091db039d9"
ENVY_REPO = "https://github.com/envytools/envytools"
ENVY = os.path.join(ROOT, "target", f"envytools-{ENVY_COMMIT}")
# rnndb's 3D classes in order (fifo/nv_object.xml); MAXWELL_B, 0xB197, is
# GM204_3D. A variant "A:B" covers A up to before B, "A-" A and later.
ENVY_3D = ["GF100_3D", "GF108_3D", "GF110_3D", "GK104_3D", "GK110_3D",
           "GK20A_3D", "GM107_3D", "GM204_3D", "GP100_3D"]
TARGET_3D = "GM204_3D"

CLASSES = [
    ("TWOD", 0x902D, "classes/twod/cl902d.h", "NV902D"),
    ("THREED", 0xB197, "classes/3d/clb197.h", "NVB197"),
    ("COMPUTE", 0xB1C0, "classes/compute/clb1c0.h", "NVB1C0"),
    ("INLINE_TO_MEMORY", 0xA140, "classes/inline-to-memory/cla140.h", "NVA140"),
    ("DMA_COPY", 0xB0B5, "classes/dma-copy/clb0b5.h", "NVB0B5"),
]

# 3D methods NVIDIA's header does not name that envytools' rnndb
# (graph/gf100_3d.xml) gives as plain value registers (booleans or numbers,
# no side effect described), which deko3d writes: kept as state, under
# rnndb's name. Each one is added when a program needs it, after checking
# rnndb's entry.
ENVY_STATE_3D = {
    0x15B4: "CSAA_ENABLE",
}

# SET_ methods that act when written (from their descriptions' fields:
# the D word of a semaphore or report is its operation).
TRIGGERS = {
    "SET_REPORT_SEMAPHORE_D",
}

LICENCE = """// NVIDIA's open-gpu-doc class headers, from which these tables are made:
//
//   Copyright (c) 2001-2019, NVIDIA CORPORATION. All rights reserved.
//
//   Permission is hereby granted, free of charge, to any person obtaining a
//   copy of this software and associated documentation files (the
//   "Software"), to deal in the Software without restriction, including
//   without limitation the rights to use, copy, modify, merge, publish,
//   distribute, sublicense, and/or sell copies of the Software, and to
//   permit persons to whom the Software is furnished to do so, subject to
//   the following conditions:
//
//   The above copyright notice and this permission notice shall be
//   included in all copies or substantial portions of the Software.
//
//   THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
//   EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
//   MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
//   IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
//   CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
//   TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
//   SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
"""


def fetch():
    for repo, commit, dest in [(REPO, COMMIT, SRC), (ENVY_REPO, ENVY_COMMIT, ENVY)]:
        if os.path.isdir(dest):
            continue
        tmp = dest + ".tmp"
        subprocess.run(["git", "clone", "-q", "--filter=blob:none", repo, tmp], check=True)
        subprocess.run(["git", "-C", tmp, "checkout", "-q", commit], check=True)
        os.rename(tmp, dest)


def applies(variants):
    """Whether an rnndb variants attribute covers the target 3D class."""
    if not variants:
        return True
    order = ENVY_3D.index(TARGET_3D)
    for part in variants.split():
        if part.endswith("-"):
            name = part[:-1]
            if name in ENVY_3D and ENVY_3D.index(name) <= order:
                return True
        elif ":" in part:
            # A range ending past the 3D classes (gf100_3d.xml's domain
            # stripe is "GF100_3D:G80_2D") runs to the last of them.
            lo, hi = part.split(":")
            end = ENVY_3D.index(hi) if hi in ENVY_3D else len(ENVY_3D)
            if lo in ENVY_3D and ENVY_3D.index(lo) <= order < end:
                return True
        elif part == TARGET_3D:
            return True
    return False


def envy_lengths():
    """(offset, stride) -> length for every register array rnndb's
    gf100_3d.xml gives the target 3D class: stripes and arrays with a
    length, and registers with one, by their absolute offsets."""
    text = open(os.path.join(ENVY, "rnndb", "graph", "gf100_3d.xml"), encoding="utf-8").read()
    text = re.sub(r'xmlns="[^"]+"', "", text, count=1)
    root = ET.fromstring(text)
    lengths = {}

    def num(v):
        return int(v, 0)

    def walk(node, base, dims):
        for ch in node:
            tag = ch.tag.split("}")[-1]
            if not applies(ch.get("variants")):
                continue
            off = base + num(ch.get("offset", "0"))
            if tag in ("stripe", "array"):
                d = dims
                if ch.get("length"):
                    d = dims + [(num(ch.get("stride")), num(ch.get("length")))]
                walk(ch, off, d)
            elif tag in ("reg32", "reg64"):
                d = dims
                if ch.get("length"):
                    d = dims + [(num(ch.get("stride", "4" if tag == "reg32" else "8")), num(ch.get("length")))]
                if d:
                    stride, length = d[0]
                    lengths.setdefault((off, stride), set()).add(length)
            elif tag == "domain":
                walk(ch, off, dims)

    walk(root, 0, [])
    return {k: max(v) for k, v in lengths.items()}


SCALAR = re.compile(r"^#define (NV[0-9A-F]+)_(\w+)\s+\(?(0x[0-9a-fA-F]+)\)?\s*$")
ARRAY1 = re.compile(
    r"^#define (NV[0-9A-F]+)_(\w+)\((\w)\)\s+\((0x[0-9a-fA-F]+)\+\(\3\)\*(\d+)\)\s*$"
)
ARRAY2 = re.compile(
    r"^#define (NV[0-9A-F]+)_(\w+)\((\w),(\w)\)\s+"
    r"\((0x[0-9a-fA-F]+)\+\(\3\)\*(\d+)\+\(\4\)\*(\d+)\)\s*$"
)


def parse(path, prefix):
    """Methods as dicts: name, base, outer stride (0 for scalars), inner
    stride (0 unless two-dimensional)."""
    methods = []
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            m = ARRAY2.match(line)
            if m and m.group(1) == prefix:
                methods.append(
                    dict(name=m.group(2), base=int(m.group(5), 16),
                         stride=int(m.group(6)), inner=int(m.group(7)))
                )
                continue
            m = ARRAY1.match(line)
            if m and m.group(1) == prefix:
                methods.append(
                    dict(name=m.group(2), base=int(m.group(4), 16),
                         stride=int(m.group(5)), inner=0)
                )
                continue
            m = SCALAR.match(line)
            if m and m.group(1) == prefix:
                name, value = m.group(2), int(m.group(3), 16)
                # Methods are word-aligned offsets below 0x4000; field
                # values (NAME_FIELD_VALUE 0x...) follow their method and
                # are told apart by their name extending a method's.
                methods.append(dict(name=name, base=value, stride=0, inner=0, scalar=True))
    # Scalars that are field values: their name extends the previous
    # method's name (fields and values are listed after their method).
    out = []
    last = None
    for m in methods:
        if m.get("scalar"):
            if last is not None and m["name"].startswith(last + "_"):
                continue
            if m["base"] % 4 != 0 or m["base"] >= 0x4000:
                continue
        out.append(m)
        last = m["name"]
    return out


def counts(methods):
    """Each array's count: from its base by its stride, up to the next
    method address that is not part of its group (arrays of the same
    stride starting within one stride of it)."""
    bases = sorted({m["base"] for m in methods})
    for m in methods:
        if m["stride"] == 0:
            m["count"], m["inner_count"] = 1, 1
            continue
        group = {
            o["base"] for o in methods
            if o["stride"] == m["stride"] and m["base"] <= o["base"] < m["base"] + m["stride"]
        }
        # The group's own bases may sit in between; the limit is the first
        # address past the base that belongs to nothing in the group.
        lo = min(group)
        limit = next((b for b in bases if b > lo and b not in group), 0x4000)
        if m["inner"]:
            m["inner_count"] = m["stride"] // m["inner"]
        else:
            m["inner_count"] = 1
        n = 0
        while m["base"] + n * m["stride"] < limit:
            n += 1
        m["count"] = max(n, 1)
    return methods


def confirm(methods, lengths):
    """Cap each array at envytools' length where it gives one; return the
    arrays it does not confirm."""
    unconfirmed = []
    for m in methods:
        if m["stride"] == 0:
            continue
        length = lengths.get((m["base"], m["stride"]))
        if length is None:
            unconfirmed.append(m)
        else:
            m["count"] = min(m["count"], length)
    return unconfirmed


def kind(name):
    if name.startswith("ENVYTOOLS_"):
        return "State"
    if name.startswith("SET_") and name not in TRIGGERS:
        return "State"
    return "Action"


def generate():
    fetch()
    out = [
        "//! The GPU engines' methods, generated by scripts/gen-gpu-methods.py from",
        f"//! NVIDIA's open-gpu-doc class headers (commit {COMMIT[:12]}). Do not",
        "//! edit by hand.",
        "//!",
        "//! A `State` method sets a register; an `Action` method acts when written",
        "//! (scripts/gen-gpu-methods.py says how they are told apart).",
        "",
        LICENCE.rstrip(),
        "",
        "use super::{Kind, Method};",
        "",
    ]
    lengths = envy_lengths()
    for label, class_id, path, prefix in CLASSES:
        methods = counts(parse(os.path.join(SRC, path), prefix))
        methods.sort(key=lambda m: m["base"])
        if label == "THREED":
            for base, name in ENVY_STATE_3D.items():
                assert not any(m["base"] == base for m in methods), name
                methods.append(dict(name="ENVYTOOLS_" + name, base=base, stride=0,
                                    inner=0, count=1, inner_count=1))
            methods.sort(key=lambda m: m["base"])
            unconfirmed = confirm(methods, lengths)
            out.append("// Arrays of the 3D class whose length envytools does not give")
            out.append("// (inferred from the next method): " + ", ".join(
                f"{m['name']} {m['count']}" for m in unconfirmed) + ".")
        out.append(f"/// Class {class_id:#06x} ({path.split('/')[-1]}): {len(methods)} methods.")
        out.append("#[rustfmt::skip]")
        out.append(f"pub static {label}: &[Method] = &[")
        for m in methods:
            out.append(
                f'    Method {{ name: "{m["name"]}", base: {m["base"]:#06x}, '
                f'stride: {m["stride"]}, count: {m["count"]}, '
                f'inner: {m["inner"]}, inner_count: {m["inner_count"]}, '
                f'kind: Kind::{kind(m["name"])} }},'
            )
        out.append("];")
        out.append("")
    return "\n".join(out)


def main():
    text = generate()
    if "--check" in sys.argv[1:]:
        with open(OUT, encoding="utf-8") as f:
            if f.read() != text:
                sys.exit("methods.rs differs from NVIDIA's headers: run scripts/gen-gpu-methods.py")
        print("methods.rs up to date")
        return
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    print(f"wrote {os.path.relpath(OUT, ROOT)}")


if __name__ == "__main__":
    main()
