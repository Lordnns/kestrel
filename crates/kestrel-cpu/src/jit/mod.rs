//! The x86_64 dynamic recompiler (phase 4): guest blocks are decoded into
//! a host-independent IR (`ir.rs`, by `frontend.rs`), compiled to x86_64
//! (`x64/`) into executable memory (`code.rs`), cached by guest address,
//! and run by a dispatcher that keeps the [`CpuBackend`] contract exactly:
//! it stops after exactly the budget it is given, as the interpreter
//! does, so the scheduler, the counted clock and the differential tester
//! see the same instruction counts on both.
//!
//! What is not translated yet runs in the interpreter, one instruction at
//! a time, from inside the block (`Op::Interp`), so every program runs.
//!
//! Hosts: Windows x86_64 (the product's, with the Windows x64 calling
//! convention) and Linux x86_64 (for testing, System V). The module is
//! built only there; elsewhere [`crate::create_backend`] says the
//! platform is not supported yet (`PLAN.md` section 3, "Platform
//! priority").
//!
//! Self-modifying code: each block keeps the guest words it was made from
//! and the memory's change count ([`Memory::epoch`]) when they were last
//! checked. When the count moves (a mapping or permission changed, the
//! only way a page that was executable can be rewritten), the words are
//! fetched again before the block runs, and a block whose words changed
//! is translated again. A block on a page that is writable and executable
//! at once is checked every time it runs.

pub mod code;
pub mod frontend;
pub mod ir;
pub mod x64;

use crate::interp::{execute, Ctx, Flow};
use crate::{CpuBackend, CpuState, Exit};
use kestrel_core::{MemFault, Memory, Perm};
use std::collections::HashMap;
use x64::{Abi, Helpers, Layout, RET_BRK, RET_EXIT, RET_JUMP, RET_SVC};

/// Host code space for translations; when it fills, every translation is
/// dropped and made again as needed (kestrel's choice).
const CODE_SIZE: usize = 64 << 20;

/// What generated code and its helpers share with the dispatcher.
pub struct JitCtx {
    state: *mut CpuState,
    mem: *const Memory,
    /// Set by a helper whose guest access faulted, with the fault.
    faulted: u8,
    fault: Option<MemFault>,
    /// An exit the interpreter returned from inside a block.
    exit: Option<Exit>,
    /// Chained jumps need room for a whole block below this retired count.
    limit: u64,
    /// The link cell of the last chained exit taken, for the dispatcher to
    /// link; 0 after other exits.
    link: usize,
    /// The memory's change count the links were made at: a chained jump
    /// is taken only while the count is still this.
    link_epoch: u64,
}

/// Link cells are handed out from chunks of this many.
const CELLS: usize = 1024;

/// One translated block.
struct Entry {
    code: *const u8,
    /// The body, which chained jumps enter; null if the block cannot be
    /// chained to.
    body: *const u8,
    insns: u32,
    /// The guest words it was made from.
    words: Vec<u32>,
    /// The memory's change count when the words were last checked.
    epoch: u64,
    /// On a page that is writable and executable: check every time.
    always_check: bool,
}

/// A hash for guest addresses: they are already well spread above bit 2.
#[derive(Default)]
struct PcHash(u64);

