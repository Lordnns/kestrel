//! Supervisor call dispatch.
//!
//! Arguments arrive in X0..X7, results go back in W0 (result code) plus any
//! output registers. A result code of 0 means success.

use crate::{layout, memory, Process, Stop};
use std::io::Write;

/// Publicly documented SVC numbers implemented so far.
pub mod nr {
    pub const SET_HEAP_SIZE: u16 = 0x01;
    pub const SET_MEMORY_PERMISSION: u16 = 0x02;
    pub const SET_MEMORY_ATTRIBUTE: u16 = 0x03;
    pub const MAP_MEMORY: u16 = 0x04;
    pub const UNMAP_MEMORY: u16 = 0x05;
    pub const QUERY_MEMORY: u16 = 0x06;
    pub const EXIT_PROCESS: u16 = 0x07;
    pub const CREATE_THREAD: u16 = 0x08;
    pub const START_THREAD: u16 = 0x09;
    pub const EXIT_THREAD: u16 = 0x0a;
    pub const SLEEP_THREAD: u16 = 0x0b;
    pub const SIGNAL_EVENT: u16 = 0x11;
    pub const CLEAR_EVENT: u16 = 0x12;
    pub const CREATE_TRANSFER_MEMORY: u16 = 0x15;
    pub const CLOSE_HANDLE: u16 = 0x16;
    pub const RESET_SIGNAL: u16 = 0x17;
    pub const GET_SYSTEM_TICK: u16 = 0x1e;
    pub const GET_PROCESS_ID: u16 = 0x24;
    pub const GET_THREAD_ID: u16 = 0x25;
    pub const BREAK: u16 = 0x26;
    pub const OUTPUT_DEBUG_STRING: u16 = 0x27;
    pub const GET_INFO: u16 = 0x29;
    pub const MAP_PHYSICAL_MEMORY: u16 = 0x2c;
    pub const UNMAP_PHYSICAL_MEMORY: u16 = 0x2d;
    pub const CREATE_EVENT: u16 = 0x45;
}

/// Result codes (module 1 = kernel), named and numbered as on Switchbrew's
/// "Error codes" page.
pub mod result {
    pub const SUCCESS: u64 = 0;
    pub const INVALID_SIZE: u64 = 0xca01;
    pub const INVALID_ADDRESS: u64 = 0xcc01;
    pub const OUT_OF_MEMORY: u64 = 0xd001;
    pub const OUT_OF_HANDLES: u64 = 0xd201;
    pub const INVALID_CURRENT_MEMORY: u64 = 0xd401;
    pub const INVALID_NEW_MEMORY_PERMISSION: u64 = 0xd801;
    pub const INVALID_MEMORY_REGION: u64 = 0xdc01;
    pub const INVALID_PRIORITY: u64 = 0xe001;
    pub const INVALID_CORE_ID: u64 = 0xe201;
    pub const LIMIT_REACHED: u64 = 0x10801;
    pub const INVALID_HANDLE: u64 = 0xe401;
    pub const INVALID_COMBINATION: u64 = 0xe801;
    pub const INVALID_STATE: u64 = 0xfa01;
}

/// Pseudo-handles that always name the caller.
pub mod handle {
    pub const CURRENT_THREAD: u64 = 0xffff_8000;
    pub const CURRENT_PROCESS: u64 = 0xffff_8001;
}

