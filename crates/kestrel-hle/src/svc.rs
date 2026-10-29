//! Supervisor call dispatch.
//!
//! Arguments arrive in X0..X7, results go back in W0 (result code) plus any
//! output registers. A result code of 0 means success.

use crate::{layout, Process, Stop};
use kestrel_core::Perm;
use std::io::Write;

/// Publicly documented SVC numbers implemented so far.
pub mod nr {
    pub const SET_HEAP_SIZE: u16 = 0x01;
    pub const EXIT_PROCESS: u16 = 0x07;
    pub const SLEEP_THREAD: u16 = 0x0b;
    pub const GET_SYSTEM_TICK: u16 = 0x1e;
    pub const BREAK: u16 = 0x26;
    pub const OUTPUT_DEBUG_STRING: u16 = 0x27;
}

/// Result codes (module 1 = kernel). Values match the public ABI.
pub mod result {
    pub const SUCCESS: u64 = 0;
    pub const INVALID_SIZE: u64 = 0xca01;
    pub const INVALID_ADDRESS: u64 = 0xcc01;
}

/// Handle one SVC. Returns `Some(stop)` if the process should stop.
pub fn dispatch(p: &mut Process, n: u16) -> Option<Stop> {
    match n {
        nr::SET_HEAP_SIZE => {
            let size = p.cpu.x[1];
            if size % 0x20_0000 != 0 {
                p.cpu.x[0] = result::INVALID_SIZE;
                return None;
            }
            if p.heap_size != 0 {
                p.mem.unmap(layout::HEAP_BASE);
            }
            if size != 0 {
                if let Err(e) = p.mem.map(layout::HEAP_BASE, size, Perm::RW, "heap") {
                    return Some(Stop::Error(format!("SetHeapSize({size:#x}) failed: {e}")));
                }
            }
            p.heap_size = size;
            p.cpu.x[0] = result::SUCCESS;
            p.cpu.x[1] = layout::HEAP_BASE;
            None
        }
        nr::EXIT_PROCESS => Some(Stop::Exited),
        nr::SLEEP_THREAD => {
            // Single thread: yielding or sleeping just returns.
            p.cpu.x[0] = result::SUCCESS;
            None
        }
        nr::GET_SYSTEM_TICK => {
            p.cpu.x[0] = p.cpu.retired;
            None
        }
        nr::BREAK => Some(Stop::Break {
            pc: p.cpu.pc - 4,
            reason: p.cpu.x[0],
        }),
        nr::OUTPUT_DEBUG_STRING => {
            let (addr, len) = (p.cpu.x[0], p.cpu.x[1]);
            match read_guest(p, addr, len) {
                Ok(bytes) => {
                    if p.echo {
                        let mut out = std::io::stdout().lock();
                        let _ = out.write_all(&bytes);
                        let _ = out.flush();
                    }
                    p.debug_output.extend_from_slice(&bytes);
                    p.cpu.x[0] = result::SUCCESS;
                }
                Err(_) => p.cpu.x[0] = result::INVALID_ADDRESS,
            }
            None
        }
        other => Some(Stop::Error(format!(
            "unimplemented SVC {other:#04x} at pc {:#x}",
            p.cpu.pc - 4
        ))),
    }
}

/// Read a guest buffer of any length. Chunked so a bogus length faults on
/// the first unmapped byte instead of allocating the whole length up front.
fn read_guest(p: &Process, addr: u64, len: u64) -> Result<Vec<u8>, kestrel_core::MemFault> {
    const CHUNK: u64 = 64 * 1024;
    let mut out = Vec::new();
    let mut done = 0;
    while done < len {
        let n = CHUNK.min(len - done);
        out.extend(p.mem.read_vec(addr.wrapping_add(done), n as usize)?);
        done += n;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_core::Memory;
    use kestrel_loader::Loaded;

    fn process() -> Process {
        let image = Loaded {
            entry: 0,
            image_start: 0,
            image_end: 0,
        };
        Process::new(Memory::new(), &image).unwrap()
    }

    #[test]
    fn debug_string_longer_than_one_chunk_is_not_truncated() {
        let mut p = process();
        let (base, len) = (0x10_0000, 0x18_0000u64);
        p.mem.map(base, 0x20_0000, Perm::RW, "buf").unwrap();
        p.mem.write_bytes(base, &vec![b'x'; len as usize]).unwrap();
        p.cpu.x[0] = base;
        p.cpu.x[1] = len;
        assert_eq!(dispatch(&mut p, nr::OUTPUT_DEBUG_STRING), None);
        assert_eq!(p.cpu.x[0], result::SUCCESS);
        assert_eq!(p.debug_output.len() as u64, len);
    }

    #[test]
    fn debug_string_past_mapped_memory_is_an_error() {
        let mut p = process();
        p.mem.map(0x10_0000, 0x1000, Perm::RW, "buf").unwrap();
        p.cpu.x[0] = 0x10_0000;
        p.cpu.x[1] = u64::MAX;
        assert_eq!(dispatch(&mut p, nr::OUTPUT_DEBUG_STRING), None);
        assert_eq!(p.cpu.x[0], result::INVALID_ADDRESS);
        assert!(p.debug_output.is_empty());
    }
}
