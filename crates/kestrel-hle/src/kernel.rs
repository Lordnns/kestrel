//! Kernel objects and the process's handle table.
//!
//! Guest code names kernel objects by 32-bit handles (Switchbrew, "SVC": the
//! `Handle<T>` argument types, svcCloseHandle). An object lives while a
//! handle names it; svcCloseHandle on the last handle releases it.
//!
//! How handle values are formed is not documented, so the format is
//! kestrel's: bits 0-14 index a slot, bits 15-29 hold a counter that
//! advances on every new handle, so a closed handle does not name a later
//! object in the same slot. A handle is never 0 and never one of the
//! pseudo-handles (`svc::handle`), which have the top bits set.

use crate::svc::{handle, result};
use crate::Process;
use kestrel_core::Perm;

/// Handle slots per process, kestrel's choice; a full table gives
/// OutOfHandles.
pub const MAX_HANDLES: usize = 1024;
/// svcGetProcessId for the one process, kestrel's choice.
pub const PROCESS_ID: u64 = 0x80;
/// The main thread's ID; later threads count up from it. Kestrel's choice.
pub const MAIN_THREAD_ID: u64 = 0x100;

const INDEX_BITS: u32 = 15;
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;
const COUNTER_MASK: u32 = (1 << 15) - 1;

/// What a handle names; one object can be named in several ways (an event
/// has a readable and a writable side).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Thread,
    ReadableEvent,
    WritableEvent,
    TransferMemory,
}

/// A kernel object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    Thread {
        id: u64,
    },
    /// One event, named by a readable and a writable handle.
    Event {
        signaled: bool,
    },
    /// Memory lent by svcCreateTransferMemory, with each block's
    /// permission before it was lent as (base, size, permission).
    TransferMemory {
        restore: Vec<(u64, u64, Perm)>,
    },
}

/// Index of an object in the table's object store.
pub type ObjectId = usize;

#[derive(Clone, Copy, Debug)]
struct Entry {
    counter: u32,
    kind: Kind,
    object: ObjectId,
}

/// The handle table and the objects its handles name.
#[derive(Default)]
pub struct HandleTable {
    slots: Vec<Option<Entry>>,
    counter: u32,
    /// Each object with the number of handles naming it.
    objects: Vec<Option<(usize, Object)>>,
}

impl HandleTable {
    /// Free handle slots.
    pub fn free(&self) -> usize {
        MAX_HANDLES - self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Store `object` and open one handle to it for each of `kinds`.
    /// Fails with OutOfHandles, storing nothing, if the handles do not fit.
    pub fn insert(&mut self, object: Object, kinds: &[Kind]) -> Result<Vec<u32>, u64> {
        if self.free() < kinds.len() {
            return Err(result::OUT_OF_HANDLES);
        }
        let id = match self.objects.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.objects.push(None);
                self.objects.len() - 1
            }
        };
        self.objects[id] = Some((kinds.len(), object));
        Ok(kinds.iter().map(|&kind| self.open(kind, id)).collect())
    }

    /// Put a handle for `object` in a free slot; the caller has checked
    /// there is one and counted the reference.
    fn open(&mut self, kind: Kind, object: ObjectId) -> u32 {
        let index = match self.slots.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        self.counter = self.counter % COUNTER_MASK + 1;
        self.slots[index] = Some(Entry {
            counter: self.counter,
            kind,
            object,
        });
        self.counter << INDEX_BITS | index as u32
    }

    fn entry(&self, h: u32) -> Option<Entry> {
        let index = (h & INDEX_MASK) as usize;
        let counter = h >> INDEX_BITS;
        let e = (*self.slots.get(index)?)?;
        (e.counter == counter).then_some(e)
    }

    /// What `h` names, if it is an open handle.
    pub fn get(&self, h: u32) -> Option<(Kind, ObjectId)> {
        self.entry(h).map(|e| (e.kind, e.object))
    }

    pub fn object(&self, id: ObjectId) -> &Object {
        &self.objects[id].as_ref().expect("live object").1
    }

    pub fn object_mut(&mut self, id: ObjectId) -> &mut Object {
        &mut self.objects[id].as_mut().expect("live object").1
    }

    /// Close `h`. Returns the object if that was its last handle.
    pub fn close(&mut self, h: u32) -> Result<Option<Object>, u64> {
        let e = self.entry(h).ok_or(result::INVALID_HANDLE)?;
        self.slots[(h & INDEX_MASK) as usize] = None;
        let slot = &mut self.objects[e.object];
        let (refs, _) = slot.as_mut().expect("live object");
        *refs -= 1;
        Ok(if *refs == 0 {
            slot.take().map(|(_, o)| o)
        } else {
            None
        })
    }
}

/// The low 32 bits of a handle register; the rest are ignored.
fn handle_arg(x: u64) -> u32 {
    x as u32
}

impl Process {
    /// svcCloseHandle (Switchbrew, "SVC"). Closing a pseudo-handle is
    /// refused with InvalidHandle (kestrel's choice; it names no slot).
    pub fn close_handle(&mut self, h: u64) -> Result<(), u64> {
        match self.handles.close(handle_arg(h))? {
            Some(Object::TransferMemory { restore }) => self.release_transfer_memory(&restore),
            Some(Object::Thread { id }) => self.thread_released(id),
            _ => {}
        }
        Ok(())
    }

