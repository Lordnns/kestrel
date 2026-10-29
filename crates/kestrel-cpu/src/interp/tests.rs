//! Instruction-level tests. Encodings were produced with
//! `llvm-mc --triple=aarch64` from the assembly shown in each comment, so
//! every test checks the interpreter against a real assembler's output.

use super::Interpreter;
use crate::{CpuBackend, CpuState, Exit};
use kestrel_core::{Memory, Perm};

const CODE: u64 = 0x1_0000;
const STACK_BASE: u64 = 0x20_0000;
const SP: u64 = STACK_BASE + 0x8000;

pub(super) fn setup(words: &[u32]) -> (CpuState, Memory) {
    let mut m = Memory::new();
    m.map(CODE, 0x1000, Perm::RX, "code").unwrap();
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    m.poke_bytes(CODE, &bytes).unwrap();
    m.map(STACK_BASE, 0x1_0000, Perm::RW, "stack").unwrap();
    let s = CpuState {
        pc: CODE,
        sp: SP,
        ..Default::default()
    };
    (s, m)
}

/// Run until the trailing BRK and return the final state.
fn run_with(words: &[u32], init: impl FnOnce(&mut CpuState)) -> (CpuState, Memory) {
    let (mut s, mut m) = setup(words);
    init(&mut s);
    let exit = Interpreter::new().run(&mut s, &mut m, 10_000);
    assert_eq!(exit, Exit::Breakpoint(0), "unexpected exit, state:\n{s:?}");
    (s, m)
}

fn run(words: &[u32]) -> CpuState {
    run_with(words, |_| {}).0
}

#[test]
fn counting_loop() {
    // mov x0,#0; mov x1,#10; 1: add x0,x0,x1; subs x1,x1,#1; b.ne 1b; brk #0
    let s = run(&[
        0xd2800000, 0xd2800141, 0x8b010000, 0xf1000421, 0x54ffffc1, 0xd4200000,
    ]);
    assert_eq!(s.x[0], 55);
    assert_eq!(s.x[1], 0);
    assert!(s.flags.z);
}

#[test]
fn bit_manipulation() {
    // movz/movk x0=0x12345678; and x1,x0,#0xff; ubfx x2,x0,#8,#8; mov w4,#0xf0;
    // sbfx x5,x4,#4,#4; lsl x6,x0,#4; orr w8,wzr,#0x55555555; extr x9,x0,x0,#8;
    // rev w10,w0; clz x11,x0; asr x12,x5,#3; bfi x13,x0,#8,#8; sxtw x14,w5; brk
    let s = run(&[
        0xd28acf00, 0xf2a24680, 0x92401c01, 0xd3483c02, 0x52801e04, 0x93441c85, 0xd37cec06,
        0x3200f3e8, 0x93c02009, 0x5ac0080a, 0xdac0100b, 0x9343fcac, 0xb3781c0d, 0x93407cae,
        0xd4200000,
    ]);
    assert_eq!(s.x[0], 0x1234_5678);
    assert_eq!(s.x[1], 0x78);
    assert_eq!(s.x[2], 0x56);
    assert_eq!(s.x[5], u64::MAX);
    assert_eq!(s.x[6], 0x1_2345_6780);
    assert_eq!(s.x[8], 0x5555_5555);
    assert_eq!(s.x[9], 0x7800_0000_0012_3456);
    assert_eq!(s.x[10], 0x7856_3412);
    assert_eq!(s.x[11], 35);
    assert_eq!(s.x[12], u64::MAX);
    assert_eq!(s.x[13], 0x7800);
    assert_eq!(s.x[14], u64::MAX);
}

#[test]
fn loads_and_stores() {
    // mov x0,#1; mov x1,#2; stp x0,x1,[sp,#-16]!; ldr x2,[sp,#8]; ldp x3,x4,[sp],#16;
    // mov w5,#0x80; strb w5,[sp,#-1]!; ldrsb x6,[sp]; ldrb w7,[sp],#1; mov x9,#3;
    // str x9,[sp,#-16]!; add x10,sp,#0; ldr x11,[x10,x9]; ldr x12,[sp,x9,lsl #3]; brk
    let s = run(&[
        0xd2800020, 0xd2800041, 0xa9bf07e0, 0xf94007e2, 0xa8c113e3, 0x52801005, 0x381fffe5,
        0x398003e6, 0x384017e7, 0xd2800069, 0xf81f0fe9, 0x910003ea, 0xf869694b, 0xf8697bec,
        0xd4200000,
    ]);
    assert_eq!(s.x[2], 2);
    assert_eq!((s.x[3], s.x[4]), (1, 2));
    assert_eq!(s.x[6], 0xffff_ffff_ffff_ff80);
    assert_eq!(s.x[7], 0x80);
    assert_eq!(s.x[10], SP - 16);
    // Unaligned 8-byte load at sp+3 spans the 3 we stored and the 2 from the pair.
    assert_eq!(s.x[11], 2 << 40);
    assert_eq!(s.x[12], 0);
    assert_eq!(s.sp, SP - 16);
}

