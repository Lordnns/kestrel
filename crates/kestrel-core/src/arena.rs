//! The guest memory arena: one range of host memory holding every byte of
//! guest memory, shared by every backend and every host thread running a
//! guest core (`PLAN.md` phase 3; `docs/ARCHITECTURE.md`, "Memory").
//!
//! The arena is the guest's physical memory. It is handed out in 4 KiB
//! frames, the guest's page size, and [`crate::Memory`] maps guest virtual
//! pages to frames. Keeping physical and virtual apart is what lets:
//!
//! - a hypervisor map the arena into a VM as a whole, once, with the
//!   guest's own 4 KiB page tables inside the VM pointing into it, whatever
//!   the host's page size (16 KiB on Apple Silicon);
//! - two guest addresses share one frame, as shared memory does;
//! - any number of host threads read and write it at once: every access
//!   goes through host atomics, so racing guest cores are defined
//!   behaviour on the host, as they are on the console.
//!
//! The host range is reserved whole (address space only) and committed in
//! [`ARENA_ALIGN`] steps as frames are first handed out, so a large arena
//! costs only what the guest uses.

use crate::host::{self, Reservation};
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Mutex;

/// The guest's page size, and the arena's unit of allocation.
pub const FRAME_SIZE: u64 = 0x1000;

/// Alignment of the arena's start and size, and the step it is committed
/// in: 2 MiB, a multiple of every host page size (4, 16 and 64 KiB), and
/// the block size a hypervisor's stage-2 tables can map in one entry.
pub const ARENA_ALIGN: u64 = 2 << 20;

/// The arena [`crate::Memory::new`] reserves: 16 GiB of host address
/// space, room for the Switch 2's 12 GiB of memory (kestrel's choice).
/// Only what the guest uses is committed.
pub const DEFAULT_ARENA_SIZE: u64 = 16 << 30;

/// Host memory holding guest physical memory. See the module comment.
pub struct Arena {
    host: Reservation,
    /// The first byte of the arena: `host.base()` rounded up to ARENA_ALIGN.
    base: *mut u8,
    size: u64,
    frames: Mutex<Frames>,
}

// Every access to the arena's bytes is atomic (see `load` and `store`),
// and the frame lists are behind a lock.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

/// Which frames are in use. Frames at and above `top` were never handed
/// out and still read as zero; freed frames below it are in `free`.
#[derive(Default)]
struct Frames {
    /// Free runs as offset -> length, never adjacent (they are merged).
    free: BTreeMap<u64, u64>,
    top: u64,
    /// Bytes committed from the start of the arena.
    committed: u64,
}

/// The arena has no room for an allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArenaFull;

