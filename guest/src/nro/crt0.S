// Start of a kestrel homebrew test program. The first 0x80 bytes are the
// NRO start block and header (Switchbrew, "NRO"); scripts/make-nro.py fills
// in the header. The program relocates itself, as homebrew does, then
// calls hb_main(config, x1).
    .section .text.nro_start, "ax"
    .global nro_start
nro_start:
    b       1f
    .word   mod0 - nro_start        // module header (MOD0) offset
    .space  0x78                    // rest of the start block, NRO header
1:  mov     x19, x0
    mov     x20, x1
    adr     x0, nro_start
    adrp    x1, _DYNAMIC
    add     x1, x1, :lo12:_DYNAMIC
    bl      hb_relocate
    mov     x0, x19
    mov     x1, x20
    bl      hb_main
    svc     #0x07                   // ExitProcess

// Module header: offsets relative to itself (Switchbrew, "NSO", MOD0).
    .section .rodata.mod0, "a"
    .balign 4
mod0:
    .ascii  "MOD0"
    .word   _DYNAMIC - mod0
    .word   __bss_start - mod0
    .word   __bss_end - mod0
    .word   0, 0                    // no eh_frame_hdr
    .word   0                       // no module object