#[test]
fn multiply_divide() {
    // mov x0,#100; mov x1,#7; udiv x2,x0,x1; msub x3,x2,x1,x0; mov x4,#-100;
    // sdiv x5,x4,x1; mov x7,#-1; umulh x6,x7,x7; udiv x8,x0,xzr; smull x9,w4,w1;
    // madd w10,w0,w1,w1; brk
    let s = run(&[
        0xd2800c80, 0xd28000e1, 0x9ac10802, 0x9b018043, 0x92800c64, 0x9ac10c85, 0x92800007,
        0x9bc77ce6, 0x9adf0808, 0x9b217c89, 0x1b01040a, 0xd4200000,
    ]);
    assert_eq!(s.x[2], 14);
    assert_eq!(s.x[3], 2);
    assert_eq!(s.x[5] as i64, -14);
    assert_eq!(s.x[6], 0xffff_ffff_ffff_fffe);
    assert_eq!(s.x[8], 0, "division by zero yields zero");
    assert_eq!(s.x[9] as i64, -700);
    assert_eq!(s.x[10], 707);
}

#[test]
fn call_and_return() {
    // mov x0,#5; bl f; add x0,x0,#1; brk; f: lsl x0,x0,#1; ret
    let s = run(&[
        0xd28000a0, 0x94000003, 0x91000400, 0xd4200000, 0xd37ff800, 0xd65f03c0,
    ]);
    assert_eq!(s.x[0], 11);
    assert_eq!(s.x[30], CODE + 8);
}

#[test]
fn conditional_ops() {
    // mov x0,#3; mov x1,#5; cmp x0,x1; csel x2,x0,x1,lt; cset x3,gt;
    // csinc x4,x0,x1,eq; ccmp x0,#3,#0,lt; cset x5,eq; cneg x6,x0,ne; brk
    let s = run(&[
        0xd2800060, 0xd28000a1, 0xeb01001f, 0x9a81b002, 0x9a9fd7e3, 0x9a810404, 0xfa43b800,
        0x9a9f17e5, 0xda800406, 0xd4200000,
    ]);
    assert_eq!(s.x[2], 3);
    assert_eq!(s.x[3], 0);
    assert_eq!(s.x[4], 6);
    assert_eq!(s.x[5], 1);
    assert_eq!(s.x[6], 3);
}

#[test]
fn test_bit_and_compare_branches() {
    // mov x0,#0x10; mov x1,#0; tbz x0,#4,1f; mov x1,#1; 1: tbnz x0,#3,2f;
    // add x1,x1,#2; 2: cbz x0,3f; add x1,x1,#4; 3: brk
    let s = run(&[
        0xd2800200, 0xd2800001, 0x36200040, 0xd2800021, 0x37180040, 0x91000821, 0xb4000040,
        0x91001021, 0xd4200000,
    ]);
    assert_eq!(s.x[1], 7);
}

#[test]
fn exclusives_and_atomics() {
    // mov x1,#42; ldxr x2,[sp]; stxr w3,x1,[sp]; ldr x4,[sp]; mov x5,#8;
    // ldadd x5,x6,[sp]; ldr x7,[sp]; mov x8,#50; mov x9,#99; cas x8,x9,[sp];
    // ldr x10,[sp]; stxr w11,x1,[sp]; brk
    let s = run(&[
        0xd2800541, 0xc85f7fe2, 0xc8037fe1, 0xf94003e4, 0xd2800105, 0xf82503e6, 0xf94003e7,
        0xd2800648, 0xd2800c69, 0xc8a87fe9, 0xf94003ea, 0xc80b7fe1, 0xd4200000,
    ]);
    assert_eq!(s.x[3], 0, "store-exclusive after load-exclusive succeeds");
    assert_eq!(s.x[4], 42);
    assert_eq!(s.x[6], 42);
    assert_eq!(s.x[7], 50);
    assert_eq!(s.x[8], 50);
    assert_eq!(s.x[10], 99);
    assert_eq!(s.x[11], 1, "store-exclusive without a monitor fails");
}

