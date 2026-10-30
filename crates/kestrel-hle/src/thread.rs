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
//!   exits.
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
}

impl Scheduler {
    fn take_turn(&mut self) -> u64 {
        self.next_turn += 1;
        self.next_turn
    }
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
    pub(crate) fn thread_released(&mut self, id: u64) {
        let i = self
            .sched
            .threads
            .iter()
            .position(|t| t.id == id)
            .expect("a thread object names a thread");
        self.sched.threads[i].object = None;
        self.reap();
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