impl std::hash::Hasher for PcHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = (v >> 2).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    fn write_u32(&mut self, v: u32) {
        self.0 ^= (v as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

type Map<K> = HashMap<K, Entry, std::hash::BuildHasherDefault<PcHash>>;

/// Counts for `kestrel run --stats` and tuning.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub blocks_compiled: u64,
    pub blocks_run: u64,
    pub code_bytes: u64,
}

pub struct Jit {
    abi: Abi,
    helpers: Helpers,
    layout: Layout,
    code: code::CodeMemory,
    /// Blocks by guest address, as long as they naturally end.
    blocks: Map<u64>,
    /// Blocks cut short to fit what was left of a budget, by guest
    /// address and length.
    short: Map<(u64, u32)>,
    /// The most instructions in a block (at least 1).
    max_insns: u32,
    /// The memory the translations were made from: one process's. Another
    /// one (a backend reused for a new process) drops them all.
    mem_id: usize,
    /// Where an unlinked exit goes: code returning to the dispatcher.
    stub: *const u8,
    /// Link cells, in chunks that never move while translations live.
    cells: Vec<Box<[usize; CELLS]>>,
    cells_used: usize,
    /// Cells pointing at a block, to reset when links must go.
    linked: Vec<*mut usize>,
    /// The memory's change count the links were made at. Another guest
    /// core's system call can move it at any time (`kestrel_hle::cores`),
    /// so generated code compares it before every chained jump, and the
    /// dispatcher resets the links when it has moved.
    link_epoch: u64,
    pub stats: Stats,
}

impl Jit {
    /// The recompiler in the host's calling convention.
    pub fn new() -> Result<Self, String> {
        Self::with_abi(Abi::host())
    }

    /// The recompiler generating code for `abi` (both run on any x86_64
    /// host, which lets Linux test the Windows convention).
    pub fn with_abi(abi: Abi) -> Result<Self, String> {
        let code = code::CodeMemory::new(CODE_SIZE)
            .map_err(|e| format!("cannot allocate memory for translated code: {e}"))?;
        let helpers = match abi {
            Abi::SysV => sysv::helpers(),
            Abi::Win64 => win64::helpers(),
        };
        let flags = std::mem::offset_of!(CpuState, flags) as i32;
        let layout = Layout {
            x: std::mem::offset_of!(CpuState, x) as i32,
            sp: std::mem::offset_of!(CpuState, sp) as i32,
            pc: std::mem::offset_of!(CpuState, pc) as i32,
            retired: std::mem::offset_of!(CpuState, retired) as i32,
            n: flags + std::mem::offset_of!(crate::Flags, n) as i32,
            z: flags + std::mem::offset_of!(crate::Flags, z) as i32,
            c: flags + std::mem::offset_of!(crate::Flags, c) as i32,
            v: flags + std::mem::offset_of!(crate::Flags, v) as i32,
            faulted: std::mem::offset_of!(JitCtx, faulted) as i32,
            limit: std::mem::offset_of!(JitCtx, limit) as i32,
            link_epoch: std::mem::offset_of!(JitCtx, link_epoch) as i32,
            link: std::mem::offset_of!(JitCtx, link) as i32,
        };
        let mut jit = Jit {
            abi,
            helpers,
            layout,
            code,
            blocks: Map::default(),
            short: Map::default(),
            max_insns: frontend::MAX_INSNS,
            mem_id: 0,
            stub: std::ptr::null(),
            cells: Vec::new(),
            cells_used: 0,
            linked: Vec::new(),
            link_epoch: 0,
            stats: Stats::default(),
        };
        jit.flush();
        Ok(jit)
    }

    /// Limit blocks to `n` instructions (at least 1); 1 makes every
    /// instruction a block of its own.
    pub fn set_max_block(&mut self, n: u32) {
        self.max_insns = n.clamp(1, frontend::MAX_INSNS);
        self.flush();
    }

    /// Drop every translation.
    pub fn flush(&mut self) {
        self.blocks.clear();
        self.short.clear();
        self.linked.clear();
        self.cells.clear();
        self.cells_used = 0;
        self.code.clear();
        self.stub = match self.code.add(&x64::unlinked_stub(&self.layout)) {
            Ok(Some(p)) => p,
            _ => panic!("cannot write the recompiler's return stub"),
        };
    }

    /// Point every linked cell back at the stub.
    fn unlink_all(&mut self) {
        for &cell in &self.linked {
            // SAFETY: cells live in `self.cells` until a flush, which
            // empties `linked` too.
            unsafe { *cell = self.stub as usize };
        }
        self.linked.clear();
    }