/// Why svcGetInfo gave no value.
enum InfoError {
    /// The guest asked for something invalid: return this result code.
    Result(u64),
    /// A valid request kestrel does not answer yet.
    Unimplemented(&'static str),
}

/// svcGetInfo (Switchbrew, "SVC"): W1 = info type, W2 = handle, X3 =
/// subtype. Process-wide types take the current-process handle and
/// subtype 0; the rest take handle 0.
fn get_info(p: &Process, kind: u32, h: u64, sub: u64) -> Result<u64, InfoError> {
    use layout::*;
    use InfoError::*;
    let process = |v: u64| {
        if h != handle::CURRENT_PROCESS {
            Err(Result(result::INVALID_HANDLE))
        } else if sub != 0 {
            Err(Result(result::INVALID_COMBINATION))
        } else {
            Ok(v)
        }
    };
    let used = p.used_memory();
    match kind {
        2 => process(ALIAS_REGION.0),
        3 => process(ALIAS_REGION.1),
        4 => process(HEAP_REGION.0),
        5 => process(HEAP_REGION.1),
        // Total and used; with no system resource, the non-system sizes
        // (21, 22) are the same.
        6 | 21 => process(TOTAL_MEMORY),
        7 | 22 => process(used),
        8 => match (h, sub) {
            (0, 0) => Ok(0), // no debugger
            (0, _) => Err(Result(result::INVALID_COMBINATION)),
            _ => Err(Result(result::INVALID_HANDLE)),
        },
        11 => match (h, sub) {
            (0, 0..=3) => Ok(RANDOM_ENTROPY[sub as usize]),
            (0, _) => Err(Result(result::INVALID_COMBINATION)),
            _ => Err(Result(result::INVALID_HANDLE)),
        },
        12 => process(ASLR_REGION.0),
        13 => process(ASLR_REGION.1),
        14 => process(STACK_REGION.0),
        15 => process(STACK_REGION.1),
        16 | 17 => process(0), // system resource size: none reserved
        18 => process(PROGRAM_ID),
        23 => process(1), // IsApplication, as the AppletType entry says
        0 => Err(Unimplemented("CoreMask")),
        1 => Err(Unimplemented("PriorityMask")),
        9 => Err(Unimplemented("ResourceLimit")),
        10 => Err(Unimplemented("IdleTickCount")),
        19 => Err(Unimplemented("InitialProcessIdRange")),
        20 => Err(Unimplemented("UserExceptionContextAddress")),
        24 => Err(Unimplemented("FreeThreadCount")),
        25 => Err(Unimplemented("ThreadTickCount")),
        26 => Err(Unimplemented("IsSvcPermitted")),
        _ => Err(Unimplemented("unknown info type")),
    }
}

/// The result code for W0.
fn code(r: Result<(), u64>) -> u64 {
    r.err().unwrap_or(result::SUCCESS)
}

/// Handle one SVC. Returns `Some(stop)` if the process should stop.
pub fn dispatch(p: &mut Process, n: u16) -> Option<Stop> {
    match n {
        nr::SET_HEAP_SIZE => {
            match p.set_heap_size(p.cpu.x[1]) {
                Ok(addr) => {
                    p.cpu.x[0] = result::SUCCESS;
                    p.cpu.x[1] = addr;
                }
                Err(code) => p.cpu.x[0] = code,
            }
            None
        }
        nr::SET_MEMORY_PERMISSION => {
            let (addr, size, perm) = (p.cpu.x[0], p.cpu.x[1], p.cpu.x[2] as u32);
            p.cpu.x[0] = code(p.set_memory_permission(addr, size, perm));
            None
        }
        nr::SET_MEMORY_ATTRIBUTE => {
            let (addr, size) = (p.cpu.x[0], p.cpu.x[1]);
            let (mask, value) = (p.cpu.x[2] as u32, p.cpu.x[3] as u32);
            p.cpu.x[0] = code(p.set_memory_attribute(addr, size, mask, value));
            None
        }
        nr::MAP_MEMORY | nr::UNMAP_MEMORY => {
            let (dst, src, size) = (p.cpu.x[0], p.cpu.x[1], p.cpu.x[2]);
            p.cpu.x[0] = code(if n == nr::MAP_MEMORY {
                p.map_memory(dst, src, size)
            } else {
                p.unmap_memory(dst, src, size)
            });
            None
        }
        nr::MAP_PHYSICAL_MEMORY | nr::UNMAP_PHYSICAL_MEMORY => {
            let (addr, size) = (p.cpu.x[0], p.cpu.x[1]);
            p.cpu.x[0] = code(if n == nr::MAP_PHYSICAL_MEMORY {
                p.map_physical_memory(addr, size)
            } else {
                p.unmap_physical_memory(addr, size)
            });
            None
        }
        nr::QUERY_MEMORY => {
            // X0 = MemoryInfo out pointer, X2 = address; W1 = page info (0).
            let (out, addr) = (p.cpu.x[0], p.cpu.x[2]);
            let info = memory::query(&p.mem, addr);
            if let Err(e) = p.mem.write_bytes(out, &info.to_bytes()) {
                return Some(Stop::Error(format!(
                    "svcQueryMemory cannot write its MemoryInfo at {out:#x}: {e}"
                )));
            }
            p.cpu.x[0] = result::SUCCESS;
            p.cpu.x[1] = 0;
            None
        }
        nr::EXIT_PROCESS => Some(Stop::Exited),
        nr::CREATE_THREAD => {
            // X1 = entry, X2 = argument, X3 = stack top, W4 = priority,
            // W5 = processor ID; W1 = the handle.
            let (entry, arg, sp) = (p.cpu.x[1], p.cpu.x[2], p.cpu.x[3]);
            let (prio, core) = (p.cpu.x[4] as i32, p.cpu.x[5] as i32);
            match p.create_thread(entry, arg, sp, prio, core) {
                Ok(h) => {
                    p.cpu.x[0] = result::SUCCESS;
                    p.cpu.x[1] = h as u64;
                }
                Err(code) => p.cpu.x[0] = code,
            }
            None
        }
        nr::START_THREAD => {
            p.cpu.x[0] = code(p.start_thread(p.cpu.x[0]));
            None
        }
        nr::EXIT_THREAD => {
            p.exit_thread();
            None
        }
        nr::SLEEP_THREAD => match p.sleep_thread(p.cpu.x[0] as i64) {
            Ok(()) => {
                p.cpu.x[0] = result::SUCCESS;
                None
            }
            Err(what) => Some(Stop::Error(format!(
                "unsupported: {what}, at pc {:#x}",
                p.cpu.pc.wrapping_sub(4)
            ))),
        },
        nr::CREATE_TRANSFER_MEMORY => {
            // X1 = address, X2 = size, W3 = permission; W1 = the handle.
            let (addr, size, perm) = (p.cpu.x[1], p.cpu.x[2], p.cpu.x[3] as u32);
            match p.create_transfer_memory(addr, size, perm) {
                Ok(h) => {
                    p.cpu.x[0] = result::SUCCESS;
                    p.cpu.x[1] = h as u64;
                }
                Err(code) => p.cpu.x[0] = code,
            }
            None
        }
        nr::CREATE_EVENT => {
            // W1 = writable side, W2 = readable side.
            match p.create_event() {
                Ok((w, r)) => {
                    p.cpu.x[0] = result::SUCCESS;
                    p.cpu.x[1] = w as u64;
                    p.cpu.x[2] = r as u64;
                }
                Err(code) => p.cpu.x[0] = code,
            }
            None
        }
        nr::SIGNAL_EVENT => {
            p.cpu.x[0] = code(p.signal_event(p.cpu.x[0]));
            None
        }
        nr::CLEAR_EVENT => {
            p.cpu.x[0] = code(p.clear_event(p.cpu.x[0]));
            None
        }
        nr::RESET_SIGNAL => {
            // A process is signaled when its state changes; kestrel does not
            // model process states yet.
            if p.cpu.x[0] as u32 as u64 == handle::CURRENT_PROCESS {
                return Some(Stop::Error(format!(
                    "unsupported: svcResetSignal on a process handle, at pc {:#x}",
                    p.cpu.pc.wrapping_sub(4)
                )));
            }
            p.cpu.x[0] = code(p.reset_signal(p.cpu.x[0]));
            None
        }
        nr::CLOSE_HANDLE => {
            p.cpu.x[0] = code(p.close_handle(p.cpu.x[0]));
            None
        }
        nr::GET_PROCESS_ID | nr::GET_THREAD_ID => {
            // W1 = handle; X1 = the ID.
            let h = p.cpu.x[1];
            match if n == nr::GET_PROCESS_ID {
                p.process_id(h)
            } else {
                p.thread_id(h)
            } {
                Ok(id) => {
                    p.cpu.x[0] = result::SUCCESS;
                    p.cpu.x[1] = id;
                }
                Err(code) => p.cpu.x[0] = code,
            }
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
        nr::GET_INFO => {
            let (kind, h, sub) = (p.cpu.x[1] as u32, p.cpu.x[2] & 0xffff_ffff, p.cpu.x[3]);
            match get_info(p, kind, h, sub) {
                Ok(v) => {
                    p.cpu.x[0] = result::SUCCESS;
                    p.cpu.x[1] = v;
                }
                Err(InfoError::Result(code)) => p.cpu.x[0] = code,
                Err(InfoError::Unimplemented(name)) => {
                    return Some(Stop::Error(format!(
                    "unimplemented svcGetInfo type {kind} ({name}), subtype {sub:#x}, at pc {:#x}",
                    p.cpu.pc.wrapping_sub(4)
                )))
                }
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
    use kestrel_core::{Memory, Perm};
    use kestrel_loader::Loaded;

    fn process() -> Process {
        let image = Loaded {
            entry: 0,
            image_start: 0,
            image_end: 0,
        };
        Process::new(Memory::new(), &image).unwrap()
    }

    /// svcQueryMemory at `addr`, through a MemoryInfo buffer in TLS.
    fn query(p: &mut Process, addr: u64) -> (u64, u64, u32, u32, u32) {
        let out = layout::TLS_BASE + 0x100;
        (p.cpu.x[0], p.cpu.x[2]) = (out, addr);
        assert_eq!(dispatch(p, nr::QUERY_MEMORY), None);
        assert_eq!(p.cpu.x[0], result::SUCCESS);
        let m = &p.mem;
        (
            m.read64(out).unwrap(),
            m.read64(out + 8).unwrap(),
            m.read32(out + 0x10).unwrap(),
            m.read32(out + 0x14).unwrap(),
            m.read32(out + 0x18).unwrap(),
        )
    }

    #[test]
    fn a_new_process_reports_the_type_of_each_mapping() {
        use memory::state;
        let base = layout::NRO_BASE;
        let mut mem = Memory::new();
        mem.map(base, 0x2000, Perm::RX, "text").unwrap();
        mem.map(base + 0x2000, 0x1000, Perm::R, "rodata").unwrap();
        mem.map(base + 0x3000, 0x2000, Perm::RW, "data").unwrap();
        let image = Loaded {
            entry: base,
            image_start: base,
            image_end: base + 0x5000,
        };
        let mut p = Process::new(mem, &image).unwrap();
        assert_eq!(
            query(&mut p, base + 0x1000),
            (base, 0x2000, state::CODE, 0, 5)
        );
        assert_eq!(
            query(&mut p, base + 0x2000),
            (base + 0x2000, 0x1000, state::CODE, 0, 1)
        );
        assert_eq!(
            query(&mut p, base + 0x4fff),
            (base + 0x3000, 0x2000, state::CODE_DATA, 0, 3)
        );
        let stack = layout::STACK_TOP - layout::STACK_SIZE;
        assert_eq!(
            query(&mut p, stack),
            (stack, layout::STACK_SIZE, state::STACK, 0, 3)
        );
        let tls = query(&mut p, layout::TLS_BASE);
        assert_eq!((tls.2, tls.4), (state::THREAD_LOCAL, 3));
        // The gap after the image is free up to the next mapping.
        let free = query(&mut p, base + 0x5000);
        assert_eq!((free.0, free.2, free.4), (base + 0x5000, state::FREE, 0));
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

    /// Call svcGetInfo; Ok(value) on success, Err(result) otherwise.
    fn info(p: &mut Process, kind: u64, h: u64, sub: u64) -> Result<u64, u64> {
        (p.cpu.x[1], p.cpu.x[2], p.cpu.x[3]) = (kind, h, sub);
        assert_eq!(dispatch(p, nr::GET_INFO), None);
        match p.cpu.x[0] {
            0 => Ok(p.cpu.x[1]),
            code => Err(code),
        }
    }

    #[test]
    fn reported_regions_are_disjoint_and_inside_the_39_bit_space() {
        let mut p = process();
        let cur = handle::CURRENT_PROCESS;
        let mut region = |base, size| {
            let b = info(&mut p, base, cur, 0).unwrap();
            (b, b + info(&mut p, size, cur, 0).unwrap())
        };
        let aslr = region(12, 13);
        let regions = [region(2, 3), region(4, 5), region(14, 15)];
        assert!(aslr.0 > 0 && aslr.1 <= 1 << 39);
        for (i, r) in regions.iter().enumerate() {
            assert!(aslr.0 <= r.0 && r.1 <= aslr.1, "region {i} outside ASLR");
            for s in &regions[i + 1..] {
                assert!(r.1 <= s.0 || s.1 <= r.0, "{r:x?} overlaps {s:x?}");
            }
        }
        // Switchbrew "16.0.0": 39-bit defaults are 64 GiB alias, 8 GiB heap.
        assert_eq!(regions[0].1 - regions[0].0, 64 << 30);
        assert_eq!(regions[1].1 - regions[1].0, 8 << 30);
        // The main stack is in the stack region; the heap starts the heap region.
        let (stack, sp) = (regions[2], p.cpu.sp);
        assert!(stack.0 < sp - layout::STACK_SIZE && sp <= stack.1);
        p.cpu.x[1] = 0x20_0000;
        assert_eq!(dispatch(&mut p, nr::SET_HEAP_SIZE), None);
        assert_eq!(p.cpu.x[1], regions[1].0);
    }

    #[test]
    fn used_memory_follows_the_heap() {
        let mut p = process();
        let cur = handle::CURRENT_PROCESS;
        let before = info(&mut p, 7, cur, 0).unwrap();
        p.cpu.x[1] = 0x40_0000;
        dispatch(&mut p, nr::SET_HEAP_SIZE);
        assert_eq!(info(&mut p, 7, cur, 0).unwrap(), before + 0x40_0000);
        assert!(info(&mut p, 6, cur, 0).unwrap() > before + 0x40_0000);
    }

    #[test]
    fn wrong_handles_and_subtypes_give_the_documented_results() {
        let mut p = process();
        let cur = handle::CURRENT_PROCESS;
        assert_eq!(info(&mut p, 4, 0, 0), Err(result::INVALID_HANDLE));
        assert_eq!(info(&mut p, 4, cur, 1), Err(result::INVALID_COMBINATION));
        assert_eq!(info(&mut p, 8, cur, 0), Err(result::INVALID_HANDLE));
        assert_eq!(info(&mut p, 8, 0, 0), Ok(0));
        for sub in 0..4 {
            assert!(info(&mut p, 11, 0, sub).is_ok());
        }
        assert_eq!(info(&mut p, 11, 0, 4), Err(result::INVALID_COMBINATION));
        // Only the low 32 bits of the handle register count.
        assert!(info(&mut p, 18, 0xdead_0000_ffff_8001, 0).is_ok());
    }

    #[test]
    fn unanswered_info_types_stop_with_their_name() {
        let mut p = process();
        (p.cpu.x[1], p.cpu.x[2], p.cpu.x[3]) = (0, handle::CURRENT_PROCESS, 0);
        match dispatch(&mut p, nr::GET_INFO) {
            Some(Stop::Error(e)) => assert!(e.contains("type 0 (CoreMask)"), "{e}"),
            other => panic!("expected a stop, got {other:?}"),
        }
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
