//! kestrel's own x86_64 encoder: the few instruction forms the recompiler
//! emits, written from the Intel 64 and IA-32 Architectures Software
//! Developer's Manual, volume 2 (instruction formats, REX prefixes,
//! ModRM and SIB bytes). Every form below is checked against llvm-mc's
//! encoding of the same instruction (`scripts/gen-x64-vectors.py` writes
//! the cases to `vectors.rs`), so where the manual allows two encodings
//! this one picks the one llvm-mc does.

/// A general-purpose register, numbered as in its encoding (RAX = 0 ..
/// R15 = 15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct R(pub u8);

pub const RAX: R = R(0);
pub const RCX: R = R(1);
pub const RDX: R = R(2);
pub const RBX: R = R(3);
pub const RSP: R = R(4);
pub const RBP: R = R(5);
pub const RSI: R = R(6);
pub const RDI: R = R(7);
pub const R8: R = R(8);
pub const R9: R = R(9);
pub const R10: R = R(10);
pub const R11: R = R(11);
pub const R12: R = R(12);
pub const R13: R = R(13);
pub const R14: R = R(14);
pub const R15: R = R(15);

/// Operand size: 32 bits (which clears the upper half of a destination
/// register) or 64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum W {
    W32,
    W64,
}
pub use W::*;

/// The eight classic ALU operations, numbered as their opcode extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alu {
    Add = 0,
    Or = 1,
    Adc = 2,
    Sbb = 3,
    And = 4,
    Sub = 5,
    Xor = 6,
    Cmp = 7,
}

/// Shifts and rotates, numbered as their opcode extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    Rol = 0,
    Ror = 1,
    Shl = 4,
    Shr = 5,
    Sar = 7,
}

/// The one-operand group 3 operations (opcode F7), by extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unary {
    Not = 2,
    Neg = 3,
    Mul = 4,
    Imul = 5,
    Div = 6,
    Idiv = 7,
}

/// Condition codes, numbered as in Jcc, SETcc and CMOVcc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cc {
    O = 0,
    No = 1,
    B = 2,
    Ae = 3,
    E = 4,
    Ne = 5,
    Be = 6,
    A = 7,
    S = 8,
    Ns = 9,
    P = 10,
    Np = 11,
    L = 12,
    Ge = 13,
    Le = 14,
    G = 15,
}

/// A position in the code to patch with a jump's target.
#[derive(Clone, Copy, Debug)]
pub struct Fixup(usize);

/// A memory operand: `[base + disp]`.
#[derive(Clone, Copy, Debug)]
pub struct M(pub R, pub i32);

#[derive(Default)]
pub struct Asm {
    pub code: Vec<u8>,
}

impl Asm {
    pub fn new() -> Self {
        Self::default()
    }

    fn b(&mut self, v: u8) {
        self.code.push(v);
    }

    fn d32(&mut self, v: i32) {
        self.code.extend_from_slice(&v.to_le_bytes());
    }

    /// A REX prefix when one is needed: 64-bit size, an extended register
    /// in ModRM.reg or ModRM.rm, or a `byte` register SPL..DIL, which only
    /// exists with a REX prefix.
    fn rex(&mut self, w: W, reg: u8, rm: u8, byte: Option<u8>) {
        let mut rex = 0x40;
        if w == W64 {
            rex |= 8;
        }
        if reg >= 8 {
            rex |= 4;
        }
        if rm >= 8 {
            rex |= 1;
        }
        if rex != 0x40 || byte.is_some_and(|r| (4..8).contains(&r)) {
            self.b(rex);
        }
    }

    fn modrm_rr(&mut self, reg: u8, rm: u8) {
        self.b(0xc0 | (reg & 7) << 3 | (rm & 7));
    }

    /// ModRM (and SIB and displacement) for `[base + disp]`: no
    /// displacement when it is 0 (except for RBP and R13, which need one),
    /// one byte when it fits, else four; RSP and R12 need a SIB byte.
    fn modrm_m(&mut self, reg: u8, m: M) {
        let M(base, disp) = m;
        let b = base.0 & 7;
        let md = if disp == 0 && b != 5 {
            0
        } else if i8::try_from(disp).is_ok() {
            1
        } else {
            2
        };
        self.b(md << 6 | (reg & 7) << 3 | b);
        if b == 4 {
            self.b(0x24);
        }
        match md {
            1 => self.b(disp as u8),
            2 => self.d32(disp),
            _ => {}
        }
    }

    /// `op reg, rm` or `op rm, reg` between registers.
    fn op_rr(&mut self, w: W, op: &[u8], reg: R, rm: R) {
        self.rex(w, reg.0, rm.0, None);
        self.code.extend_from_slice(op);
        self.modrm_rr(reg.0, rm.0);
    }

