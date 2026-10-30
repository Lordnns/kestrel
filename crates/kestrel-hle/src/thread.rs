//! Threads and the scheduler.
//!
//! Switchbrew's "SVC" page documents the thread calls but not how the
//! kernel schedules threads across the console's cores, so the model is
//! kestrel's, run on one host thread so every run repeats exactly
//! (`PLAN.md`, phase 2):
//!
//! - Each thread runs on one of four virtual cores, its ideal core.
//! - On each core the ready thread with the best priority (lowest number)
//!   runs. Threads of equal priority take turns only when the running one
//!   yields, as on the console, where a core does not time-slice them.
//! - The cores take turns on the host thread: kestrel moves on to the next
//!   core with a ready thread when the running thread yields, sleeps or
//!   exits, or has run for a slice of `SLICE` instructions, so a thread
//!   spinning on one core cannot starve the others.
//! - One process-wide clock counts guest instructions as ticks of the
//!   documented 19.2 MHz counter ("SVC", GetSystemTick). When every live
//!   thread sleeps, the clock jumps to the earliest wake-up.

use crate::kernel::{Kind, Object, ObjectId};
use crate::memory::{state, tag};
use crate::svc::{handle, result};
use crate::{layout, Process, Stop};
use kestrel_core::Perm;
use kestrel_cpu::CpuState;

/// Virtual cores; svcCreateThread takes ProcessorId 0 to 3 ("SVC").
pub const CORES: u32 = 4;
/// The main thread's core, and the one ProcessorId -2 picks. Kestrel's
/// choice.
pub const DEFAULT_CORE: u32 = 0;
/// The main thread's priority. Kestrel's choice.
pub const MAIN_PRIORITY: u32 = 0x2c;
/// The lowest priority; 0 is the highest ("SVC", SetThreadPriority).
pub const LOWEST_PRIORITY: u32 = 0x3f;
/// Instructions a core runs before the next core gets its turn: 1 ms of
/// the 19.2 MHz clock. Kestrel's choice.
pub const SLICE: u64 = 19_200;
/// The size of a thread local region ("Thread Local Region").
pub const TLR_SIZE: u64 = 0x200;
/// Thread local regions per page: the first slot of each page is reserved
/// for user-mode exception handling ("Thread Local Region").
const TLRS_PER_PAGE: u64 = layout::TLS_SIZE / TLR_SIZE - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Created by svcCreateThread, not started yet.
    Created,
    /// Running or able to run.
    Ready,
    /// Asleep until the clock reaches `until`.
    Sleeping {
        until: u64,
    },
    Exited,
}

pub struct Thread {
    pub id: u64,
    /// The saved registers; out of date while the thread runs, when
    /// `Process::cpu` holds them.
    pub cpu: CpuState,
    pub state: State,
    pub priority: u32,
    /// The ideal core, where kestrel runs the thread.
    pub core: u32,
    /// The cores the thread may run on, one bit per core.
    pub affinity: u64,
    /// The thread's thread local region.
    pub tlr: u64,
    /// The thread's kernel object, while a handle names it.
    pub object: Option<ObjectId>,
    /// Turn order among threads of equal priority on a core.
    turn: u64,
}

/// Every thread of the process.
#[derive(Default)]
pub struct Scheduler {
    pub threads: Vec<Thread>,
    /// Index of the running thread in `threads`.
    pub current: usize,
    /// Threads created so far; thread IDs count up from the main thread's.
    created: u64,
    next_turn: u64,
    /// Thread local regions in use.
    tlrs: Vec<u64>,
    /// Pages of thread local regions mapped from `layout::TLS_BASE`.
    tls_pages: u64,
    /// Clock ticks that passed with every thread asleep.
    pub idle: u64,
    /// Set by a call after which the next core should get its turn.
    pub(crate) rotate: bool,
    /// Instructions left in the running core's slice.
    pub(crate) slice_left: u64,
}

