//! Threaded execution: each guest core on its own host thread, guest time
//! from the host's clock (`PLAN.md` section 3, "Time and cores"; the
//! owner's decision of 2026-10-31 to build it now and test it on the
//! interpreter).
//!
//! The deterministic scheduler in `thread.rs` stays the default: the
//! self-test, unattended runs and the differential tester need runs that
//! repeat. This mode is for real time, where a game's threads must run in
//! parallel as on the console. It drives any [`CpuBackend`], one per core,
//! through the same interface as `Process::run`, so a faster backend plugs
//! in unchanged.
//!
//! The model, kestrel's choices where Switchbrew is silent:
//!
//! - Each of the `CORES` cores is a host thread with its own backend. It
//!   runs the best ready thread whose ideal core it is, as the
//!   deterministic scheduler picks it: lowest priority number first, then
//!   the longest ready. Equal priorities take turns only when the running
//!   thread yields, waits, sleeps or exits.
//! - Guest code runs without any lock, on the shared guest memory, whose
//!   atomics and exclusive monitor hold between host threads
//!   (`kestrel_core::Memory`, the interpreter's `ldst.rs`).
//! - The OS layer (kernel objects, handles, threads, synchronisation, IPC
//!   and services) sits behind one lock: a system call, and every look at
//!   the scheduler, takes it. Calls from different cores are serialised,
//!   so the OS layer needs no locks of its own; the kernel's changes to
//!   guest words other cores also change are compare-and-swaps
//!   (`sync.rs`). One lock is the simplest correct start; finer locks can
//!   follow where a game shows contention.
//! - A core looks at the scheduler after every system call and every
//!   `thread::SLICE` instructions: a higher-priority thread made ready by
//!   another core, a thread moved to another core, a sleeper due, a wait
//!   timed out, hid's samples and the display's vsync are all seen then,
//!   so within about 19,200 instructions (19 µs at the Switch 2's rate).
//! - A core with nothing to run sleeps on the host until another core
//!   changes the threads, or the next sleeper or timeout is due, waking at
//!   least every 10 ms to see whether the window closed.
//! - The process stops at the first stop any core meets (a call that ends
//!   it, a fault, an unimplemented instruction); the other cores finish
//!   their slice and stop too. The registers kept as the process's are
//!   those of the thread that stopped it.

use crate::thread::{nanoseconds, State, CORES, NO_THREAD, SLICE};
use crate::{guest_stop, svc, Process, Stop};
use kestrel_core::Memory;
use kestrel_cpu::{Clock, CpuBackend, CpuState, Exit};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// The longest an idle core sleeps before looking again: kestrel's choice,
/// as for the deterministic run loop's waits on the host clock.
const MAX_IDLE: Duration = Duration::from_millis(10);

/// What the cores share: the OS layer behind its lock, the condition idle
/// cores wait on, and the instruction count.
struct Shared<'p> {
    kernel: Mutex<Kernel<'p>>,
    changed: Condvar,
    instructions: AtomicU64,
    max_steps: u64,
    clock: Clock,
}

/// Everything behind the lock.
struct Kernel<'p> {
    p: &'p mut Process,
    /// The first stop, and the thread whose registers go with it.
    stop: Option<(Stop, Option<u64>)>,
}

/// A thread a core runs: its ID and its registers.
type Held = Option<(u64, CpuState)>;

