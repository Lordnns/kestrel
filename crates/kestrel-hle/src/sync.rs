//! Synchronisation: waiting on kernel objects, cancelling a wait, mutexes
//! and condition variables.
//!
//! From Switchbrew's "SVC" page (WaitSynchronization, its object types and
//! result codes, CancelSynchronization). A readable event is signaled while
//! its event is; a thread is signaled once it has exited. Waiting holds a
//! reference to every object, so closing their handles meanwhile does not
//! end the wait.
//!
//! Mutexes and condition variables live in guest memory; the kernel only
//! arbitrates. Switchbrew lists the arguments of ArbitrateLock,
//! ArbitrateUnlock, WaitProcessWideKeyAtomic and SignalProcessWideKey; the
//! protocol comes from the open-source libnx (`nx/source/kernel/mutex.c`
//! and `condvar.c`, ISC licence), which runs on the console:
//!
//! - A mutex word holds its owner's tag, the owning thread's handle, with
//!   bit 30 (`WAIT_MASK`) set while threads wait for it; 0 is unlocked.
//! - A thread that finds the mutex held sets bit 30 and calls
//!   ArbitrateLock(owner tag, word, own tag), then re-reads the word and
//!   owns the mutex if the word holds its tag; otherwise it tries again.
//!   So ArbitrateLock may return at once when the word no longer reads
//!   owner | bit 30, and must, on return after waiting, have written the
//!   caller's tag.
//! - An owner that finds bit 30 set calls ArbitrateUnlock(word), which
//!   hands the mutex to a waiter or writes 0.
//! - A condition variable wait passes the mutex word in X0 and the
//!   condition variable in X1 (Switchbrew labels them the other way round;
//!   libnx's order is what the console accepts). The wait releases the
//!   mutex; once signalled, the thread holds the mutex again when the call
//!   returns success. After TimedOut it does not, and libnx locks it
//!   itself. SignalProcessWideKey(-1) wakes every waiter.
//!
//! Address arbitration (WaitForAddress, SignalToAddress) is documented by
//! the names of its ArbitrationType and SignalType values only. Kestrel
//! implements the types whose names leave one reading and stops on the
//! others.

use crate::kernel::{Kind, Object, ObjectId};
use crate::svc::result;
use crate::thread::{State, Wait};
use crate::Process;
use kestrel_core::Perm;

/// svcWaitSynchronization takes at most this many handles ("SVC").
pub const MAX_WAIT_OBJECTS: i32 = 0x40;
/// Set in a mutex word while threads wait for it (libnx `HANDLE_WAIT_MASK`).
pub const WAIT_MASK: u32 = 0x4000_0000;

/// Why an address arbitration call failed.
#[derive(Debug, PartialEq, Eq)]
pub enum ArbitrationError {
    /// Return this result code.
    Result(u64),
    /// A type whose behaviour is not documented well enough to implement.
    Unsupported(&'static str),
}

impl From<u64> for ArbitrationError {
    fn from(code: u64) -> Self {
        ArbitrationError::Result(code)
    }
}

/// The clock reading at which a wait of `timeout` nanoseconds ends: never
/// for a negative timeout.
pub(crate) fn deadline(now: u64, timeout: i64) -> Option<u64> {
    (timeout >= 0).then(|| now.saturating_add(crate::thread::ticks(timeout as u64)))
}

impl Process {
    /// Whether object `id` is signaled.
    fn signaled(&self, id: ObjectId) -> bool {
        match self.handles.object(id) {
            Object::Event { signaled } => *signaled,
            Object::Thread { id } => self
                .sched
                .threads
                .iter()
                .find(|t| t.id == *id)
                .map_or(true, |t| t.state == State::Exited),
            o => unreachable!("waiting on {o:?}"),
        }
    }

