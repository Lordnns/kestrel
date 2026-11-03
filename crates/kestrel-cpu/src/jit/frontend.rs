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
//! processing (register) except CRC32; branches, SVC and BRK; loads and
//! stores of general-purpose registers and of SIMD and FP registers (B
//! to Q) (single, pair, literal; every addressing mode); LDAR, STLR and
//! the single-register exclusives; hints, barriers, cache maintenance, DC
//! ZVA, CLREX, MRS/MSR of the system registers kept in `CpuState`, and
//! the counters; of the SIMD and FP instructions, those that only move
//! bits: DUP, INS, UMOV, SMOV, the vector logic (AND to BIF), the
//! modified immediates (MOVI, MVNI, ORR, BIC, FMOV) and the scalar FMOV,
//! FABS and FNEG. Arithmetic on lanes and floating point, the structure
//! loads and stores (LD1 to ST4), exclusive pairs, compare-and-swap and
//! the other atomics go to the interpreter.

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

    fn set(&mut self, reg: Reg, src: Val) {
        self.ops.push(Op::Set { reg, src });
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

    fn byte_swap(&mut self, w: Width, a: Val) -> Val {
        let dst = self.val();
        self.ops.push(Op::ByteSwap { w, dst, a });
        dst
    }

    fn clz(&mut self, w: Width, a: Val) -> Val {
        let dst = self.val();
        self.ops.push(Op::Clz { w, dst, a });
        dst
    }

    /// Swap the groups of `shift` bits that `mask` selects with their
    /// neighbours above: `((a >> shift) & mask) | ((a & mask) << shift)`.
    fn swap_bits(&mut self, w: Width, a: Val, mask: u64, shift: u64) -> Val {
        let hi = self.bin_k(BinOp::Lsr, w, a, shift);
        let hi = self.bin_k(BinOp::And, w, hi, mask);
        let lo = self.bin_k(BinOp::And, w, a, mask);
        let lo = self.bin_k(BinOp::Shl, w, lo, shift);
        self.bin(BinOp::Or, w, hi, lo)
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
            0b0111 | 0b1111 => simd(&mut b, insn).map(|()| Next::On),
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
    // System instructions; none ends the block.
    if field(i, 31, 22) == 0b11_0101_0100 {
        return system(b, i).map(|()| Next::On);
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

/// System instructions as `interp/system.rs` runs them: hints, ISB, SB
/// and the cache maintenance EL0 may do are nothing (IC IVAU too:
/// rewritten code is caught by the blocks' own checks, `jit/mod.rs`); DMB
/// and DSB are a full host fence, which other cores on other host threads
/// rely on; DC ZVA; CLREX, MRS and MSR of the registers kept in
/// `CpuState` and the constant ID registers, and MRS of the counters
/// (`Op::Counter`, which sees the retired count). Everything else goes to
/// the interpreter.
fn system(b: &mut B, i: u32) -> Option<()> {
    use crate::interp::system::*;
    let (l, op0, rt) = (bit(i, 21), field(i, 20, 19), field(i, 4, 0));
    let (op1, crn, crm, op2) = (
        field(i, 18, 16),
        field(i, 15, 12),
        field(i, 11, 8),
        field(i, 7, 5),
    );
    match op0 {
        0 if !l && op1 == 3 && rt == 31 => match (crn, op2) {
            (0b0010, _) | (0b0011, 0b110 | 0b111) => Some(()),
            (0b0011, 0b100 | 0b101) => {
                b.ops.push(Op::Fence);
                Some(())
            }
            (0b0011, 0b010) => {
                b.ops.push(Op::ClearExclusive);
                Some(())
            }
            _ => None,
        },
        1 if !l && op1 == 3 && crn == 7 && op2 == 1 && matches!(crm, 5 | 10..=14) => Some(()),
        // DC ZVA.
        1 if !l && op1 == 3 && crn == 7 && op2 == 1 && crm == 4 => {
            let v = b.xr(rt);
            let addr = b.bin_k(BinOp::And, Width::W64, v, !63);
            b.ops.push(Op::ZeroBlock { addr });
            Some(())
        }
        2 | 3 => {
            let reg = field(i, 19, 5);
            let sys = match reg {
                TPIDR_EL0 => Some(Reg::Tpidr),
                TPIDRRO_EL0 if l => Some(Reg::TpidrRo),
                FPCR => Some(Reg::Fpcr),
                FPSR => Some(Reg::Fpsr),
                _ => None,
            };
            if l {
                let v = match (reg, sys) {
                    (_, Some(r)) => b.get(r),
                    (NZCV, _) => {
                        let dst = b.val();
                        b.ops.push(Op::GetNzcv { dst });
                        dst
                    }
                    (CTR_EL0, _) => b.k(CTR_VALUE),
                    (DCZID_EL0, _) => b.k(DCZID_VALUE),
                    (CNTFRQ_EL0, _) => b.k(crate::state::COUNTER_HZ),
                    (CNTPCT_EL0 | CNTVCT_EL0, _) => {
                        let dst = b.val();
                        b.ops.push(Op::Counter { dst });
                        dst
                    }
                    _ => return None,
                };
                b.set_xr(rt, v);
            } else {
                let v = b.xr(rt);
                match (reg, sys) {
                    (_, Some(r)) => b.ops.push(Op::Set { reg: r, src: v }),
                    (NZCV, _) => b.ops.push(Op::SetNzcv { src: v }),
                    _ => return None,
                }
            }
            Some(())
        }
        _ => None,
    }
}

/// The single-register forms of `interp/ldst.rs`'s `exclusive`. LDAR and
/// STLR are plain loads and stores: x86 keeps loads in order with later
/// accesses and stores with earlier ones, which is acquire and release;
/// STLR's fence after the store (sequential consistency with a later
/// LDAR) is `Op::Fence`. LDXR/LDAXR and STXR/STLXR go through the monitor
/// operations; STLXR's compare-and-swap is a locked x86 instruction, a
/// full fence. Pairs and compare-and-swap go to the interpreter.
fn exclusive(b: &mut B, i: u32) -> Option<()> {
    let size = 1u8 << field(i, 31, 30);
    let (l, rs, rt) = (bit(i, 22), field(i, 20, 16), field(i, 4, 0));
    let addr = b.xsp(field(i, 9, 5));
    match (bit(i, 23), bit(i, 21)) {
        (true, false) if l => {
            let v = b.load(addr, size);
            b.set_xr(rt, v);
        }
        (true, false) => {
            let v = b.xr(rt);
            b.store(addr, v, size);
            if bit(i, 15) {
                b.ops.push(Op::Fence);
            }
        }
        (false, false) if l => {
            let dst = b.val();
            b.ops.push(Op::LoadExclusive { dst, addr, size });
            b.set_xr(rt, dst);
        }
        (false, false) => {
            let src = b.xr(rt);
            let status = b.val();
            b.ops.push(Op::StoreExclusive {
                status,
                addr,
                src,
                size,
            });
            b.set_xr(rs, status);
        }
        _ => return None,
    }
    Some(())
}

fn ldst(b: &mut B, i: u32, pc: u64) -> Option<Next> {
    if field(i, 29, 24) == 0b00_1000 {
        return exclusive(b, i).map(|()| Next::On);
    }
    match field(i, 29, 27) {
        0b011 if field(i, 25, 24) == 0 => {
            let rt = field(i, 4, 0);
            let addr = pc.wrapping_add(sext((field(i, 23, 5) as u64) << 2, 21) as u64);
            let addr = b.k(addr);
            if bit(i, 26) {
                let bytes = match field(i, 31, 30) {
                    0 => 4,
                    1 => 8,
                    2 => 16,
                    _ => return None,
                };
                load_v(b, addr, field(i, 4, 0), bytes);
                return Some(Next::On);
            }
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
            let off = (field(i, 21, 10) as u64) << log2_bytes(i);
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
            let amount = if bit(i, 12) { log2_bytes(i) } else { 0 };
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

/// The log2 of a single load or store's size: 4 for a Q register.
fn log2_bytes(i: u32) -> u32 {
    let size = field(i, 31, 30);
    if bit(i, 26) && size == 0 && bit(i, 23) {
        4
    } else {
        size
    }
}

/// Load `bytes` at `addr` into Vt, as `interp/ldst.rs`'s `load_v`: the
/// rest of the register is cleared, and a Q register's two halves are
/// read (as `Memory::read128` reads them) before it is written.
fn load_v(b: &mut B, addr: Val, rt: u32, bytes: u8) {
    let (lo, hi) = if bytes == 16 {
        let lo = b.load(addr, 8);
        let addr8 = b.bin_k(BinOp::Add, Width::W64, addr, 8);
        (lo, b.load(addr8, 8))
    } else {
        (b.load(addr, bytes), b.k(0))
    };
    b.set(Reg::VLo(rt as u8), lo);
    b.set(Reg::VHi(rt as u8), hi);
}

/// Store the low `bytes` of Vt at `addr`, as `store_v`: a Q register as
/// two halves, the low one first, as `Memory::write128` writes them.
fn store_v(b: &mut B, addr: Val, rt: u32, bytes: u8) {
    let lo = b.get(Reg::VLo(rt as u8));
    if bytes == 16 {
        let hi = b.get(Reg::VHi(rt as u8));
        b.store(addr, lo, 8);
        let addr8 = b.bin_k(BinOp::Add, Width::W64, addr, 8);
        b.store(addr8, hi, 8);
    } else {
        b.store(addr, lo, bytes);
    }
}

/// A load or store of one register at `addr`.
fn single(b: &mut B, i: u32, addr: Val) -> Option<()> {
    let rt = field(i, 4, 0);
    let size = field(i, 31, 30);
    let bytes = 1u8 << log2_bytes(i);
    if bit(i, 26) {
        let opc = field(i, 23, 22);
        if bytes != 16 && opc >> 1 != 0 {
            return None;
        }
        if opc & 1 == 0 {
            store_v(b, addr, rt, bytes);
        } else {
            load_v(b, addr, rt, bytes);
        }
        return Some(());
    }
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
    let simd = bit(i, 26);
    let bytes: u8 = match (simd, opc) {
        (true, 0) | (false, 0 | 1) => 4,
        (true, 1) | (false, 2) => 8,
        (true, 2) => 16,
        _ => return None,
    };
    if !simd && opc == 1 && !load {
        return None;
    }
    let (rt, rt2, rn) = (field(i, 4, 0), field(i, 14, 10), field(i, 9, 5));
    let off = (sext(field(i, 21, 15) as u64, 7) * bytes as i64) as u64;
    let base = b.xsp(rn);
    let mode = field(i, 24, 23);
    let moved = b.bin_k(BinOp::Add, Width::W64, base, off);
    let addr = if mode == 0b01 { base } else { moved };
    let addr2 = b.bin_k(BinOp::Add, Width::W64, addr, bytes as u64);
    if simd {
        // One register after the other, as the interpreter does: a fault
        // on the second leaves the first written.
        if load {
            load_v(b, addr, rt, bytes);
            load_v(b, addr2, rt2, bytes);
        } else {
            store_v(b, addr, rt, bytes);
            store_v(b, addr2, rt2, bytes);
        }
    } else if load {
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
        0b0110 if bit(i, 30) => {
            // One-source: RBIT, REV16, REV, REV32, REV64, CLZ, CLS.
            if rm != 0 || bit(i, 29) {
                return None;
            }
            let a = b.xr(rn);
            let r = match (sf, field(i, 15, 10)) {
                (_, 0) => {
                    // RBIT: swap bits, pairs and nibbles, then bytes.
                    let a = b.swap_bits(w, a, 0x5555_5555_5555_5555, 1);
                    let a = b.swap_bits(w, a, 0x3333_3333_3333_3333, 2);
                    let a = b.swap_bits(w, a, 0x0f0f_0f0f_0f0f_0f0f, 4);
                    b.byte_swap(w, a)
                }
                (_, 1) => b.swap_bits(w, a, 0x00ff_00ff_00ff_00ff, 8),
                (false, 2) | (true, 3) => b.byte_swap(w, a),
                (true, 2) => {
                    // REV32: each word's bytes reversed, the words kept
                    // in place.
                    let s = b.byte_swap(w, a);
                    b.bin_k(BinOp::Ror, w, s, 32)
                }
                (_, 4) => b.clz(w, a),
                (_, 5) => {
                    // CLS: the leading bits equal to the sign bit, not
                    // counting it.
                    let s = b.bin_k(BinOp::Asr, w, a, 1);
                    let x = b.bin(BinOp::Xor, w, a, s);
                    let z = b.clz(w, x);
                    b.bin_k(BinOp::Sub, w, z, 1)
                }
                _ => return None,
            };
            b.set_xr(rd, r);
        }
        0b0110 => {
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

// ---- SIMD and FP registers ---------------------------------------------------
//
// Vn is two 64-bit values, `Reg::VLo` and `Reg::VHi`. What is translated
// here moves or combines bits without arithmetic on lanes, so it is exact
// on those halves: the copies of `interp/neon/perm.rs` (DUP, INS, UMOV,
// SMOV), its vector logic and modified immediates, and of `interp/fp.rs`
// FMOV (general, register, immediate), FABS and FNEG. Every register is
// read before any is written, as there.

/// The low or high half of Vn.
fn vhalf(n: u32, hi: bool) -> Reg {
    if hi {
        Reg::VHi(n as u8)
    } else {
        Reg::VLo(n as u8)
    }
}

/// The low `bits` (at most 64) of a value as a mask.
fn ones64(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1 << bits) - 1
    }
}

impl B {
    /// Write Vd's halves.
    fn set_v(&mut self, d: u32, lo: Val, hi: Val) {
        self.set(Reg::VLo(d as u8), lo);
        self.set(Reg::VHi(d as u8), hi);
    }

    /// Write a scalar result: `v` in the low half, the rest cleared.
    fn set_v_scalar(&mut self, d: u32, v: Val) {
        let zero = self.k(0);
        self.set_v(d, v, zero);
    }

    /// `v & mask`, or `v` when the mask is all ones.
    fn mask(&mut self, v: Val, mask: u64) -> Val {
        if mask == u64::MAX {
            v
        } else {
            self.bin_k(BinOp::And, Width::W64, v, mask)
        }
    }

    /// Element `idx` of `es` bits of Vn, zero-extended.
    fn elem(&mut self, n: u32, es: u32, idx: u32) -> Val {
        let at = es * idx;
        let h = self.get(vhalf(n, at >= 64));
        let v = if at % 64 == 0 {
            h
        } else {
            self.bin_k(BinOp::Lsr, Width::W64, h, (at % 64) as u64)
        };
        self.mask(v, ones64(es))
    }

    /// Replace element `idx` of `es` bits of Vd with the low bits of `x`.
    fn insert(&mut self, d: u32, es: u32, idx: u32, x: Val) {
        let at = es * idx;
        let reg = vhalf(d, at >= 64);
        if es == 64 {
            self.set(reg, x);
            return;
        }
        let sh = (at % 64) as u64;
        let old = self.get(reg);
        let kept = self.bin_k(BinOp::And, Width::W64, old, !(ones64(es) << sh));
        let x = self.mask(x, ones64(es));
        let x = if sh == 0 {
            x
        } else {
            self.bin_k(BinOp::Shl, Width::W64, x, sh)
        };
        let new = self.bin(BinOp::Or, Width::W64, kept, x);
        self.set(reg, new);
    }

    /// An `es`-bit value (upper bits clear) repeated across 64 bits.
    fn replicate(&mut self, v: Val, es: u32) -> Val {
        if es == 64 {
            v
        } else {
            let rep = u64::MAX / ones64(es);
            self.bin_k(BinOp::Mul, Width::W64, v, rep)
        }
    }
}

/// The scalar floating-point and Advanced SIMD group (bits 28..25 x111),
/// dispatched as `interp/simd.rs` does; `None` for what is not translated.
fn simd(b: &mut B, i: u32) -> Option<()> {
    if !bit(i, 30) && field(i, 28, 25) == 0b1111 {
        return fp(b, i);
    }
    let scalar = bit(i, 28);
    if (scalar && field(i, 31, 30) != 0b01) || (!scalar && bit(i, 31)) {
        return None;
    }
    if !bit(i, 24) {
        if bit(i, 21) && bit(i, 10) && field(i, 15, 11) == 0b00011 && !scalar {
            return logic(b, i);
        }
        if !bit(i, 21) && field(i, 23, 21) == 0 && !bit(i, 15) && bit(i, 10) {
            return copy(b, i, scalar);
        }
        return None;
    }
    if bit(i, 10) && !bit(i, 23) && field(i, 22, 19) == 0 && !scalar {
        return modified_imm(b, i);
    }
    None
}

/// AND, BIC, ORR, ORN, EOR, BSL, BIT, BIF, as `perm::logic`.
fn logic(b: &mut B, i: u32) -> Option<()> {
    let q = bit(i, 30);
    let (n, m, d) = (field(i, 9, 5), field(i, 20, 16), field(i, 4, 0));
    let op = (bit(i, 29), field(i, 23, 22));
    let w = Width::W64;
    let half = |b: &mut B, hi: bool| {
        let (x, y) = (b.get(vhalf(n, hi)), b.get(vhalf(m, hi)));
        match op {
            (false, 0b00) => b.bin(BinOp::And, w, x, y),
            (false, 0b01) => {
                let y = b.not(w, y);
                b.bin(BinOp::And, w, x, y)
            }
            (false, 0b10) => b.bin(BinOp::Or, w, x, y),
            (false, 0b11) => {
                let y = b.not(w, y);
                b.bin(BinOp::Or, w, x, y)
            }
            (true, 0b00) => b.bin(BinOp::Xor, w, x, y),
            // BSL, BIT and BIF pick each bit from one of two registers by
            // a third: (sel & a) | (!sel & c).
            _ => {
                let z = b.get(vhalf(d, hi));
                let (sel, a, c) = match op.1 {
                    0b01 => (z, x, y), // BSL: Vd selects Vn over Vm
                    0b10 => (y, x, z), // BIT: Vm selects Vn over Vd
                    _ => (y, z, x),    // BIF: Vm selects Vd over Vn
                };
                let a = b.bin(BinOp::And, w, sel, a);
                let ns = b.not(w, sel);
                let c = b.bin(BinOp::And, w, ns, c);
                b.bin(BinOp::Or, w, a, c)
            }
        }
    };
    let lo = half(b, false);
    let hi = if q { half(b, true) } else { b.k(0) };
    b.set_v(d, lo, hi);
    Some(())
}

/// DUP, INS, SMOV, UMOV and the scalar DUP (element), as `perm::copy`.
fn copy(b: &mut B, i: u32, scalar: bool) -> Option<()> {
    let (q, op, imm4, imm5) = (bit(i, 30), bit(i, 29), field(i, 14, 11), field(i, 20, 16));
    let tz = imm5.trailing_zeros();
    if tz > 3 {
        return None;
    }
    let es = 8 << tz;
    let index = imm5 >> (tz + 1);
    let (n, d) = (field(i, 9, 5), field(i, 4, 0));
    if scalar {
        if op || imm4 != 0 {
            return None;
        }
        let v = b.elem(n, es, index);
        b.set_v_scalar(d, v);
        return Some(());
    }
    match (op, imm4) {
        (false, 0b0000 | 0b0001) => {
            // DUP (element), DUP (general).
            if es == 64 && !q {
                return None;
            }
            let v = if imm4 == 0 {
                b.elem(n, es, index)
            } else {
                let x = b.xr(n);
                b.mask(x, ones64(es))
            };
            let r = b.replicate(v, es);
            let hi = if q { r } else { b.k(0) };
            b.set_v(d, r, hi);
        }
        (false, 0b0011) if q => {
            // INS (general).
            let x = b.xr(n);
            b.insert(d, es, index, x);
        }
        (false, 0b0101) => {
            // SMOV.
            if es == 64 || (es == 32 && !q) {
                return None;
            }
            let v = b.elem(n, es, index);
            let v = b.extend(v, es as u8, true);
            let v = if q { v } else { b.trunc(Width::W32, v) };
            b.set_xr(d, v);
        }
        (false, 0b0111) => {
            // UMOV.
            if (es == 64) != q {
                return None;
            }
            let v = b.elem(n, es, index);
            b.set_xr(d, v);
        }
        (true, _) if q => {
            // INS (element).
            let v = b.elem(n, es, imm4 >> tz);
            b.insert(d, es, index, v);
        }
        _ => return None,
    }
    Some(())
}

/// MOVI, MVNI, ORR and BIC (immediate), FMOV (vector, immediate): the
/// constant as the interpreter decodes it.
fn modified_imm(b: &mut B, i: u32) -> Option<()> {
    use crate::interp::neon::ModImm;
    let q = bit(i, 30);
    let d = field(i, 4, 0);
    let op = crate::interp::neon::decode_modified_imm(i)?;
    let half = |b: &mut B, hi: bool| {
        let pick = |v: u128| if hi { (v >> 64) as u64 } else { v as u64 };
        match op {
            ModImm::Set(v) => b.k(pick(v)),
            ModImm::Or(v) => {
                let x = b.get(vhalf(d, hi));
                b.bin_k(BinOp::Or, Width::W64, x, pick(v))
            }
            ModImm::Bic(v) => {
                let x = b.get(vhalf(d, hi));
                b.bin_k(BinOp::And, Width::W64, x, !pick(v))
            }
        }
    };
    let lo = half(b, false);
    let hi = if q { half(b, true) } else { b.k(0) };
    b.set_v(d, lo, hi);
    Some(())
}

/// The scalar floating-point instructions that only move bits, as
/// `interp/fp.rs` runs them: FMOV (general, register and immediate),
/// FABS and FNEG. The rest goes to the interpreter.
fn fp(b: &mut B, i: u32) -> Option<()> {
    if bit(i, 29) || bit(i, 24) || !bit(i, 21) {
        return None;
    }
    let (n, d) = (field(i, 9, 5), field(i, 4, 0));
    let ftype = field(i, 23, 22);
    if field(i, 15, 10) == 0 {
        // FMOV (general), as `fp::fmov_general`; the conversions go to
        // the interpreter.
        let (sf, rmode, opcode) = (bit(i, 31), field(i, 20, 19), field(i, 18, 16));
        let width = match (sf, ftype, rmode) {
            (false, 0b00, 0b00) => 32,
            (true, 0b01, 0b00) => 64,
            (_, 0b11, 0b00) => 16,
            // Vn.D[1].
            (true, 0b10, 0b01) => 0,
            _ => return None,
        };
        match (width, opcode) {
            (0, 0b110) => {
                let v = b.get(Reg::VHi(n as u8));
                b.set_xr(d, v);
            }
            (0, 0b111) => {
                let x = b.xr(n);
                b.set(Reg::VHi(d as u8), x);
            }
            (_, 0b110) => {
                let v = b.get(Reg::VLo(n as u8));
                let v = b.mask(v, ones64(width));
                b.set_xr(d, v);
            }
            (_, 0b111) => {
                let x = b.xr(n);
                let x = b.mask(x, ones64(width));
                b.set_v_scalar(d, x);
            }
            _ => return None,
        }
        return Some(());
    }
    // The format's width, as `fp::ftype` accepts it: S, D or H.
    let width = match ftype {
        0b00 => 32,
        0b01 => 64,
        0b11 => 16,
        _ => return None,
    };
    if bit(i, 31) {
        return None;
    }
    let sign = 1u64 << (width - 1);
    if field(i, 14, 10) == 0b10000 {
        let v = b.get(Reg::VLo(n as u8));
        let v = b.mask(v, ones64(width));
        let r = match field(i, 20, 15) {
            0b000000 => v,                                         // FMOV
            0b000001 => b.bin_k(BinOp::And, Width::W64, v, !sign), // FABS
            0b000010 => b.bin_k(BinOp::Xor, Width::W64, v, sign),  // FNEG
            _ => return None,
        };
        b.set_v_scalar(d, r);
        return Some(());
    }
    if field(i, 13, 10) != 0b1000 && field(i, 12, 10) == 0b100 {
        // FMOV (scalar, immediate).
        if n != 0 {
            return None;
        }
        let f = match ftype {
            0b00 => crate::float::Fmt::S,
            0b01 => crate::float::Fmt::D,
            _ => crate::float::Fmt::H,
        };
        let v = b.k(crate::float::expand_imm(field(i, 20, 13), f));
        b.set_v_scalar(d, v);
        return Some(());
    }
    None
}
