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
- read-controls, with A pressed, then Plus while A is still held: on
  any key change its loop clears the screen and prints a slightly
  different header ("stick", not "joystick"), both sticks at 0000; 0000,
  then the key's state: "A down" and "A held", then "A held" on the next
  frame (it stops on Plus before printing again). A program slower than
  a frame may still show the first, and "A up" is accepted too, so the
  check does not depend on timing.
- sdmc, run from sdmc:/switch on an empty card: the listing of its own
  folder, which holds only itself.
- barrier: "Creating threads", then for each of its four threads
  "Entering barrier n", "Leaving barrier n" twice, where nobody leaves a
  round before all four have entered it. Threads may interleave in any
  order that keeps those rules, as on a console.

Phase 5A acceptance (PLAN.md), the framebuffer and 2D examples:

- vt52-demo (graphics/printing/vt52-demo): its escape codes move the
  cursor and set colours, bold, faint, reverse, underline and
  strikethrough. Its frame is checked pixel for pixel against a model of
  libnx 4.12.0's console written from its source (console.c's escape
  handling, console_sw.c's palette and drawChar, both ISC): the text
  grid with each cell's colours and lines, drawn with the font, the
  RGB 565 colours widened as kestrel's display documents (each
  channel's top bits repeated below it).
- simplegfx (graphics/simplegfx): an RGBA 8888 framebuffer made linear
  by libnx, filled each frame with one grey, 4 * cnt in each byte, where
  cnt counts frames from 1 to 60 and back to 0. The frame must be that
  one grey everywhere; which one depends on how many frames ran.
- deko_examples (graphics/deko3d/deko_examples), its first example
  ("01: Simple Setup", A pressed in the menu): the GPU clears an RGBA8
  render target in four scissored rectangles, Example01_SimpleSetup.cpp's
  (a 1280x720 background, a 400-pixel box in the middle, two tiles in
  it), and presents it through the swapchain. Each pixel must be its
  rectangle's clear colour, the floats as 8-bit unorm (times 255,
  rounded to the nearest, halves up: kestrel's documented choice).

Usage: check-examples.py <default_font.bin> <frames folder> [--skip <example>...]
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


# libnx 4.12.0's console palette (console_sw.c's colorTable), as RGB 8 per
# channel before the RGB 565 packing: normal, bright (bold), faint.
PALETTE = [
    (0, 0, 0), (128, 0, 0), (0, 128, 0), (128, 128, 0),
    (0, 0, 128), (128, 0, 128), (0, 128, 128), (192, 192, 192),
    (128, 128, 128), (255, 0, 0), (0, 255, 0), (255, 255, 0),
    (0, 0, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255),
    (0, 0, 0), (64, 0, 0), (0, 64, 0), (64, 64, 0),
    (0, 0, 64), (64, 0, 64), (0, 64, 64), (96, 96, 96),
]
BOLD, FAINT, UNDERLINE, REVERSE, CROSSED = 1, 2, 4, 8, 16


def rgb565_shown(r, g, b):
    """RGB8 packed as libnx's RGB565_FROM_RGB8 (top 5, 6, 5 bits), then
    widened back as kestrel shows RGB 565: each channel's bits repeated
    below it."""
    r5, g6, b5 = r >> 3, g >> 2, b >> 3
    return ((r5 << 3) | (r5 >> 2), (g6 << 2) | (g6 >> 4), (b5 << 3) | (b5 >> 2))