    /// svcWaitSynchronization: wait until one of the readable events or
    /// threads that `handles` names is signaled. Returns the index of the
    /// first signaled one, `None` if the thread now waits, or a result code:
    /// OutOfRange (0xee01) for more than 0x40 handles, InvalidPointer
    /// (0xe601) for an unreadable handle list, InvalidHandle for anything
    /// that cannot be waited on (pseudo-handles included), Cancelled
    /// (0xec01) if svcCancelSynchronization came first, TimedOut (0xea01)
    /// when the timeout is 0 and nothing is signaled. With no handles the
    /// thread waits for the timeout or a cancel.
    pub fn wait_synchronization(
        &mut self,
        list: u64,
        count: i32,
        timeout: i64,
    ) -> Result<Option<u64>, u64> {
        if !(0..=MAX_WAIT_OBJECTS).contains(&count) {
            return Err(result::OUT_OF_RANGE);
        }
        let bytes = self
            .mem
            .read_vec(list, count as usize * 4)
            .map_err(|_| result::INVALID_POINTER)?;
        let mut objects = Vec::with_capacity(count as usize);
        for h in bytes.chunks_exact(4) {
            let h = u32::from_le_bytes(h.try_into().expect("four bytes"));
            match self.handles.get(h) {
                Some((Kind::ReadableEvent | Kind::Thread, id)) => objects.push(id),
                _ => return Err(result::INVALID_HANDLE),
            }
        }
        let cur = self.sched.current;
        if std::mem::take(&mut self.sched.threads[cur].cancelled) {
            return Err(result::CANCELLED);
        }
        if let Some(i) = objects.iter().position(|&id| self.signaled(id)) {
            return Ok(Some(i as u64));
        }
        if timeout == 0 {
            return Err(result::TIMED_OUT);
        }
        for &id in &objects {
            self.handles.retain(id);
        }
        let until = deadline(self.cpu.retired, timeout);
        self.block(Wait::Objects(objects), until);
        Ok(None)
    }

    /// Wake every thread waiting on an object that is now signaled.
    pub(crate) fn wake_object_waiters(&mut self) {
        for i in 0..self.sched.threads.len() {
            let Some(Wait::Objects(ids)) = &self.sched.threads[i].wait else {
                continue;
            };
            if let Some(index) = ids.iter().position(|&id| self.signaled(id)) {
                self.wake(i, result::SUCCESS, Some(index as u64));
            }
        }
    }

    /// Check a word the kernel reads and writes for user code: aligned
    /// (InvalidAddress) and mapped read-write (InvalidCurrentMemory). The
    /// result codes are kestrel's choice.
    fn check_word(&self, addr: u64) -> Result<(), u64> {
        if addr % 4 != 0 {
            return Err(result::INVALID_ADDRESS);
        }
        match self.mem.region(addr) {
            Some(b) if b.perm.contains(Perm::RW) => Ok(()),
            _ => Err(result::INVALID_CURRENT_MEMORY),
        }
    }

    fn word(&self, addr: u64) -> u32 {
        self.mem.read32(addr).expect("checked word")
    }

    fn set_word(&mut self, addr: u64, v: u32) {
        self.mem.write32(addr, v).expect("checked word");
    }

    /// The thread whose handle is `tag`, as a thread ID.
    fn tag_owner(&self, tag: u32) -> Option<u64> {
        let i = self.thread_index(tag as u64).ok()?;
        Some(self.sched.threads[i].id)
    }

