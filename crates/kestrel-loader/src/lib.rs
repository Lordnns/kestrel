//! Executable loaders.
//!
//! Today: static AArch64 ELF64 executables, which is what the test programs
//! in `guest/` build to. Next on the roadmap is the open homebrew executable
//! format, so community-written homebrew can run without any console files.
//!
//! The loaders deliberately do not handle encrypted containers. Decryption
//! keys come from the user's own console and are out of scope for this repo.

pub mod elf;

pub use elf::{load_elf, LoadError, Loaded};