class Console:
    """The part of libnx's console (console.c) the demo uses: an 80x45
    grid, 1-based cursor, printable characters, '\n', and the escapes
    H, A, B, C, D, J (2) and m with codes 0, 1, 2, 4, 7, 9 and 30-37."""

    def __init__(self):
        self.x, self.y = 1, 1
        self.fg, self.bg, self.flags = 7, 0, 0
        self.cells = {}
        self.clear()

    def clear(self):
        for row in range(ROWS):
            for col in range(COLUMNS):
                self.cells[row, col] = (" ", self.fg, self.bg, self.flags)
        self.x, self.y = 1, 1

    def put(self, c):
        if c == "\n":
            self.y += 1  # the demo never reaches the last row
            self.x = 1
            return
        if self.x > COLUMNS:
            self.x = 1
            self.y += 1
        self.cells[self.y - 1, self.x - 1] = (c, self.fg, self.bg, self.flags)
        self.x += 1

    def sgr(self, code):
        if code == 0:
            self.fg, self.bg, self.flags = 7, 0, 0
        elif code == 1:
            self.flags = (self.flags & ~FAINT) | BOLD
        elif code == 2:
            self.flags = (self.flags & ~BOLD) | FAINT
        elif code == 4:
            self.flags |= UNDERLINE
        elif code == 7:
            self.flags |= REVERSE
        elif code == 9:
            self.flags |= CROSSED
        elif 30 <= code <= 37:
            self.fg = code - 30
        else:
            raise ValueError(f"code {code} is outside the model")

    def write(self, text):
        at = 0
        while at < len(text):
            c = text[at]
            if c != "\x1b":
                self.put(c)
                at += 1
                continue
            assert text[at + 1] == "["
            end = at + 2
            while not text[end].isalpha():
                end += 1
            args = [int(a) for a in text[at + 2 : end].split(";") if a]
            op = text[end]
            n = args[0] if args else 1
            if op == "H":
                self.y = min(max(args[0], 1), ROWS)
                self.x = min(max(args[1], 1), COLUMNS)
            elif op == "A":
                self.y = max(self.y - n, 1)
            elif op == "B":
                self.y = min(self.y + n, ROWS)
            elif op == "C":
                self.x = min(self.x + n, COLUMNS)
            elif op == "D":
                self.x = max(self.x - n, 1)
            elif op == "J" and args == [2]:
                self.clear()
            elif op == "m":
                for code in args or [0]:
                    self.sgr(code)
            else:
                raise ValueError(f"escape {text[at:end + 1]!r} is outside the model")
            at = end + 1

    def pixels(self, font):
        """The screen as drawChar paints it: rows of (r, g, b)."""
        rows = [[None] * (COLUMNS * CELL) for _ in range(ROWS * CELL)]
        for (row, col), (c, fg, bg, flags) in self.cells.items():
            if flags & BOLD:
                fgc = PALETTE[fg + 8]
            elif flags & FAINT:
                fgc = PALETTE[fg + 16]
            else:
                fgc = PALETTE[fg]
            bgc = PALETTE[bg]
            if flags & REVERSE:
                fgc, bgc = bgc, fgc
            fgc, bgc = rgb565_shown(*fgc), rgb565_shown(*bgc)
            shape = list(struct.unpack_from("<16H", font, 32 * ord(c)))
            if flags & UNDERLINE:
                shape[15] = 0xFFFF
            if flags & CROSSED:
                shape[7] = 0xFFFF
            for j in range(CELL):
                for i in range(CELL):
                    on = shape[j] >> (15 - i) & 1
                    rows[row * CELL + j][col * CELL + i] = fgc if on else bgc
        return rows


def vt52_demo_text():
    """What graphics/printing/vt52-demo's main.c prints, its escapes
    expanded as its CONSOLE_ESC macro and printf's %1$d do."""
    e = "\x1b["
    text = e + "2J"
    text += e + "10;10H" + "VT52 codes demo"
    text += e + "10A" + "Line 0"
    text += e + "28D" + "Column 0"
    text += e + "19B" + "Line 19"
    text += e + "5C" + "Column 20"
    text += "\n"
    for i in range(8):
        c = f"{e}{i + 30};1m"
        text += (
            c + "Default " + e + "1m" + "Bold " + e + "7m" + "Reversed "
            + e + "0m" + c + e + "2m" + "Light " + e + "7m" + "Reversed "
            + e + "0m" + c + e + "4m" + "Underline "
            + e + "0m" + c + e + "9m" + "Strikethrough " + "\n" + e + "0m"
        )
    return text


