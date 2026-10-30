//! Helpers for unit tests that drive a process through system calls.

use crate::svc::{dispatch, nr};
use crate::Process;
use kestrel_core::Memory;
use kestrel_loader::Loaded;

/// A process whose main thread starts at 0x1000, with no image.
pub fn process() -> Process {
    let image = Loaded {
        entry: 0x1000,
        image_start: 0,
        image_end: 0,
    };
    Process::new(Memory::new(), &image).unwrap()
}

/// Make system call `n` with X0.. = `args` as the run loop does, then
/// schedule. Returns W0 and X1 as the call left them.
pub fn svc(p: &mut Process, n: u16, args: &[u64]) -> (u64, u64) {
    p.cpu.x[..args.len()].copy_from_slice(args);
    assert_eq!(dispatch(p, n), None);
    let regs = (p.cpu.x[0], p.cpu.x[1]);
    let rotate = std::mem::take(&mut p.sched.rotate);
    assert_eq!(p.schedule(rotate), None);
    regs
}

/// Create a thread with SP 0x9000 and X0 0x55; returns its handle.
pub fn spawn(p: &mut Process, entry: u64, prio: u64, core: u64) -> u64 {
    let (rc, h) = svc(p, nr::CREATE_THREAD, &[0, entry, 0x55, 0x9000, prio, core]);
    assert_eq!(rc, 0);
    h
}

/// Where the running thread is.
pub fn running(p: &Process) -> u64 {
    p.cpu.pc
}
