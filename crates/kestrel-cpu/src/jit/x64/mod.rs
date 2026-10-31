//! IR to x86_64 machine code.
//!
//! A block becomes one host function, `fn(state, ctx) -> u64`, in either
//! calling convention: the Windows x64 one (the product's host first) or
//! System V (Linux). Both can be generated and run on any x86_64 host,
//! since Rust calls either, so the tests run Windows code on Linux.
//!
//! Registers while a block runs:
//! - RBX: the guest's `CpuState`; guest registers, flags, PC and the
//!   retired count are read and written there, so the interpreter and
//!   the OS layer see them whenever the block calls out or returns.
//! - R12: the [`JitCtx`](super::JitCtx), passed to every helper.
//! - RBP, R13, R14, R15: IR values (callee-saved in both conventions, so
//!   they survive helper calls); further values live in stack slots.
//! - RAX, RCX, RDX, R8-R11: scratch, and the helpers' arguments.
//!
//! The function returns a code saying how the block ended
//! (`RET_*` below), with `CpuState::pc` set to where the guest goes on.

pub mod asm;

use super::ir::{BinOp, Block, Carry, Op, Reg, Term, Val, Width};
use asm::*;

/// The block left normally; `CpuState::pc` is the next guest address.
pub const RET_JUMP: u64 = 1;
/// The block stopped with an `Exit` (or a fault) left in the context.
pub const RET_EXIT: u64 = 2;
/// SVC: the immediate is in bits 8 and up.
pub const RET_SVC: u64 = 3;
/// BRK: the immediate is in bits 8 and up.
pub const RET_BRK: u64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abi {
    /// System V AMD64 (Linux, macOS).
    SysV,
    /// The Windows x64 calling convention.
    Win64,
}

impl Abi {
    /// The host's convention.
    pub fn host() -> Self {
        if cfg!(windows) {
            Abi::Win64
        } else {
            Abi::SysV
        }
    }

    fn args(self) -> [R; 3] {
        match self {
            Abi::SysV => [RDI, RSI, RDX],
            Abi::Win64 => [RCX, RDX, R8],
        }
    }
}

/// Addresses of the helper functions generated code calls, in the
/// block's calling convention.
#[derive(Clone, Copy)]
pub struct Helpers {
    /// `read(ctx, addr) -> value` for 1, 2, 4 and 8 bytes.
    pub read: [usize; 4],
    /// `write(ctx, addr, value)` for 1, 2, 4 and 8 bytes.
    pub write: [usize; 4],
    /// `interp(ctx, pc, insn) -> 0 (go on), RET_JUMP or RET_EXIT`.
    pub interp: usize,
}

/// Where host code finds things, as byte offsets.
#[derive(Clone, Copy)]
pub struct Layout {
    /// In `CpuState`.
    pub x: i32,
    pub sp: i32,
    pub pc: i32,
    pub retired: i32,
    /// N, Z, C and V, one byte each (0 or 1).
    pub n: i32,
    pub z: i32,
    pub c: i32,
    pub v: i32,
    /// In the context: a byte set when a helper's access faulted.
    pub faulted: i32,
}

const STATE: R = RBX;
const CTX: R = R12;
const POOL: [R; 4] = [RBP, R13, R14, R15];
/// Bytes below the slots: the Windows convention's shadow space for the
/// four register arguments, kept in both conventions.
const SHADOW: i32 = 32;

#[derive(Clone, Copy, Debug)]
enum Loc {
    Reg(R),
    /// `[rsp + disp]`.
    Slot(i32),
    Imm(u64),
    /// Never read.
    Unused,
}

struct Gen<'a> {
    a: Asm,
    abi: Abi,
    h: &'a Helpers,
    l: &'a Layout,
    loc: Vec<Loc>,
    /// Instructions of the block already added to `CpuState::retired`.
    flushed: u32,
    index: u32,
    pc: u64,
    /// Jumps to the shared epilogue, which returns RAX.
    to_epilogue: Vec<Fixup>,
    /// Faulting accesses: (jump, pc, instructions to retire).
    faults: Vec<(Fixup, u64, u32)>,
}

