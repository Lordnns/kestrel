#!/usr/bin/env python3
"""Check the last frame each standard homebrew example showed in kestrel.

Phase 2 acceptance (PLAN.md): the examples' output is checked
independently of kestrel. They print with libnx's console, which draws
16x16 character cells from libnx's font (nx/data/default_font.bin, ISC):
each glyph is 16 rows of 16 bits, the top bit the leftmost pixel, as
libnx's ConsoleSwRenderer_drawChar reads it. This script reads the text
back from kestrel's PNG cell by cell against that font, and compares it
with what each example's source (switchbrew/switch-examples, public
domain mark) prints:

- hello-world: "Hello World!" at row 16, column 20 (its escape code).
- read-controls, with A pressed, then Plus: on any key change its loop
  clears the screen and prints a slightly different header ("stick", not
  "joystick"), both sticks at 0000; 0000, then the key's state: "A down"
  and "A held", "A held", or "A up", depending on the last change it saw
  before Plus (it stops on Plus before printing again). Which one depends
  on timing, so any of the three passes.
- sdmc, run from sdmc:/switch on an empty card: the listing of its own
  folder, which holds only itself.
- barrier: "Creating threads", then for each of its four threads
  "Entering barrier n", "Leaving barrier n" twice, where nobody leaves a
  round before all four have entered it. Threads may interleave in any
  order that keeps those rules, as on a console.

Usage: check-examples.py <default_font.bin> <frames folder>
"""

import struct
import sys
import zlib
from pathlib import Path

COLUMNS, ROWS, CELL = 80, 45, 16


def read_png(path):
    """(width, height, rows of (r, g, b)) from an 8-bit RGB PNG whose rows
    all use filter 0, as kestrel writes them."""
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        sys.exit(f"{path}: not a PNG")
    at, idat, width, height = 8, b"", 0, 0
    while at < len(data):
        (length,) = struct.unpack(">I", data[at : at + 4])
        kind, body = data[at + 4 : at + 8], data[at + 8 : at + 8 + length]
        if kind == b"IHDR":
            width, height, depth, colour = struct.unpack(">IIBB", body[:10])
            if (depth, colour) != (8, 2):
                sys.exit(f"{path}: not 8-bit RGB")
        elif kind == b"IDAT":
            idat += body
        at += 12 + length
    raw = zlib.decompress(idat)
    stride = 1 + 3 * width
    rows = []
    for y in range(height):
        line = raw[y * stride : (y + 1) * stride]
        if line[0] != 0:
            sys.exit(f"{path}: row {y} uses filter {line[0]}")
        rows.append([tuple(line[1 + 3 * x : 4 + 3 * x]) for x in range(width)])
    return width, height, rows


def glyphs(font):
    """Each character's 16 rows of 16 bits, printable ASCII first so a
    shape shared by several codes reads as the plain character."""
    table = {}
    for c in list(range(32, 127)) + [c for c in range(256) if not 32 <= c < 127]:
        shape = struct.unpack_from("<16H", font, 32 * c)
        table.setdefault(shape, chr(c))
    return table


def screen_text(path, table):
    width, height, rows = read_png(path)
    if (width, height) != (COLUMNS * CELL, ROWS * CELL):
        sys.exit(f"{path}: a {width}x{height} frame, not the console's 1280x720")
    background = rows[0][0]
    lines = []
    for row in range(ROWS):
        text = ""
        for col in range(COLUMNS):
            shape = tuple(
                sum(
                    (rows[row * CELL + j][col * CELL + i] != background) << (15 - i)
                    for i in range(CELL)
                )
                for j in range(CELL)
            )
            text += table.get(shape, "?")
        lines.append(text.rstrip())
    while lines and not lines[-1]:
        lines.pop()
    return lines


def expect_lines(lines, want):
    return None if lines == want else f"expected {want!r}, read {lines!r}"


def check_read_controls(lines):
    header = [
        "Press PLUS to exit.",
        "Left stick position:",
        "0000; 0000",
        "Right stick position:",
        "0000; 0000",
    ]
    states = [["A down", "A held"], ["A held"], ["A up"]]
    if lines[:5] != header or lines[5:] not in states:
        return f"expected {header!r} then one of {states!r}, read {lines!r}"
    return None


def check_barrier(lines):
    if not lines or lines[0] != "Creating threads":
        return f"expected 'Creating threads' first, read {lines[:1]!r}"
    events = lines[1:]
    if len(events) != 16:
        return f"expected 16 barrier lines, read {len(events)}: {events!r}"
    steps = {n: [] for n in range(4)}
    entered = [0, 0]
    for at, line in enumerate(events):
        words = line.split(" ")
        if len(words) != 3 or words[1] != "barrier" or words[0] not in ("Entering", "Leaving"):
            return f"line {at + 2}: {line!r}"
        n = int(words[2])
        if n not in steps:
            return f"line {at + 2}: thread {n}"
        steps[n].append(words[0])
        rnd = (len(steps[n]) - 1) // 2
        if rnd > 1:
            return f"line {at + 2}: thread {n} passes the barrier a third time"
        if words[0] == "Entering":
            entered[rnd] += 1
        elif entered[rnd] < 4:
            return f"line {at + 2}: thread {n} leaves round {rnd + 1} before all four entered"
    for n, s in steps.items():
        if s != ["Entering", "Leaving", "Entering", "Leaving"]:
            return f"thread {n}: {s!r}"
    return None


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    table = glyphs(Path(sys.argv[1]).read_bytes())
    frames = Path(sys.argv[2])
    hello = [""] * 15 + [" " * 19 + "Hello World!"]
    checks = {
        "hello-world": lambda t: expect_lines(t, hello),
        "read-controls": check_read_controls,
        "sdmc": lambda t: expect_lines(t, ["Dir-listing for '':", "d_name: sdmc.nro", "Done."]),
        "barrier": check_barrier,
    }
    failed = 0
    for name, check in checks.items():
        problem = check(screen_text(frames / f"{name}.png", table))
        print(f"{'FAIL' if problem else 'PASS'}  {name}" + (f": {problem}" if problem else ""))
        failed += problem is not None
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
