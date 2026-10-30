// Hand-written ARM64 self-checks. Each `expect` compares a register with a
// value worked out from the Arm architecture rules (not from running kestrel),
// prints the check's name on mismatch, and the program ends with a summary.
//
// Registers: x27 = passed, x28 = failed. `expect` clobbers x16, flags and,
// on failure, x0-x2. Keep live values out of those.

    .text
    .globl _start

.macro expect reg, val, name
    ldr     x16, =\val
    cmp     \reg, x16
    b.ne    98f
    add     x27, x27, #1
    b       99f
98:
    .pushsection .rodata
97: .asciz  "\name"
    .popsection
    adrp    x0, 97b
    add     x0, x0, :lo12:97b
    bl      fail
99:
.endm

.macro lea reg, sym
    adrp    \reg, \sym
    add     \reg, \reg, :lo12:\sym
.endm

_start:
    mov     x27, #0
    mov     x28, #0

// ---- arithmetic and carries ------------------------------------------------
    mov     x3, #-1
    mov     x4, #1
    adds    x5, x3, x4
    adc     x6, xzr, xzr
    expect  x5, 0, "adds wraps to zero"
    expect  x6, 1, "adc picks up carry"

    mov     x3, #0
    subs    x5, x3, #1
    sbc     x6, xzr, xzr
    expect  x5, 0xffffffffffffffff, "subs borrows"
    expect  x6, 0xffffffffffffffff, "sbc propagates borrow"

    mov     w3, #-1
    add     w5, w3, #1
    expect  x5, 0, "32-bit add wraps"
    mov     x3, #-1
    add     w6, w3, #2
    expect  x6, 1, "32-bit result zero-extends"

// ---- move wide -------------------------------------------------------------
    movz    x7, #0xbabe
    movk    x7, #0xcafe, lsl #16
    movk    x7, #0xbeef, lsl #32
    movk    x7, #0xdead, lsl #48
    expect  x7, 0xdeadbeefcafebabe, "movz/movk chain"
    movn    w8, #0
    expect  x8, 0xffffffff, "movn 32-bit"

// ---- bit manipulation (x7 = 0xdeadbeefcafebabe) ----------------------------
    mov     x4, #1
    rbit    x9, x4
    clz     x10, x9
    mov     x3, #-1
    cls     x11, x3
    mov     x12, #0xf0
    ubfiz   x13, x12, #4, #8
    sbfx    x14, x12, #4, #4
    mov     x15, #0
    bfxil   x15, x7, #8, #8
    ror     x19, x7, #16
    rev     x20, x7
    rev16   w21, w7
    extr    x22, x7, x4, #4
    expect  x9, 0x8000000000000000, "rbit"
    expect  x10, 0, "clz of top bit"
    expect  x11, 63, "cls of -1"
    expect  x13, 0xf00, "ubfiz"
    expect  x14, 0xffffffffffffffff, "sbfx sign-extends"
    expect  x15, 0xba, "bfxil"
    expect  x19, 0xbabedeadbeefcafe, "ror immediate"
    expect  x20, 0xbebafecaefbeadde, "rev 64-bit"
    expect  x21, 0xfecabeba, "rev16 32-bit"
    expect  x22, 0xe000000000000000, "extr"

// ---- logical and shifted operands ------------------------------------------
    and     x3, x7, #0x00ff00ff00ff00ff
    orr     x4, xzr, #0xaaaaaaaaaaaaaaaa
    eor     x5, x7, x7, ror #32
    bic     x6, x7, x4
    orn     w8, wzr, w7
    add     x11, x7, w4, uxtb #2
    neg     x12, x7
    mov     x21, #68
    lslv    x20, x7, x21
    asrv    x22, x7, x21
    expect  x3, 0x00ad00ef00fe00be, "and bitmask immediate"
    expect  x4, 0xaaaaaaaaaaaaaaaa, "orr bitmask immediate"
    expect  x5, 0x1453045114530451, "eor with ror operand"
    expect  x6, 0x5405144540541014, "bic"
    expect  x8, 0x35014541, "orn 32-bit"
    expect  x11, 0xdeadbeefcafebd66, "add extended uxtb #2"
    expect  x12, 0x2152411035014542, "neg"
    expect  x20, 0xeadbeefcafebabe0, "lslv uses shift mod 64"
    expect  x22, 0xfdeadbeefcafebab, "asrv keeps sign"

    ands    x9, x7, #0xf
    cset    x10, ne
    tst     x7, #0x8000000000000000
    cset    x13, mi
    expect  x9, 0xe, "ands result"
    expect  x10, 1, "ands sets Z correctly"
    expect  x13, 1, "tst sets N"

    mov     x13, sp
    sub     sp, sp, #32
    mov     x14, sp
    add     sp, sp, #32
    sub     x15, x13, x14
    expect  x15, 32, "add/sub on sp"