    /// svcGetProcessId: only the current-process pseudo-handle names a
    /// process, as no process handles exist yet.
    pub fn process_id(&self, h: u64) -> Result<u64, u64> {
        if handle_arg(h) as u64 == handle::CURRENT_PROCESS {
            Ok(PROCESS_ID)
        } else {
            Err(result::INVALID_HANDLE)
        }
    }
}

/// Events (Switchbrew, "SVC": CreateEvent, SignalEvent, ClearEvent,
/// ResetSignal). Nothing waits on them until the synchronisation calls
/// exist, so signalling only sets the state.
impl Process {
    /// The signaled state of the event `h` names, if `h` is one of `kinds`.
    fn event(&mut self, h: u64, kinds: &[Kind]) -> Result<&mut bool, u64> {
        match self.handles.get(handle_arg(h)) {
            Some((kind, id)) if kinds.contains(&kind) => match self.handles.object_mut(id) {
                Object::Event { signaled } => Ok(signaled),
                o => unreachable!("an event handle names {o:?}"),
            },
            _ => Err(result::INVALID_HANDLE),
        }
    }

    /// svcCreateEvent: a new, unsignaled event as (writable, readable).
    pub fn create_event(&mut self) -> Result<(u32, u32), u64> {
        let hs = self.handles.insert(
            Object::Event { signaled: false },
            &[Kind::WritableEvent, Kind::ReadableEvent],
        )?;
        Ok((hs[0], hs[1]))
    }

    /// svcSignalEvent: only the writable side signals.
    pub fn signal_event(&mut self, h: u64) -> Result<(), u64> {
        *self.event(h, &[Kind::WritableEvent])? = true;
        Ok(())
    }

    /// svcClearEvent: either side clears.
    pub fn clear_event(&mut self, h: u64) -> Result<(), u64> {
        *self.event(h, &[Kind::WritableEvent, Kind::ReadableEvent])? = false;
        Ok(())
    }

