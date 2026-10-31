//! ARM64 to IR: decodes a block of guest instructions into [`ir::Block`].
//!
//! Each instruction is translated as the interpreter executes it (the
//! files under `interp/`), which is the reference: the same operands, the
//! same order of loads, stores and register writes, so a fault part-way
//! through leaves the same state. Anything not translated here (yet)
//! becomes [`Op::Interp`], which runs that one instruction in the
//! interpreter, so every program runs whatever the coverage.
//!
//! Translated so far: all of data processing (immediate); data
//! processing (register) except CRC32 and the one-source group (RBIT,
//! REV, CLZ, CLS); branches, SVC and BRK; general-purpose loads and stores
//! (single, pair, literal; every addressing mode). Floating point, NEON,
//! system instructions, exclusives and atomics go to the interpreter.

use super::ir::{BinOp, Block, Carry, Op, Reg, Term, Val, Width};
use crate::interp::bits::{bit, decode_bit_masks, field, sext};
use kestrel_core::{MemFault, Memory};

/// The most instructions a block holds.
pub const MAX_INSNS: u32 = 64;

struct B {
    ops: Vec<Op>,
    vals: u32,
}

/// How an instruction left the block.
enum Next {
    /// Go on with the next instruction.
    On,
    /// The block ends here.
    End(Term),
}

impl B {
    fn val(&mut self) -> Val {
        self.vals += 1;
        Val(self.vals - 1)
    }

    fn k(&mut self, value: u64) -> Val {
        let dst = self.val();
        self.ops.push(Op::Const { dst, value });
        dst
    }

    fn get(&mut self, reg: Reg) -> Val {
        let dst = self.val();
        self.ops.push(Op::Get { dst, reg });
        dst
    }

    /// Xn, where 31 is XZR.
    fn xr(&mut self, n: u32) -> Val {
        if n == 31 {
            self.k(0)
        } else {
            self.get(Reg::X(n as u8))
        }
    }

    /// Xn, where 31 is SP.
    fn xsp(&mut self, n: u32) -> Val {
        if n == 31 {
            self.get(Reg::Sp)
        } else {
            self.get(Reg::X(n as u8))
        }
    }

    fn set_xr(&mut self, n: u32, src: Val) {
        if n != 31 {
            self.ops.push(Op::Set {
                reg: Reg::X(n as u8),
                src,
            });
        }
    }

    fn set_xsp(&mut self, n: u32, src: Val) {
        let reg = if n == 31 { Reg::Sp } else { Reg::X(n as u8) };
        self.ops.push(Op::Set { reg, src });
    }

    fn bin(&mut self, op: BinOp, w: Width, a: Val, b: Val) -> Val {
        let dst = self.val();
        self.ops.push(Op::Bin { op, w, dst, a, b });
        dst
    }

    fn bin_k(&mut self, op: BinOp, w: Width, a: Val, b: u64) -> Val {
        let b = self.k(b);
        self.bin(op, w, a, b)
    }

    fn not(&mut self, w: Width, a: Val) -> Val {
        let dst = self.val();
        self.ops.push(Op::Not { w, dst, a });
        dst
    }

    fn extend(&mut self, a: Val, bits: u8, signed: bool) -> Val {
        let dst = self.val();
        self.ops.push(Op::Extend {
            dst,
            a,
            bits,
            signed,
        });
        dst
    }

    /// The value at width `w`, upper bits cleared.
    fn trunc(&mut self, w: Width, a: Val) -> Val {
        match w {
            Width::W32 => self.extend(a, 32, false),
            Width::W64 => a,
        }
    }

    fn adc(&mut self, w: Width, a: Val, b: Val, carry: Carry, flags: bool) -> Val {
        let dst = self.val();
        self.ops.push(Op::Adc {
            w,
            dst,
            a,
            b,
            carry,
            flags,
        });
        dst
    }