    fn op_rm(&mut self, w: W, op: &[u8], reg: u8, m: M) {
        self.rex(w, reg, m.0 .0, None);
        self.code.extend_from_slice(op);
        self.modrm_m(reg, m);
    }

    /// `mov dst, src`.
    pub fn mov_rr(&mut self, w: W, dst: R, src: R) {
        self.op_rr(w, &[0x89], src, dst);
    }

    /// `mov dst, [m]`.
    pub fn load(&mut self, w: W, dst: R, m: M) {
        self.op_rm(w, &[0x8b], dst.0, m);
    }

    /// `mov [m], src`.
    pub fn store(&mut self, w: W, m: M, src: R) {
        self.op_rm(w, &[0x89], src.0, m);
    }

    /// `mov byte [m], src` (the low byte of src).
    pub fn store8(&mut self, m: M, src: R) {
        let (reg, base) = (src.0, m.0 .0);
        self.rex(W32, reg, base, Some(reg));
        self.b(0x88);
        self.modrm_m(reg, m);
    }

    /// `mov byte [m], imm`.
    pub fn store8_imm(&mut self, m: M, imm: u8) {
        self.op_rm(W32, &[0xc6], 0, m);
        self.b(imm);
    }

    /// `movzx dst32, byte [m]`.
    pub fn load8_zx(&mut self, dst: R, m: M) {
        self.op_rm(W32, &[0x0f, 0xb6], dst.0, m);
    }

    /// `mov dst, imm`, in the shortest form: `mov r32, imm32` (which
    /// clears the upper half) for values up to 2^32 - 1, `mov r64, imm32`
    /// (sign-extended) for other values that fit, else `movabs`.
    pub fn mov_ri(&mut self, dst: R, imm: u64) {
        if imm <= u32::MAX as u64 {
            self.rex(W32, 0, dst.0, None);
            self.b(0xb8 + (dst.0 & 7));
            self.d32(imm as u32 as i32);
        } else if let Ok(v) = i32::try_from(imm as i64) {
            self.rex(W64, 0, dst.0, None);
            self.b(0xc7);
            self.modrm_rr(0, dst.0);
            self.d32(v);
        } else {
            self.rex(W64, 0, dst.0, None);
            self.b(0xb8 + (dst.0 & 7));
            self.code.extend_from_slice(&imm.to_le_bytes());
        }
    }

    /// `op dst, src`.
    pub fn alu_rr(&mut self, op: Alu, w: W, dst: R, src: R) {
        self.op_rr(w, &[(op as u8) << 3 | 1], src, dst);
    }

    /// `op dst, [m]`.
    pub fn alu_rm(&mut self, op: Alu, w: W, dst: R, m: M) {
        self.op_rm(w, &[(op as u8) << 3 | 3], dst.0, m);
    }

    /// `op dst, imm`: a sign-extended byte when it fits, the accumulator's
    /// short form for RAX, else a 32-bit immediate (sign-extended to 64).
    pub fn alu_ri(&mut self, op: Alu, w: W, dst: R, imm: i32) {
        if let Ok(v) = i8::try_from(imm) {
            self.op_rr(w, &[0x83], R(op as u8), dst);
            self.b(v as u8);
        } else if dst == RAX {
            self.rex(w, 0, 0, None);
            self.b((op as u8) << 3 | 5);
            self.d32(imm);
        } else {
            self.op_rr(w, &[0x81], R(op as u8), dst);
            self.d32(imm);
        }
    }

    /// `test a, b`.
    pub fn test_rr(&mut self, w: W, a: R, b: R) {
        self.op_rr(w, &[0x85], b, a);
    }

    /// `shift dst, imm` (the one-bit form for 1).
    pub fn shift_ri(&mut self, op: Shift, w: W, dst: R, imm: u8) {
        if imm == 1 {
            self.op_rr(w, &[0xd1], R(op as u8), dst);
        } else {
            self.op_rr(w, &[0xc1], R(op as u8), dst);
            self.b(imm);
        }
    }

    /// `shift dst, cl`.
    pub fn shift_cl(&mut self, op: Shift, w: W, dst: R) {
        self.op_rr(w, &[0xd3], R(op as u8), dst);
    }

    /// `imul dst, src`.
    pub fn imul_rr(&mut self, w: W, dst: R, src: R) {
        self.op_rr(w, &[0x0f, 0xaf], dst, src);
    }

    /// `not`, `neg`, `mul`, `imul`, `div` or `idiv` of one register.
    pub fn unary(&mut self, op: Unary, w: W, r: R) {
        self.op_rr(w, &[0xf7], R(op as u8), r);
    }