    /// svcResetSignal on an event: the readable side, and only when
    /// signaled; otherwise InvalidState (0xfa01).
    pub fn reset_signal(&mut self, h: u64) -> Result<(), u64> {
        let signaled = self.event(h, &[Kind::ReadableEvent])?;
        if !*signaled {
            return Err(result::INVALID_STATE);
        }
        *signaled = false;
        Ok(())
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
            entry: 0,
            image_start: 0,
            image_end: 0,
        };
        Process::new(Memory::new(), &image).unwrap()
    }

    /// Call `n` with X0 and X1; returns (W0, X1).
    fn call(p: &mut Process, n: u16, x0: u64, x1: u64) -> (u64, u64) {
        (p.cpu.x[0], p.cpu.x[1]) = (x0, x1);
        assert_eq!(dispatch(p, n), None);
        (p.cpu.x[0], p.cpu.x[1])
    }

    #[test]
    fn the_main_thread_handle_and_the_pseudo_handles_name_the_caller() {
        let mut p = process();
        let main = p.main_thread as u64;
        let tid = MAIN_THREAD_ID;
        assert_eq!(call(&mut p, nr::GET_THREAD_ID, 0, main), (0, tid));
        assert_eq!(
            call(&mut p, nr::GET_THREAD_ID, 0, handle::CURRENT_THREAD),
            (0, tid)
        );
        let pid = call(&mut p, nr::GET_PROCESS_ID, 0, handle::CURRENT_PROCESS);
        assert_eq!(pid, (0, PROCESS_ID));
        // Only the low 32 bits of a handle register count.
        assert_eq!(call(&mut p, nr::GET_THREAD_ID, 0, 1 << 40 | main).0, 0);
        // Wrong kinds and closed handles.
        let bad = result::INVALID_HANDLE;
        assert_eq!(call(&mut p, nr::GET_PROCESS_ID, 0, main).0, bad);
        assert_eq!(
            call(&mut p, nr::GET_THREAD_ID, 0, handle::CURRENT_PROCESS).0,
            bad
        );
        assert_eq!(
            call(&mut p, nr::CLOSE_HANDLE, handle::CURRENT_THREAD, 0).0,
            bad
        );
        assert_eq!(call(&mut p, nr::CLOSE_HANDLE, 0, 0).0, bad);
        assert_eq!(call(&mut p, nr::CLOSE_HANDLE, main, 0).0, 0);
        assert_eq!(call(&mut p, nr::CLOSE_HANDLE, main, 0).0, bad);
        assert_eq!(call(&mut p, nr::GET_THREAD_ID, 0, main).0, bad);
        // The thread still runs and still has its ID.
        assert_eq!(
            call(&mut p, nr::GET_THREAD_ID, 0, handle::CURRENT_THREAD),
            (0, tid)
        );
    }

    fn thread(id: u64) -> Object {
        Object::Thread { id }
    }

    #[test]
    fn an_event_is_signaled_by_its_writable_side_and_reset_by_its_readable_side() {
        let mut p = process();
        p.cpu.x[1] = 0;
        assert_eq!(dispatch(&mut p, nr::CREATE_EVENT), None);
        let (w, r) = (p.cpu.x[1], p.cpu.x[2]);
        assert_eq!(p.cpu.x[0], 0);
        assert_ne!(w, r);
        let (bad, unsignaled) = (result::INVALID_HANDLE, result::INVALID_STATE);
        assert_eq!(call(&mut p, nr::RESET_SIGNAL, r, 0).0, unsignaled);
        assert_eq!(call(&mut p, nr::SIGNAL_EVENT, r, 0).0, bad);
        assert_eq!(call(&mut p, nr::RESET_SIGNAL, w, 0).0, bad);
        assert_eq!(call(&mut p, nr::SIGNAL_EVENT, w, 0).0, 0);
        assert_eq!(call(&mut p, nr::SIGNAL_EVENT, w, 0).0, 0);
        assert_eq!(call(&mut p, nr::RESET_SIGNAL, r, 0).0, 0);
        assert_eq!(call(&mut p, nr::RESET_SIGNAL, r, 0).0, unsignaled);
        // Clearing works from either side and needs no signal.
        for h in [w, r, w] {
            assert_eq!(call(&mut p, nr::SIGNAL_EVENT, w, 0).0, 0);
            assert_eq!(call(&mut p, nr::CLEAR_EVENT, h, 0).0, 0);
            assert_eq!(call(&mut p, nr::RESET_SIGNAL, r, 0).0, unsignaled);
        }
        assert_eq!(call(&mut p, nr::CLEAR_EVENT, r, 0).0, 0);
        // Thread handles are not events.
        let main = p.main_thread as u64;
        assert_eq!(call(&mut p, nr::CLEAR_EVENT, main, 0).0, bad);
        // Each side outlives the other.
        assert_eq!(call(&mut p, nr::CLOSE_HANDLE, w, 0).0, 0);
        assert_eq!(call(&mut p, nr::SIGNAL_EVENT, w, 0).0, bad);
        assert_eq!(call(&mut p, nr::CLEAR_EVENT, r, 0).0, 0);
        assert_eq!(call(&mut p, nr::CLOSE_HANDLE, r, 0).0, 0);
        assert_eq!(call(&mut p, nr::CLEAR_EVENT, r, 0).0, bad);
    }

    #[test]
    fn resetting_the_process_signal_stops_as_unsupported() {
        let mut p = process();
        p.cpu.x[0] = handle::CURRENT_PROCESS;
        match dispatch(&mut p, nr::RESET_SIGNAL) {
            Some(crate::Stop::Error(e)) => assert!(e.contains("svcResetSignal"), "{e}"),
            other => panic!("expected a stop, got {other:?}"),
        }
    }

    #[test]
    fn handles_are_never_zero_or_pseudo_handles() {
        let mut t = HandleTable::default();
        for i in 0..100 {
            let h = t.insert(thread(i), &[Kind::Thread]).unwrap()[0];
            assert_ne!(h, 0);
            assert_eq!(h >> 30, 0, "{h:#x} could be a pseudo-handle");
            assert_eq!(t.get(h).map(|(k, _)| k), Some(Kind::Thread));
        }
    }

    #[test]
    fn a_closed_handle_does_not_name_the_next_object_in_its_slot() {
        let mut t = HandleTable::default();
        let a = t.insert(thread(1), &[Kind::Thread]).unwrap()[0];
        assert_eq!(t.close(a), Ok(Some(thread(1))));
        let b = t.insert(thread(2), &[Kind::Thread]).unwrap()[0];
        assert_eq!(a & INDEX_MASK, b & INDEX_MASK, "slot reused");
        assert_ne!(a, b);
        assert_eq!(t.get(a), None);
        assert_eq!(t.close(a), Err(result::INVALID_HANDLE));
    }

    #[test]
    fn an_object_lives_until_its_last_handle_closes() {
        let mut t = HandleTable::default();
        let hs = t.insert(thread(7), &[Kind::Thread, Kind::Thread]).unwrap();
        assert_eq!(t.close(hs[0]), Ok(None));
        assert_eq!(t.close(hs[0]), Err(result::INVALID_HANDLE));
        assert_eq!(t.close(hs[1]), Ok(Some(thread(7))));
    }

    #[test]
    fn a_full_table_gives_out_of_handles_and_stores_nothing() {
        let mut t = HandleTable::default();
        for i in 0..MAX_HANDLES - 1 {
            t.insert(thread(i as u64), &[Kind::Thread]).unwrap();
        }
        assert_eq!(
            t.insert(thread(0), &[Kind::Thread, Kind::Thread]),
            Err(result::OUT_OF_HANDLES)
        );
        assert_eq!(t.free(), 1);
        assert!(t.insert(thread(0), &[Kind::Thread]).is_ok());
        assert_eq!(
            t.insert(thread(0), &[Kind::Thread]),
            Err(result::OUT_OF_HANDLES)
        );
    }
}