    fn cond(&mut self, cond: u32) -> Val {
        let dst = self.val();
        self.ops.push(Op::Cond {
            dst,
            cond: cond as u8,
        });
        dst
    }

    fn select(&mut self, w: Width, c: Val, a: Val, b: Val) -> Val {
        let dst = self.val();
        self.ops.push(Op::Select { w, dst, c, a, b });
        dst
    }

    fn load(&mut self, addr: Val, size: u8) -> Val {
        let dst = self.val();
        self.ops.push(Op::Load { dst, addr, size });
        dst
    }

    fn store(&mut self, addr: Val, src: Val, size: u8) {
        self.ops.push(Op::Store { addr, src, size });
    }

    /// The shifted-register operand: `kind` 0 LSL, 1 LSR, 2 ASR, 3 ROR.
    fn shift(&mut self, w: Width, v: Val, kind: u32, amount: u32) -> Val {
        if amount == 0 {
            return v;
        }
        let op = [BinOp::Shl, BinOp::Lsr, BinOp::Asr, BinOp::Ror][kind as usize];
        self.bin_k(op, w, v, amount as u64)
    }

    /// The extended-register operand (UXTB..SXTX), shifted left.
    fn ext_reg(&mut self, v: Val, option: u32, amount: u32) -> Val {
        let e = match option & 3 {
            3 => v,
            n => self.extend(v, 8 << n, option & 4 != 0),
        };
        if amount == 0 {
            e
        } else {
            self.bin_k(BinOp::Shl, Width::W64, e, amount as u64)
        }
    }
}

/// Translate the block at `pc`, of at most `max` instructions (at least
/// one). A fault fetching the first instruction is returned; one fetching
/// a later instruction ends the block before it.
pub fn translate(mem: &Memory, pc: u64, max: u32) -> Result<(Block, Vec<u32>), MemFault> {
    let mut b = B {
        ops: Vec::new(),
        vals: 0,
    };
    let mut words = Vec::new();
    let mut at = pc;
    let term = loop {
        if words.len() as u32 == max {
            break Term::Jump(at);
        }
        let insn = match mem.fetch32(at) {
            Ok(insn) => insn,
            Err(f) if words.is_empty() => return Err(f),
            Err(_) => break Term::Jump(at),
        };
        b.ops.push(Op::Insn {
            index: words.len() as u32,
            pc: at,
        });
        words.push(insn);
        let mark = (b.ops.len(), b.vals);
        let next = match insn >> 25 & 0xf {
            0b1000 | 0b1001 => dp_imm(&mut b, insn, at),
            0b1010 | 0b1011 => branch(&mut b, insn, at),
            0b0100 | 0b0110 | 0b1100 | 0b1110 => ldst(&mut b, insn, at),
            0b0101 | 0b1101 => dp_reg(&mut b, insn),
            _ => None,
        };
        let next = next.unwrap_or_else(|| {
            // Not translated: the interpreter runs it. Branches,
            // exception-generating and other instructions of the branch
            // group may leave, so they end the block.
            b.ops.truncate(mark.0);
            b.vals = mark.1;
            b.ops.push(Op::Interp { insn });
            let system = field(insn, 31, 22) == 0b11_0101_0100;
            if insn >> 26 & 0b111 == 0b101 && !system {
                Next::End(Term::Jump(at.wrapping_add(4)))
            } else {
                Next::On
            }
        });
        at = at.wrapping_add(4);
        if let Next::End(t) = next {
            break t;
        }
    };
    let block = Block {
        pc,
        insns: words.len() as u32,
        ops: b.ops,
        term,
        vals: b.vals,
    };
    Ok((block, words))
}

// Each decoder returns None for what it does not translate; the caller
// then hands the instruction to the interpreter, which also stops on
// encodings that are undefined.