    /// Two new link cells, unlinked.
    fn new_cells(&mut self) -> [u64; 2] {
        let mut take = || {
            if self.cells_used == self.cells.len() * CELLS {
                self.cells.push(Box::new([0; CELLS]));
            }
            let chunk = &mut self.cells[self.cells_used / CELLS];
            let cell = &mut chunk[self.cells_used % CELLS];
            *cell = self.stub as usize;
            self.cells_used += 1;
            cell as *mut usize as u64
        };
        [take(), take()]
    }

    /// Is `e` still what the guest has at `pc`?
    fn valid(e: &mut Entry, mem: &Memory, pc: u64) -> bool {
        let epoch = mem.epoch();
        if e.epoch == epoch && !e.always_check {
            return true;
        }
        let same = e
            .words
            .iter()
            .enumerate()
            .all(|(k, &w)| mem.fetch32(pc.wrapping_add(4 * k as u64)) == Ok(w));
        if same {
            e.epoch = epoch;
        }
        same
    }

    /// The code for the block at `pc`, at most `left` instructions long,
    /// and the body a chained jump may enter (null if none may).
    fn block(&mut self, mem: &Memory, pc: u64, left: u64) -> Result<(*const u8, *const u8), Exit> {
        let max = left.min(self.max_insns as u64) as u32;
        if let Some(e) = self.blocks.get_mut(&pc) {
            if Self::valid(e, mem, pc) {
                if e.insns <= max {
                    return Ok((e.code, e.body));
                }
            } else {
                self.blocks.remove(&pc);
            }
        }
        // Either nothing is cached or the cached block is longer than the
        // budget allows.
        let natural = !self.blocks.contains_key(&pc);
        if !natural {
            if let Some(e) = self.short.get_mut(&(pc, max)) {
                if Self::valid(e, mem, pc) {
                    return Ok((e.code, std::ptr::null()));
                }
                self.short.remove(&(pc, max));
            }
        }
        let e = self.translate(mem, pc, max)?;
        // A block that ended before `max` is the natural one too.
        if natural && (e.insns < max || max == self.max_insns) {
            let found = (e.code, e.body);
            self.blocks.insert(pc, e);
            Ok(found)
        } else {
            let found = (e.code, std::ptr::null());
            self.short.insert((pc, max), e);
            Ok(found)
        }
    }

    fn translate(&mut self, mem: &Memory, pc: u64, max: u32) -> Result<Entry, Exit> {
        let (block, words) =
            frontend::translate(mem, pc, max).map_err(|fault| Exit::Fault { pc, fault })?;
        let cells = self.new_cells();
        let mut host = x64::compile(
            &block,
            self.abi,
            &self.helpers,
            &self.layout,
            Some(cells),
            mem.raw_tables(),
        );
        let code = match self.code.add(&host.code) {
            Ok(Some(p)) => p,
            Ok(None) => {
                // Full: start again (the caller holds no other entry).
                self.flush();
                let cells = self.new_cells();
                host = x64::compile(
                    &block,
                    self.abi,
                    &self.helpers,
                    &self.layout,
                    Some(cells),
                    mem.raw_tables(),
                );
                match self.code.add(&host.code) {
                    Ok(Some(p)) => p,
                    _ => panic!("a block does not fit the code memory"),
                }
            }
            Err(e) => panic!("cannot make translated code executable: {e}"),
        };
        self.stats.blocks_compiled += 1;
        self.stats.code_bytes = self.code.used() as u64;
        let end = pc.wrapping_add(4 * words.len() as u64 - 1);
        let writable = |a: u64| mem.translate(a).is_some_and(|(_, p)| p.contains(Perm::W));
        let always_check = writable(pc) || writable(end);
        // A block checked every time it runs is never entered by a jump
        // that skips the dispatcher.
        let body = if host.chained && !always_check {
            // SAFETY: the body lies inside the code just written.
            unsafe { code.add(host.body) }
        } else {
            std::ptr::null()
        };
        Ok(Entry {
            code,
            body,
            insns: block.insns,
            always_check,
            words,
            epoch: mem.epoch(),
        })
    }
}

// SAFETY: a `Jit` is moved to the host thread of the guest core it runs
// (`kestrel_hle::cores`) and used only there. Its raw pointers point into
// memory it owns (its code memory and its link cells), never into another
// thread's; generated code reaches guest memory only through the shared
// `Memory`, which is `Sync`.
unsafe impl Send for Jit {}

impl CpuBackend for Jit {
    fn name(&self) -> &'static str {
        "jit"
    }