// ---- multiply and divide ---------------------------------------------------
    mov     x3, #-1
    umulh   x5, x3, x3
    smulh   x6, x3, x3
    mov     x8, #-5
    smull   x9, w8, w8
    umull   x10, w8, w8
    movz    x11, #0x8000, lsl #48
    mov     x12, #-1
    sdiv    x13, x11, x12
    udiv    x14, x11, xzr
    msub    x15, x8, x8, x11
    expect  x5, 0xfffffffffffffffe, "umulh"
    expect  x6, 0, "smulh"
    expect  x9, 25, "smull"
    expect  x10, 0xfffffff600000019, "umull"
    expect  x13, 0x8000000000000000, "sdiv INT_MIN / -1"
    expect  x14, 0, "udiv by zero is zero"
    expect  x15, 0x7fffffffffffffe7, "msub"

// ---- conditional compare and select ----------------------------------------
    mov     x3, #10
    mov     x4, #20
    cmp     x3, x4
    ccmp    x3, #10, #0b0000, lt
    cset    x5, eq
    cmp     x3, x4
    ccmp    x3, #31, #0b0100, gt
    cset    x6, eq
    cmp     x3, x4
    csneg   x9, x3, x4, eq
    cmp     x3, x4
    csinv   x10, x3, x4, hi
    cmp     x3, x4
    cinc    x11, x3, lo
    expect  x5, 1, "ccmp taken compares"
    expect  x6, 1, "ccmp not taken uses nzcv"
    expect  x9, 0xffffffffffffffec, "csneg"
    expect  x10, 0xffffffffffffffeb, "csinv"
    expect  x11, 11, "cinc"