fn dp_imm(b: &mut B, i: u32, pc: u64) -> Option<Next> {
    let sf = bit(i, 31);
    let w = Width::of(sf);
    let rd = field(i, 4, 0);
    let rn = field(i, 9, 5);
    match field(i, 25, 23) {
        0b000 | 0b001 => {
            let imm = ((field(i, 23, 5) << 2) | field(i, 30, 29)) as u64;
            let off = sext(imm, 21);
            let v = if bit(i, 31) {
                (pc & !0xfff).wrapping_add((off << 12) as u64)
            } else {
                pc.wrapping_add(off as u64)
            };
            let v = b.k(v);
            b.set_xr(rd, v);
        }
        0b010 => {
            if bit(i, 22) && field(i, 23, 22) != 0b01 {
                return None;
            }
            let imm = (field(i, 21, 10) as u64) << if bit(i, 22) { 12 } else { 0 };
            let a = b.xsp(rn);
            let y = b.k(imm);
            arith(b, w, a, y, bit(i, 30), bit(i, 29), rd, true);
        }
        0b100 => {
            let n = bit(i, 22);
            if !sf && n {
                return None;
            }
            let (imm, _) = decode_bit_masks(n, field(i, 15, 10), field(i, 21, 16), true, w.bits())?;
            let a = b.xr(rn);
            let opc = field(i, 30, 29);
            let op = [BinOp::And, BinOp::Or, BinOp::Xor, BinOp::And][opc as usize];
            let r = b.bin_k(op, w, a, imm);
            if opc == 0b11 {
                b.ops.push(Op::LogicFlags { w, a: r });
                b.set_xr(rd, r);
            } else {
                b.set_xsp(rd, r);
            }
        }
        0b101 => {
            let hw = field(i, 22, 21);
            if !sf && hw > 1 {
                return None;
            }
            let shift = hw * 16;
            let imm = (field(i, 20, 5) as u64) << shift;
            let mask = if sf { u64::MAX } else { 0xffff_ffff };
            let v = match field(i, 30, 29) {
                0b00 => b.k(!imm & mask),
                0b10 => b.k(imm),
                0b11 => {
                    let old = b.xr(rd);
                    let kept = b.bin_k(BinOp::And, w, old, !(0xffffu64 << shift) & mask);
                    b.bin_k(BinOp::Or, w, kept, imm)
                }
                _ => return None,
            };
            b.set_xr(rd, v);
        }
        0b110 => {
            let n = bit(i, 22);
            if sf != n {
                return None;
            }
            let (immr, imms) = (field(i, 21, 16), field(i, 15, 10));
            let (wmask, tmask) = decode_bit_masks(n, imms, immr, false, w.bits())?;
            let src = b.xr(rn);
            let rot = if immr == 0 {
                src
            } else {
                b.bin_k(BinOp::Ror, w, src, immr as u64)
            };
            let r = match field(i, 30, 29) {
                0b00 => {
                    // SBFM: above the field, copies of bit `imms` of src.
                    let up = b.bin_k(BinOp::Shl, Width::W64, src, 63 - imms as u64);
                    let top = b.bin_k(BinOp::Asr, Width::W64, up, 63);
                    let top = b.bin_k(BinOp::And, w, top, !tmask);
                    let bot = b.bin_k(BinOp::And, w, rot, wmask & tmask);
                    b.bin(BinOp::Or, w, top, bot)
                }
                0b01 => {
                    // BFM: bot = (dst & !wmask) | (rot & wmask), then
                    // (dst & !tmask) | (bot & tmask).
                    let dst = b.xr(rd);
                    let keep = b.bin_k(BinOp::And, w, dst, !(wmask & tmask));
                    let ins = b.bin_k(BinOp::And, w, rot, wmask & tmask);
                    b.bin(BinOp::Or, w, keep, ins)
                }
                0b10 => b.bin_k(BinOp::And, w, rot, wmask & tmask),
                _ => return None,
            };
            b.set_xr(rd, r);
        }
        0b111 => {
            if sf != bit(i, 22) || field(i, 30, 29) != 0 {
                return None;
            }
            let lsb = field(i, 15, 10);
            if lsb >= w.bits() {
                return None;
            }
            let hi = b.xr(rn);
            let lo = b.xr(field(i, 20, 16));
            let r = if lsb == 0 {
                b.trunc(w, lo)
            } else {
                let l = b.bin_k(BinOp::Lsr, w, lo, lsb as u64);
                let h = b.bin_k(BinOp::Shl, w, hi, (w.bits() - lsb) as u64);
                b.bin(BinOp::Or, w, l, h)
            };
            b.set_xr(rd, r);
        }
        _ => return None,
    }
    Some(Next::On)
}

