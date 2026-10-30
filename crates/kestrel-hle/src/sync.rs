//! Synchronisation: waiting on kernel objects, cancelling a wait.
//!
//! From Switchbrew's "SVC" page (WaitSynchronization, its object types and
//! result codes, CancelSynchronization). A readable event is signaled while
//! its event is; a thread is signaled once it has exited. Waiting holds a
//! reference to every object, so closing their handles meanwhile does not
//! end the wait.

use crate::kernel::{Kind, Object, ObjectId};
use crate::svc::result;
use crate::thread::{State, Wait};
use crate::Process;

/// svcWaitSynchronization takes at most this many handles ("SVC").
pub const MAX_WAIT_OBJECTS: i32 = 0x40;

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