def check_vt52_demo(path, font):
    width, height, rows = read_png(path)
    if (width, height) != (COLUMNS * CELL, ROWS * CELL):
        return f"a {width}x{height} frame, not the console's 1280x720"
    console = Console()
    console.write(vt52_demo_text())
    want = console.pixels(font)
    for y in range(height):
        for x in range(width):
            if rows[y][x] != want[y][x]:
                return (
                    f"pixel ({x}, {y}), cell ({y // CELL}, {x // CELL}): "
                    f"{rows[y][x]} instead of {want[y][x]}"
                )
    return None


def check_simplegfx(path, font):
    width, height, rows = read_png(path)
    if (width, height) != (1280, 720):
        return f"a {width}x{height} frame, not its 1280x720"
    first = rows[0][0]
    v = first[0]
    if first != (v, v, v) or v % 4 or v > 240:
        return f"pixel (0, 0) is {first}, not a grey 4 * n with n at most 60"
    for y, row in enumerate(rows):
        for x, p in enumerate(row):
            if p != first:
                return f"pixel ({x}, {y}) is {p}, pixel (0, 0) {first}"
    return None


def deko_simple_setup():
    """The rectangles Example01_SimpleSetup.cpp clears, last on top:
    (x, y, width, height, colour) in pixels and 8-bit channels."""
    width, height, box = 1280, 720, 400
    box_x, box_y = width // 2 - box // 2, height // 2 - box // 2
    tile_w, tile_h = box // 5, box // 4

    def unorm8(v):
        return int(min(max(v, 0.0), 1.0) * 255 + 0.5)

    def colour(r, g, b):
        return (unorm8(r), unorm8(g), unorm8(b))

    green = colour(0.0, 0.5, 0.0)
    return [
        (0, 0, width, height, colour(0.0, 0.25, 0.0)),
        (box_x, box_y, box, box, colour(229 / 255.0, 1.0, 232 / 255.0)),
        (box_x + 2 * tile_w, box_y + tile_h, tile_w, tile_h, green),
        (box_x + tile_w, box_y + 2 * tile_h, 3 * tile_w, tile_h, green),
    ]


def check_deko_examples(path, font):
    width, height, rows = read_png(path)
    if (width, height) != (1280, 720):
        return f"a {width}x{height} frame, not its 1280x720"
    rects = deko_simple_setup()
    for y, row in enumerate(rows):
        for x, p in enumerate(row):
            want = [c for (rx, ry, rw, rh, c) in rects
                    if rx <= x < rx + rw and ry <= y < ry + rh][-1]
            if p != want:
                return f"pixel ({x}, {y}) is {p}, not {want}"
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
    skip = sys.argv[4::2]
    if len(sys.argv) < 3 or sys.argv[3::2] != ["--skip"] * len(skip):
        sys.exit(__doc__)
    font = Path(sys.argv[1]).read_bytes()
    table = glyphs(font)
    frames = Path(sys.argv[2])
    hello = [""] * 15 + [" " * 19 + "Hello World!"]
    checks = {
        "hello-world": lambda t: expect_lines(t, hello),
        "read-controls": check_read_controls,
        "sdmc": lambda t: expect_lines(t, ["Dir-listing for '':", "d_name: sdmc.nro", "Done."]),
        "barrier": check_barrier,
    }
    # Checks that read the frame's pixels rather than its text.
    frame_checks = {
        "vt52-demo": check_vt52_demo,
        "simplegfx": check_simplegfx,
        "deko_examples": check_deko_examples,
    }
    failed = 0
    results = [(n, c(screen_text(frames / f"{n}.png", table)))
               for n, c in checks.items() if n not in skip]
    results += [(n, c(frames / f"{n}.png", font))
                for n, c in frame_checks.items() if n not in skip]
    for name, problem in results:
        print(f"{'FAIL' if problem else 'PASS'}  {name}" + (f": {problem}" if problem else ""))
        failed += problem is not None
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
