//! Executable loaders.
//!
//! Static AArch64 ELF64 executables, which is what the test programs in
//! `guest/` build to, and the open homebrew executable format (NRO), so
//! community-written homebrew runs without any console files.
//!
//! The loaders deliberately do not handle encrypted containers. Decryption
//! keys come from the user's own console and are out of scope for this repo.

pub mod elf;
pub mod nro;

pub use elf::load_elf;
pub use nro::{is_nro, load_nro, Assets, Nro};

use kestrel_core::MemFault;
use std::fmt;

#[derive(Debug)]
pub enum LoadError {
    NotElf,
    Unsupported(&'static str),
    Truncated,
    InvalidNro(&'static str),
    UnsupportedNro(&'static str),
    Memory(MemFault),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::NotElf => write!(f, "not an ELF file"),
            LoadError::Unsupported(what) => write!(f, "unsupported ELF: {what}"),
            LoadError::Truncated => write!(f, "ELF file is truncated"),
            LoadError::InvalidNro(why) => write!(f, "invalid NRO: {why}"),
            LoadError::UnsupportedNro(what) => write!(f, "unsupported NRO: {what}"),
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

pub(crate) fn rd16(b: &[u8], o: usize) -> Result<u16, LoadError> {
    b.get(o..o + 2)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
        .ok_or(LoadError::Truncated)
}
pub(crate) fn rd32(b: &[u8], o: usize) -> Result<u32, LoadError> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or(LoadError::Truncated)
}
pub(crate) fn rd64(b: &[u8], o: usize) -> Result<u64, LoadError> {
    b.get(o..o + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or(LoadError::Truncated)
}
