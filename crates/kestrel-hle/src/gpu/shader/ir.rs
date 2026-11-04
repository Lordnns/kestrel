//! kestrel's shader IR: straight-line operations on 32-bit registers, as
//! Maxwell's are (255 registers and RZ, which reads 0 and drops writes).
//! A register holds bits; each operation says how it reads them (as a
//! float, a signed or an unsigned integer). The Maxwell decoder
//! (`maxwell.rs`) produces it; `spirv.rs` translates it.

use super::Header;

/// RZ: reads as zero, writes are dropped.
pub const RZ: u8 = 0xff;

/// An operand: a register, an immediate (its 32 bits), or a word of a
/// constant buffer, at a byte offset, plus a register's value if
/// indexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operand {
    Reg(u8),
    Imm(u32),
    Cbuf { index: u8, offset: i32, reg: u8 },
}

/// A float operand with its modifiers: absolute value first, then
/// negation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FSrc {
    pub op: Operand,
    pub neg: bool,
    pub abs: bool,
}

impl FSrc {
    pub fn plain(op: Operand) -> FSrc {
        FSrc {
            op,
            neg: false,
            abs: false,
        }
    }
}

/// MUFU's functions kestrel translates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mufu {
    Rcp,
    Rsq,
    Ex2,
    Lg2,
    Sqrt,
}

/// How an attribute is interpolated (IPA's mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Interp {
    /// Linear in screen space ("pass").
    Linear,
    /// Perspective-correct, then multiplied by a register.
    Perspective,
    /// The provoking vertex's value ("constant").
    Flat,
}

/// Rounding for conversions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Round {
    /// To the nearest, ties to even.
    Nearest,
    Floor,
    Ceil,
    Trunc,
}

/// One operation. `dst` registers of RZ drop the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    /// dst = the operand's bits.
    Mov { dst: u8, src: Operand },
    /// dst = a + b, saturated to 0..1 if `sat`.
    FAdd {
        dst: u8,
        a: FSrc,
        b: FSrc,
        sat: bool,
    },
    /// dst = a * b * 2^scale.
    FMul {
        dst: u8,
        a: FSrc,
        b: FSrc,
        scale: i8,
        sat: bool,
    },
    /// dst = a * b + c, fused.
    FFma {
        dst: u8,
        a: FSrc,
        b: FSrc,
        c: FSrc,
        sat: bool,
    },
    Mufu {
        dst: u8,
        func: Mufu,
        src: FSrc,
        sat: bool,
    },
    /// dst = a << b; b past 31 gives 0, or with `wrap` shifts by b & 31.
    Shl {
        dst: u8,
        a: Operand,
        b: Operand,
        wrap: bool,
    },
    /// dst = the integer `src` as a float.
    I2F {
        dst: u8,
        src: Operand,
        signed: bool,
        neg: bool,
        abs: bool,
    },
    /// dst = the float `src` as an integer of `bits` (8, 16 or 32),
    /// rounded, saturated to the integer's range, then widened to 32 bits
    /// (sign-extended if signed).
    F2I {
        dst: u8,
        src: FSrc,
        signed: bool,
        bits: u8,
        round: Round,
    },
    /// dst = the float `src`, rounded to an integral value if `round`.
    F2F {
        dst: u8,
        src: FSrc,
        round: Option<Round>,
        sat: bool,
    },
    /// Load `count` words of attributes from `addr`.
    LdAttr { dst: u8, count: u8, addr: u16 },
    /// Store `count` registers from `src` to the output attributes at
    /// `addr`.
    StAttr { src: u8, count: u8, addr: u16 },
    /// Interpolate the input attribute at `addr`: perspective-correct
    /// values are multiplied by `mult` (Maxwell's way of finishing the
    /// division by w).
    Ipa {
        dst: u8,
        addr: u16,
        mode: Interp,
        mult: u8,
        sat: bool,
    },
    /// Sample a 2D texture whose handle is word `handle` of the texture
    /// constant buffer, at (x, y); the components in `mask` (bit 0 R ...
    /// bit 3 A) go, in order, to dsts[0], dsts[0] + 1, dsts[1],
    /// dsts[1] + 1.
    Tex2d {
        dsts: [u8; 2],
        mask: u8,
        x: u8,
        y: u8,
        handle: u16,
    },
    /// The end of the program.
    Exit,
}

/// A decoded program: its header and its operations, ending with Exit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Program {
    pub header: Header,
    pub ops: Vec<Op>,
}