/// ADD, ADDS, SUB, SUBS: `a` op `y`, to Rd (SP when `rd_sp` and no flags
/// are set, as the interpreter's `arith` and `add_sub_imm` do).
#[allow(clippy::too_many_arguments)]
fn arith(b: &mut B, w: Width, a: Val, y: Val, sub: bool, flags: bool, rd: u32, rd_sp: bool) {
    let r = if flags {
        let (y, carry) = if sub {
            (b.not(w, y), Carry::One)
        } else {
            (y, Carry::Zero)
        };
        b.adc(w, a, y, carry, true)
    } else {
        b.bin(if sub { BinOp::Sub } else { BinOp::Add }, w, a, y)
    };
    if flags || !rd_sp {
        b.set_xr(rd, r);
    } else {
        b.set_xsp(rd, r);
    }
}

fn branch(b: &mut B, i: u32, pc: u64) -> Option<Next> {
    let target = |imm: u32, bits: u32| pc.wrapping_add(sext((imm as u64) << 2, bits + 2) as u64);
    let next = pc.wrapping_add(4);
    // B, BL
    if field(i, 30, 26) == 0b00101 {
        if bit(i, 31) {
            let ret = b.k(next);
            b.set_xr(30, ret);
        }
        return Some(Next::End(Term::Jump(target(field(i, 25, 0), 26))));
    }
    // CBZ, CBNZ
    if field(i, 30, 25) == 0b011010 {
        let v = b.xr(field(i, 4, 0));
        let c = b.val();
        b.ops.push(Op::IsZero {
            w: Width::of(bit(i, 31)),
            dst: c,
            a: v,
        });
        let t = target(field(i, 23, 5), 19);
        let (taken, fallthrough) = if bit(i, 24) { (next, t) } else { (t, next) };
        return Some(Next::End(Term::Branch {
            c,
            taken,
            fallthrough,
        }));
    }
    // TBZ, TBNZ
    if field(i, 30, 25) == 0b011011 {
        let n = ((bit(i, 31) as u32) << 5) | field(i, 23, 19);
        let v = b.xr(field(i, 4, 0));
        let s = b.bin_k(BinOp::Lsr, Width::W64, v, n as u64);
        let c = b.bin_k(BinOp::And, Width::W64, s, 1);
        let t = target(field(i, 18, 5), 14);
        let (taken, fallthrough) = if bit(i, 24) { (t, next) } else { (next, t) };
        return Some(Next::End(Term::Branch {
            c,
            taken,
            fallthrough,
        }));
    }
    // B.cond
    if field(i, 31, 24) == 0b0101_0100 && !bit(i, 4) {
        let t = target(field(i, 23, 5), 19);
        let cond = field(i, 3, 0);
        if cond >= 14 {
            return Some(Next::End(Term::Jump(t)));
        }
        let c = b.cond(cond);
        return Some(Next::End(Term::Branch {
            c,
            taken: t,
            fallthrough: next,
        }));
    }
    // SVC, BRK
    if field(i, 31, 24) == 0b1101_0100 {
        let imm = field(i, 20, 5) as u16;
        return match (field(i, 23, 21), field(i, 4, 0)) {
            (0b000, 0b00001) => Some(Next::End(Term::Svc { imm, next })),
            (0b001, 0b00000) => Some(Next::End(Term::Brk { imm, pc })),
            _ => None,
        };
    }
    // BR, BLR, RET (the plain forms).
    if field(i, 31, 25) == 0b110_1011
        && field(i, 20, 16) == 0b11111
        && field(i, 15, 10) == 0
        && field(i, 4, 0) == 0
    {
        let opc = field(i, 24, 21);
        if opc > 0b0010 {
            return None;
        }
        let t = b.xr(field(i, 9, 5));
        if opc == 0b0001 {
            let ret = b.k(next);
            b.set_xr(30, ret);
        }
        return Some(Next::End(Term::JumpReg(t)));
    }
    None
}