impl Scheduler {
    fn take_turn(&mut self) -> u64 {
        self.next_turn += 1;
        self.next_turn
    }
}

/// Why svcSetThreadCoreMask failed.
#[derive(Debug, PartialEq, Eq)]
pub enum CoreMaskError {
    /// Return this result code.
    Result(u64),
    /// An undocumented negative ideal core.
    Unsupported(i32),
}

/// The clock ticks in `ns` nanoseconds at 19.2 MHz, rounded up.
pub fn ticks(ns: u64) -> u64 {
    // 19.2 MHz is 12 ticks every 625 ns; the result always fits.
    (ns as u128 * 12).div_ceil(625) as u64
}

impl Process {
    /// The running thread.
    pub fn current_thread(&self) -> &Thread {
        &self.sched.threads[self.sched.current]
    }

    /// Guest instructions retired by every thread together.
    pub fn instructions(&self) -> u64 {
        self.cpu.retired - self.sched.idle
    }

    /// A zeroed thread local region, mapping a new page when the mapped
    /// ones are full.
    fn alloc_tlr(&mut self) -> Result<u64, u64> {
        let addr = (0..)
            .map(|i| {
                layout::TLS_BASE
                    + (i / TLRS_PER_PAGE) * layout::TLS_SIZE
                    + (i % TLRS_PER_PAGE + 1) * TLR_SIZE
            })
            .find(|a| !self.sched.tlrs.contains(a))
            .expect("an unbounded search finds a free slot");
        let page = (addr - layout::TLS_BASE) / layout::TLS_SIZE;
        if page >= self.sched.tls_pages {
            let base = layout::TLS_BASE + page * layout::TLS_SIZE;
            self.mem
                .map_tagged(
                    base,
                    layout::TLS_SIZE,
                    Perm::RW,
                    tag(state::THREAD_LOCAL),
                    "tls",
                )
                .map_err(|_| result::OUT_OF_MEMORY)?;
            self.sched.tls_pages = page + 1;
        }
        self.mem
            .poke_bytes(addr, &[0; TLR_SIZE as usize])
            .map_err(|_| result::OUT_OF_MEMORY)?;
        self.sched.tlrs.push(addr);
        Ok(addr)
    }

    /// Create a thread, not started, with a handle naming it. Returns the
    /// handle.
    pub(crate) fn new_thread(
        &mut self,
        entry: u64,
        arg: u64,
        stack_top: u64,
        priority: u32,
        core: u32,
    ) -> Result<u32, u64> {
        let id = crate::kernel::MAIN_THREAD_ID + self.sched.created;
        let h = self
            .handles
            .insert(Object::Thread { id }, &[Kind::Thread])?[0];
        let tlr = match self.alloc_tlr() {
            Ok(a) => a,
            Err(code) => {
                self.handles.close(h).expect("handle just opened");
                return Err(code);
            }
        };
        let object = self.handles.get(h).map(|(_, o)| o);
        let mut cpu = CpuState {
            pc: entry,
            sp: stack_top,
            tpidrro_el0: tlr,
            ..Default::default()
        };
        cpu.x[0] = arg;
        let turn = self.sched.take_turn();
        self.sched.created += 1;
        self.sched.threads.push(Thread {
            id,
            cpu,
            state: State::Created,
            priority,
            core,
            affinity: 1 << core,
            tlr,
            object,
            turn,
        });
        Ok(h)
    }

    /// The index of the thread `h` names: a thread handle or the
    /// current-thread pseudo-handle.
    pub(crate) fn thread_index(&self, h: u64) -> Result<usize, u64> {
        let h = h as u32;
        if h as u64 == handle::CURRENT_THREAD {
            return Ok(self.sched.current);
        }
        match self.handles.get(h) {
            Some((Kind::Thread, object)) => Ok(self
                .sched
                .threads
                .iter()
                .position(|t| t.object == Some(object))
                .expect("a thread object names a thread")),
            _ => Err(result::INVALID_HANDLE),
        }
    }