/// Generate the host code for `block`.
pub fn compile(block: &Block, abi: Abi, h: &Helpers, l: &Layout) -> Vec<u8> {
    let (loc, slots) = allocate(block);
    // On entry RSP is 8 past a multiple of 16; six pushes keep it so, and
    // the frame must restore the alignment for calls.
    let mut frame = SHADOW + 8 * slots as i32;
    if frame % 16 != 8 {
        frame += 8;
    }
    let mut g = Gen {
        a: Asm::new(),
        abi,
        h,
        l,
        loc,
        flushed: 0,
        index: 0,
        pc: block.pc,
        to_epilogue: Vec::new(),
        faults: Vec::new(),
    };
    for r in [RBX, RBP, R12, R13, R14, R15] {
        g.a.push(r);
    }
    g.a.alu_ri(Alu::Sub, W64, RSP, frame);
    let [arg0, arg1, _] = abi.args();
    g.a.mov_rr(W64, STATE, arg0);
    g.a.mov_rr(W64, CTX, arg1);
    for op in &block.ops {
        g.op(op);
    }
    g.term(&block.term, block.insns);
    // The epilogue, reached by falling through from the terminator.
    let epilogue = g.a.pos();
    for f in std::mem::take(&mut g.to_epilogue) {
        g.a.bind_to(f, epilogue);
    }
    g.a.alu_ri(Alu::Add, W64, RSP, frame);
    for r in [R15, R14, R13, R12, RBP, RBX] {
        g.a.pop(r);
    }
    g.a.ret();
    // Fault exits: the guest stops at the faulting instruction.
    for (f, pc, retire) in std::mem::take(&mut g.faults) {
        g.a.bind(f);
        g.retire(retire);
        g.set_pc_imm(pc);
        g.a.mov_ri(RAX, RET_EXIT);
        let j = g.a.jmp();
        g.a.bind_to(j, epilogue);
    }
    g.a.code
}

/// Give every value a place: a constant stays an immediate; others take a
/// pool register free from their definition to their last use, or a
/// stack slot. Returns the places and the number of slots.
fn allocate(block: &Block) -> (Vec<Loc>, usize) {
    let n = block.vals as usize;
    let mut last = vec![None; n];
    let mut uses = |v: Val, at: usize| last[v.0 as usize] = Some(at);
    for (i, op) in block.ops.iter().enumerate() {
        for v in operands(op) {
            uses(v, i);
        }
    }
    match block.term {
        Term::JumpReg(v) | Term::Branch { c: v, .. } => uses(v, block.ops.len()),
        _ => {}
    }
    let mut loc = vec![Loc::Unused; n];
    let mut free: Vec<R> = POOL.iter().rev().copied().collect();
    let mut live: Vec<(usize, R)> = Vec::new(); // (last use, register)
    let mut slots = 0;
    for (i, op) in block.ops.iter().enumerate() {
        // Values whose last use is this operation give their register
        // back: operands are read before the result is written.
        live.retain(|&(end, r)| {
            if end <= i {
                free.push(r);
                false
            } else {
                true
            }
        });
        let Some(dst) = result(op) else { continue };
        let d = dst.0 as usize;
        if let Op::Const { value, .. } = op {
            loc[d] = Loc::Imm(*value);
            continue;
        }
        let Some(end) = last[d] else { continue };
        loc[d] = match free.pop() {
            Some(r) => {
                live.push((end, r));
                Loc::Reg(r)
            }
            None => {
                slots += 1;
                Loc::Slot(SHADOW + 8 * (slots as i32 - 1))
            }
        };
    }
    (loc, slots)
}

fn result(op: &Op) -> Option<Val> {
    match *op {
        Op::Const { dst, .. }
        | Op::Get { dst, .. }
        | Op::Bin { dst, .. }
        | Op::Adc { dst, .. }
        | Op::Not { dst, .. }
        | Op::Extend { dst, .. }
        | Op::Cond { dst, .. }
        | Op::IsZero { dst, .. }
        | Op::Select { dst, .. }
        | Op::GetNzcv { dst }
        | Op::Load { dst, .. } => Some(dst),
        _ => None,
    }
}