fn ldst(b: &mut B, i: u32, pc: u64) -> Option<Next> {
    if field(i, 29, 24) == 0b00_1000 || bit(i, 26) {
        // Exclusives and acquire/release; SIMD and FP registers.
        return None;
    }
    match field(i, 29, 27) {
        0b011 if field(i, 25, 24) == 0 => {
            let rt = field(i, 4, 0);
            let addr = pc.wrapping_add(sext((field(i, 23, 5) as u64) << 2, 21) as u64);
            let addr = b.k(addr);
            match field(i, 31, 30) {
                0 => {
                    let v = b.load(addr, 4);
                    b.set_xr(rt, v);
                }
                1 => {
                    let v = b.load(addr, 8);
                    b.set_xr(rt, v);
                }
                2 => {
                    let v = b.load(addr, 4);
                    let v = b.extend(v, 32, true);
                    b.set_xr(rt, v);
                }
                _ => {} // PRFM (literal)
            }
        }
        0b101 if !bit(i, 25) => pair(b, i)?,
        0b111 if bit(i, 24) => {
            let size = field(i, 31, 30);
            let off = (field(i, 21, 10) as u64) << size;
            let base = b.xsp(field(i, 9, 5));
            let addr = b.bin_k(BinOp::Add, Width::W64, base, off);
            single(b, i, addr)?;
        }
        0b111 if !bit(i, 25) && !bit(i, 21) => {
            let rn = field(i, 9, 5);
            let off = sext(field(i, 20, 12) as u64, 9) as u64;
            let base = b.xsp(rn);
            let moved = b.bin_k(BinOp::Add, Width::W64, base, off);
            match field(i, 11, 10) {
                0b00 | 0b10 => single(b, i, moved)?,
                0b01 => {
                    single(b, i, base)?;
                    b.set_xsp(rn, moved);
                }
                _ => {
                    single(b, i, moved)?;
                    b.set_xsp(rn, moved);
                }
            }
        }
        0b111 if !bit(i, 25) && field(i, 11, 10) == 0b10 => {
            let option = field(i, 15, 13);
            if option & 0b010 == 0 {
                return None;
            }
            let amount = if bit(i, 12) { field(i, 31, 30) } else { 0 };
            let m = b.xr(field(i, 20, 16));
            let off = b.ext_reg(m, option, amount);
            let base = b.xsp(field(i, 9, 5));
            let addr = b.bin(BinOp::Add, Width::W64, base, off);
            single(b, i, addr)?;
        }
        _ => return None,
    }
    Some(Next::On)
}

/// A general-purpose load or store of one register at `addr`.
fn single(b: &mut B, i: u32, addr: Val) -> Option<()> {
    let rt = field(i, 4, 0);
    let size = field(i, 31, 30);
    let bytes = 1u8 << size;
    match field(i, 23, 22) {
        0b00 => {
            let v = b.xr(rt);
            b.store(addr, v, bytes);
        }
        0b01 => {
            let v = b.load(addr, bytes);
            b.set_xr(rt, v);
        }
        0b10 if size == 3 => {} // PRFM
        0b10 => {
            let v = b.load(addr, bytes);
            let v = b.extend(v, 8 << size, true);
            b.set_xr(rt, v);
        }
        0b11 if size < 2 => {
            let v = b.load(addr, bytes);
            let v = b.extend(v, 8 << size, true);
            let v = b.trunc(Width::W32, v);
            b.set_xr(rt, v);
        }
        _ => return None,
    }
    Some(())
}