    /// svcGetThreadId ("SVC").
    pub fn thread_id(&self, h: u64) -> Result<u64, u64> {
        Ok(self.sched.threads[self.thread_index(h)?].id)
    }

    /// The last handle to thread `id` closed: forget it once it has exited.
    /// A thread that was never started can no longer be, so it goes too.
    pub(crate) fn thread_released(&mut self, id: u64) {
        let i = self
            .sched
            .threads
            .iter()
            .position(|t| t.id == id)
            .expect("a thread object names a thread");
        self.sched.threads[i].object = None;
        if self.sched.threads[i].state == State::Created {
            self.end_thread(i);
        }
        self.reap();
    }

    /// Mark thread `i` exited and free its thread local region.
    fn end_thread(&mut self, i: usize) {
        let t = &mut self.sched.threads[i];
        t.state = State::Exited;
        let tlr = t.tlr;
        self.sched.tlrs.retain(|&a| a != tlr);
    }

    /// svcCreateThread ("SVC"): a thread that starts at `entry` with X0 =
    /// `arg` and SP = `stack_top` once svcStartThread is called. Priority
    /// is 0 to 0x3F (InvalidPriority otherwise); ProcessorId is 0 to 3, or
    /// -2 for the process's default core (InvalidCoreId otherwise).
    pub fn create_thread(
        &mut self,
        entry: u64,
        arg: u64,
        stack_top: u64,
        priority: i32,
        core: i32,
    ) -> Result<u32, u64> {
        if !(0..=LOWEST_PRIORITY as i32).contains(&priority) {
            return Err(result::INVALID_PRIORITY);
        }
        let core = match core {
            -2 => DEFAULT_CORE,
            0..=3 => core as u32,
            _ => return Err(result::INVALID_CORE_ID),
        };
        self.new_thread(entry, arg, stack_top, priority as u32, core)
    }

    /// svcStartThread: a created thread becomes ready. Starting it twice is
    /// InvalidState (kestrel's choice; Switchbrew lists no result codes).
    pub fn start_thread(&mut self, h: u64) -> Result<(), u64> {
        let i = self.thread_index(h)?;
        if self.sched.threads[i].state != State::Created {
            return Err(result::INVALID_STATE);
        }
        let turn = self.sched.take_turn();
        let t = &mut self.sched.threads[i];
        t.state = State::Ready;
        t.turn = turn;
        Ok(())
    }

    /// svcGetThreadPriority ("SVC").
    pub fn thread_priority(&self, h: u64) -> Result<u32, u64> {
        Ok(self.sched.threads[self.thread_index(h)?].priority)
    }

    /// svcSetThreadPriority: 0 to 0x3F. The thread goes behind the others
    /// of its new priority, and its core picks its best thread again.
    pub fn set_thread_priority(&mut self, h: u64, priority: i32) -> Result<(), u64> {
        let i = self.thread_index(h)?;
        if !(0..=LOWEST_PRIORITY as i32).contains(&priority) {
            return Err(result::INVALID_PRIORITY);
        }
        let turn = self.sched.take_turn();
        let t = &mut self.sched.threads[i];
        t.priority = priority as u32;
        t.turn = turn;
        Ok(())
    }

    /// svcGetThreadCoreMask: the ideal core and the affinity mask.
    pub fn thread_core_mask(&self, h: u64) -> Result<(u32, u64), u64> {
        let t = &self.sched.threads[self.thread_index(h)?];
        Ok((t.core, t.affinity))
    }

