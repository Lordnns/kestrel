//! Shared building blocks: the guest address space and common types.
//!
//! Everything here is host-architecture neutral so the same code runs on
//! x86_64 and aarch64 hosts.

pub mod memory;

pub use memory::{MemFault, Memory, Perm, PAGE_SIZE};

/// Host CPU architecture the emulator was compiled for.
pub fn host_arch() -> &'static str {
    std::env::consts::ARCH
}
