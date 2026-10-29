//! Minimal static ELF64 (AArch64, little-endian) loader.

use kestrel_core::{MemFault, Memory, Perm, PAGE_SIZE};
use std::fmt;

const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

#[derive(Debug)]
pub enum LoadError {
    NotElf,
    Unsupported(&'static str),
    Truncated,
    Memory(MemFault),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NotElf => write!(f, "not an ELF file"),
            LoadError::Unsupported(what) => write!(f, "unsupported ELF: {what}"),
            LoadError::Truncated => write!(f, "ELF file is truncated"),
            LoadError::Memory(e) => write!(f, "could not map segment: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<MemFault> for LoadError {
    fn from(e: MemFault) -> Self {
        LoadError::Memory(e)
    }
}

/// Result of loading an image.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub entry: u64,
    /// Lowest and highest (exclusive) mapped addresses of the image.
    pub image_start: u64,
    pub image_end: u64,
}

fn rd16(b: &[u8], o: usize) -> Result<u16, LoadError> {
    b.get(o..o + 2)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
        .ok_or(LoadError::Truncated)
}
fn rd32(b: &[u8], o: usize) -> Result<u32, LoadError> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or(LoadError::Truncated)
}
fn rd64(b: &[u8], o: usize) -> Result<u64, LoadError> {
    b.get(o..o + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or(LoadError::Truncated)
}

/// Map every PT_LOAD segment of `file` into `mem`.
pub fn load_elf(file: &[u8], mem: &mut Memory) -> Result<Loaded, LoadError> {
    if file.get(0..4) != Some(b"\x7fELF") {
        return Err(LoadError::NotElf);
    }
    if file[4] != 2 {
        return Err(LoadError::Unsupported("not 64-bit"));
    }
    if file[5] != 1 {
        return Err(LoadError::Unsupported("not little-endian"));
    }
    if rd16(file, 18)? != EM_AARCH64 {
        return Err(LoadError::Unsupported("machine is not AArch64"));
    }
    if rd16(file, 16)? != 2 {
        return Err(LoadError::Unsupported(
            "only static executables (ET_EXEC) are supported",
        ));
    }
    let entry = rd64(file, 24)?;
    let phoff = rd64(file, 32)? as usize;
    let phentsize = rd16(file, 54)? as usize;
    let phnum = rd16(file, 56)? as usize;

    let mut start = u64::MAX;
    let mut end = 0u64;
    for idx in 0..phnum {
        let ph = phoff + idx * phentsize;
        if rd32(file, ph)? != PT_LOAD {
            continue;
        }
        let flags = rd32(file, ph + 4)?;
        let offset = rd64(file, ph + 8)? as usize;
        let vaddr = rd64(file, ph + 16)?;
        let filesz = rd64(file, ph + 32)? as usize;
        let memsz = rd64(file, ph + 40)?;
        if memsz == 0 {
            continue;
        }
        let base = vaddr & !(PAGE_SIZE - 1);
        let top = (vaddr + memsz + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let mut perm = Perm::NONE;
        if flags & PF_R != 0 {
            perm = perm | Perm::R;
        }
        if flags & PF_W != 0 {
            perm = perm | Perm::W;
        }
        if flags & PF_X != 0 {
            perm = perm | Perm::X;
        }
        // Segments may share a page boundary; map only pages not yet mapped.
        let mut page = base;
        while page < top {
            if !mem.is_mapped(page) {
                let mut run_end = page;
                while run_end < top && !mem.is_mapped(run_end) {
                    run_end += PAGE_SIZE;
                }
                mem.map(page, run_end - page, perm, &format!("elf:{vaddr:#x}"))?;
                page = run_end;
            } else {
                page += PAGE_SIZE;
            }
        }
        let data = file
            .get(offset..offset + filesz)
            .ok_or(LoadError::Truncated)?;
        mem.poke_bytes(vaddr, data)?;
        start = start.min(base);
        end = end.max(top);
    }
    if start == u64::MAX {
        return Err(LoadError::Unsupported("no loadable segments"));
    }
    Ok(Loaded {
        entry,
        image_start: start,
        image_end: end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_elf() {
        let mut m = Memory::new();
        assert!(matches!(load_elf(b"hello", &mut m), Err(LoadError::NotElf)));
    }
}
