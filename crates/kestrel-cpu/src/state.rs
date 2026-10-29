//! Architectural state of one guest ARM64 (AArch64, EL0) thread.

use std::fmt;

/// NZCV condition flags.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub n: bool,
    pub z: bool,
    pub c: bool,
    pub v: bool,
}

impl Flags {
    /// Pack into the NZCV system-register layout (bits 31..28).
    pub fn to_nzcv(self) -> u64 {
        (self.n as u64) << 31
            | (self.z as u64) << 30
            | (self.c as u64) << 29
            | (self.v as u64) << 28
    }

    pub fn from_nzcv(v: u64) -> Self {
        Flags {
            n: v >> 31 & 1 != 0,
            z: v >> 30 & 1 != 0,
            c: v >> 29 & 1 != 0,
            v: v >> 28 & 1 != 0,
        }
    }

    /// Evaluate an A64 condition code (EQ, NE, ... AL, NV).
    #[inline]
    pub fn cond(self, cond: u32) -> bool {
        let r = match cond >> 1 {
            0 => self.z,
            1 => self.c,
            2 => self.n,
            3 => self.v,
            4 => self.c && !self.z,
            5 => self.n == self.v,
            6 => self.n == self.v && !self.z,
            _ => true,
        };
        // Odd codes invert, except 0b1111 (NV) which also means "always".
        if cond & 1 == 1 && cond != 0b1111 {
            !r
        } else {
            r
        }
    }
}

impl fmt::Debug for Flags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = |b: bool, ch: char| if b { ch } else { '-' };
        write!(
            f,
            "{}{}{}{}",
            c(self.n, 'N'),
            c(self.z, 'Z'),
            c(self.c, 'C'),
            c(self.v, 'V')
        )
    }
}

/// Register file and user-visible system state.
#[derive(Clone, Default)]
pub struct CpuState {
    /// X0..X30. Register 31 is SP or XZR depending on the instruction.
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub flags: Flags,
    /// V0..V31, 128 bits each.
    pub v: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
    /// Thread pointer registers (TLS).
    pub tpidr_el0: u64,
    pub tpidrro_el0: u64,
    /// Exclusive monitor for LDXR/STXR.
    pub exclusive_addr: Option<u64>,
    /// Instructions retired, also used as the virtual counter.
    pub retired: u64,
}

impl CpuState {
    /// Read Xn where 31 means XZR.
    #[inline(always)]
    pub fn xr(&self, n: u32) -> u64 {
        if n == 31 {
            0
        } else {
            self.x[n as usize]
        }
    }

    /// Write Xn where 31 means XZR (discard).
    #[inline(always)]
    pub fn set_xr(&mut self, n: u32, v: u64) {
        if n != 31 {
            self.x[n as usize] = v;
        }
    }

    /// Read Xn where 31 means SP.
    #[inline(always)]
    pub fn xsp(&self, n: u32) -> u64 {
        if n == 31 {
            self.sp
        } else {
            self.x[n as usize]
        }
    }

    /// Write Xn where 31 means SP.
    #[inline(always)]
    pub fn set_xsp(&mut self, n: u32, v: u64) {
        if n == 31 {
            self.sp = v;
        } else {
            self.x[n as usize] = v;
        }
    }
}

impl fmt::Debug for CpuState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "pc={:#018x} sp={:#018x} nzcv={:?}",
            self.pc, self.sp, self.flags
        )?;
        for row in 0..8 {
            for col in 0..4 {
                let i = row * 4 + col;
                if i < 31 {
                    write!(f, "x{:<2}={:#018x}  ", i, self.x[i])?;
                }
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_codes() {
        let z = Flags {
            z: true,
            ..Default::default()
        };
        assert!(z.cond(0)); // EQ
        assert!(!z.cond(1)); // NE
        assert!(z.cond(14)); // AL
        assert!(z.cond(15)); // NV behaves as always
        let lt = Flags {
            n: true,
            ..Default::default()
        };
        assert!(lt.cond(11)); // LT
        assert!(!lt.cond(10)); // GE
    }
}