    /// svcSetThreadCoreMask. Switchbrew documents only the arguments, so
    /// the checks are kestrel's: the ideal core must be 0 to 3 and the mask
    /// a non-empty set of cores 0 to 3 (InvalidCoreId otherwise), holding
    /// the ideal core (InvalidCombination otherwise). The thread moves to
    /// its new ideal core. Negative ideal cores have undocumented meanings,
    /// so they stop the process.
    pub fn set_thread_core_mask(
        &mut self,
        h: u64,
        core: i32,
        mask: u64,
    ) -> Result<(), CoreMaskError> {
        let i = self.thread_index(h).map_err(CoreMaskError::Result)?;
        if core < 0 {
            return Err(CoreMaskError::Unsupported(core));
        }
        let all = (1 << CORES) - 1;
        if core >= CORES as i32 || mask == 0 || mask & !all != 0 {
            return Err(CoreMaskError::Result(result::INVALID_CORE_ID));
        }
        if mask & 1 << core == 0 {
            return Err(CoreMaskError::Result(result::INVALID_COMBINATION));
        }
        let turn = self.sched.take_turn();
        let t = &mut self.sched.threads[i];
        (t.core, t.affinity, t.turn) = (core as u32, mask, turn);
        Ok(())
    }

    /// svcExitThread: the running thread ends; its handles stay valid.
    pub fn exit_thread(&mut self) {
        self.end_thread(self.sched.current);
        self.sched.rotate = true;
    }

    /// svcSleepThread: 0, -1 and -2 yield; kestrel treats all three alike,
    /// as threads do not migrate between its cores. A positive time puts
    /// the thread to sleep. Other negative values are not documented.
    pub fn sleep_thread(&mut self, ns: i64) -> Result<(), String> {
        let until = match ns {
            1.. => Some(self.cpu.retired.saturating_add(ticks(ns as u64))),
            -2..=0 => None,
            _ => return Err(format!("svcSleepThread with {ns} nanoseconds")),
        };
        let turn = self.sched.take_turn();
        let t = &mut self.sched.threads[self.sched.current];
        t.turn = turn;
        if let Some(until) = until {
            t.state = State::Sleeping { until };
        }
        self.sched.rotate = true;
        Ok(())
    }

    /// Drop exited threads no handle names, other than the running one.
    fn reap(&mut self) {
        let current = self.sched.threads[self.sched.current].id;
        self.sched
            .threads
            .retain(|t| t.id == current || t.object.is_some() || t.state != State::Exited);
        self.sched.current = self
            .sched
            .threads
            .iter()
            .position(|t| t.id == current)
            .expect("the running thread is kept");
    }

    /// Make thread `next` the running one.
    fn switch_to(&mut self, next: usize) {
        let s = &mut self.sched;
        if next == s.current {
            return;
        }
        let mut incoming = std::mem::take(&mut s.threads[next].cpu);
        incoming.retired = self.cpu.retired; // the clock is process-wide
        let mut outgoing = std::mem::replace(&mut self.cpu, incoming);
        // A switch clears the exclusive monitor, so a store-exclusive that
        // another thread may have overtaken fails and is retried.
        outgoing.exclusive_addr = None;
        s.threads[s.current].cpu = outgoing;
        s.current = next;
    }