    /// `cqo` (64-bit) or `cdq` (32-bit): sign-extend RAX into RDX.
    pub fn cqo(&mut self, w: W) {
        self.rex(w, 0, 0, None);
        self.b(0x99);
    }

    /// `movzx dst32, src8`.
    pub fn movzx8(&mut self, dst: R, src: R) {
        self.rex(W32, dst.0, src.0, Some(src.0));
        self.code.extend_from_slice(&[0x0f, 0xb6]);
        self.modrm_rr(dst.0, src.0);
    }

    /// `movzx dst32, src16`.
    pub fn movzx16(&mut self, dst: R, src: R) {
        self.op_rr(W32, &[0x0f, 0xb7], dst, src);
    }

    /// `movsx dst64, src8`.
    pub fn movsx8(&mut self, dst: R, src: R) {
        self.rex(W64, dst.0, src.0, Some(src.0));
        self.code.extend_from_slice(&[0x0f, 0xbe]);
        self.modrm_rr(dst.0, src.0);
    }

    /// `movsx dst64, src16`.
    pub fn movsx16(&mut self, dst: R, src: R) {
        self.op_rr(W64, &[0x0f, 0xbf], dst, src);
    }

    /// `movsxd dst64, src32`.
    pub fn movsxd(&mut self, dst: R, src: R) {
        self.op_rr(W64, &[0x63], dst, src);
    }

    /// `setcc byte [m]`.
    pub fn setcc_m(&mut self, cc: Cc, m: M) {
        self.op_rm(W32, &[0x0f, 0x90 | cc as u8], 0, m);
    }

    /// `setcc dst8`.
    pub fn setcc_r(&mut self, cc: Cc, dst: R) {
        self.rex(W32, 0, dst.0, Some(dst.0));
        self.code.extend_from_slice(&[0x0f, 0x90 | cc as u8]);
        self.modrm_rr(0, dst.0);
    }

    /// `cmovcc dst, src`.
    pub fn cmov(&mut self, cc: Cc, w: W, dst: R, src: R) {
        self.op_rr(w, &[0x0f, 0x40 | cc as u8], dst, src);
    }

    /// `bt r, imm`: CF = bit `imm` of r.
    pub fn bt_ri(&mut self, w: W, r: R, imm: u8) {
        self.op_rr(w, &[0x0f, 0xba], R(4), r);
        self.b(imm);
    }

    /// `bswap r`.
    pub fn bswap(&mut self, w: W, r: R) {
        self.rex(w, 0, r.0, None);
        self.code.extend_from_slice(&[0x0f, 0xc8 + (r.0 & 7)]);
    }

    /// `lea dst, [m]`.
    pub fn lea(&mut self, dst: R, m: M) {
        self.op_rm(W64, &[0x8d], dst.0, m);
    }

    pub fn stc(&mut self) {
        self.b(0xf9);
    }

    pub fn clc(&mut self) {
        self.b(0xf8);
    }

    pub fn push(&mut self, r: R) {
        self.rex(W32, 0, r.0, None);
        self.b(0x50 + (r.0 & 7));
    }

    pub fn pop(&mut self, r: R) {
        self.rex(W32, 0, r.0, None);
        self.b(0x58 + (r.0 & 7));
    }

    /// `call r`.
    pub fn call_r(&mut self, r: R) {
        self.rex(W32, 0, r.0, None);
        self.b(0xff);
        self.modrm_rr(2, r.0);
    }

    pub fn ret(&mut self) {
        self.b(0xc3);
    }

    /// `jcc rel32` to a target bound later with [`Asm::bind`].
    pub fn jcc(&mut self, cc: Cc) -> Fixup {
        self.code.extend_from_slice(&[0x0f, 0x80 | cc as u8]);
        self.d32(0);
        Fixup(self.code.len())
    }

    /// `jmp rel32` to a target bound later with [`Asm::bind`].
    pub fn jmp(&mut self) -> Fixup {
        self.b(0xe9);
        self.d32(0);
        Fixup(self.code.len())
    }

    /// Point the jump at `f` to the current position.
    pub fn bind(&mut self, f: Fixup) {
        let rel = (self.code.len() - f.0) as i32;
        self.code[f.0 - 4..f.0].copy_from_slice(&rel.to_le_bytes());
    }

    /// Point the jump at `f` to an earlier position `to`.
    pub fn bind_to(&mut self, f: Fixup, to: usize) {
        let rel = to as i64 - f.0 as i64;
        self.code[f.0 - 4..f.0].copy_from_slice(&(rel as i32).to_le_bytes());
    }

    pub fn pos(&self) -> usize {
        self.code.len()
    }
}

#[cfg(test)]
#[rustfmt::skip] // generated by scripts/gen-x64-vectors.py
mod vectors;