    fn run(&mut self, state: &mut CpuState, mem: &Memory, max_steps: u64) -> Exit {
        let start = state.retired;
        if self.mem_id != mem as *const Memory as usize {
            self.flush();
            self.mem_id = mem as *const Memory as usize;
        }
        let state: *mut CpuState = state;
        let mem: *const Memory = mem;
        let mut ctx = JitCtx {
            state,
            mem,
            faulted: 0,
            fault: None,
            exit: None,
            limit: start.saturating_add(max_steps),
            link: 0,
            link_epoch: self.link_epoch,
        };
        loop {
            // SAFETY: generated code and helpers touch the state and memory
            // only while a block runs; between blocks only this loop does.
            let (pc, used) = unsafe { ((*state).pc, (*state).retired - start) };
            if used >= max_steps {
                return Exit::StepLimit;
            }
            // A mapping or permission changed since the links were made
            // (here, or on another core): they may lead to stale code.
            let epoch = unsafe { (*mem).epoch() };
            if epoch != self.link_epoch {
                self.unlink_all();
                self.link_epoch = epoch;
                ctx.link_epoch = epoch;
                ctx.link = 0;
            }
            let (entry, body) = match self.block(unsafe { &*mem }, pc, max_steps - used) {
                Ok(b) => b,
                Err(exit) => return exit,
            };
            // The last block left through a chained exit that is not
            // linked yet: link it here, if this block may be entered so.
            let cell = std::mem::take(&mut ctx.link) as *mut usize;
            if !cell.is_null() && !body.is_null() {
                // SAFETY: the cell is one of `self.cells`.
                unsafe { *cell = body as usize };
                self.linked.push(cell);
            }
            self.stats.blocks_run += 1;
            // SAFETY: `entry` is a block compiled for this ABI, taking the
            // state and the context, both valid for the call.
            let ret = unsafe {
                match self.abi {
                    Abi::SysV => {
                        let f: unsafe extern "sysv64" fn(*mut CpuState, *mut JitCtx) -> u64 =
                            std::mem::transmute(entry);
                        f(state, &mut ctx)
                    }
                    Abi::Win64 => {
                        let f: unsafe extern "win64" fn(*mut CpuState, *mut JitCtx) -> u64 =
                            std::mem::transmute(entry);
                        f(state, &mut ctx)
                    }
                }
            };
            match ret & 0xff {
                RET_JUMP => {}
                RET_SVC => return Exit::Svc((ret >> 8) as u16),
                RET_BRK => return Exit::Breakpoint((ret >> 8) as u16),
                RET_EXIT => {
                    ctx.faulted = 0;
                    if let Some(fault) = ctx.fault.take() {
                        return Exit::Fault {
                            pc: unsafe { (*state).pc },
                            fault,
                        };
                    }
                    return ctx.exit.take().expect("a block stopped without an exit");
                }
                other => panic!("a block returned {other:#x}"),
            }
        }
    }

    fn invalidate(&mut self, addr: u64, len: u64) {
        self.unlink_all();
        let end = addr.saturating_add(len);
        let hit = |pc: u64, e: &Entry| pc < end && pc + 4 * e.words.len() as u64 > addr;
        self.blocks.retain(|&pc, e| !hit(pc, e));
        self.short.retain(|&(pc, _), e| !hit(pc, e));
    }
}

/// A guest load for generated code; a fault is left in the context.
///
/// # Safety
/// `ctx` must be the context of the running block.
unsafe fn read(ctx: *mut JitCtx, addr: u64, size: u32) -> u64 {
    let ctx = &mut *ctx;
    let m = &*ctx.mem;
    let r = match size {
        1 => m.read8(addr).map(u64::from),
        2 => m.read16(addr).map(u64::from),
        4 => m.read32(addr).map(u64::from),
        _ => m.read64(addr),
    };
    r.unwrap_or_else(|f| {
        ctx.faulted = 1;
        ctx.fault = Some(f);
        0
    })
}