fn pair(b: &mut B, i: u32) -> Option<()> {
    let opc = field(i, 31, 30);
    let load = bit(i, 22);
    let bytes: u8 = match opc {
        0 | 1 => 4,
        2 => 8,
        _ => return None,
    };
    if opc == 1 && !load {
        return None;
    }
    let (rt, rt2, rn) = (field(i, 4, 0), field(i, 14, 10), field(i, 9, 5));
    let off = (sext(field(i, 21, 15) as u64, 7) * bytes as i64) as u64;
    let base = b.xsp(rn);
    let mode = field(i, 24, 23);
    let moved = b.bin_k(BinOp::Add, Width::W64, base, off);
    let addr = if mode == 0b01 { base } else { moved };
    let addr2 = b.bin_k(BinOp::Add, Width::W64, addr, bytes as u64);
    if load {
        let mut x = b.load(addr, bytes);
        let mut y = b.load(addr2, bytes);
        if opc == 1 {
            x = b.extend(x, 32, true);
            y = b.extend(y, 32, true);
        }
        b.set_xr(rt, x);
        b.set_xr(rt2, y);
    } else {
        let (x, y) = (b.xr(rt), b.xr(rt2));
        b.store(addr, x, bytes);
        b.store(addr2, y, bytes);
    }
    match mode {
        0b01 => b.set_xsp(rn, moved),
        0b11 => b.set_xsp(rn, addr),
        _ => {}
    }
    Some(())
}