    /// The waiters matching `f`, best first.
    fn waiters(&self, f: impl Fn(&Wait) -> bool) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.sched.threads.len())
            .filter(|&i| self.sched.threads[i].wait.as_ref().is_some_and(&f))
            .collect();
        v.sort_by_key(|&i| self.wait_order(i));
        v
    }

    /// svcArbitrateLock: wait for the mutex word at `addr` while it reads
    /// `owner` | WAIT_MASK. Returns whether the caller now waits; if the
    /// word reads anything else, the call returns at once. `owner` must be
    /// a thread handle (InvalidHandle).
    pub fn arbitrate_lock(&mut self, owner: u64, addr: u64, tag: u32) -> Result<bool, u64> {
        self.check_word(addr)?;
        let owner_tag = owner as u32;
        let owner = self.tag_owner(owner_tag).ok_or(result::INVALID_HANDLE)?;
        if self.word(addr) != owner_tag | WAIT_MASK {
            return Ok(false);
        }
        self.block(Wait::Lock { addr, owner, tag }, None);
        Ok(true)
    }

    /// svcArbitrateUnlock: hand the caller's mutex at `addr` to its best
    /// waiter, or unlock it.
    pub fn arbitrate_unlock(&mut self, addr: u64) -> Result<(), u64> {
        self.check_word(addr)?;
        self.unlock(addr);
        Ok(())
    }

    /// Release the running thread's mutex at `addr`: the best waiter gets
    /// it (with WAIT_MASK if others still wait, who now wait on the new
    /// owner), or the word becomes 0.
    fn unlock(&mut self, addr: u64) {
        let me = self.current_thread().id;
        let waiters = self.waiters(
            |w| matches!(w, Wait::Lock { addr: a, owner, .. } if *a == addr && *owner == me),
        );
        let Some((&next, rest)) = waiters.split_first() else {
            self.set_word(addr, 0);
            return;
        };
        let (new_owner, tag) = match self.sched.threads[next].wait {
            Some(Wait::Lock { tag, .. }) => (self.sched.threads[next].id, tag),
            _ => unreachable!("a lock waiter"),
        };
        for &i in rest {
            if let Some(Wait::Lock { owner, .. }) = &mut self.sched.threads[i].wait {
                *owner = new_owner;
            }
        }
        let mask = if rest.is_empty() { 0 } else { WAIT_MASK };
        self.set_word(addr, tag | mask);
        self.wake(next, result::SUCCESS, None);
    }

    /// svcWaitProcessWideKeyAtomic: release the mutex at `mutex` and wait
    /// on the condition variable at `key` until signalled or timed out.
    /// With timeout 0 the call times out at once, the mutex released. The
    /// condition variable word itself is neither read nor written: libnx
    /// never reads it.
    pub fn wait_process_wide_key(
        &mut self,
        mutex: u64,
        key: u64,
        tag: u32,
        timeout: i64,
    ) -> Result<(), u64> {
        self.check_word(mutex)?;
        if key % 4 != 0 {
            return Err(result::INVALID_ADDRESS);
        }
        self.unlock(mutex);
        if timeout == 0 {
            return Err(result::TIMED_OUT);
        }
        let until = deadline(self.cpu.retired, timeout);
        self.block(Wait::CondVar { key, mutex, tag }, until);
        Ok(())
    }

    /// svcSignalProcessWideKey: wake up to `count` waiters on `key`, best
    /// first (all of them when `count` is 0 or less, as libnx's -1 asks).
    /// Each takes its mutex back: at once if it is free, otherwise it
    /// waits for the mutex like svcArbitrateLock, with WAIT_MASK set.
    pub fn signal_process_wide_key(&mut self, key: u64, count: i32) {
        let mut woken = self.waiters(|w| matches!(w, Wait::CondVar { key: k, .. } if *k == key));
        if count > 0 {
            woken.truncate(count as usize);
        }
        for i in woken {
            let Some(Wait::CondVar { mutex, tag, .. }) = self.sched.threads[i].wait else {
                unreachable!("a condition variable waiter");
            };
            self.relock(i, mutex, tag);
        }
    }

    /// Thread `i`, signalled, takes the mutex at `addr` back with `tag`.
    /// A mutex word that became unusable, or names no thread, ends the
    /// wait with the check's result code (kestrel's choice).
    fn relock(&mut self, i: usize, addr: u64, tag: u32) {
        if let Err(code) = self.check_word(addr) {
            return self.wake(i, code, None);
        }
        let value = self.word(addr);
        if value == 0 {
            self.set_word(addr, tag);
            return self.wake(i, result::SUCCESS, None);
        }
        let Some(owner) = self.tag_owner(value & !WAIT_MASK) else {
            return self.wake(i, result::INVALID_HANDLE, None);
        };
        self.set_word(addr, value | WAIT_MASK);
        let t = &mut self.sched.threads[i];
        t.wait = Some(Wait::Lock { addr, owner, tag });
        t.state = State::Waiting { until: None };
    }

    /// svcWaitForAddress ("SVC", ArbitrationType): wait while the signed
    /// word at `addr` is less than `value` (WaitIfLessThan, 0) or equal to
    /// it (WaitIfEqual, 2). Returns whether the caller now waits. Kestrel's
    /// choices: InvalidState when the condition does not hold, TimedOut at
    /// once for timeout 0, InvalidEnumValue for an unknown type.
    /// DecrementAndWaitIfLessThan (1) could decrement always or only when
    /// waiting, so it stops the process.
    pub fn wait_for_address(
        &mut self,
        addr: u64,
        kind: u32,
        value: i32,
        timeout: i64,
    ) -> Result<bool, ArbitrationError> {
        self.check_word(addr)?;
        let current = self.word(addr) as i32;
        let wait = match kind {
            0 => current < value,
            2 => current == value,
            1 => return Err(ArbitrationError::Unsupported("DecrementAndWaitIfLessThan")),
            _ => return Err(result::INVALID_ENUM_VALUE.into()),
        };
        if !wait {
            return Err(result::INVALID_STATE.into());
        }
        if timeout == 0 {
            return Err(result::TIMED_OUT.into());
        }
        let until = deadline(self.cpu.retired, timeout);
        self.block(Wait::Address { addr }, until);
        Ok(true)
    }

    /// svcSignalToAddress ("SVC", SignalType): wake up to `count` threads
    /// waiting for `addr`, best first (all for `count` <= 0, as for
    /// svcSignalProcessWideKey). SignalAndIncrementIfEqual (1) first
    /// requires the word to equal `value` (InvalidState otherwise, kestrel's
    /// choice) and adds 1 to it. The type that modifies the word by the
    /// number of waiters (2) has no documented rule, so it stops.
    pub fn signal_to_address(
        &mut self,
        addr: u64,
        kind: u32,
        value: i32,
        count: i32,
    ) -> Result<(), ArbitrationError> {
        self.check_word(addr)?;
        match kind {
            0 => {}
            1 => {
                if self.word(addr) as i32 != value {
                    return Err(result::INVALID_STATE.into());
                }
                self.set_word(addr, value.wrapping_add(1) as u32);
            }
            2 => {
                return Err(ArbitrationError::Unsupported(
                    "SignalAndModifyBasedOnWaitingThreadCountIfEqual",
                ))
            }
            _ => return Err(result::INVALID_ENUM_VALUE.into()),
        }
        let mut woken = self.waiters(|w| matches!(w, Wait::Address { addr: a } if *a == addr));
        if count > 0 {
            woken.truncate(count as usize);
        }
        for i in woken {
            self.wake(i, result::SUCCESS, None);
        }
        Ok(())
    }

    /// svcCancelSynchronization: end the thread's svcWaitSynchronization
    /// with Cancelled, or make its next one return Cancelled at once.
    pub fn cancel_synchronization(&mut self, h: u64) -> Result<(), u64> {
        let i = self.thread_index(h)?;
        if matches!(self.sched.threads[i].wait, Some(Wait::Objects(_))) {
            self.wake(i, result::CANCELLED, None);
        } else {
            self.sched.threads[i].cancelled = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svc::{dispatch, handle, nr};
    use crate::testutil::{process, running, spawn, svc};
    use crate::Stop;
    use kestrel_core::Perm;

    /// Where tests put handle lists.
    const LIST: u64 = 0x10_0000;

    fn with_list(p: &mut Process, handles: &[u64]) {
        if p.mem.region(LIST).is_none() {
            p.mem.map(LIST, 0x1000, Perm::RW, "list").unwrap();
        }
        for (i, &h) in handles.iter().enumerate() {
            p.mem.write32(LIST + 4 * i as u64, h as u32).unwrap();
        }
    }

    /// svcWaitSynchronization on `handles`; returns (W0, X1) as left by the
    /// call (stale when the thread now waits).
    fn wait(p: &mut Process, handles: &[u64], timeout: i64) -> (u64, u64) {
        with_list(p, handles);
        svc(
            p,
            nr::WAIT_SYNCHRONIZATION,
            &[0, LIST, handles.len() as u64, timeout as u64],
        )
    }

    /// A new event as (writable, readable).
    fn event(p: &mut Process) -> (u64, u64) {
        p.cpu.x[1] = 0;
        assert_eq!(dispatch(p, nr::CREATE_EVENT), None);
        (p.cpu.x[1], p.cpu.x[2])
    }

    #[test]
    fn wait_checks_its_arguments() {
        let mut p = process();
        let (w, r) = event(&mut p);
        with_list(&mut p, &[r]);
        let call = |p: &mut Process, list: u64, n: i64, t: i64| {
            svc(p, nr::WAIT_SYNCHRONIZATION, &[0, list, n as u64, t as u64]).0
        };
        assert_eq!(call(&mut p, LIST, 0x41, 0), result::OUT_OF_RANGE);
        assert_eq!(call(&mut p, LIST, -1, 0), result::OUT_OF_RANGE);
        assert_eq!(call(&mut p, 0x20_0000, 1, 0), result::INVALID_POINTER);
        assert_eq!(call(&mut p, LIST, 1, 0), result::TIMED_OUT);
        let bad = result::INVALID_HANDLE;
        for h in [w, 0, handle::CURRENT_THREAD, handle::CURRENT_PROCESS] {
            assert_eq!(wait(&mut p, &[r, h], 0).0, bad, "handle {h:#x}");
        }
        // A signaled object answers at once with its index, and stays
        // signaled: waiting does not clear an event.
        svc(&mut p, nr::SIGNAL_EVENT, &[w]);
        let (w2, r2) = event(&mut p);
        assert_eq!(wait(&mut p, &[r2, r], 0), (0, 1));
        assert_eq!(wait(&mut p, &[r2, r], -1), (0, 1));
        svc(&mut p, nr::SIGNAL_EVENT, &[w2]);
        assert_eq!(wait(&mut p, &[r2, r], -1), (0, 0), "the first signaled");
    }

    #[test]
    fn signalling_an_event_wakes_its_waiters() {
        let mut p = process();
        let (_, idle) = event(&mut p);
        let (w, r) = event(&mut p);
        let t = spawn(&mut p, 0x2000, 0x2c, 1);
        svc(&mut p, nr::START_THREAD, &[t]);
        wait(&mut p, &[idle, r], -1);
        assert_eq!(running(&p), 0x2000, "the main thread waits");
        svc(&mut p, nr::SIGNAL_EVENT, &[w]);
        assert_eq!(running(&p), 0x2000, "the waiter is on another core");
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x1000);
        assert_eq!((p.cpu.x[0], p.cpu.x[1]), (0, 1));
    }

    #[test]
    fn a_thread_signals_its_waiters_when_it_exits() {
        let mut p = process();
        let t = spawn(&mut p, 0x2000, 0x2c, 1);
        svc(&mut p, nr::START_THREAD, &[t]);
        wait(&mut p, &[t], -1);
        assert_eq!(running(&p), 0x2000);
        svc(&mut p, nr::EXIT_THREAD, &[]);
        assert_eq!(running(&p), 0x1000);
        assert_eq!((p.cpu.x[0], p.cpu.x[1]), (0, 0));
        // An exited thread stays signaled.
        assert_eq!(wait(&mut p, &[t], 0), (0, 0));
    }

    #[test]
    fn a_wait_times_out_on_the_process_clock() {
        let mut p = process();
        let (_, r) = event(&mut p);
        let before = p.cpu.retired;
        wait(&mut p, &[r], 1_000_000);
        assert_eq!(p.cpu.retired, before + 19_200);
        // X1 keeps the list address: the index is not updated.
        assert_eq!((p.cpu.x[0], p.cpu.x[1]), (result::TIMED_OUT, LIST));
        // No handles: only the timeout ends the wait.
        wait(&mut p, &[], 1_000);
        assert_eq!(p.cpu.x[0], result::TIMED_OUT);
    }

    #[test]
    fn cancelling_ends_a_wait_or_the_next_one() {
        let mut p = process();
        let (_, r) = event(&mut p);
        let t = spawn(&mut p, 0x2000, 0x2c, 1);
        svc(&mut p, nr::START_THREAD, &[t]);
        // The main thread is not waiting: its next wait is cancelled, once.
        assert_eq!(
            svc(
                &mut p,
                nr::CANCEL_SYNCHRONIZATION,
                &[handle::CURRENT_THREAD]
            )
            .0,
            0
        );
        assert_eq!(wait(&mut p, &[r], 0).0, result::CANCELLED);
        assert_eq!(wait(&mut p, &[r], 0).0, result::TIMED_OUT);
        // A waiting thread is woken with Cancelled.
        wait(&mut p, &[r], -1);
        assert_eq!(running(&p), 0x2000);
        let main = p.main_thread as u64;
        assert_eq!(svc(&mut p, nr::CANCEL_SYNCHRONIZATION, &[main]).0, 0);
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x1000);
        assert_eq!(p.cpu.x[0], result::CANCELLED);
        assert_eq!(
            svc(&mut p, nr::CANCEL_SYNCHRONIZATION, &[0]).0,
            result::INVALID_HANDLE
        );
    }

    #[test]
    fn a_wait_keeps_its_objects_alive() {
        let mut p = process();
        let (w, r) = event(&mut p);
        let t = spawn(&mut p, 0x2000, 0x2c, 1);
        svc(&mut p, nr::START_THREAD, &[t]);
        wait(&mut p, &[r], -1);
        // The worker closes the readable handle the main thread waits on.
        assert_eq!(svc(&mut p, nr::CLOSE_HANDLE, &[r]).0, 0);
        svc(&mut p, nr::SIGNAL_EVENT, &[w]);
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!((running(&p), p.cpu.x[0]), (0x1000, 0));
        // With both handles closed and the wait over, the event is gone.
        let (_, id) = p.handles.get(w as u32).unwrap();
        svc(&mut p, nr::CLOSE_HANDLE, &[w]);
        let (_, r2) = event(&mut p);
        assert_eq!(
            p.handles.get(r2 as u32).map(|(_, i)| i),
            Some(id),
            "slot reused"
        );
    }

    /// Mutex and condition variable words.
    const M: u64 = 0x11_0000;
    const K: u64 = 0x11_0040;

    fn words(p: &mut Process) {
        p.mem.map(M & !0xfff, 0x1000, Perm::RW, "words").unwrap();
    }

    /// Two threads on core 1 at priorities 0x2c and 0x20, started; returns
    /// their handles. The main thread keeps running.
    fn two_workers(p: &mut Process) -> (u64, u64) {
        let a = spawn(p, 0x2000, 0x2c, 1);
        let b = spawn(p, 0x3000, 0x20, 1);
        svc(p, nr::START_THREAD, &[a]);
        svc(p, nr::START_THREAD, &[b]);
        (a, b)
    }

    /// What libnx's mutexLock does once it sees the mutex held by `owner`:
    /// set the wait bit and ask the kernel.
    fn lock_contended(p: &mut Process, owner: u64, me: u64) -> u64 {
        let v = p.mem.read32(M).unwrap();
        p.mem.write32(M, v | WAIT_MASK).unwrap();
        svc(p, nr::ARBITRATE_LOCK, &[owner, M, me]).0
    }

    #[test]
    fn unlocking_hands_the_mutex_to_the_best_waiter() {
        let mut p = process();
        words(&mut p);
        let main = p.main_thread as u64;
        let (a, b) = two_workers(&mut p);
        p.mem.write32(M, main as u32).unwrap(); // the main thread owns it
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x3000, "b, the better priority on core 1");
        lock_contended(&mut p, main, b);
        assert_eq!(running(&p), 0x1000, "b waits; the next core runs");
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x2000);
        lock_contended(&mut p, main, a);
        assert_eq!(running(&p), 0x1000, "both wait; the main thread runs");
        assert_eq!(p.mem.read32(M).unwrap(), main as u32 | WAIT_MASK);
        // Unlock: b (priority 0x20) gets it; a still waits, now on b.
        assert_eq!(svc(&mut p, nr::ARBITRATE_UNLOCK, &[M]).0, 0);
        assert_eq!(p.mem.read32(M).unwrap(), b as u32 | WAIT_MASK);
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!((running(&p), p.cpu.x[0]), (0x3000, 0));
        assert_eq!(svc(&mut p, nr::ARBITRATE_UNLOCK, &[M]).0, 0);
        assert_eq!(p.mem.read32(M).unwrap(), a as u32, "no waiters left");
        svc(&mut p, nr::EXIT_THREAD, &[]);
        assert_eq!(running(&p), 0x1000, "core 0 is next after core 1");
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!((running(&p), p.cpu.x[0]), (0x2000, 0));
        // Unlocking with no waiters clears the word.
        assert_eq!(svc(&mut p, nr::ARBITRATE_UNLOCK, &[M]).0, 0);
        assert_eq!(p.mem.read32(M).unwrap(), 0);
    }

    #[test]
    fn arbitrate_lock_returns_at_once_when_the_word_changed() {
        let mut p = process();
        words(&mut p);
        let main = p.main_thread as u64;
        let (a, _) = two_workers(&mut p);
        // The word does not read a | WAIT_MASK: nothing to wait for.
        p.mem.write32(M, a as u32).unwrap();
        assert_eq!(svc(&mut p, nr::ARBITRATE_LOCK, &[a, M, main]).0, 0);
        assert_eq!(running(&p), 0x1000);
        p.mem.write32(M, 0).unwrap();
        assert_eq!(svc(&mut p, nr::ARBITRATE_LOCK, &[a, M, main]).0, 0);
        assert_eq!(running(&p), 0x1000);
    }

    #[test]
    fn mutex_calls_check_their_arguments() {
        let mut p = process();
        words(&mut p);
        let main = p.main_thread as u64;
        let lock = |p: &mut Process, owner: u64, addr: u64| {
            svc(p, nr::ARBITRATE_LOCK, &[owner, addr, main]).0
        };
        assert_eq!(lock(&mut p, main, M + 2), result::INVALID_ADDRESS);
        assert_eq!(
            lock(&mut p, main, 0x50_0000),
            result::INVALID_CURRENT_MEMORY
        );
        assert_eq!(lock(&mut p, 0x1234, M), result::INVALID_HANDLE);
        assert_eq!(
            svc(&mut p, nr::ARBITRATE_UNLOCK, &[M + 1]).0,
            result::INVALID_ADDRESS
        );
        let wait = |p: &mut Process, mutex: u64, key: u64| {
            svc(p, nr::WAIT_PROCESS_WIDE_KEY_ATOMIC, &[mutex, key, main, 0]).0
        };
        assert_eq!(wait(&mut p, M + 1, K), result::INVALID_ADDRESS);
        assert_eq!(wait(&mut p, M, K + 2), result::INVALID_ADDRESS);
        assert_eq!(wait(&mut p, 0x50_0000, K), result::INVALID_CURRENT_MEMORY);
    }

    #[test]
    fn a_signalled_condition_variable_waiter_takes_its_mutex_back() {
        let mut p = process();
        words(&mut p);
        let main = p.main_thread as u64;
        let a = spawn(&mut p, 0x2000, 0x2c, 1);
        svc(&mut p, nr::START_THREAD, &[a]);
        p.mem.write32(M, main as u32).unwrap();
        // The wait releases the mutex.
        svc(
            &mut p,
            nr::WAIT_PROCESS_WIDE_KEY_ATOMIC,
            &[M, K, main, u64::MAX],
        );
        assert_eq!(running(&p), 0x2000);
        assert_eq!(p.mem.read32(M).unwrap(), 0);
        // a takes the mutex and signals: the main thread must wait for a.
        p.mem.write32(M, a as u32).unwrap();
        assert_eq!(svc(&mut p, nr::SIGNAL_PROCESS_WIDE_KEY, &[K, 1]).0, 0);
        assert_eq!(p.mem.read32(M).unwrap(), a as u32 | WAIT_MASK);
        assert_eq!(running(&p), 0x2000);
        // a unlocks: the main thread owns the mutex and returns success.
        svc(&mut p, nr::ARBITRATE_UNLOCK, &[M]);
        assert_eq!(p.mem.read32(M).unwrap(), main as u32);
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!((running(&p), p.cpu.x[0]), (0x1000, 0));
    }

    #[test]
    fn a_condition_variable_wait_can_time_out_without_the_mutex() {
        let mut p = process();
        words(&mut p);
        let main = p.main_thread as u64;
        p.mem.write32(M, main as u32).unwrap();
        let rc = svc(&mut p, nr::WAIT_PROCESS_WIDE_KEY_ATOMIC, &[M, K, main, 0]).0;
        assert_eq!(rc, result::TIMED_OUT);
        assert_eq!(p.mem.read32(M).unwrap(), 0, "released, not taken back");
        p.mem.write32(M, main as u32).unwrap();
        svc(
            &mut p,
            nr::WAIT_PROCESS_WIDE_KEY_ATOMIC,
            &[M, K, main, 1_000_000],
        );
        assert_eq!(p.cpu.x[0], result::TIMED_OUT);
        assert_eq!(p.mem.read32(M).unwrap(), 0);
    }

    #[test]
    fn signal_wakes_the_best_waiters_or_all_of_them() {
        let mut p = process();
        words(&mut p);
        let (a, b) = two_workers(&mut p);
        let (ma, mb) = (M + 0x100, M + 0x104);
        // b (the better priority) runs first on core 1 and waits on K with
        // its own mutex word; then a does.
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x3000);
        svc(
            &mut p,
            nr::WAIT_PROCESS_WIDE_KEY_ATOMIC,
            &[mb, K, b, u64::MAX],
        );
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x2000);
        svc(
            &mut p,
            nr::WAIT_PROCESS_WIDE_KEY_ATOMIC,
            &[ma, K, a, u64::MAX],
        );
        assert_eq!(running(&p), 0x1000);
        // One: b, the better priority, takes its free mutex back.
        svc(&mut p, nr::SIGNAL_PROCESS_WIDE_KEY, &[K, 1]);
        assert_eq!(p.mem.read32(mb).unwrap(), b as u32);
        assert_eq!(p.mem.read32(ma).unwrap(), 0, "a still waits");
        // All the rest: -1.
        svc(&mut p, nr::SIGNAL_PROCESS_WIDE_KEY, &[K, -1i64 as u64]);
        assert_eq!(p.mem.read32(ma).unwrap(), a as u32);
    }

    /// svcWaitForAddress on M; returns W0 as the call left it.
    fn wait_addr(p: &mut Process, kind: u64, value: i64, timeout: i64) -> u64 {
        svc(
            p,
            nr::WAIT_FOR_ADDRESS,
            &[M, kind, value as u64, timeout as u64],
        )
        .0
    }

    fn signal_addr(p: &mut Process, kind: u64, value: i64, count: i64) -> u64 {
        svc(
            p,
            nr::SIGNAL_TO_ADDRESS,
            &[M, kind, value as u64, count as u64],
        )
        .0
    }

    #[test]
    fn address_waits_check_their_condition() {
        let mut p = process();
        words(&mut p);
        p.mem.write32(M, -5i32 as u32).unwrap();
        // WaitIfLessThan compares signed values.
        assert_eq!(wait_addr(&mut p, 0, -5, 0), result::INVALID_STATE);
        assert_eq!(wait_addr(&mut p, 0, -4, 0), result::TIMED_OUT);
        assert_eq!(wait_addr(&mut p, 2, -4, 0), result::INVALID_STATE);
        assert_eq!(wait_addr(&mut p, 2, -5, 0), result::TIMED_OUT);
        assert_eq!(wait_addr(&mut p, 3, 0, 0), result::INVALID_ENUM_VALUE);
        assert_eq!(signal_addr(&mut p, 3, 0, 1), result::INVALID_ENUM_VALUE);
        let at = |p: &mut Process, addr: u64| svc(p, nr::WAIT_FOR_ADDRESS, &[addr, 2, 0, 0]).0;
        assert_eq!(at(&mut p, M + 2), result::INVALID_ADDRESS);
        assert_eq!(at(&mut p, 0x50_0000), result::INVALID_CURRENT_MEMORY);
        // A timed wait ends on the clock.
        wait_addr(&mut p, 2, -5, 1_000_000);
        assert_eq!(p.cpu.x[0], result::TIMED_OUT);
    }

    #[test]
    fn signalling_an_address_wakes_its_best_waiters() {
        let mut p = process();
        words(&mut p);
        let (_, _) = two_workers(&mut p);
        // b, then a, wait while the word is 0.
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x3000);
        wait_addr(&mut p, 2, 0, -1);
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x2000);
        wait_addr(&mut p, 2, 0, -1);
        assert_eq!(running(&p), 0x1000);
        // SignalAndIncrementIfEqual: only when the word equals the value.
        assert_eq!(signal_addr(&mut p, 1, 7, 1), result::INVALID_STATE);
        assert_eq!(signal_addr(&mut p, 1, 0, 1), 0);
        assert_eq!(p.mem.read32(M).unwrap(), 1);
        // b, the better priority, was woken; a still waits.
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!((running(&p), p.cpu.x[0]), (0x3000, 0));
        svc(&mut p, nr::EXIT_THREAD, &[]);
        assert_eq!(running(&p), 0x1000);
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!(running(&p), 0x1000, "a still waits");
        // Signal with count 0 wakes the rest, word unchanged.
        assert_eq!(signal_addr(&mut p, 0, 0, 0), 0);
        assert_eq!(p.mem.read32(M).unwrap(), 1);
        svc(&mut p, nr::SLEEP_THREAD, &[0]);
        assert_eq!((running(&p), p.cpu.x[0]), (0x2000, 0));
    }

    #[test]
    fn undocumented_arbitration_types_stop_the_process() {
        let mut p = process();
        words(&mut p);
        for (n, kind, what) in [
            (nr::WAIT_FOR_ADDRESS, 1, "DecrementAndWaitIfLessThan"),
            (
                nr::SIGNAL_TO_ADDRESS,
                2,
                "SignalAndModifyBasedOnWaitingThreadCountIfEqual",
            ),
        ] {
            p.cpu.x[..4].copy_from_slice(&[M, kind, 0, 1]);
            match dispatch(&mut p, n) {
                Some(Stop::Error(e)) => assert!(e.contains(what), "{e}"),
                other => panic!("expected a stop, got {other:?}"),
            }
        }
    }

    #[test]
    fn waiting_forever_with_nothing_to_wake_is_a_deadlock() {
        let mut p = process();
        let (_, r) = event(&mut p);
        with_list(&mut p, &[r]);
        p.cpu.x[1..4].copy_from_slice(&[LIST, 1, u64::MAX]);
        assert_eq!(dispatch(&mut p, nr::WAIT_SYNCHRONIZATION), None);
        let rotate = std::mem::take(&mut p.sched.rotate);
        match p.schedule(rotate) {
            Some(Stop::Error(e)) => {
                assert!(e.contains("deadlock"), "{e}");
                assert!(e.contains("thread 0x100 waits on 1 object(s)"), "{e}");
            }
            other => panic!("expected a stop, got {other:?}"),
        }
    }
}
