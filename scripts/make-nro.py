#!/usr/bin/env python3
"""Turn a kestrel homebrew test program, linked by guest/src/nro/nro.ld,
into an NRO.

The NRO layout is Switchbrew's public "NRO" page: the file is the memory
image from address 0, with a 0x10-byte start block and the 0x70-byte header
at the start of .text, and .bss after .data. The input ELF must have exactly
three PT_LOAD segments at page-aligned addresses: read-execute from 0 (the
startup code reserves the header space), read-only, then read-write.

usage: make-nro.py <in.elf> <out.nro>
"""
import hashlib
import struct
import sys

PAGE = 0x1000
PT_LOAD = 1


def page_up(v):
    return (v + PAGE - 1) & ~(PAGE - 1)


def segments(elf):
    if elf[:4] != b"\x7fELF" or elf[4] != 2 or elf[5] != 1:
        sys.exit("make-nro: not a 64-bit little-endian ELF")
    phoff, = struct.unpack_from("<Q", elf, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", elf, 0x36)
    out = []
    for i in range(phnum):
        p_type, p_flags, off, vaddr, _, filesz, memsz, _ = struct.unpack_from(
            "<IIQQQQQQ", elf, phoff + i * phentsize)
        if p_type == PT_LOAD:
            out.append((p_flags, off, vaddr, filesz, memsz))
    return out


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    elf = open(sys.argv[1], "rb").read()
    segs = segments(elf)
    if [s[0] for s in segs] != [5, 4, 6]:
        sys.exit("make-nro: expected PT_LOAD segments r-x, r--, rw- (link with nro.ld)")
    (_, t_off, t_addr, t_file, t_mem), (_, r_off, r_addr, r_file, r_mem), \
        (_, d_off, d_addr, d_file, d_mem) = segs
    text_size, ro_size, data_size = page_up(t_mem), page_up(r_mem), page_up(d_file)
    if t_addr != 0 or r_addr != text_size or d_addr != r_addr + ro_size:
        sys.exit("make-nro: segments are not contiguous page-aligned text, rodata, data")
    if t_file != t_mem or r_file != r_mem:
        sys.exit("make-nro: text or rodata has zero-filled memory")
    bss_size = max(0, page_up(d_addr + d_mem) - (d_addr + data_size))
    image = bytearray(d_addr + data_size)
    image[0:t_file] = elf[t_off:t_off + t_file]
    image[r_addr:r_addr + r_file] = elf[r_off:r_off + r_file]
    image[d_addr:d_addr + d_file] = elf[d_off:d_off + d_file]
    if any(image[0x8:0x80]):
        sys.exit("make-nro: the startup code did not reserve the NRO header space")
    header = struct.pack(
        "<4sIII6III",
        b"NRO0", 0, len(image), 0,
        0, text_size, r_addr, ro_size, d_addr, data_size,
        bss_size, 0)
    build_id = hashlib.sha256(bytes(image)).digest()
    header += build_id + bytes(0x80 - 0x10 - len(header) - len(build_id))
    image[0x10:0x80] = header
    open(sys.argv[2], "wb").write(image)


if __name__ == "__main__":
    main()