fn dp_reg(b: &mut B, i: u32) -> Option<Next> {
    let sf = bit(i, 31);
    let w = Width::of(sf);
    let rd = field(i, 4, 0);
    let rn = field(i, 9, 5);
    let rm = field(i, 20, 16);
    if !bit(i, 28) {
        match (bit(i, 24), bit(i, 21)) {
            (false, _) => {
                // Logical (shifted register).
                let amount = field(i, 15, 10);
                if !sf && amount >= 32 {
                    return None;
                }
                let m = b.xr(rm);
                let mut y = b.shift(w, m, field(i, 23, 22), amount);
                if bit(i, 21) {
                    y = b.not(w, y);
                }
                let a = b.xr(rn);
                let opc = field(i, 30, 29);
                let op = [BinOp::And, BinOp::Or, BinOp::Xor, BinOp::And][opc as usize];
                let r = b.bin(op, w, a, y);
                if opc == 0b11 {
                    b.ops.push(Op::LogicFlags { w, a: r });
                }
                b.set_xr(rd, r);
            }
            (true, false) => {
                // Add/subtract (shifted register).
                let kind = field(i, 23, 22);
                let amount = field(i, 15, 10);
                if kind == 0b11 || (!sf && amount >= 32) {
                    return None;
                }
                let m = b.xr(rm);
                let y = b.shift(w, m, kind, amount);
                let a = b.xr(rn);
                arith(b, w, a, y, bit(i, 30), bit(i, 29), rd, false);
            }
            (true, true) => {
                // Add/subtract (extended register).
                let amount = field(i, 12, 10);
                if amount > 4 || field(i, 23, 22) != 0 {
                    return None;
                }
                let m = b.xr(rm);
                let y = b.ext_reg(m, field(i, 15, 13), amount);
                let a = b.xsp(rn);
                arith(b, w, a, y, bit(i, 30), bit(i, 29), rd, true);
            }
        }
        return Some(Next::On);
    }
    match field(i, 24, 21) {
        0b0000 => {
            // ADC, ADCS, SBC, SBCS.
            if field(i, 15, 10) != 0 {
                return None;
            }
            let a = b.xr(rn);
            let mut y = b.xr(rm);
            if bit(i, 30) {
                y = b.not(w, y);
            }
            let r = b.adc(w, a, y, Carry::Flag, bit(i, 29));
            b.set_xr(rd, r);
        }
        0b0010 => {
            // CCMN, CCMP: flags from the comparison if the condition
            // holds, else the immediate NZCV.
            if !bit(i, 29) || bit(i, 10) || bit(i, 4) {
                return None;
            }
            let c = b.cond(field(i, 15, 12));
            let a = b.xr(rn);
            let mut y = if bit(i, 11) { b.k(rm as u64) } else { b.xr(rm) };
            let sub = bit(i, 30);
            if sub {
                y = b.not(w, y);
            }
            let carry = if sub { Carry::One } else { Carry::Zero };
            b.adc(w, a, y, carry, true);
            let compared = b.val();
            b.ops.push(Op::GetNzcv { dst: compared });
            let given = b.k((field(i, 3, 0) as u64) << 28);
            let nzcv = b.select(Width::W64, c, compared, given);
            b.ops.push(Op::SetNzcv { src: nzcv });
        }
        0b0100 => {
            // CSEL, CSINC, CSINV, CSNEG.
            if bit(i, 29) || bit(i, 11) {
                return None;
            }
            let c = b.cond(field(i, 15, 12));
            let a = b.xr(rn);
            let y = b.xr(rm);
            let y = match (bit(i, 30), bit(i, 10)) {
                (false, false) => y,
                (false, true) => b.bin_k(BinOp::Add, w, y, 1),
                (true, false) => b.not(w, y),
                (true, true) => {
                    let zero = b.k(0);
                    b.bin(BinOp::Sub, w, zero, y)
                }
            };
            let r = b.select(w, c, a, y);
            b.set_xr(rd, r);
        }
        0b0110 if !bit(i, 30) => {
            // Two-source: UDIV, SDIV, LSLV, LSRV, ASRV, RORV.
            let op = match field(i, 15, 10) {
                0b000010 => BinOp::UDiv,
                0b000011 => BinOp::SDiv,
                0b001000 => BinOp::Shl,
                0b001001 => BinOp::Lsr,
                0b001010 => BinOp::Asr,
                0b001011 => BinOp::Ror,
                _ => return None,
            };
            if bit(i, 29) {
                return None;
            }
            let a = b.xr(rn);
            let y = b.xr(rm);
            let r = b.bin(op, w, a, y);
            b.set_xr(rd, r);
        }
        0b1000..=0b1111 => {
            // Three-source: MADD, MSUB, SMADDL, SMSUBL, UMADDL, UMSUBL,
            // SMULH, UMULH.
            if field(i, 30, 29) != 0 {
                return None;
            }
            let sub = bit(i, 15);
            let n = b.xr(rn);
            let m = b.xr(rm);
            let r = match field(i, 23, 21) {
                0b000 => {
                    let p = b.bin(BinOp::Mul, w, n, m);
                    let a = b.xr(field(i, 14, 10));
                    b.bin(if sub { BinOp::Sub } else { BinOp::Add }, w, a, p)
                }
                op @ (0b001 | 0b101) if sf => {
                    let signed = op == 0b001;
                    let n = b.extend(n, 32, signed);
                    let m = b.extend(m, 32, signed);
                    let p = b.bin(BinOp::Mul, Width::W64, n, m);
                    let a = b.xr(field(i, 14, 10));
                    let op = if sub { BinOp::Sub } else { BinOp::Add };
                    b.bin(op, Width::W64, a, p)
                }
                0b010 if sf && !sub => b.bin(BinOp::SMulHigh, w, n, m),
                0b110 if sf && !sub => b.bin(BinOp::UMulHigh, w, n, m),
                _ => return None,
            };
            b.set_xr(rd, r);
        }
        _ => return None,
    }
    Some(Next::On)
}