#[test]
fn svc_exits_and_resumes() {
    // mov x0,#7; svc #0x27; mov x0,#9; brk
    let (mut s, mut m) = setup(&[0xd28000e0, 0xd40004e1, 0xd2800120, 0xd4200000]);
    let mut cpu = Interpreter::new();
    assert_eq!(cpu.run(&mut s, &mut m, 100), Exit::Svc(0x27));
    assert_eq!(s.x[0], 7);
    assert_eq!(s.pc, CODE + 8);
    assert_eq!(cpu.run(&mut s, &mut m, 100), Exit::Breakpoint(0));
    assert_eq!(s.x[0], 9);
}

#[test]
fn pc_relative_addresses() {
    // nop; adr x0,.; adrp x1,.+0x2000; brk
    let s = run(&[0xd503201f, 0x10000000, 0xd0000001, 0xd4200000]);
    assert_eq!(s.x[0], CODE + 4);
    assert_eq!(s.x[1], ((CODE + 8) & !0xfff) + 0x2000);
}

#[test]
fn simd_moves() {
    // fmov d0,x0; mov v1.16b,v0.16b; umov x1,v1.d[0]; movi v2.2d,#0; dup v3.16b,w0;
    // umov x2,v3.d[1]; mov v2.d[1],x0; umov x3,v2.d[1]; movi v4.16b,#0x5a; fmov x4,d4; brk
    let (s, _) = run_with(
        &[
            0x9e670000, 0x4ea01c01, 0x4e083c21, 0x6f00e402, 0x4e010c03, 0x4e183c62, 0x4e181c02,
            0x4e183c43, 0x4f02e744, 0x9e660084, 0xd4200000,
        ],
        |s| s.x[0] = 0x1122_3344_5566_7788,
    );
    assert_eq!(s.x[1], 0x1122_3344_5566_7788);
    assert_eq!(s.x[2], 0x8888_8888_8888_8888);
    assert_eq!(s.x[3], 0x1122_3344_5566_7788);
    assert_eq!(s.x[4], 0x5a5a_5a5a_5a5a_5a5a);
}

#[test]
fn system_registers() {
    // mov x0,#0x1000; msr tpidr_el0,x0; mrs x1,tpidr_el0; mov x2,#0x20000000;
    // msr nzcv,x2; cset x3,cs; mrs x4,dczid_el0; dmb ish; brk
    let s = run(&[
        0xd2820000, 0xd51bd040, 0xd53bd041, 0xd2a40002, 0xd51b4202, 0x9a9f37e3, 0xd53b00e4,
        0xd5033bbf, 0xd4200000,
    ]);
    assert_eq!(s.x[1], 0x1000);
    assert_eq!(s.x[3], 1);
    assert_eq!(s.x[4], 4);
}

#[test]
fn hints_barriers_and_cache_maintenance() {
    // paciasp; bti c; isb; dmb ish; dsb sy; clrex; ic ivau,x0; dc civac,x0;
    // dc cvau,x0; brk
    let s = run_with(
        &[
            0xd503233f, 0xd503245f, 0xd5033fdf, 0xd5033bbf, 0xd5033f9f, 0xd5033f5f, 0xd50b7520,
            0xd50b7e20, 0xd50b7b20, 0xd4200000,
        ],
        |s| {
            s.x[0] = STACK_BASE;
            s.exclusive_addr = Some(STACK_BASE);
        },
    )
    .0;
    assert_eq!(s.exclusive_addr, None, "clrex clears the monitor");
}

#[test]
fn unsupported_system_instructions_are_reported() {
    for insn in [
        0xd50342df, // msr daifset, #2
        0xd503413f, // msr ssbs, #1
        0xd52b7520, // sysl x0, #3, c7, c5, #1
        0xd508751f, // ic iallu (EL1 only)
        0xd5380000, // mrs x0, midr_el1
    ] {
        let (mut s, mut m) = setup(&[insn]);
        let exit = Interpreter::new().run(&mut s, &mut m, 10);
        assert_eq!(exit, Exit::Undefined { pc: CODE, insn }, "{insn:08x}");
    }
}

#[test]
fn unknown_encoding_is_reported() {
    // add v0.4s, v1.4s, v2.4s (NEON integer arithmetic is not implemented yet)
    let (mut s, mut m) = setup(&[0x4ea28420]);
    let exit = Interpreter::new().run(&mut s, &mut m, 10);
    assert_eq!(
        exit,
        Exit::Undefined {
            pc: CODE,
            insn: 0x4ea28420
        }
    );
}

#[test]
fn fault_on_unmapped_access() {
    // ldr x0, [xzr]  -> encoded as ldr x0, [x1] with x1 = 0
    let (mut s, mut m) = setup(&[0xf9400020]);
    let exit = Interpreter::new().run(&mut s, &mut m, 10);
    assert!(matches!(exit, Exit::Fault { pc: CODE, .. }));
}