// ---- loads and stores ------------------------------------------------------
    lea     x19, buf
    mov     x3, #-2
    strh    w3, [x19]
    ldrsh   x5, [x19]
    ldrh    w6, [x19]
    movz    w4, #0x8000, lsl #16
    str     w4, [x19, #4]
    ldrsw   x7, [x19, #4]
    mov     x8, #-8
    add     x20, x19, #16
    str     x7, [x20, x8]
    ldur    x9, [x20, #-8]
    mov     w21, #-16
    ldr     x10, [x20, w21, sxtw]
    stp     w3, w4, [x19, #32]
    ldpsw   x11, x12, [x19, #32]
    add     x13, x19, #64
    stp     x7, x8, [x13, #-16]!
    ldp     x14, x15, [x13], #16
    sub     x22, x13, x19
    expect  x5, 0xfffffffffffffffe, "ldrsh"
    expect  x6, 0xfffe, "ldrh"
    expect  x7, 0xffffffff80000000, "ldrsw"
    expect  x9, 0xffffffff80000000, "register offset store + ldur"
    expect  x10, 0x800000000000fffe, "ldr with sxtw offset"
    expect  x11, 0xfffffffffffffffe, "ldpsw first"
    expect  x12, 0xffffffff80000000, "ldpsw second"
    expect  x14, 0xffffffff80000000, "stp pre-index / ldp post-index"
    expect  x15, 0xfffffffffffffff8, "ldp second register"
    expect  x22, 64, "post-index writeback"

    ldr     x23, lit64
    ldr     w24, lit32
    ldrsw   x25, lit32
    expect  x23, 0x0123456789abcdef, "ldr literal 64-bit"
    expect  x24, 0x89abcdef, "ldr literal 32-bit"
    expect  x25, 0xffffffff89abcdef, "ldrsw literal"

    add     x26, x19, #128
    mov     x3, #-1
    str     x3, [x26, #8]
    dc      zva, x26
    ldr     x4, [x26, #8]
    expect  x4, 0, "dc zva zeroes the block"

// ---- exclusives and atomics ------------------------------------------------
    add     x20, x19, #192
    str     xzr, [x20]
    mov     x4, #5
1:  ldaxr   x5, [x20]
    add     x5, x5, #1
    stlxr   w6, x5, [x20]
    cbnz    w6, 1b
    subs    x4, x4, #1
    b.ne    1b
    ldar    x7, [x20]
    expect  x7, 5, "ldaxr/stlxr increment loop"

    mov     x8, #3
    ldadd   x8, x9, [x20]
    mov     x10, #0x10
    ldset   x10, x11, [x20]
    mov     x10, #0x08
    ldclr   x10, x12, [x20]
    mov     x10, #0xff
    ldeor   x10, x13, [x20]
    mov     x10, #-1
    ldsmax  x10, x14, [x20]
    mov     x10, #7
    ldumin  x10, x15, [x20]
    mov     x10, #42
    swp     x10, x21, [x20]
    mov     x22, #42
    mov     x23, #100
    casal   x22, x23, [x20]
    mov     x24, #1
    mov     x25, #200
    cas     x24, x25, [x20]
    ldr     x26, [x20]
    expect  x9, 5, "ldadd returns old"
    expect  x11, 8, "ldset returns old"
    expect  x12, 0x18, "ldclr returns old"
    expect  x13, 0x10, "ldeor returns old"
    expect  x14, 0xef, "ldsmax returns old"
    expect  x15, 0xef, "ldumin returns old"
    expect  x21, 7, "swp returns old"
    expect  x22, 42, "casal returns old on match"
    expect  x24, 100, "cas returns current on mismatch"
    expect  x26, 100, "cas mismatch leaves memory"

    mov     w3, #-1
    str     w3, [x20]
    mov     w4, #1
    ldadd   w4, w5, [x20]
    ldr     w6, [x20]
    expect  x5, 0xffffffff, "ldadd 32-bit old value"
    expect  x6, 0, "ldadd 32-bit wraps"

    add     x21, x20, #16
    mov     x3, #1
    mov     x4, #2
    ldxp    x5, x6, [x21]
    stxp    w7, x3, x4, [x21]
    ldp     x8, x9, [x21]
    expect  x7, 0, "stxp succeeds"
    expect  x8, 1, "stxp first"
    expect  x9, 2, "stxp second"
    stxr    w10, x3, [x21]
    expect  x10, 1, "stxr without monitor fails"

// ---- branches --------------------------------------------------------------
    movz    x3, #0x8000, lsl #48
    mov     x5, #0
    tbnz    x3, #63, 1f
    mov     x5, #99
1:  tbz     x3, #62, 2f
    mov     x5, #98
2:  movz    x4, #1, lsl #32
    cbnz    w4, 3f
    add     x5, x5, #1
3:  expect  x5, 1, "tbz/tbnz/cbnz on bit and width"

    adr     x7, jump_table
    mov     x8, #2
    ldrsw   x9, [x7, x8, lsl #2]
    add     x7, x7, x9
    br      x7
jump_table:
    .word   jt0 - jump_table, jt1 - jump_table, jt2 - jump_table
jt0: mov    x10, #10
    b       jt_done
jt1: mov    x10, #11
    b       jt_done
jt2: mov    x10, #12
jt_done:
    expect  x10, 12, "br through jump table"

    adr     x11, double_it
    mov     x12, #21
    blr     x11
    expect  x12, 42, "blr and ret"

// ---- system registers ------------------------------------------------------
    mrs     x3, tpidrro_el0
    cmp     x3, #0
    cset    x4, ne
    mov     x5, #0x1234
    msr     tpidr_el0, x5
    mrs     x6, tpidr_el0
    mrs     x7, cntvct_el0
    nop
    nop
    mrs     x8, cntvct_el0
    cmp     x8, x7
    cset    x9, hi
    mrs     x10, cntfrq_el0
    cmp     x10, #0
    cset    x11, ne
    mov     x12, #0x60000000
    msr     nzcv, x12
    cset    x13, eq
    cset    x14, cs
    expect  x4, 1, "tpidrro_el0 holds TLS pointer"
    expect  x6, 0x1234, "tpidr_el0 round trip"
    expect  x9, 1, "cntvct_el0 advances"
    expect  x11, 1, "cntfrq_el0 non-zero"
    expect  x13, 1, "msr nzcv sets Z"
    expect  x14, 1, "msr nzcv sets C"

// ---- SIMD register moves ---------------------------------------------------
    ldr     x3, =0x1122334455667788
    fmov    d0, x3
    dup     v1.4s, w3
    umov    x4, v1.d[1]
    mov     v2.16b, v0.16b
    mov     v2.d[1], x3
    umov    x5, v2.d[1]
    movi    v3.2d, #0xff00ff00ff00ff00
    umov    x6, v3.d[0]
    movi    v4.4s, #0xab, lsl #8
    umov    x7, v4.d[0]
    mvni    v5.4s, #0x12
    umov    x8, v5.d[1]
    mov     v6.16b, v3.16b
    bsl     v6.16b, v0.16b, v4.16b
    umov    x9, v6.d[0]
    fmov    w10, s0
    eor     v7.16b, v0.16b, v0.16b
    umov    x11, v7.d[0]
    expect  x4, 0x5566778855667788, "dup 4s"
    expect  x5, 0x1122334455667788, "ins/umov d[1]"
    expect  x6, 0xff00ff00ff00ff00, "movi 2d byte mask"
    expect  x7, 0x0000ab000000ab00, "movi 4s shifted"
    expect  x8, 0xffffffedffffffed, "mvni 4s"
    expect  x9, 0x1100330055007700, "bsl"
    expect  x10, 0x55667788, "fmov w from s"
    expect  x11, 0, "eor vector clears"

// ---- floating point: NaNs ----------------------------------------------------
// FPProcessNaNs: the first signalling NaN wins, then the first quiet NaN;
// a signalling NaN is returned with its quiet bit set.
    ldr     x3, =0x7ff8000000000123     // quiet NaN
    fmov    d1, x3
    ldr     x3, =0x7ff0000000000456     // signalling NaN
    fmov    d2, x3
    fmov    d3, #1.0
    ldr     w3, =0x7f800456             // single signalling NaN
    fmov    s4, w3
    fmov    s5, #1.0
    fadd    d6, d1, d3
    fmov    x4, d6
    fadd    d6, d1, d2
    fmov    x5, d6
    fmul    s6, s4, s5
    fmov    w6, s6
    fmax    d6, d1, d3
    fmov    x7, d6
    fmaxnm  d6, d1, d3
    fmov    x8, d6
    ldr     x3, =0x7ff0000000000000     // +inf
    fmov    d7, x3
    fsub    d6, d7, d7
    fmov    x9, d6
    fcmp    d1, d3
    mrs     x10, nzcv
    fcvtzs  x11, d1
    expect  x4, 0x7ff8000000000123, "fadd propagates quiet NaN"
    expect  x5, 0x7ff8000000000456, "signalling NaN wins over quiet"
    expect  x6, 0x7fc00456, "fmul quietens single signalling NaN"
    expect  x7, 0x7ff8000000000123, "fmax returns the NaN"
    expect  x8, 0x3ff0000000000000, "fmaxnm prefers the number"
    expect  x9, 0x7ff8000000000000, "inf - inf is the default NaN"
    expect  x10, 0x30000000, "fcmp unordered sets C and V"
    expect  x11, 0, "fcvtzs of NaN is zero"

// FPCR.DN: every processed NaN becomes the default NaN; FNEG is not NaN
// processing, so it still flips the sign.
    mov     x3, #0x2000000
    msr     fpcr, x3
    fadd    d6, d1, d3
    fmov    x4, d6
    fneg    d6, d1
    fmov    x5, d6
    msr     fpcr, xzr
    expect  x4, 0x7ff8000000000000, "DN gives the default NaN"
    expect  x5, 0xfff8000000000123, "fneg flips a NaN under DN"

// ---- floating point: subnormals and flush-to-zero ----------------------------
    ldr     x3, =0x0010000000000000     // smallest normal double, 2^-1022
    fmov    d1, x3
    fmov    d2, #0.5
    mov     x3, #1                      // smallest subnormal double, 2^-1074
    fmov    d3, x3
    mov     w3, #1                      // smallest subnormal single, 2^-149
    fmov    s4, w3
    ldr     w3, =0x80800000             // -2^-126
    fmov    s5, w3
    fmov    s6, #0.5
    fmul    d7, d1, d2                  // 2^-1023: a subnormal
    fmov    x4, d7
    fadd    s7, s4, s4                  // 2^-148
    fmov    w5, s7
    fcvt    d7, s4                      // 2^-149 is a normal double
    fmov    x6, d7
    fcvt    s7, d3                      // far below single range: +0
    fmov    w7, s7
    fmov    d7, #-1.0
    fmax    d7, d3, d7
    fmov    x8, d7
    expect  x4, 0x0008000000000000, "subnormal product"
    expect  x5, 0x00000002, "subnormal sum"
    expect  x6, 0x36a0000000000000, "single subnormal widens exactly"
    expect  x7, 0, "double subnormal narrows to zero"
    expect  x8, 1, "fmax keeps a subnormal"
// FPCR.FZ: subnormal inputs and results (judged before rounding) become
// zero with their sign.
    mov     x3, #0x1000000
    msr     fpcr, x3
    fmov    d7, xzr
    fadd    d7, d3, d7
    fmov    x4, d7
    fmul    d7, d1, d2
    fmov    x5, d7
    fmul    s7, s5, s6
    fmov    w6, s7
    fcvt    d7, s4
    fmov    x7, d7
    fmov    d7, #-1.0
    fmax    d7, d3, d7
    fmov    x8, d7
    fmov    d7, #2.0
    fmul    d7, d1, d7                  // 2^-1021 is normal: kept
    fmov    x9, d7
    msr     fpcr, xzr
    expect  x4, 0, "FZ flushes a subnormal input"
    expect  x5, 0, "FZ flushes a subnormal result"
    expect  x6, 0x80000000, "FZ keeps the sign"
    expect  x7, 0, "FZ flushes fcvt input"
    expect  x8, 0, "FZ: fmax sees +0"
    expect  x9, 0x0020000000000000, "FZ keeps normal results"

// ---- saturation and FPSR.QC --------------------------------------------------
    msr     fpsr, xzr
    movi    v1.16b, #0x7f
    movi    v2.16b, #0x01
    add     v3.16b, v1.16b, v2.16b      // wraps, never saturates
    mrs     x4, fpsr
    sqadd   b3, b1, b2                  // 0x7f + 1 saturates to 0x7f
    umov    w5, v3.b[0]
    mrs     x6, fpsr
    msr     fpsr, xzr
    movi    v1.4s, #1
    movi    v2.4s, #2
    uqsub   v3.4s, v1.4s, v2.4s         // 1 - 2 saturates to 0
    umov    x7, v3.d[1]
    mrs     x8, fpsr
    msr     fpsr, xzr
    ldr     w3, =0x00012345
    fmov    s1, w3
    sqxtn   h3, s1                      // 0x12345 saturates to 0x7fff
    umov    w9, v3.h[0]
    movi    v1.8h, #0x80, lsl #8        // -32768 in every lane
    sqdmulh v3.8h, v1.8h, v1.8h         // 2 * (-2^15)^2 >> 16 saturates
    umov    w10, v3.h[7]
    movi    v1.16b, #0x80
    sqabs   v3.16b, v1.16b              // |-128| saturates to 127
    umov    w11, v3.b[15]
    mrs     x12, fpsr
    msr     fpsr, xzr
    expect  x4, 0, "wrapping add leaves QC clear"
    expect  x5, 0x7f, "sqadd saturates"
    expect  x6, 0x8000000, "sqadd sets QC"
    expect  x7, 0, "uqsub saturates at zero"
    expect  x8, 0x8000000, "uqsub sets QC"
    expect  x9, 0x7fff, "sqxtn saturates"
    expect  x10, 0x7fff, "sqdmulh saturates"
    expect  x11, 0x7f, "sqabs saturates"
    expect  x12, 0x8000000, "QC is sticky"

// ---- summary ---------------------------------------------------------------
    lea     x0, msg_head
    bl      print
    mov     x0, x27
    bl      print_u64
    lea     x0, msg_mid
    bl      print
    mov     x0, x28
    bl      print_u64
    lea     x0, msg_tail
    bl      print
    svc     #0x07

// ---- helpers ---------------------------------------------------------------
double_it:
    add     x12, x12, x12
    ret

// print(x0 = NUL-terminated string). Clobbers x0-x2.
print:
    mov     x1, #0
1:  ldrb    w2, [x0, x1]
    cbz     w2, 2f
    add     x1, x1, #1
    b       1b
2:  svc     #0x27
    ret

// print_u64(x0). Clobbers x0-x4.
print_u64:
    stp     x29, x30, [sp, #-48]!
    add     x1, sp, #47
    strb    wzr, [x1]
    mov     x2, #10
1:  udiv    x3, x0, x2
    msub    x4, x3, x2, x0
    add     w4, w4, #'0'
    strb    w4, [x1, #-1]!
    mov     x0, x3
    cbnz    x0, 1b
    mov     x0, x1
    bl      print
    ldp     x29, x30, [sp], #48
    ret

// fail(x0 = check name): print it and count the failure.
fail:
    stp     x29, x30, [sp, #-32]!
    str     x0, [sp, #16]
    lea     x0, msg_fail
    bl      print
    ldr     x0, [sp, #16]
    bl      print
    lea     x0, msg_nl
    bl      print
    add     x28, x28, #1
    ldp     x29, x30, [sp], #32
    ret

    .balign 8
lit64:  .quad 0x0123456789abcdef
lit32:  .word 0x89abcdef
    .ltorg

    .section .rodata
msg_head: .asciz "asm checks: "
msg_mid:  .asciz " passed, "
msg_tail: .asciz " failed\n"
msg_fail: .asciz "FAIL: "
msg_nl:   .asciz "\n"

    .bss
    .balign 64
buf:    .zero 256
