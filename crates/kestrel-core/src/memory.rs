//! Guest virtual address space.
//!
//! This first version is a sorted set of mapped regions with per-region
//! permissions. It is simple and portable; it is not fast. The roadmap
//! (docs/ARCHITECTURE.md) replaces it with a host-mmap-backed arena plus a
//! software page table, which is also what the native (hypervisor) backend
//! on ARM hosts needs.

use std::collections::BTreeMap;
use std::fmt;

pub const PAGE_SIZE: u64 = 0x1000;

/// Region permissions.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Perm(u8);

impl Perm {
    pub const NONE: Perm = Perm(0);
    pub const R: Perm = Perm(1);
    pub const W: Perm = Perm(2);
    pub const X: Perm = Perm(4);
    pub const RW: Perm = Perm(3);
    pub const RX: Perm = Perm(5);
    pub const RWX: Perm = Perm(7);

    pub const fn contains(self, other: Perm) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Perm {
    type Output = Perm;
    fn bitor(self, rhs: Perm) -> Perm {
        Perm(self.0 | rhs.0)
    }
}

impl fmt::Debug for Perm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = |p: Perm, ch: char| if self.contains(p) { ch } else { '-' };
        write!(
            f,
            "{}{}{}",
            c(Perm::R, 'r'),
            c(Perm::W, 'w'),
            c(Perm::X, 'x')
        )
    }
}

/// Why a guest memory access failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemFault {
    Unmapped { addr: u64 },
    Permission { addr: u64, needed: Perm },
    Overlap { addr: u64 },
    Misaligned { addr: u64 },
}

impl fmt::Display for MemFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemFault::Unmapped { addr } => write!(f, "access to unmapped address {addr:#x}"),
            MemFault::Permission { addr, needed } => {
                write!(f, "permission fault at {addr:#x} (needed {needed:?})")
            }
            MemFault::Overlap { addr } => {
                write!(f, "mapping overlaps existing region at {addr:#x}")
            }
            MemFault::Misaligned { addr } => write!(f, "mapping not page aligned at {addr:#x}"),
        }
    }
}

impl std::error::Error for MemFault {}

struct Region {
    data: Vec<u8>,
    perm: Perm,
    name: String,
}

/// A sparse 64-bit guest address space.
#[derive(Default)]
pub struct Memory {
    regions: BTreeMap<u64, Region>,
    /// Single-entry lookup cache: (base, end). Most accesses hit the same region.
    last: std::cell::Cell<(u64, u64)>,
}

impl Memory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Map `size` zeroed bytes at `base`. Both must be page aligned.
    pub fn map(&mut self, base: u64, size: u64, perm: Perm, name: &str) -> Result<(), MemFault> {
        if base % PAGE_SIZE != 0 || size % PAGE_SIZE != 0 || size == 0 {
            return Err(MemFault::Misaligned { addr: base });
        }
        let end = base
            .checked_add(size)
            .ok_or(MemFault::Overlap { addr: base })?;
        if let Some((&b, r)) = self.regions.range(..end).next_back() {
            if b + r.data.len() as u64 > base {
                return Err(MemFault::Overlap { addr: base });
            }
        }
        self.regions.insert(
            base,
            Region {
                data: vec![0; size as usize],
                perm,
                name: name.to_string(),
            },
        );
        Ok(())
    }

    pub fn unmap(&mut self, base: u64) -> bool {
        self.last.set((0, 0));
        self.regions.remove(&base).is_some()
    }

    pub fn protect(&mut self, base: u64, perm: Perm) -> bool {
        match self.regions.get_mut(&base) {
            Some(r) => {
                r.perm = perm;
                true
            }
            None => false,
        }
    }

    pub fn is_mapped(&self, addr: u64) -> bool {
        self.find(addr).is_some()
    }

    /// Iterate regions as (base, size, perm, name).
    pub fn regions(&self) -> impl Iterator<Item = (u64, u64, Perm, &str)> {
        self.regions
            .iter()
            .map(|(&b, r)| (b, r.data.len() as u64, r.perm, r.name.as_str()))
    }

    fn find(&self, addr: u64) -> Option<u64> {
        let (b, e) = self.last.get();
        if addr >= b && addr < e {
            return Some(b);
        }
        let (&base, r) = self.regions.range(..=addr).next_back()?;
        let end = base + r.data.len() as u64;
        if addr < end {
            self.last.set((base, end));
            Some(base)
        } else {
            None
        }
    }

    /// Borrow `len` contiguous bytes. Accesses may not straddle regions.
    fn slice(&self, addr: u64, len: usize, need: Perm) -> Result<&[u8], MemFault> {
        let base = self.find(addr).ok_or(MemFault::Unmapped { addr })?;
        let r = &self.regions[&base];
        if !r.perm.contains(need) {
            return Err(MemFault::Permission { addr, needed: need });
        }
        let off = (addr - base) as usize;
        r.data.get(off..off + len).ok_or(MemFault::Unmapped {
            addr: base + r.data.len() as u64,
        })
    }

    fn slice_mut(&mut self, addr: u64, len: usize, need: Perm) -> Result<&mut [u8], MemFault> {
        let base = self.find(addr).ok_or(MemFault::Unmapped { addr })?;
        let r = self.regions.get_mut(&base).expect("region from find");
        if !r.perm.contains(need) {
            return Err(MemFault::Permission { addr, needed: need });
        }
        let off = (addr - base) as usize;
        let end = base + r.data.len() as u64;
        r.data
            .get_mut(off..off + len)
            .ok_or(MemFault::Unmapped { addr: end })
    }

    /// Read bytes, allowed to cross region boundaries.
    pub fn read_bytes(&self, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        self.copy_out(addr, out, Perm::R)
    }

    /// Write bytes, allowed to cross region boundaries.
    pub fn write_bytes(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.copy_in(addr, data, Perm::W)
    }

    /// Loader/kernel write that ignores page permissions.
    pub fn poke_bytes(&mut self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.copy_in(addr, data, Perm::NONE)
    }

    fn copy_out(&self, mut addr: u64, mut out: &mut [u8], need: Perm) -> Result<(), MemFault> {
        while !out.is_empty() {
            let base = self.find(addr).ok_or(MemFault::Unmapped { addr })?;
            let avail = (base + self.regions[&base].data.len() as u64 - addr) as usize;
            let n = avail.min(out.len());
            out[..n].copy_from_slice(self.slice(addr, n, need)?);
            out = &mut out[n..];
            addr += n as u64;
        }
        Ok(())
    }

    fn copy_in(&mut self, mut addr: u64, mut data: &[u8], need: Perm) -> Result<(), MemFault> {
        while !data.is_empty() {
            let base = self.find(addr).ok_or(MemFault::Unmapped { addr })?;
            let avail = (base + self.regions[&base].data.len() as u64 - addr) as usize;
            let n = avail.min(data.len());
            self.slice_mut(addr, n, need)?.copy_from_slice(&data[..n]);
            data = &data[n..];
            addr += n as u64;
        }
        Ok(())
    }

    /// Fetch one instruction word (needs execute permission).
    #[inline]
    pub fn fetch32(&self, addr: u64) -> Result<u32, MemFault> {
        let s = self.slice(addr, 4, Perm::X)?;
        Ok(u32::from_le_bytes(s.try_into().unwrap()))
    }

    /// Read a little-endian guest string of at most `max` bytes.
    pub fn read_vec(&self, addr: u64, len: usize) -> Result<Vec<u8>, MemFault> {
        let mut v = vec![0; len];
        self.read_bytes(addr, &mut v)?;
        Ok(v)
    }
}

