/* Link a kestrel homebrew test program as a position-independent image at
 * address 0 with three segments, text (with the NRO header at offset 0),
 * read-only data and read-write data plus bss, for scripts/make-nro.py. */
OUTPUT_FORMAT(elf64-littleaarch64)
ENTRY(nro_start)
PHDRS {
    text PT_LOAD FLAGS(5);
    ro PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
    dyn PT_DYNAMIC;
}
SECTIONS {
    . = 0;
    .text : { KEEP(*(.text.nro_start)) *(.text .text.*) } :text
    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) } :ro
    .dynsym : { *(.dynsym) } :ro
    .dynstr : { *(.dynstr) } :ro
    .hash : { *(.hash) } :ro
    .gnu.hash : { *(.gnu.hash) } :ro
    .rela.dyn : { *(.rela.dyn) } :ro
    . = ALIGN(0x1000);
    /* Relocated at startup, so read-write even when logically read-only. */
    .data : { *(.data .data.*) *(.data.rel.ro .data.rel.ro.*) } :data
    .dynamic : { *(.dynamic) } :data :dyn
    .got : { *(.got) *(.got.plt) } :data
    .bss : { __bss_start = .; *(.bss .bss.*) *(COMMON) __bss_end = .; } :data
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.interp) }
}