    /// Pick the thread to run after a system call: the best ready thread of
    /// the running thread's core, or, when `rotate` is set, of the next
    /// core with a ready thread. Wakes sleepers whose time has come and
    /// skips idle time. Stops the process when no thread is left to run.
    pub(crate) fn schedule(&mut self, rotate: bool) -> Option<Stop> {
        loop {
            let now = self.cpu.retired;
            for t in &mut self.sched.threads {
                if let State::Sleeping { until } = t.state {
                    if until <= now {
                        self.sched.next_turn += 1;
                        t.state = State::Ready;
                        t.turn = self.sched.next_turn;
                    }
                }
            }
            let first = self.current_thread().core + rotate as u32;
            for core in (0..CORES).map(|c| (first + c) % CORES) {
                let best = self
                    .sched
                    .threads
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.state == State::Ready && t.core == core)
                    .min_by_key(|(_, t)| (t.priority, t.turn))
                    .map(|(i, _)| i);
                if let Some(i) = best {
                    if rotate || i != self.sched.current {
                        self.sched.slice_left = SLICE;
                    }
                    self.switch_to(i);
                    self.reap();
                    return None;
                }
            }
            let wake = self
                .sched
                .threads
                .iter()
                .filter_map(|t| match t.state {
                    State::Sleeping { until } => Some(until),
                    _ => None,
                })
                .min();
            match wake {
                Some(until) => {
                    self.sched.idle += until - now;
                    self.cpu.retired = until;
                }
                None if self.sched.threads.iter().all(|t| t.state == State::Exited) => {
                    return Some(Stop::Exited)
                }
                None => {
                    let created = self
                        .sched
                        .threads
                        .iter()
                        .filter(|t| t.state == State::Created)
                        .count();
                    return Some(Stop::Error(format!(
                        "no thread can run: {created} thread(s) created but never started"
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svc::{dispatch, nr};
    use kestrel_core::Memory;
    use kestrel_loader::Loaded;

    fn process() -> Process {
        let image = Loaded {
            entry: 0x1000,
            image_start: 0,
            image_end: 0,
        };
        Process::new(Memory::new(), &image).unwrap()
    }

    /// Make system call `n` with X0.. = `args` as the run loop does, then
    /// schedule. Returns W0 and X1 as the call left them.
    fn svc(p: &mut Process, n: u16, args: &[u64]) -> (u64, u64) {
        p.cpu.x[..args.len()].copy_from_slice(args);
        assert_eq!(dispatch(p, n), None);
        let regs = (p.cpu.x[0], p.cpu.x[1]);
        let rotate = std::mem::take(&mut p.sched.rotate);
        assert_eq!(p.schedule(rotate), None);
        regs
    }

    /// Create a thread with SP 0x9000 and X0 0x55; returns its handle.
    fn spawn(p: &mut Process, entry: u64, prio: u64, core: u64) -> u64 {
        let (rc, h) = svc(p, nr::CREATE_THREAD, &[0, entry, 0x55, 0x9000, prio, core]);
        assert_eq!(rc, 0);
        h
    }

    /// Where the running thread is.
    fn running(p: &Process) -> u64 {
        p.cpu.pc
    }

    #[test]
    fn create_thread_checks_priority_and_processor() {
        let mut p = process();
        let create = |p: &mut Process, prio: i64, core: i64| {
            let args = [0, 0x2000, 0, 0x9000, prio as u64, core as u64];
            svc(p, nr::CREATE_THREAD, &args).0
        };
        assert_eq!(create(&mut p, 0x40, 0), result::INVALID_PRIORITY);
        assert_eq!(create(&mut p, -1, 0), result::INVALID_PRIORITY);
        for core in [4, -1, -3] {
            assert_eq!(
                create(&mut p, 0x2c, core),
                result::INVALID_CORE_ID,
                "core {core}"
            );
        }
        assert_eq!(create(&mut p, 0x3f, -2), 0);
        assert_eq!(create(&mut p, 0, 3), 0);
        let t = &p.sched.threads;
        assert_eq!((t[1].priority, t[1].core), (0x3f, DEFAULT_CORE));
        assert_eq!((t[2].priority, t[2].core), (0, 3));
        assert_eq!(t[1].state, State::Created);
    }

    #[test]
    fn a_started_thread_gets_its_registers_and_its_own_zeroed_tlr() {
        let mut p = process();
        let main_tlr = p.cpu.tpidrro_el0;
        assert_eq!(main_tlr, layout::TLS_BASE + TLR_SIZE);
        p.mem.write64(main_tlr, 7).unwrap();
        let h = spawn(&mut p, 0x2000, 0x20, 0);
        assert_eq!(running(&p), 0x1000, "created, not started");
        assert_eq!(svc(&mut p, nr::START_THREAD, &[h]).0, 0);
        // Higher priority on the same core: it runs at once.
        assert_eq!(running(&p), 0x2000);
        assert_eq!((p.cpu.x[0], p.cpu.sp), (0x55, 0x9000));
        let tlr = p.cpu.tpidrro_el0;
        assert_eq!(tlr, main_tlr + TLR_SIZE);
        assert_eq!(p.mem.read64(tlr).unwrap(), 0);
        assert_eq!(svc(&mut p, nr::START_THREAD, &[h]).0, result::INVALID_STATE);
        assert_eq!(
            svc(&mut p, nr::START_THREAD, &[0]).0,
            result::INVALID_HANDLE
        );
    }

    #[test]
    fn priorities_order_a_core_and_yielding_lets_other_cores_run() {
        let mut p = process();
        // Lower priority on the main thread's core: does not preempt.
        let low = spawn(&mut p, 0x2000, 0x30, 0);
        svc(&mut p, nr::START_THREAD, &[low]);
        assert_eq!(running(&p), 0x1000);
        // Yielding gives other cores a turn, but core 0 has nothing better.
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x1000);
        // A thread on core 1 runs when the main thread yields.
        let other = spawn(&mut p, 0x3000, 0x30, 1);
        svc(&mut p, nr::START_THREAD, &[other]);
        assert_eq!(running(&p), 0x1000);
        svc(&mut p, nr::SLEEP_THREAD, &[-1i64 as u64]);
        assert_eq!(running(&p), 0x3000);
        // When it yields, core 0's best thread, the main thread, runs.
        svc(&mut p, nr::SLEEP_THREAD, &[-2i64 as u64]);
        assert_eq!(running(&p), 0x1000);
        // Once the main thread exits, the low-priority thread gets core 0.
        svc(&mut p, nr::EXIT_THREAD, &[]);
        assert_eq!(running(&p), 0x3000, "core 1 is next after core 0");
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x2000);
    }

    #[test]
    fn equal_priorities_on_a_core_take_turns_on_yield() {
        let mut p = process();
        let h = spawn(&mut p, 0x2000, MAIN_PRIORITY as u64, 0);
        svc(&mut p, nr::START_THREAD, &[h]);
        assert_eq!(running(&p), 0x1000, "equal priority does not preempt");
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x2000);
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x1000);
    }

    #[test]
    fn sleeping_skips_idle_time_without_counting_instructions() {
        let mut p = process();
        p.cpu.retired = 100;
        svc(&mut p, nr::SLEEP_THREAD, &[1_000_000]); // 1 ms
        assert_eq!(running(&p), 0x1000);
        assert_eq!(p.cpu.retired, 100 + 19_200);
        assert_eq!(p.instructions(), 100);
        // A sleeping thread lets a lower-priority one run until it wakes.
        let h = spawn(&mut p, 0x2000, 0x3f, 0);
        svc(&mut p, nr::START_THREAD, &[h]);
        svc(&mut p, nr::SLEEP_THREAD, &[1_000_000]);
        assert_eq!(running(&p), 0x2000);
        p.cpu.retired += 19_200; // the low-priority thread runs for 1 ms
        svc(&mut p, nr::GET_SYSTEM_TICK, &[]);
        assert_eq!(running(&p), 0x1000, "woken and preempting");
        assert_eq!(p.cpu.retired, 100 + 2 * 19_200);
        assert_eq!(p.instructions(), 100 + 19_200);
    }

    #[test]
    fn undocumented_sleep_values_stop_the_process() {
        let mut p = process();
        p.cpu.x[0] = -3i64 as u64;
        match dispatch(&mut p, nr::SLEEP_THREAD) {
            Some(Stop::Error(e)) => assert!(e.contains("svcSleepThread with -3"), "{e}"),
            other => panic!("expected a stop, got {other:?}"),
        }
    }

    #[test]
    fn exited_threads_are_forgotten_once_no_handle_names_them() {
        let mut p = process();
        let h = spawn(&mut p, 0x2000, 0x20, 0);
        svc(&mut p, nr::START_THREAD, &[h]);
        let (tlr, id) = (p.cpu.tpidrro_el0, p.current_thread().id);
        svc(&mut p, nr::EXIT_THREAD, &[]);
        assert_eq!(running(&p), 0x1000);
        // The handle still names the exited thread.
        assert_eq!(svc(&mut p, nr::GET_THREAD_ID, &[0, h]), (0, id));
        assert_eq!(svc(&mut p, nr::START_THREAD, &[h]).0, result::INVALID_STATE);
        assert_eq!(p.sched.threads.len(), 2);
        svc(&mut p, nr::CLOSE_HANDLE, &[h]);
        assert_eq!(p.sched.threads.len(), 1);
        // Its thread local region is reused, and the ID is not.
        let h = spawn(&mut p, 0x3000, 0x20, 0);
        svc(&mut p, nr::START_THREAD, &[h]);
        assert_eq!(p.cpu.tpidrro_el0, tlr);
        assert_ne!(p.current_thread().id, id);
        // Closing the handle of a thread never started forgets it too.
        let h = spawn(&mut p, 0x4000, 0x20, 0);
        svc(&mut p, nr::CLOSE_HANDLE, &[h]);
        assert_eq!(p.sched.threads.len(), 2);
    }

    #[test]
    fn the_process_ends_with_its_last_thread() {
        let mut p = process();
        let h = spawn(&mut p, 0x2000, 0x30, 1);
        svc(&mut p, nr::START_THREAD, &[h]);
        svc(&mut p, nr::EXIT_THREAD, &[]);
        assert_eq!(running(&p), 0x2000);
        p.exit_thread();
        assert_eq!(p.schedule(true), Some(Stop::Exited));
    }

    #[test]
    fn threads_that_were_never_started_cannot_keep_the_process_alive() {
        let mut p = process();
        spawn(&mut p, 0x2000, 0x30, 1);
        p.exit_thread();
        match p.schedule(true) {
            Some(Stop::Error(e)) => assert!(e.contains("never started"), "{e}"),
            other => panic!("expected a stop, got {other:?}"),
        }
    }

    #[test]
    fn priorities_can_be_read_and_changed() {
        let mut p = process();
        let cur = handle::CURRENT_THREAD;
        assert_eq!(svc(&mut p, nr::GET_THREAD_PRIORITY, &[0, cur]), (0, 0x2c));
        let h = spawn(&mut p, 0x2000, 0x30, 0);
        svc(&mut p, nr::START_THREAD, &[h]);
        assert_eq!(svc(&mut p, nr::GET_THREAD_PRIORITY, &[0, h]), (0, 0x30));
        let bad = result::INVALID_PRIORITY;
        assert_eq!(svc(&mut p, nr::SET_THREAD_PRIORITY, &[h, 0x40]).0, bad);
        assert_eq!(svc(&mut p, nr::SET_THREAD_PRIORITY, &[h, u64::MAX]).0, bad);
        assert_eq!(running(&p), 0x1000);
        // Raising it above the main thread's takes core 0 at once.
        assert_eq!(svc(&mut p, nr::SET_THREAD_PRIORITY, &[h, 0x10]).0, 0);
        assert_eq!(running(&p), 0x2000);
        assert_eq!(svc(&mut p, nr::GET_THREAD_PRIORITY, &[0, cur]), (0, 0x10));
        // Lowering itself below the main thread gives core 0 back.
        assert_eq!(svc(&mut p, nr::SET_THREAD_PRIORITY, &[cur, 0x3f]).0, 0);
        assert_eq!(running(&p), 0x1000);
        let closed = result::INVALID_HANDLE;
        assert_eq!(svc(&mut p, nr::GET_THREAD_PRIORITY, &[0, 0]).0, closed);
    }

    #[test]
    fn a_thread_moves_to_its_new_ideal_core() {
        let mut p = process();
        let cur = handle::CURRENT_THREAD;
        assert_eq!(svc(&mut p, nr::GET_CURRENT_PROCESSOR_NUMBER, &[]).0, 0);
        let (rc, core) = svc(&mut p, nr::GET_THREAD_CORE_MASK, &[0, 0, cur]);
        assert_eq!((rc, core, p.cpu.x[2]), (0, 0, 0b1));
        let h = spawn(&mut p, 0x2000, 0x10, 2);
        let (_, core) = svc(&mut p, nr::GET_THREAD_CORE_MASK, &[0, 0, h]);
        assert_eq!((core, p.cpu.x[2]), (2, 0b100));
        svc(&mut p, nr::START_THREAD, &[h]);
        assert_eq!(running(&p), 0x1000, "another core");
        // Checks: core range, mask range, ideal core inside the mask.
        let set = |p: &mut Process, core: u64, mask: u64| {
            svc(p, nr::SET_THREAD_CORE_MASK, &[h, core, mask]).0
        };
        assert_eq!(set(&mut p, 4, 0b1111), result::INVALID_CORE_ID);
        assert_eq!(set(&mut p, 0, 0), result::INVALID_CORE_ID);
        assert_eq!(set(&mut p, 0, 0b1_0001), result::INVALID_CORE_ID);
        assert_eq!(set(&mut p, 0, 0b0110), result::INVALID_COMBINATION);
        // Moving the higher-priority thread to core 0 preempts the main thread.
        assert_eq!(set(&mut p, 0, 0b0011), 0);
        assert_eq!(running(&p), 0x2000);
        assert_eq!(svc(&mut p, nr::GET_CURRENT_PROCESSOR_NUMBER, &[]).0, 0);
        let (_, core) = svc(&mut p, nr::GET_THREAD_CORE_MASK, &[0, 0, cur]);
        assert_eq!((core, p.cpu.x[2]), (0, 0b0011));
        // Negative ideal cores are undocumented.
        p.cpu.x[..3].copy_from_slice(&[h, -1i64 as u64, 1]);
        match dispatch(&mut p, nr::SET_THREAD_CORE_MASK) {
            Some(Stop::Error(e)) => assert!(e.contains("ideal core -1"), "{e}"),
            other => panic!("expected a stop, got {other:?}"),
        }
    }

    #[test]
    fn a_switch_clears_the_outgoing_exclusive_monitor() {
        let mut p = process();
        p.cpu.exclusive_addr = Some(0x5000);
        let h = spawn(&mut p, 0x2000, 0x20, 0);
        svc(&mut p, nr::START_THREAD, &[h]);
        assert_eq!(running(&p), 0x2000);
        assert_eq!(p.sched.threads[0].cpu.exclusive_addr, None);
    }

    #[test]
    fn a_slice_is_reset_when_another_thread_or_core_gets_its_turn() {
        let mut p = process();
        assert_eq!(p.sched.slice_left, SLICE);
        p.sched.slice_left = 5;
        svc(&mut p, nr::GET_SYSTEM_TICK, &[]);
        assert_eq!(p.sched.slice_left, 5, "no switch, same slice");
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(p.sched.slice_left, SLICE);
        p.sched.slice_left = 5;
        let h = spawn(&mut p, 0x2000, 0x20, 0);
        svc(&mut p, nr::START_THREAD, &[h]);
        assert_eq!(p.sched.slice_left, SLICE);
    }

    #[test]
    fn ticks_follow_the_19_2_mhz_counter() {
        assert_eq!(ticks(0), 0);
        assert_eq!(ticks(1_000_000_000), 19_200_000);
        assert_eq!(ticks(1_000), 20); // 19.2, rounded up
        assert_eq!(
            ticks(u64::MAX),
            (u64::MAX as u128 * 12).div_ceil(625) as u64
        );
    }
}
