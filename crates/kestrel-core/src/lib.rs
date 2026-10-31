//! Shared building blocks: the guest address space and common types.
//!
//! Everything here is host-architecture neutral so the same code runs on
//! x86_64 and aarch64 hosts.

pub mod arena;
mod atomic128;
mod host;
pub mod memory;

pub use arena::{Arena, ARENA_ALIGN, DEFAULT_ARENA_SIZE};
pub use memory::{Block, MemFault, Memory, Perm, Tag, PAGE_SIZE};

/// Host CPU architecture the emulator was compiled for.
pub fn host_arch() -> &'static str {
    std::env::consts::ARCH
}

/// The host's page size: 4 KiB on x86_64 and Windows, 16 KiB on Apple
/// Silicon (macOS and Asahi Linux), 64 KiB on some aarch64 Linux kernels.
/// The guest's is always [`PAGE_SIZE`], 4 KiB.
pub fn host_page_size() -> usize {
    host::page_size()
}