/// A guest store for generated code; a fault is left in the context.
///
/// # Safety
/// As [`read`].
unsafe fn write(ctx: *mut JitCtx, addr: u64, v: u64, size: u32) {
    let ctx = &mut *ctx;
    let m = &*ctx.mem;
    let r = match size {
        1 => m.write8(addr, v as u8),
        2 => m.write16(addr, v as u16),
        4 => m.write32(addr, v as u32),
        _ => m.write64(addr, v),
    };
    if let Err(f) = r {
        ctx.faulted = 1;
        ctx.fault = Some(f);
    }
}

/// One instruction in the interpreter, from inside a block, as the
/// interpreter's own loop runs it: 0 to go on in the block, `RET_JUMP`
/// when it branched (the PC is set), `RET_EXIT` with the exit in the
/// context.
///
/// # Safety
/// As [`read`].
unsafe fn interp(ctx: *mut JitCtx, pc: u64, insn: u32) -> u64 {
    let ctx = &mut *ctx;
    let s = &mut *ctx.state;
    let m = &*ctx.mem;
    let r = execute(&mut Ctx { s, m, pc, insn });
    let s = &mut *ctx.state;
    match r {
        Ok(Flow::Next) => {
            s.retired += 1;
            0
        }
        Ok(Flow::Jump(t)) => {
            s.retired += 1;
            s.pc = t;
            RET_JUMP
        }
        Err(e) => {
            if let Exit::Svc(_) = e {
                s.retired += 1;
                s.pc = pc.wrapping_add(4);
            } else {
                s.pc = pc;
            }
            ctx.exit = Some(e);
            RET_EXIT
        }
    }
}

/// The helpers in one calling convention, for code generated for it.
macro_rules! helpers {
    ($m:ident, $abi:literal) => {
        mod $m {
            use super::*;

            type Read = unsafe extern $abi fn(*mut JitCtx, u64) -> u64;
            type Write = unsafe extern $abi fn(*mut JitCtx, u64, u64);
            type Step = unsafe extern $abi fn(*mut JitCtx, u64, u64) -> u64;

            unsafe extern $abi fn read1(c: *mut JitCtx, a: u64) -> u64 {
                read(c, a, 1)
            }
            unsafe extern $abi fn read2(c: *mut JitCtx, a: u64) -> u64 {
                read(c, a, 2)
            }
            unsafe extern $abi fn read4(c: *mut JitCtx, a: u64) -> u64 {
                read(c, a, 4)
            }
            unsafe extern $abi fn read8(c: *mut JitCtx, a: u64) -> u64 {
                read(c, a, 8)
            }
            unsafe extern $abi fn write1(c: *mut JitCtx, a: u64, v: u64) {
                write(c, a, v, 1)
            }
            unsafe extern $abi fn write2(c: *mut JitCtx, a: u64, v: u64) {
                write(c, a, v, 2)
            }
            unsafe extern $abi fn write4(c: *mut JitCtx, a: u64, v: u64) {
                write(c, a, v, 4)
            }
            unsafe extern $abi fn write8(c: *mut JitCtx, a: u64, v: u64) {
                write(c, a, v, 8)
            }
            unsafe extern $abi fn step(c: *mut JitCtx, pc: u64, insn: u64) -> u64 {
                interp(c, pc, insn as u32)
            }

            pub fn helpers() -> Helpers {
                Helpers {
                    read: [
                        read1 as Read as usize,
                        read2 as Read as usize,
                        read4 as Read as usize,
                        read8 as Read as usize,
                    ],
                    write: [
                        write1 as Write as usize,
                        write2 as Write as usize,
                        write4 as Write as usize,
                        write8 as Write as usize,
                    ],
                    interp: step as Step as usize,
                }
            }
        }
    };
}

helpers!(sysv, "sysv64");
helpers!(win64, "win64");

#[cfg(test)]
mod tests;