macro_rules! rw_impl {
    ($read:ident, $write:ident, $t:ty) => {
        impl Memory {
            #[inline]
            pub fn $read(&self, addr: u64) -> Result<$t, MemFault> {
                const N: usize = std::mem::size_of::<$t>();
                match self.slice(addr, N, Perm::R) {
                    Ok(s) => Ok(<$t>::from_le_bytes(s.try_into().unwrap())),
                    Err(_) => {
                        let mut b = [0u8; N];
                        self.read_bytes(addr, &mut b)?;
                        Ok(<$t>::from_le_bytes(b))
                    }
                }
            }
            #[inline]
            pub fn $write(&mut self, addr: u64, v: $t) -> Result<(), MemFault> {
                const N: usize = std::mem::size_of::<$t>();
                match self.slice_mut(addr, N, Perm::W) {
                    Ok(s) => {
                        s.copy_from_slice(&v.to_le_bytes());
                        Ok(())
                    }
                    Err(_) => self.write_bytes(addr, &v.to_le_bytes()),
                }
            }
        }
    };
}

rw_impl!(read8, write8, u8);
rw_impl!(read16, write16, u16);
rw_impl!(read32, write32, u32);
rw_impl!(read64, write64, u64);
rw_impl!(read128, write128, u128);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_read_write() {
        let mut m = Memory::new();
        m.map(0x1000, 0x2000, Perm::RW, "test").unwrap();
        m.write64(0x1ffc, 0x1122_3344_5566_7788).unwrap();
        assert_eq!(m.read64(0x1ffc).unwrap(), 0x1122_3344_5566_7788);
        assert_eq!(m.read8(0x1ffc).unwrap(), 0x88);
        assert!(matches!(m.read8(0x3000), Err(MemFault::Unmapped { .. })));
    }

    #[test]
    fn overlap_rejected() {
        let mut m = Memory::new();
        m.map(0x1000, 0x2000, Perm::RW, "a").unwrap();
        assert!(m.map(0x2000, 0x1000, Perm::RW, "b").is_err());
        assert!(m.map(0x3000, 0x1000, Perm::RW, "c").is_ok());
    }

    #[test]
    fn cross_region_access() {
        let mut m = Memory::new();
        m.map(0x1000, 0x1000, Perm::RW, "a").unwrap();
        m.map(0x2000, 0x1000, Perm::RW, "b").unwrap();
        m.write32(0x1ffe, 0xdead_beef).unwrap();
        assert_eq!(m.read32(0x1ffe).unwrap(), 0xdead_beef);
    }

    #[test]
    fn permissions_enforced() {
        let mut m = Memory::new();
        m.map(0x1000, 0x1000, Perm::RX, "code").unwrap();
        assert!(matches!(
            m.write8(0x1000, 1),
            Err(MemFault::Permission { .. })
        ));
        m.poke_bytes(0x1000, &[0x1f, 0x20, 0x03, 0xd5]).unwrap();
        assert_eq!(m.fetch32(0x1000).unwrap(), 0xd503201f);
    }
}
