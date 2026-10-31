//! The recompiler's intermediate representation: what a block of guest
//! ARM64 code does, in terms no host instruction set appears in. The
//! frontend (`frontend.rs`) writes it from decoded instructions; a code
//! generator (`x64/`) turns it into host code. Another host's generator
//! could read the same blocks.
//!
//! A block is a straight list of operations on values (`Val`), each set
//! once, ending in a terminator that says where the guest goes next.
//! Guest registers, flags and memory are read and written only through
//! operations, in guest program order, so a generator may keep guest
//! state wherever it likes between them.

/// A value: the result of one operation, set once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Val(pub u32);

/// Operation width: 32-bit operations see and produce the low 32 bits,
/// with the upper 32 bits of the result zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    W32,
    W64,
}

impl Width {
    pub fn bits(self) -> u32 {
        match self {
            Width::W32 => 32,
            Width::W64 => 64,
        }
    }

    pub fn of(sf: bool) -> Self {
        if sf {
            Width::W64
        } else {
            Width::W32
        }
    }
}

/// A guest register an operation reads or writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reg {
    /// X0..X30.
    X(u8),
    Sp,
}

/// Two-operand integer operations. Shifts and rotates take their amount
/// modulo the width, as Arm's LSLV, LSRV, ASRV and RORV do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    And,
    Or,
    Xor,
    Shl,
    Lsr,
    Asr,
    Ror,
    Mul,
    /// The upper 64 bits of the 128-bit product, unsigned or signed
    /// (64-bit width only).
    UMulHigh,
    SMulHigh,
    /// Arm's division: by zero gives 0, and the most negative value
    /// divided by -1 gives itself.
    UDiv,
    SDiv,
}

/// The carry into an add-with-carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carry {
    Zero,
    One,
    /// The guest's C flag.
    Flag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Start of the guest instruction at `pc`, the `index`th of the block
    /// (counting from 0): the instructions before it have retired. A fault
    /// in the operations that follow stops at this instruction.
    Insn {
        index: u32,
        pc: u64,
    },
    Const {
        dst: Val,
        value: u64,
    },
    Get {
        dst: Val,
        reg: Reg,
    },
    Set {
        reg: Reg,
        src: Val,
    },
    Bin {
        op: BinOp,
        w: Width,
        dst: Val,
        a: Val,
        b: Val,
    },
    /// `dst = a + b + carry`, Arm's AddWithCarry; with `flags`, NZCV are
    /// set from it as Arm defines them.
    Adc {
        w: Width,
        dst: Val,
        a: Val,
        b: Val,
        carry: Carry,
        flags: bool,
    },
    /// Set N and Z from `a` at width `w`, and C and V to 0 (the logical
    /// operations' flags).
    LogicFlags {
        w: Width,
        a: Val,
    },
    /// Bitwise not at width `w`.
    Not {
        w: Width,
        dst: Val,
        a: Val,
    },
    /// Extend the low `bits` (8, 16 or 32) of `a` to 64 bits.
    Extend {
        dst: Val,
        a: Val,
        bits: u8,
        signed: bool,
    },
    /// 1 if the Arm condition `cond` (0..15) holds on NZCV, else 0.
    Cond {
        dst: Val,
        cond: u8,
    },
    /// 1 if `a` is zero at width `w`, else 0.
    IsZero {
        w: Width,
        dst: Val,
        a: Val,
    },
    /// `dst = c != 0 ? a : b`, at width `w`.
    Select {
        w: Width,
        dst: Val,
        c: Val,
        a: Val,
        b: Val,
    },
    /// NZCV as bits 31..28 of a value.
    GetNzcv {
        dst: Val,
    },
    /// Set NZCV from bits 31..28 of `src`.
    SetNzcv {
        src: Val,
    },
    /// Load `size` (1, 2, 4 or 8) bytes, zero-extended.
    Load {
        dst: Val,
        addr: Val,
        size: u8,
    },
    /// Store the low `size` bytes of `src`.
    Store {
        addr: Val,
        src: Val,
        size: u8,
    },
    /// Run the current instruction (`insn`, at the `pc` of the last
    /// [`Op::Insn`]) in the interpreter. It may end the block: a branch
    /// it takes, a system call or a fault.
    Interp {
        insn: u32,
    },
}

/// Where the guest goes when the block ends. Every instruction of the
/// block has retired by then, except BRK, which does not retire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    /// Go on at a known address.
    Jump(u64),
    /// Go on at a computed address.
    JumpReg(Val),
    /// `taken` if `c` is non-zero, else `fallthrough`.
    Branch {
        c: Val,
        taken: u64,
        fallthrough: u64,
    },
    /// SVC #imm: the call goes to the OS layer, then the guest goes on at
    /// `next`.
    Svc { imm: u16, next: u64 },
    /// BRK #imm at `pc`: the program stops there.
    Brk { imm: u16, pc: u64 },
}

/// A translated block of guest code.
#[derive(Clone, Debug)]
pub struct Block {
    /// Guest address of the first instruction.
    pub pc: u64,
    /// Guest instructions in the block.
    pub insns: u32,
    pub ops: Vec<Op>,
    pub term: Term,
    /// Values used: `Val(0)` to `Val(vals - 1)`.
    pub vals: u32,
}