impl Arena {
    /// Reserve an arena of `size` bytes, rounded up to [`ARENA_ALIGN`].
    pub fn new(size: u64) -> io::Result<Self> {
        let page = host::page_size() as u64;
        if !page.is_power_of_two() || ARENA_ALIGN % page != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("host page size {page:#x} does not divide the arena's alignment"),
            ));
        }
        let too_large = || io::Error::new(io::ErrorKind::InvalidInput, "arena too large");
        let size = size
            .max(1)
            .checked_next_multiple_of(ARENA_ALIGN)
            .ok_or_else(too_large)?;
        // One extra ARENA_ALIGN of address space so the start can be aligned.
        let total = size
            .checked_add(ARENA_ALIGN)
            .and_then(|t| usize::try_from(t).ok())
            .ok_or_else(too_large)?;
        let host = Reservation::new(total)?;
        let skip = (host.base() as u64).wrapping_neg() % ARENA_ALIGN;
        // SAFETY: skip < ARENA_ALIGN, inside the reservation.
        let base = unsafe { host.base().add(skip as usize) };
        Ok(Self {
            host,
            base,
            size,
            frames: Mutex::new(Frames::default()),
        })
    }

    /// The host address of the arena's first byte, aligned to
    /// [`ARENA_ALIGN`]. A hypervisor maps `[host_base, host_base + size)`
    /// into the VM; frame offsets are offsets from here.
    pub fn host_base(&self) -> *mut u8 {
        self.base
    }

    /// The arena's size in bytes, a multiple of [`ARENA_ALIGN`].
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Bytes committed from the start of the arena: every frame ever
    /// handed out lies below this.
    pub fn committed(&self) -> u64 {
        self.lock().committed
    }

    /// Bytes in frames handed out and not yet freed.
    pub fn allocated(&self) -> u64 {
        let f = self.lock();
        f.top - f.free.values().sum::<u64>()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Frames> {
        // The frame lists stay consistent even if a holder panicked.
        self.frames.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hand out `len` bytes of zeroed frames (`len` a multiple of
    /// [`FRAME_SIZE`]), as runs of (offset, length) in address order of
    /// the guest pages they will back. One run when possible.
    pub(crate) fn alloc(&self, len: u64) -> Result<Vec<(u64, u64)>, ArenaFull> {
        debug_assert!(len > 0 && len % FRAME_SIZE == 0);
        let mut f = self.lock();
        // A free run that fits whole, else fresh frames from the top, else
        // free runs and the top together.
        let fit = f
            .free
            .iter()
            .find(|&(_, &n)| n >= len)
            .map(|(&o, &n)| (o, n));
        if let Some((off, n)) = fit {
            f.free.remove(&off);
            if n > len {
                f.free.insert(off + len, n - len);
            }
            drop(f);
            self.zero(off, len);
            return Ok(vec![(off, len)]);
        }
        let free: u64 = f.free.values().sum();
        if len > free + (self.size - f.top) {
            return Err(ArenaFull);
        }
        // Every free run is needed, and the rest comes from the top.
        let mut runs = Vec::new();
        let mut left = len;
        while left > self.size - f.top {
            let (off, n) = f.free.pop_first().expect("enough free frames");
            let take = n.min(left - (self.size - f.top));
            if n > take {
                f.free.insert(off + take, n - take);
            }
            runs.push((off, take));
            left -= take;
        }
        let reused = runs.len();
        if left > 0 {
            let top = f.top;
            if let Err(e) = self.commit_to(&mut f, top + left) {
                for &(off, n) in &runs {
                    f.free.insert(off, n);
                }
                return Err(e);
            }
            f.top += left;
            runs.push((top, left));
        }
        drop(f);
        // Fresh frames from the top are zero already.
        for &(off, n) in &runs[..reused] {
            self.zero(off, n);
        }
        Ok(runs)
    }

    /// Commit the arena up to at least `end`.
    fn commit_to(&self, f: &mut Frames, end: u64) -> Result<(), ArenaFull> {
        if end <= f.committed {
            return Ok(());
        }
        let new = end.next_multiple_of(ARENA_ALIGN).min(self.size);
        let skip = self.base as usize - self.host.base() as usize;
        self.host
            .commit(skip + f.committed as usize, (new - f.committed) as usize)
            .map_err(|_| ArenaFull)?;
        f.committed = new;
        Ok(())
    }

    /// Give `len` bytes of frames at `off` back.
    pub(crate) fn free(&self, off: u64, len: u64) {
        let mut f = self.lock();
        debug_assert!(off + len <= f.top);
        let (mut off, mut len) = (off, len);
        if let Some((&prev, &n)) = f.free.range(..off).next_back() {
            debug_assert!(prev + n <= off, "frame {off:#x} freed twice");
            if prev + n == off {
                f.free.remove(&prev);
                off = prev;
                len += n;
            }
        }
        if let Some(n) = f.free.remove(&(off + len)) {
            len += n;
        }
        f.free.insert(off, len);
    }

    /// The host address of arena offset `off`.
    #[inline]
    pub(crate) fn ptr(&self, off: u64) -> *mut u8 {
        debug_assert!(off < self.size);
        // SAFETY: off is inside the arena.
        unsafe { self.base.add(off as usize) }
    }

    fn zero(&self, off: u64, len: u64) {
        // SAFETY: the frames are committed and inside the arena.
        unsafe { fill_zero(self.ptr(off), len as usize) }
    }
}

// Byte copies in and out of the arena. Guest cores on other host threads
// may touch the same bytes at the same time, so every access is a relaxed
// atomic: 8 bytes at a time where aligned, single bytes at the edges.
// Relaxed atomic loads and stores are plain loads and stores on x86_64 and
// aarch64, so this costs nothing over a memcpy's inner loop but its width.

/// Copy `out.len()` bytes from arena memory at `src`.
///
/// # Safety
/// `[src, src + out.len())` must be committed arena memory.
pub(crate) unsafe fn load(src: *const u8, out: &mut [u8]) {
    let mut i = 0;
    let n = out.len();
    while i < n && (src as usize + i) % 8 != 0 {
        out[i] = AtomicU8::from_ptr(src.add(i).cast_mut()).load(Relaxed);
        i += 1;
    }
    while i + 8 <= n {
        let w = AtomicU64::from_ptr(src.add(i).cast_mut().cast()).load(Relaxed);
        out[i..i + 8].copy_from_slice(&w.to_ne_bytes());
        i += 8;
    }
    while i < n {
        out[i] = AtomicU8::from_ptr(src.add(i).cast_mut()).load(Relaxed);
        i += 1;
    }
}

/// Copy `data` into arena memory at `dst`.
///
/// # Safety
/// `[dst, dst + data.len())` must be committed arena memory.
pub(crate) unsafe fn store(dst: *mut u8, data: &[u8]) {
    let mut i = 0;
    let n = data.len();
    while i < n && (dst as usize + i) % 8 != 0 {
        AtomicU8::from_ptr(dst.add(i)).store(data[i], Relaxed);
        i += 1;
    }
    while i + 8 <= n {
        let w = u64::from_ne_bytes(data[i..i + 8].try_into().unwrap());
        AtomicU64::from_ptr(dst.add(i).cast()).store(w, Relaxed);
        i += 8;
    }
    while i < n {
        AtomicU8::from_ptr(dst.add(i)).store(data[i], Relaxed);
        i += 1;
    }
}

/// Zero `len` bytes of arena memory at `dst`, which is 8-byte aligned, as
/// `len` is.
///
/// # Safety
/// `[dst, dst + len)` must be committed arena memory.
unsafe fn fill_zero(dst: *mut u8, len: usize) {
    debug_assert!(dst as usize % 8 == 0 && len % 8 == 0);
    for i in (0..len).step_by(8) {
        AtomicU64::from_ptr(dst.add(i).cast()).store(0, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB2: u64 = ARENA_ALIGN;

    fn read(a: &Arena, off: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0xee; len];
        unsafe { load(a.ptr(off), &mut v) };
        v
    }

    #[test]
    fn the_arena_is_aligned_and_committed_as_used() {
        let a = Arena::new(64 << 20).unwrap();
        assert_eq!(a.host_base() as u64 % ARENA_ALIGN, 0);
        assert_eq!(a.size(), 64 << 20);
        assert_eq!(a.committed(), 0);
        let r = a.alloc(3 * FRAME_SIZE).unwrap();
        assert_eq!(r, [(0, 3 * FRAME_SIZE)]);
        assert_eq!(a.committed(), MB2);
        let r = a.alloc(MB2).unwrap();
        assert_eq!(r, [(3 * FRAME_SIZE, MB2)]);
        assert_eq!(a.committed(), 2 * MB2);
        assert_eq!(a.allocated(), MB2 + 3 * FRAME_SIZE);
        // Odd sizes round up to whole 2 MiB steps.
        assert_eq!(Arena::new(1).unwrap().size(), MB2);
    }

    #[test]
    fn freed_frames_merge_and_come_back_zeroed() {
        let a = Arena::new(MB2).unwrap();
        let x = a.alloc(2 * FRAME_SIZE).unwrap()[0].0;
        let y = a.alloc(FRAME_SIZE).unwrap()[0].0;
        let z = a.alloc(FRAME_SIZE).unwrap()[0].0;
        unsafe { store(a.ptr(y), &[1, 2, 3]) };
        unsafe { store(a.ptr(x + 5), &[9; 20]) };
        a.free(y, FRAME_SIZE);
        a.free(x, 2 * FRAME_SIZE);
        assert_eq!(a.allocated(), FRAME_SIZE);
        // x and y merged into one run of three frames, which fits whole.
        assert_eq!(a.alloc(3 * FRAME_SIZE).unwrap(), [(x, 3 * FRAME_SIZE)]);
        assert!(read(&a, x, 3 * FRAME_SIZE as usize).iter().all(|&b| b == 0));
        let _ = z;
    }

    #[test]
    fn a_full_arena_hands_out_scattered_runs_then_refuses() {
        let a = Arena::new(MB2).unwrap();
        let frames = MB2 / FRAME_SIZE;
        let all = a.alloc(MB2).unwrap();
        assert_eq!(all, [(0, MB2)]);
        assert_eq!(a.alloc(FRAME_SIZE), Err(ArenaFull));
        // Free every other frame of the first eight, dirtying them first.
        for i in (0..8).step_by(2) {
            unsafe { store(a.ptr(i * FRAME_SIZE), &[0xff; 16]) };
            a.free(i * FRAME_SIZE, FRAME_SIZE);
        }
        assert_eq!(a.allocated(), (frames - 4) * FRAME_SIZE);
        assert_eq!(a.alloc(5 * FRAME_SIZE), Err(ArenaFull));
        let runs = a.alloc(3 * FRAME_SIZE).unwrap();
        assert_eq!(
            runs,
            [
                (0, FRAME_SIZE),
                (2 * FRAME_SIZE, FRAME_SIZE),
                (4 * FRAME_SIZE, FRAME_SIZE)
            ]
        );
        for (off, n) in runs {
            assert!(read(&a, off, n as usize).iter().all(|&b| b == 0));
        }
        assert_eq!(a.alloc(FRAME_SIZE).unwrap(), [(6 * FRAME_SIZE, FRAME_SIZE)]);
        assert_eq!(a.alloc(FRAME_SIZE), Err(ArenaFull));
    }

    #[test]
    fn copies_keep_every_byte_at_any_alignment() {
        let a = Arena::new(MB2).unwrap();
        a.alloc(FRAME_SIZE).unwrap();
        let data: Vec<u8> = (0..61).map(|i| i as u8 ^ 0x5a).collect();
        for start in 0..9 {
            unsafe { store(a.ptr(start), &data) };
            assert_eq!(read(&a, start, data.len()), data);
            // The byte after was never written.
            assert_eq!(read(&a, start + data.len() as u64, 1), [0]);
        }
    }
}