impl<'p> Shared<'p> {
    fn lock(&self) -> MutexGuard<'_, Kernel<'p>> {
        // A core that panicked leaves the process stopped, not poisoned.
        self.kernel.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Kernel<'_> {
    fn index(&self, id: u64) -> usize {
        self.p
            .sched
            .threads
            .iter()
            .position(|t| t.id == id)
            .expect("a thread a core runs is kept")
    }

    /// Stop the process, unless another core already did.
    fn stop(&mut self, stop: Stop, thread: Option<u64>) {
        self.stop.get_or_insert((stop, thread));
    }

    /// Thread `id`, on a core, makes system call `n`: its registers are
    /// the process's for the call, as on the deterministic scheduler.
    fn call(&mut self, id: u64, cpu: &mut CpuState, n: u16, clock: Clock) {
        let i = self.index(id);
        let p = &mut *self.p;
        p.sched.current = i;
        p.cpu = std::mem::take(cpu);
        p.svc_count += 1;
        let stop = svc::dispatch(p, n);
        *cpu = std::mem::replace(
            &mut p.cpu,
            CpuState {
                clock,
                ..CpuState::default()
            },
        );
        p.sched.current = NO_THREAD;
        p.sched.rotate = false;
        if let Some(stop) = stop {
            self.stop(stop, Some(id));
        }
    }

    /// Give the thread a core holds back to the scheduler. Returns
    /// whether it is ready, so another core may take it.
    fn release(&mut self, held: &mut Held) -> bool {
        let Some((id, cpu)) = held.take() else {
            return false;
        };
        let i = self.index(id);
        let t = &mut self.p.sched.threads[i];
        (t.cpu, t.running) = (cpu, false);
        t.state == State::Ready
    }

    /// The best thread for `core`: ready, on that core, and not running
    /// on another (the one `core` holds counts as its own).
    fn best(&self, core: u32, held: Option<u64>) -> Option<u64> {
        self.p
            .sched
            .threads
            .iter()
            .filter(|t| t.core == core && t.state == State::Ready)
            .filter(|t| !t.running || Some(t.id) == held)
            .min_by_key(|t| (t.priority, t.turn))
            .map(|t| t.id)
    }

    fn ready(&self) -> usize {
        let threads = &self.p.sched.threads;
        threads.iter().filter(|t| t.state == State::Ready).count()
    }
}

impl Process {
    /// Run with each guest core on its own host thread, `backends[i]`
    /// running core `i`, until the process stops or `max_steps`
    /// instructions retire on all cores together. Guest time is the
    /// host's clock: a counted clock is replaced by it.
    pub fn run_threaded(&mut self, backends: Vec<Box<dyn CpuBackend>>, max_steps: u64) -> Stop {
        assert_eq!(backends.len(), CORES as usize, "one backend per core");
        if let Clock::Counted { .. } = self.cpu.clock {
            self.cpu.clock = Clock::host();
        }
        let clock = self.cpu.clock;
        let before = self.cpu.retired;
        // The running thread goes back to its record: no thread is in a
        // call, and the cores pick what runs.
        let current = self.sched.current;
        if let Some(t) = self.sched.threads.get_mut(current) {
            t.cpu = std::mem::take(&mut self.cpu);
        }
        self.cpu.clock = clock;
        self.sched.current = NO_THREAD;
        let mem = self.mem.clone();
        let shared = Shared {
            kernel: Mutex::new(Kernel {
                p: self,
                stop: None,
            }),
            changed: Condvar::new(),
            instructions: AtomicU64::new(0),
            max_steps,
            clock,
        };
        std::thread::scope(|s| {
            for (core, backend) in backends.into_iter().enumerate() {
                let (shared, mem) = (&shared, &*mem);
                std::thread::Builder::new()
                    .name(format!("guest core {core}"))
                    .spawn_scoped(s, move || run_core(core as u32, backend, shared, mem))
                    .expect("cannot start a host thread for a guest core");
            }
        });
        let ran = shared.instructions.load(Relaxed);
        let kernel = shared
            .kernel
            .into_inner()
            .unwrap_or_else(|e| e.into_inner());
        let (stop, thread) = kernel.stop.expect("the cores end only once stopped");
        // The thread that stopped the process, or the first one, is the
        // running thread again.
        let s = &mut self.sched;
        let i = thread
            .and_then(|id| s.threads.iter().position(|t| t.id == id))
            .unwrap_or(0);
        if let Some(t) = s.threads.get_mut(i) {
            self.cpu = std::mem::take(&mut t.cpu);
            s.current = i;
        }
        (self.cpu.clock, self.cpu.retired) = (clock, before + ran);
        stop
    }
}

/// One guest core's host thread.
fn run_core(core: u32, mut backend: Box<dyn CpuBackend>, shared: &Shared, mem: &Memory) {
    let mut held: Held = None;
    let mut exit = None;
    loop {
        let mut k = shared.lock();
        // How the last run ended.
        if let (Some((id, cpu)), Some(e)) = (held.as_mut(), exit.take()) {
            match e {
                Exit::Svc(n) => {
                    k.call(*id, cpu, n, shared.clock);
                    shared.changed.notify_all();
                }
                Exit::StepLimit => {}
                other => {
                    let stop = guest_stop(other, cpu, backend.name());
                    k.stop(stop, Some(*id));
                }
            }
        }
        // What runs next, waiting while nothing can.
        loop {
            if let Some(flag) = &k.p.interrupt {
                if flag.load(Relaxed) {
                    k.stop(Stop::Interrupted, None);
                }
            }
            if shared.instructions.load(Relaxed) >= shared.max_steps {
                k.stop(Stop::StepLimit, None);
            }
            if k.stop.is_some() {
                k.release(&mut held);
                shared.changed.notify_all();
                return;
            }
            let ready = k.ready();
            let now = k.p.now();
            k.p.wake_due(now);
            k.p.hid_tick();
            k.p.vsync_tick();
            if k.ready() > ready {
                shared.changed.notify_all();
            }
            let held_id = held.as_ref().map(|(id, _)| *id);
            match k.best(core, held_id) {
                Some(id) if Some(id) == held_id => break,
                Some(id) => {
                    if k.release(&mut held) {
                        shared.changed.notify_all();
                    }
                    let i = k.index(id);
                    let t = &mut k.p.sched.threads[i];
                    t.running = true;
                    let mut cpu = std::mem::take(&mut t.cpu);
                    cpu.clock = shared.clock;
                    held = Some((id, cpu));
                    break;
                }
                None => {
                    if k.release(&mut held) {
                        shared.changed.notify_all();
                    }
                    let wake = k.p.next_wake();
                    if wake.is_none() && k.ready() == 0 {
                        // Nothing runs on any core and nothing will wake.
                        let stop = k.p.nothing_to_run();
                        k.stop(stop, None);
                        continue;
                    }
                    let wait = wake.map_or(MAX_IDLE, |until| {
                        let ns = nanoseconds(until.saturating_sub(k.p.now())).max(1);
                        Duration::from_nanos(ns).min(MAX_IDLE)
                    });
                    k = shared
                        .changed
                        .wait_timeout(k, wait)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
        drop(k);
        let (_, cpu) = held.as_mut().expect("a thread to run");
        let left = shared
            .max_steps
            .saturating_sub(shared.instructions.load(Relaxed));
        let start = cpu.retired;
        exit = Some(backend.run(cpu, mem, SLICE.min(left)));
        shared.instructions.fetch_add(cpu.retired - start, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svc::nr;
    use crate::testutil::{process, spawn, svc};
    use kestrel_core::Perm;
    use kestrel_cpu::interp::Interpreter;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    // Encodings from clang (--target=aarch64-none-elf) and llvm-objdump.
    /// b . (spin forever)
    const SPIN: u32 = 0x1400_0000;
    /// mov x1, #0; ldr x0, [x1] (a fault)
    const FAULT: [u32; 2] = [0xd280_0001, 0xf940_0020];
    /// mov x1, #0; mov x2, #0; mov x3, #-1; svc #0x18 (wait for nothing,
    /// forever)
    const WAIT_FOREVER: [u32; 4] = [0xd280_0001, 0xd280_0002, 0x9280_0003, 0xd400_0301];
    /// mov x0, #50000000 (50 ms); svc #0xb (SleepThread); svc #0x7
    /// (ExitProcess)
    const SLEEP_50MS: [u32; 4] = [0xd29e_1000, 0xf2a0_5f40, 0xd400_0161, 0xd400_00e1];
    /// svc #0xa (ExitThread)
    const EXIT_THREAD: u32 = 0xd400_0141;

    fn backends() -> Vec<Box<dyn CpuBackend>> {
        (0..CORES)
            .map(|_| Box::new(Interpreter::new()) as Box<dyn CpuBackend>)
            .collect()
    }

    /// A process whose main thread runs `main` at 0x1000, and `other` at
    /// 0x2000 when a thread is started there.
    fn with_code(main: &[u32], other: &[u32]) -> Process {
        let p = process();
        p.mem.map(0x1000, 0x2000, Perm::RX, "code").unwrap();
        for (base, code) in [(0x1000, main), (0x2000, other)] {
            let bytes: Vec<u8> = code.iter().flat_map(|i| i.to_le_bytes()).collect();
            p.mem.poke_bytes(base, &bytes).unwrap();
        }
        p
    }

    #[test]
    fn a_fault_on_another_core_stops_the_process_with_its_registers() {
        let mut p = with_code(&[SPIN], &FAULT);
        let h = spawn(&mut p, 0x2000, 0x2c, 2);
        svc(&mut p, nr::START_THREAD, &[h]);
        match p.run_threaded(backends(), u64::MAX) {
            Stop::Error(e) => assert!(e.contains("guest fault at pc 0x2004"), "{e}"),
            other => panic!("expected a fault, got {other:?}"),
        }
        assert_eq!(p.cpu.pc, 0x2004, "the faulting thread's registers");
        assert_eq!(p.current_thread().core, 2);
        assert!(p.sched.threads.iter().all(|t| !t.running));
    }

    #[test]
    fn the_process_ends_when_every_thread_exits() {
        let mut p = with_code(&[EXIT_THREAD], &[EXIT_THREAD]);
        let h = spawn(&mut p, 0x2000, 0x2c, 1);
        svc(&mut p, nr::START_THREAD, &[h]);
        assert_eq!(p.run_threaded(backends(), u64::MAX), Stop::Exited);
        assert_eq!(p.instructions(), 2);
    }

    #[test]
    fn a_deadlock_is_reported() {
        let mut p = with_code(&WAIT_FOREVER, &[]);
        match p.run_threaded(backends(), u64::MAX) {
            Stop::Error(e) => assert!(e.starts_with("deadlock"), "{e}"),
            other => panic!("expected a deadlock, got {other:?}"),
        }
    }

    #[test]
    fn spinning_cores_stop_at_the_budget_or_when_interrupted() {
        let mut p = with_code(&[SPIN], &[SPIN]);
        let h = spawn(&mut p, 0x2000, 0x2c, 3);
        svc(&mut p, nr::START_THREAD, &[h]);
        assert_eq!(p.run_threaded(backends(), 1_000_000), Stop::StepLimit);
        // Each core stops within a slice of the budget.
        assert!((1_000_000..1_000_000 + 2 * SLICE).contains(&p.instructions()));
        let flag = Arc::new(AtomicBool::new(false));
        p.interrupt = Some(flag.clone());
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                flag.store(true, Relaxed);
            });
            assert_eq!(p.run_threaded(backends(), u64::MAX), Stop::Interrupted);
        });
    }

    #[test]
    fn guest_time_is_the_hosts() {
        let mut p = with_code(&SLEEP_50MS, &[]);
        let t = std::time::Instant::now();
        assert_eq!(p.run_threaded(backends(), u64::MAX), Stop::Exited);
        assert!(
            t.elapsed() >= Duration::from_millis(50),
            "{:?}",
            t.elapsed()
        );
        assert!(matches!(p.cpu.clock, Clock::Host { .. }));
        assert!(p.now() >= crate::thread::ticks(50_000_000));
        assert_eq!(p.instructions(), 4);
    }
}