fn operands(op: &Op) -> Vec<Val> {
    match *op {
        Op::Set { src, .. } | Op::SetNzcv { src } => vec![src],
        Op::Bin { a, b, .. } | Op::Adc { a, b, .. } => vec![a, b],
        Op::LogicFlags { a, .. }
        | Op::Not { a, .. }
        | Op::Extend { a, .. }
        | Op::IsZero { a, .. } => vec![a],
        Op::Select { c, a, b, .. } => vec![c, a, b],
        Op::Load { addr, .. } => vec![addr],
        Op::Store { addr, src, .. } => vec![addr, src],
        _ => Vec::new(),
    }
}

fn w(w: Width) -> W {
    match w {
        Width::W32 => W32,
        Width::W64 => W64,
    }
}

impl Gen<'_> {
    fn st(&self, off: i32) -> M {
        M(STATE, off)
    }

    fn reg_off(&self, reg: Reg) -> i32 {
        match reg {
            Reg::X(n) => self.l.x + 8 * n as i32,
            Reg::Sp => self.l.sp,
        }
    }

    /// Put `v` in `r`.
    fn get(&mut self, r: R, v: Val) {
        match self.loc[v.0 as usize] {
            Loc::Reg(s) => {
                if s != r {
                    self.a.mov_rr(W64, r, s)
                }
            }
            Loc::Slot(d) => self.a.load(W64, r, M(RSP, d)),
            Loc::Imm(k) => self.a.mov_ri(r, k),
            Loc::Unused => unreachable!("a value read before it is set"),
        }
    }

    /// Store `r` as the value `v`.
    fn put(&mut self, v: Val, r: R) {
        match self.loc[v.0 as usize] {
            Loc::Reg(d) => self.a.mov_rr(W64, d, r),
            Loc::Slot(d) => self.a.store(W64, M(RSP, d), r),
            Loc::Unused => {}
            Loc::Imm(_) => unreachable!("a constant is never written"),
        }
    }

    /// `op dst, v` for an ALU operation, with `v` as an immediate,
    /// register or memory operand as it is.
    fn alu(&mut self, op: Alu, ww: W, dst: R, v: Val) {
        match self.loc[v.0 as usize] {
            Loc::Reg(s) => self.a.alu_rr(op, ww, dst, s),
            Loc::Slot(d) => self.a.alu_rm(op, ww, dst, M(RSP, d)),
            Loc::Imm(k) => {
                let k = if ww == W32 {
                    k as u32 as i32 as i64 as u64
                } else {
                    k
                };
                match i32::try_from(k as i64) {
                    Ok(i) => self.a.alu_ri(op, ww, dst, i),
                    Err(_) => {
                        self.a.mov_ri(RCX, k);
                        self.a.alu_rr(op, ww, dst, RCX);
                    }
                }
            }
            Loc::Unused => unreachable!(),
        }
    }

    /// Add the instructions retired since the last flush to
    /// `CpuState::retired`.
    fn retire(&mut self, n: u32) {
        if n > 0 {
            let m = self.st(self.l.retired);
            self.a.load(W64, RAX, m);
            self.a.alu_ri(Alu::Add, W64, RAX, n as i32);
            self.a.store(W64, m, RAX);
        }
    }

    fn set_pc_imm(&mut self, pc: u64) {
        self.a.mov_ri(RAX, pc);
        let m = self.st(self.l.pc);
        self.a.store(W64, m, RAX);
    }

    fn call(&mut self, f: usize) {
        self.a.mov_ri(RAX, f as u64);
        self.a.call_r(RAX);
    }

    /// After a helper's access: if it faulted, stop at this instruction.
    fn check_fault(&mut self) {
        self.a.load8_zx(RCX, M(CTX, self.l.faulted));
        self.a.test_rr(W32, RCX, RCX);
        let f = self.a.jcc(Cc::Ne);
        self.faults.push((f, self.pc, self.index - self.flushed));
    }

    fn flag(&self, which: char) -> M {
        let off = match which {
            'n' => self.l.n,
            'z' => self.l.z,
            'c' => self.l.c,
            _ => self.l.v,
        };
        M(STATE, off)
    }

    /// Set NZCV from the host flags of the last operation, as x86 and Arm
    /// define the same N (SF), Z (ZF), C (CF, for additions) and V (OF).
    fn flags_from_host(&mut self) {
        let (n, z, c, v) = (
            self.flag('n'),
            self.flag('z'),
            self.flag('c'),
            self.flag('v'),
        );
        self.a.setcc_m(Cc::S, n);
        self.a.setcc_m(Cc::E, z);
        self.a.setcc_m(Cc::B, c);
        self.a.setcc_m(Cc::O, v);
    }

    fn op(&mut self, op: &Op) {
        match *op {
            Op::Insn { index, pc } => {
                self.index = index;
                self.pc = pc;
            }
            Op::Const { .. } => {}
            Op::Get { dst, reg } => {
                let m = self.st(self.reg_off(reg));
                match self.loc[dst.0 as usize] {
                    Loc::Reg(r) => self.a.load(W64, r, m),
                    Loc::Unused => {}
                    _ => {
                        self.a.load(W64, RAX, m);
                        self.put(dst, RAX);
                    }
                }
            }
            Op::Set { reg, src } => {
                let m = self.st(self.reg_off(reg));
                match self.loc[src.0 as usize] {
                    Loc::Reg(r) => self.a.store(W64, m, r),
                    _ => {
                        self.get(RAX, src);
                        self.a.store(W64, m, RAX);
                    }
                }
            }
            Op::Bin {
                op,
                w: width,
                dst,
                a,
                b,
            } => self.bin(op, w(width), dst, a, b),
            Op::Adc {
                w: width,
                dst,
                a,
                b,
                carry,
                flags,
            } => {
                let ww = w(width);
                self.get(RAX, a);
                self.get(RDX, b);
                match carry {
                    Carry::Zero => self.a.clc(),
                    Carry::One => self.a.stc(),
                    Carry::Flag => {
                        let c = self.flag('c');
                        self.a.load8_zx(RCX, c);
                        self.a.bt_ri(W32, RCX, 0);
                    }
                }
                self.a.alu_rr(Alu::Adc, ww, RAX, RDX);
                if flags {
                    self.flags_from_host();
                }
                self.put(dst, RAX);
            }
            Op::LogicFlags { w: width, a } => {
                self.get(RAX, a);
                self.a.test_rr(w(width), RAX, RAX);
                let (n, z, c, v) = (
                    self.flag('n'),
                    self.flag('z'),
                    self.flag('c'),
                    self.flag('v'),
                );
                self.a.setcc_m(Cc::S, n);
                self.a.setcc_m(Cc::E, z);
                self.a.store8_imm(c, 0);
                self.a.store8_imm(v, 0);
            }
            Op::Not { w: width, dst, a } => {
                self.get(RAX, a);
                self.a.unary(Unary::Not, w(width), RAX);
                self.put(dst, RAX);
            }
            Op::Extend {
                dst,
                a,
                bits,
                signed,
            } => {
                self.get(RAX, a);
                match (bits, signed) {
                    (8, false) => self.a.movzx8(RAX, RAX),
                    (16, false) => self.a.movzx16(RAX, RAX),
                    (32, false) => self.a.mov_rr(W32, RAX, RAX),
                    (8, true) => self.a.movsx8(RAX, RAX),
                    (16, true) => self.a.movsx16(RAX, RAX),
                    (32, true) => self.a.movsxd(RAX, RAX),
                    _ => unreachable!("extension from {bits} bits"),
                }
                self.put(dst, RAX);
            }
            Op::Cond { dst, cond } => {
                self.cond(cond);
                self.put(dst, RAX);
            }
            Op::IsZero { w: width, dst, a } => {
                self.get(RAX, a);
                self.a.alu_rr(Alu::Xor, W32, RCX, RCX);
                self.a.test_rr(w(width), RAX, RAX);
                self.a.setcc_r(Cc::E, RCX);
                self.put(dst, RCX);
            }
            Op::Select {
                w: width,
                dst,
                c,
                a,
                b,
            } => {
                self.get(RDX, c);
                self.get(RAX, b);
                self.get(RCX, a);
                self.a.test_rr(W64, RDX, RDX);
                self.a.cmov(Cc::Ne, w(width), RAX, RCX);
                self.put(dst, RAX);
            }
            Op::GetNzcv { dst } => {
                self.a.alu_rr(Alu::Xor, W32, RAX, RAX);
                for (f, bit) in [('n', 31), ('z', 30), ('c', 29), ('v', 28)] {
                    let m = self.flag(f);
                    self.a.load8_zx(RCX, m);
                    self.a.shift_ri(Shift::Shl, W32, RCX, bit);
                    self.a.alu_rr(Alu::Or, W32, RAX, RCX);
                }
                self.put(dst, RAX);
            }
            Op::SetNzcv { src } => {
                self.get(RAX, src);
                for (f, bit) in [('n', 31), ('z', 30), ('c', 29), ('v', 28)] {
                    let m = self.flag(f);
                    self.a.bt_ri(W32, RAX, bit);
                    self.a.setcc_m(Cc::B, m);
                }
            }
            Op::Load { dst, addr, size } => {
                let [a0, a1, _] = self.abi.args();
                self.get(a1, addr);
                self.a.mov_rr(W64, a0, CTX);
                self.call(self.h.read[size.trailing_zeros() as usize]);
                self.check_fault();
                self.put(dst, RAX);
            }
            Op::Store { addr, src, size } => {
                let [a0, a1, a2] = self.abi.args();
                // RDX is an argument in both conventions: load it last
                // only if it is not one of the others' sources.
                self.get(a1, addr);
                self.get(a2, src);
                self.a.mov_rr(W64, a0, CTX);
                self.call(self.h.write[size.trailing_zeros() as usize]);
                self.check_fault();
            }
            Op::Interp { insn } => {
                self.retire(self.index - self.flushed);
                self.flushed = self.index;
                let [a0, a1, a2] = self.abi.args();
                self.a.mov_rr(W64, a0, CTX);
                self.a.mov_ri(a1, self.pc);
                self.a.mov_ri(a2, insn as u64);
                self.call(self.h.interp);
                self.a.test_rr(W64, RAX, RAX);
                let f = self.a.jcc(Cc::Ne);
                self.to_epilogue.push(f);
                // The interpreter retired the instruction.
                self.flushed = self.index + 1;
            }
        }
    }

    fn bin(&mut self, op: BinOp, ww: W, dst: Val, a: Val, b: Val) {
        let alu = match op {
            BinOp::Add => Some(Alu::Add),
            BinOp::Sub => Some(Alu::Sub),
            BinOp::And => Some(Alu::And),
            BinOp::Or => Some(Alu::Or),
            BinOp::Xor => Some(Alu::Xor),
            _ => None,
        };
        if let Some(alu) = alu {
            self.get(RAX, a);
            self.alu(alu, ww, RAX, b);
            self.put(dst, RAX);
            return;
        }
        let shift = match op {
            BinOp::Shl => Some(Shift::Shl),
            BinOp::Lsr => Some(Shift::Shr),
            BinOp::Asr => Some(Shift::Sar),
            BinOp::Ror => Some(Shift::Ror),
            _ => None,
        };
        if let Some(sh) = shift {
            let mask = if ww == W64 { 63 } else { 31 };
            match self.loc[b.0 as usize] {
                Loc::Imm(k) => {
                    self.get(RAX, a);
                    self.a.shift_ri(sh, ww, RAX, (k & mask) as u8);
                }
                _ => {
                    self.get(RCX, b);
                    self.get(RAX, a);
                    self.a.shift_cl(sh, ww, RAX);
                }
            }
            if ww == W32 {
                // A 32-bit shift by 0 leaves the upper half as it was.
                self.a.mov_rr(W32, RAX, RAX);
            }
            self.put(dst, RAX);
            return;
        }
        self.get(RAX, a);
        self.get(RCX, b);
        match op {
            BinOp::Mul => self.a.imul_rr(ww, RAX, RCX),
            BinOp::UMulHigh | BinOp::SMulHigh => {
                let u = if op == BinOp::UMulHigh {
                    Unary::Mul
                } else {
                    Unary::Imul
                };
                self.a.unary(u, ww, RCX);
                self.a.mov_rr(W64, RAX, RDX);
            }
            BinOp::UDiv | BinOp::SDiv => {
                // Arm's division: x / 0 = 0, and MIN / -1 = MIN, which
                // x86 would trap on.
                self.a.test_rr(ww, RCX, RCX);
                let zero = self.a.jcc(Cc::E);
                let mut done = Vec::new();
                if op == BinOp::SDiv {
                    self.a.alu_ri(Alu::Cmp, ww, RCX, -1);
                    let divide = self.a.jcc(Cc::Ne);
                    self.a.unary(Unary::Neg, ww, RAX);
                    done.push(self.a.jmp());
                    self.a.bind(divide);
                    self.a.cqo(ww);
                    self.a.unary(Unary::Idiv, ww, RCX);
                } else {
                    self.a.alu_rr(Alu::Xor, W32, RDX, RDX);
                    self.a.unary(Unary::Div, ww, RCX);
                }
                done.push(self.a.jmp());
                self.a.bind(zero);
                self.a.alu_rr(Alu::Xor, W32, RAX, RAX);
                for f in done {
                    self.a.bind(f);
                }
            }
            _ => unreachable!(),
        }
        self.put(dst, RAX);
    }

    /// RAX = 1 if the Arm condition holds on NZCV, else 0
    /// (ConditionHolds in the Arm ARM).
    fn cond(&mut self, cond: u8) {
        let (n, z, c, v) = (
            self.flag('n'),
            self.flag('z'),
            self.flag('c'),
            self.flag('v'),
        );
        match cond >> 1 {
            0 => self.a.load8_zx(RAX, z),
            1 => self.a.load8_zx(RAX, c),
            2 => self.a.load8_zx(RAX, n),
            3 => self.a.load8_zx(RAX, v),
            4 => {
                // HI: C && !Z, as C > Z.
                self.a.load8_zx(RAX, c);
                self.a.load8_zx(RCX, z);
                self.a.alu_ri(Alu::Xor, W32, RCX, 1);
                self.a.alu_rr(Alu::And, W32, RAX, RCX);
            }
            5 | 6 => {
                // GE: N == V; GT: N == V && !Z.
                self.a.load8_zx(RAX, n);
                self.a.load8_zx(RCX, v);
                self.a.alu_rr(Alu::Xor, W32, RAX, RCX);
                if cond >> 1 == 6 {
                    self.a.load8_zx(RCX, z);
                    self.a.alu_rr(Alu::Or, W32, RAX, RCX);
                }
                self.a.alu_ri(Alu::Xor, W32, RAX, 1);
            }
            _ => {
                self.a.mov_ri(RAX, 1);
                return;
            }
        }
        if cond & 1 == 1 {
            self.a.alu_ri(Alu::Xor, W32, RAX, 1);
        }
    }

    fn term(&mut self, t: &Term, insns: u32) {
        match *t {
            Term::Brk { imm, pc } => {
                // BRK does not retire.
                self.retire(insns - 1 - self.flushed);
                self.set_pc_imm(pc);
                self.a.mov_ri(RAX, RET_BRK | (imm as u64) << 8);
                return;
            }
            Term::JumpReg(v) => {
                self.get(RDX, v);
                self.retire(insns - self.flushed);
                let m = self.st(self.l.pc);
                self.a.store(W64, m, RDX);
            }
            Term::Branch {
                c,
                taken,
                fallthrough,
            } => {
                self.get(RDX, c);
                self.retire(insns - self.flushed);
                self.a.mov_ri(RAX, fallthrough);
                self.a.mov_ri(RCX, taken);
                self.a.test_rr(W64, RDX, RDX);
                self.a.cmov(Cc::Ne, W64, RAX, RCX);
                let m = self.st(self.l.pc);
                self.a.store(W64, m, RAX);
            }
            Term::Jump(pc) => {
                self.retire(insns - self.flushed);
                self.set_pc_imm(pc);
            }
            Term::Svc { imm, next } => {
                self.retire(insns - self.flushed);
                self.set_pc_imm(next);
                self.a.mov_ri(RAX, RET_SVC | (imm as u64) << 8);
                return;
            }
        }
        self.a.mov_ri(RAX, RET_JUMP);
    }
}
