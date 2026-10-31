//! Guest virtual address space.
//!
//! Guest pages live in an [`Arena`], one range of host memory shared by
//! every backend (docs/ARCHITECTURE.md, "Memory"). A two-level page table
//! maps each 4 KiB guest page to a frame of the arena with its permission,
//! so an access is two loads and a host atomic. Next to it, the OS layer's
//! view: the regions, each with its permission, memory type and name, as
//! the memory calls create, split and change them.

use crate::arena::{self, Arena, DEFAULT_ARENA_SIZE};
use std::alloc::{alloc_zeroed, handle_alloc_error, Layout};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicPtr, AtomicU16, AtomicU32, AtomicU64, AtomicU8};
use std::sync::{Arc, Mutex};

pub const PAGE_SIZE: u64 = 0x1000;

/// Region permissions.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Perm(u8);

impl Perm {
    pub const NONE: Perm = Perm(0);
    pub const R: Perm = Perm(1);
    pub const W: Perm = Perm(2);
    pub const X: Perm = Perm(4);
    pub const RW: Perm = Perm(3);
    pub const RX: Perm = Perm(5);
    pub const RWX: Perm = Perm(7);

    pub const fn contains(self, other: Perm) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Perm {
    type Output = Perm;
    fn bitor(self, rhs: Perm) -> Perm {
        Perm(self.0 | rhs.0)
    }
}

impl fmt::Debug for Perm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = |p: Perm, ch: char| if self.contains(p) { ch } else { '-' };
        write!(
            f,
            "{}{}{}",
            c(Perm::R, 'r'),
            c(Perm::W, 'w'),
            c(Perm::X, 'x')
        )
    }
}

/// Why a guest memory access failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemFault {
    Unmapped {
        addr: u64,
    },
    Permission {
        addr: u64,
        needed: Perm,
    },
    Overlap {
        addr: u64,
    },
    Misaligned {
        addr: u64,
    },
    /// An atomic access not aligned to its size.
    Unaligned {
        addr: u64,
    },
    /// A mapping above the address space the page table covers.
    OutOfRange {
        addr: u64,
    },
    /// The arena has no room left for a mapping.
    OutOfMemory {
        addr: u64,
    },
}

impl fmt::Display for MemFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemFault::Unmapped { addr } => write!(f, "access to unmapped address {addr:#x}"),
            MemFault::Permission { addr, needed } => {
                write!(f, "permission fault at {addr:#x} (needed {needed:?})")
            }
            MemFault::Overlap { addr } => {
                write!(f, "mapping overlaps existing region at {addr:#x}")
            }
            MemFault::Misaligned { addr } => write!(f, "mapping not page aligned at {addr:#x}"),
            MemFault::OutOfRange { addr } => {
                write!(f, "mapping at {addr:#x} is outside the address space")
            }
            MemFault::Unaligned { addr } => write!(f, "unaligned atomic access at {addr:#x}"),
            MemFault::OutOfMemory { addr } => {
                write!(f, "no guest memory left for a mapping at {addr:#x}")
            }
        }
    }
}

impl std::error::Error for MemFault {}

/// What the OS layer records about a region (for the HLE kernel, the memory
/// type and attribute). Opaque here: carried along when regions split.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tag {
    pub state: u32,
    pub attr: u32,
}

/// One mapped region, or a clipped part of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub base: u64,
    pub size: u64,
    pub perm: Perm,
    pub tag: Tag,
}

/// What the OS layer knows about one mapped region. Its pages are in the
/// page table.
struct Region {
    size: u64,
    perm: Perm,
    tag: Tag,
    name: String,
}

type Regions = BTreeMap<u64, Region>;

/// Guest virtual addresses the page table covers: 39 bits, the largest
/// address space a Switch process has (Switchbrew, "SVC",
/// AddressSpace64Bit). Mapping above it fails with
/// [`MemFault::OutOfRange`]; accesses there are unmapped.
pub const ADDRESS_BITS: u32 = 39;

const PAGE_BITS: u32 = 12;
/// A leaf table maps 512 pages (2 MiB); the top table points at leaves.
const LEAF_BITS: u32 = 9;
const LEAF_ENTRIES: usize = 1 << LEAF_BITS;
const TOP_ENTRIES: usize = 1 << (ADDRESS_BITS - PAGE_BITS - LEAF_BITS);

/// A page table entry: the frame's offset in the arena in bits 12 and up,
/// the permission in bits 1 to 3, bit 0 set when the page is mapped. Zero
/// is an unmapped page.
const PRESENT: u64 = 1;
const PERM_SHIFT: u32 = 1;

const fn entry(frame: u64, perm: Perm) -> u64 {
    frame | (perm.0 as u64) << PERM_SHIFT | PRESENT
}

const fn entry_perm(e: u64) -> Perm {
    Perm((e >> PERM_SHIFT) as u8 & 7)
}

const fn entry_frame(e: u64) -> u64 {
    e & !(PAGE_SIZE - 1)
}

struct Leaf([AtomicU64; LEAF_ENTRIES]);

/// Guest page -> arena frame, in two levels so an access costs two loads.
/// Readers take no lock: leaves are published with release ordering and
/// never freed while the table lives, and entries are atomics. Writers are
/// serialised by `Memory::regions`.
struct PageTable {
    top: Box<[AtomicPtr<Leaf>]>,
}

impl PageTable {
    fn new() -> Self {
        let layout = Layout::array::<AtomicPtr<Leaf>>(TOP_ENTRIES).expect("small table");
        // 2 MiB of null pointers: zeroed memory, which the host provides
        // lazily, so a process pays only for the parts it maps.
        // SAFETY: the layout is not zero-sized; all-zero bits are a null
        // AtomicPtr; a boxed slice of TOP_ENTRIES frees with this layout.
        let top = unsafe {
            let p = alloc_zeroed(layout).cast::<AtomicPtr<Leaf>>();
            if p.is_null() {
                handle_alloc_error(layout);
            }
            Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, TOP_ENTRIES))
        };
        Self { top }
    }

    /// The entry for `addr`, or 0 when unmapped.
    #[inline]
    fn get(&self, addr: u64) -> u64 {
        let Some(slot) = self.top.get((addr >> (PAGE_BITS + LEAF_BITS)) as usize) else {
            return 0;
        };
        let leaf = slot.load(Acquire);
        if leaf.is_null() {
            return 0;
        }
        // SAFETY: published leaves stay alive as long as the table.
        let leaf = unsafe { &*leaf };
        leaf.0[(addr >> PAGE_BITS) as usize % LEAF_ENTRIES].load(Acquire)
    }

    /// The slot for `addr`, which is below 2^ADDRESS_BITS, making its leaf
    /// if needed.
    fn slot(&self, addr: u64) -> &AtomicU64 {
        let top = &self.top[(addr >> (PAGE_BITS + LEAF_BITS)) as usize];
        let mut leaf = top.load(Acquire);
        if leaf.is_null() {
            let new = Box::into_raw(Box::new(Leaf(std::array::from_fn(|_| AtomicU64::new(0)))));
            leaf = match top.compare_exchange(std::ptr::null_mut(), new, AcqRel, Acquire) {
                Ok(_) => new,
                Err(theirs) => {
                    // SAFETY: `new` was never published.
                    drop(unsafe { Box::from_raw(new) });
                    theirs
                }
            };
        }
        // SAFETY: as in `get`.
        unsafe { &(*leaf).0[(addr >> PAGE_BITS) as usize % LEAF_ENTRIES] }
    }
}

impl Drop for PageTable {
    fn drop(&mut self) {
        for slot in self.top.iter_mut() {
            let leaf = *slot.get_mut();
            if !leaf.is_null() {
                // SAFETY: every leaf came from Box::into_raw.
                drop(unsafe { Box::from_raw(leaf) });
            }
        }
    }
}

/// A sparse guest virtual address space, backed by an [`Arena`].
///
/// Every method takes `&self`, and `Memory` is `Sync`, so host threads
/// running guest cores can share one. Reads, writes and fetches take no
/// lock (the page table and the arena are read with atomics); mapping,
/// unmapping and permission changes are serialised by a lock and come from
/// the OS layer.
pub struct Memory {
    arena: Arc<Arena>,
    table: PageTable,
    regions: Mutex<Regions>,
    /// Counts mapping and permission changes; see [`Memory::epoch`].
    epoch: AtomicU64,
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

impl Memory {
    /// An empty address space with an arena of its own,
    /// [`DEFAULT_ARENA_SIZE`] bytes. Panics if the host cannot reserve the
    /// address space; [`Memory::with_arena`] lets the caller handle that.
    pub fn new() -> Self {
        let arena = Arena::new(DEFAULT_ARENA_SIZE).unwrap_or_else(|e| {
            panic!(
                "cannot reserve {} GiB of address space for guest memory: {e}",
                DEFAULT_ARENA_SIZE >> 30
            )
        });
        Self::with_arena(Arc::new(arena))
    }

    /// An empty address space whose pages come from `arena`.
    pub fn with_arena(arena: Arc<Arena>) -> Self {
        Self {
            arena,
            table: PageTable::new(),
            regions: Mutex::new(Regions::new()),
            epoch: AtomicU64::new(0),
        }
    }

    /// The arena holding this address space's pages.
    pub fn arena(&self) -> &Arc<Arena> {
        &self.arena
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Regions> {
        // Each change completes before the lock is released, so a panic
        // elsewhere leaves nothing half done.
        self.regions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Map `size` zeroed bytes at `base`. Both must be page aligned.
    pub fn map(&self, base: u64, size: u64, perm: Perm, name: &str) -> Result<(), MemFault> {
        self.map_tagged(base, size, perm, Tag::default(), name)
    }

    /// Map `size` zeroed bytes at `base` with an OS tag.
    pub fn map_tagged(
        &self,
        base: u64,
        size: u64,
        perm: Perm,
        tag: Tag,
        name: &str,
    ) -> Result<(), MemFault> {
        let end = Self::range_end(base, size)?;
        if end > 1 << ADDRESS_BITS {
            return Err(MemFault::OutOfRange { addr: base });
        }
        let mut regions = self.lock();
        if !is_free(&regions, base, size) {
            return Err(MemFault::Overlap { addr: base });
        }
        let runs = self
            .arena
            .alloc(size)
            .map_err(|_| MemFault::OutOfMemory { addr: base })?;
        let mut page = base;
        for (frame, len) in runs {
            for off in (0..len).step_by(PAGE_SIZE as usize) {
                self.table
                    .slot(page)
                    .store(entry(frame + off, perm), Release);
                page += PAGE_SIZE;
            }
        }
        regions.insert(
            base,
            Region {
                size,
                perm,
                tag,
                name: name.to_string(),
            },
        );
        self.epoch.fetch_add(1, Release);
        Ok(())
    }

    /// The end of a page-aligned, non-empty range that does not wrap.
    fn range_end(base: u64, size: u64) -> Result<u64, MemFault> {
        if base % PAGE_SIZE != 0 || size % PAGE_SIZE != 0 || size == 0 {
            return Err(MemFault::Misaligned { addr: base });
        }
        base.checked_add(size)
            .ok_or(MemFault::Overlap { addr: base })
    }

    /// Whether no region overlaps `[base, base + size)`.
    pub fn is_free(&self, base: u64, size: u64) -> bool {
        is_free(&self.lock(), base, size)
    }

    /// The region containing `addr`.
    pub fn region(&self, addr: u64) -> Option<Block> {
        region(&self.lock(), addr)
    }

    /// The unmapped gap around an unmapped `addr`, as (start, end): start is
    /// the end of the region below (or 0), end the start of the region
    /// above (`None` when nothing is mapped above).
    pub fn gap(&self, addr: u64) -> Option<(u64, Option<u64>)> {
        let regions = self.lock();
        if region(&regions, addr).is_some() {
            return None;
        }
        let start = regions
            .range(..=addr)
            .next_back()
            .map_or(0, |(&b, r)| b + r.size);
        let end = regions.range(addr..).next().map(|(&b, _)| b);
        Some((start, end))
    }

    /// The regions covering `[base, base + size)`, clipped to it. Fails if
    /// any page in the range is unmapped or the range is not page aligned.
    pub fn range_regions(&self, base: u64, size: u64) -> Result<Vec<Block>, MemFault> {
        range_regions(&self.lock(), base, size)
    }

    /// Change the permission and tag of every page in `[base, base + size)`,
    /// which must be fully mapped; regions are split at the range edges.
    pub fn update_range(
        &self,
        base: u64,
        size: u64,
        mut f: impl FnMut(&mut Perm, &mut Tag),
    ) -> Result<(), MemFault> {
        let mut regions = self.lock();
        range_regions(&regions, base, size)?;
        split(&mut regions, base);
        split(&mut regions, base + size);
        for (&b, r) in regions.range_mut(base..base + size) {
            f(&mut r.perm, &mut r.tag);
            self.set_perm(b, r.size, r.perm);
        }
        Ok(())
    }

    /// Unmap every page in `[base, base + size)`, which must be fully mapped.
    pub fn unmap_range(&self, base: u64, size: u64) -> Result<(), MemFault> {
        let mut regions = self.lock();
        range_regions(&regions, base, size)?;
        split(&mut regions, base);
        split(&mut regions, base + size);
        let doomed: Vec<u64> = regions.range(base..base + size).map(|(&b, _)| b).collect();
        for b in doomed {
            regions.remove(&b);
        }
        self.clear(base, size);
        Ok(())
    }

    pub fn unmap(&self, base: u64) -> bool {
        let mut regions = self.lock();
        match regions.remove(&base) {
            Some(r) => {
                self.clear(base, r.size);
                true
            }
            None => false,
        }
    }

    pub fn protect(&self, base: u64, perm: Perm) -> bool {
        let mut regions = self.lock();
        match regions.get_mut(&base) {
            Some(r) => {
                r.perm = perm;
                self.set_perm(base, r.size, perm);
                true
            }
            None => false,
        }
    }

    /// Give every mapped page in `[base, base + size)` permission `perm`.
    fn set_perm(&self, base: u64, size: u64, perm: Perm) {
        for page in (base..base + size).step_by(PAGE_SIZE as usize) {
            let slot = self.table.slot(page);
            let e = slot.load(Acquire);
            slot.store(entry(entry_frame(e), perm), Release);
        }
        self.epoch.fetch_add(1, Release);
    }

    /// Unmap the pages of `[base, base + size)` and give their frames back
    /// to the arena, in runs of adjacent frames.
    fn clear(&self, base: u64, size: u64) {
        let mut run: Option<(u64, u64)> = None;
        for page in (base..base + size).step_by(PAGE_SIZE as usize) {
            let frame = entry_frame(self.table.slot(page).swap(0, AcqRel));
            run = match run {
                Some((start, len)) if start + len == frame => Some((start, len + PAGE_SIZE)),
                Some((start, len)) => {
                    self.arena.free(start, len);
                    Some((frame, PAGE_SIZE))
                }
                None => Some((frame, PAGE_SIZE)),
            };
        }
        if let Some((start, len)) = run {
            self.arena.free(start, len);
        }
        self.epoch.fetch_add(1, Release);
    }

    /// The page holding `addr` as the backends see it: its frame's offset
    /// in the arena and its permission. A hypervisor backend builds the
    /// VM's page tables from this, a recompiler its view of the address
    /// space; both rebuild what changed when [`Memory::epoch`] moves.
    pub fn translate(&self, addr: u64) -> Option<(u64, Perm)> {
        let e = self.table.get(addr);
        (e & PRESENT != 0).then_some((entry_frame(e), entry_perm(e)))
    }

    /// A count that moves on after every change to the mappings or their
    /// permissions, so a backend holding its own copy of the page table
    /// (the VM's, or a recompiler's) can tell that it must look again.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Acquire)
    }

    pub fn is_mapped(&self, addr: u64) -> bool {
        self.table.get(addr) & PRESENT != 0
    }

    /// The regions as (base, size, perm, name), in address order, as they
    /// were when called.
    pub fn regions(&self) -> impl Iterator<Item = (u64, u64, Perm, String)> {
        let list: Vec<_> = self
            .lock()
            .iter()
            .map(|(&b, r)| (b, r.size, r.perm, r.name.clone()))
            .collect();
        list.into_iter()
    }

    /// The host address of `addr`, if its page is mapped with `need`.
    #[inline]
    fn host(&self, addr: u64, need: Perm) -> Result<*mut u8, MemFault> {
        let e = self.table.get(addr);
        if e & PRESENT == 0 {
            return Err(MemFault::Unmapped { addr });
        }
        if !entry_perm(e).contains(need) {
            return Err(MemFault::Permission { addr, needed: need });
        }
        Ok(self.arena.ptr(entry_frame(e) + addr % PAGE_SIZE))
    }

    /// Read bytes, allowed to cross region boundaries.
    pub fn read_bytes(&self, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        self.copy_out(addr, out, Perm::R)
    }

    /// Write bytes, allowed to cross region boundaries.
    pub fn write_bytes(&self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.copy_in(addr, data, Perm::W)
    }

    /// Loader/kernel write that ignores page permissions.
    pub fn poke_bytes(&self, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.copy_in(addr, data, Perm::NONE)
    }

    /// Copy out page by page; a fault names the first byte of the page
    /// (or of the access) that failed.
    fn copy_out(&self, mut addr: u64, mut out: &mut [u8], need: Perm) -> Result<(), MemFault> {
        while !out.is_empty() {
            let n = ((PAGE_SIZE - addr % PAGE_SIZE) as usize).min(out.len());
            let p = self.host(addr, need)?;
            // SAFETY: the n bytes lie in one mapped page, a committed frame.
            unsafe { arena::load(p, &mut out[..n]) };
            out = &mut out[n..];
            addr = addr.wrapping_add(n as u64);
        }
        Ok(())
    }

    /// Copy in page by page; the pages before a fault keep what was
    /// written to them.
    fn copy_in(&self, mut addr: u64, mut data: &[u8], need: Perm) -> Result<(), MemFault> {
        while !data.is_empty() {
            let n = ((PAGE_SIZE - addr % PAGE_SIZE) as usize).min(data.len());
            let p = self.host(addr, need)?;
            // SAFETY: as in copy_out.
            unsafe { arena::store(p, &data[..n]) };
            data = &data[n..];
            addr = addr.wrapping_add(n as u64);
        }
        Ok(())
    }

    /// Fetch one instruction word (needs execute permission).
    #[inline]
    pub fn fetch32(&self, addr: u64) -> Result<u32, MemFault> {
        self.load32(addr, Perm::X)
    }

    /// Read a little-endian guest string of at most `max` bytes.
    pub fn read_vec(&self, addr: u64, len: usize) -> Result<Vec<u8>, MemFault> {
        let mut v = vec![0; len];
        self.read_bytes(addr, &mut v)?;
        Ok(v)
    }
}

fn is_free(regions: &Regions, base: u64, size: u64) -> bool {
    let end = base.saturating_add(size);
    match regions.range(..end).next_back() {
        Some((&b, r)) => b + r.size <= base,
        None => true,
    }
}

fn region(regions: &Regions, addr: u64) -> Option<Block> {
    let (&base, r) = regions.range(..=addr).next_back()?;
    (addr < base + r.size).then_some(Block {
        base,
        size: r.size,
        perm: r.perm,
        tag: r.tag,
    })
}

fn range_regions(regions: &Regions, base: u64, size: u64) -> Result<Vec<Block>, MemFault> {
    let end = Memory::range_end(base, size)?;
    let mut out = Vec::new();
    let mut at = base;
    while at < end {
        let b = region(regions, at).ok_or(MemFault::Unmapped { addr: at })?;
        let stop = (b.base + b.size).min(end);
        out.push(Block {
            base: at,
            size: stop - at,
            ..b
        });
        at = stop;
    }
    Ok(out)
}

/// Make `addr` a region boundary, splitting the region containing it. The
/// pages stay where they are.
fn split(regions: &mut Regions, addr: u64) {
    let Some(b) = region(regions, addr) else {
        return;
    };
    if b.base == addr {
        return;
    }
    let r = regions.get_mut(&b.base).expect("region found above");
    let tail = Region {
        size: b.base + r.size - addr,
        perm: r.perm,
        tag: r.tag,
        name: r.name.clone(),
    };
    r.size = addr - b.base;
    regions.insert(addr, tail);
}

// Loads and stores of one value. An aligned access lies in one page and is
// a single host atomic, so a guest core on another host thread sees all of
// it or none of it, as Arm's single-copy atomicity requires; an unaligned
// one is copied byte by byte, possibly across pages.
macro_rules! rw_impl {
    ($load:ident, $read:ident, $write:ident, $t:ty, $atomic:ty) => {
        impl Memory {
            #[inline]
            fn $load(&self, addr: u64, need: Perm) -> Result<$t, MemFault> {
                const N: usize = std::mem::size_of::<$t>();
                if addr % N as u64 == 0 {
                    let p = self.host(addr, need)?;
                    // SAFETY: an aligned value in a mapped page.
                    let v = unsafe { <$atomic>::from_ptr(p.cast()).load(Relaxed) };
                    return Ok(<$t>::from_le(v));
                }
                let mut b = [0u8; N];
                self.copy_out(addr, &mut b, need)?;
                Ok(<$t>::from_le_bytes(b))
            }
            #[inline]
            pub fn $read(&self, addr: u64) -> Result<$t, MemFault> {
                self.$load(addr, Perm::R)
            }
            #[inline]
            pub fn $write(&self, addr: u64, v: $t) -> Result<(), MemFault> {
                const N: usize = std::mem::size_of::<$t>();
                if addr % N as u64 == 0 {
                    let p = self.host(addr, Perm::W)?;
                    // SAFETY: as in the load.
                    unsafe { <$atomic>::from_ptr(p.cast()).store(v.to_le(), Relaxed) };
                    return Ok(());
                }
                self.copy_in(addr, &v.to_le_bytes(), Perm::W)
            }
        }
    };
}

rw_impl!(load8, read8, write8, u8, AtomicU8);
rw_impl!(load16, read16, write16, u16, AtomicU16);
rw_impl!(load32, read32, write32, u32, AtomicU32);
rw_impl!(load64, read64, write64, u64, AtomicU64);

// Compare-and-swap on guest memory, for guest atomics on backends that run
// guest cores on several host threads: LSE's CAS family directly, and the
// exclusive pairs (LDXR/STXR) as a compare-and-swap of the value the load
// saw, the usual way to share one exclusive monitor between host threads.
// The page needs read and write permission, like the guest's own atomics.
macro_rules! cas_impl {
    ($cas:ident, $t:ty, $atomic:ty) => {
        impl Memory {
            /// Replace the value at `addr` with `new` if it is `current`:
            /// `Ok(Ok(current))` if it was replaced, `Ok(Err(seen))` with
            /// the value found if not. `addr` must be aligned to the size.
            pub fn $cas(
                &self,
                addr: u64,
                current: $t,
                new: $t,
            ) -> Result<Result<$t, $t>, MemFault> {
                if addr % std::mem::size_of::<$t>() as u64 != 0 {
                    return Err(MemFault::Unaligned { addr });
                }
                let p = self.host(addr, Perm::RW)?;
                // SAFETY: an aligned value in a mapped page.
                let a = unsafe { <$atomic>::from_ptr(p.cast()) };
                Ok(
                    a.compare_exchange(current.to_le(), new.to_le(), AcqRel, Acquire)
                        .map(<$t>::from_le)
                        .map_err(<$t>::from_le),
                )
            }
        }
    };
}

cas_impl!(compare_exchange8, u8, AtomicU8);
cas_impl!(compare_exchange16, u16, AtomicU16);
cas_impl!(compare_exchange32, u32, AtomicU32);
cas_impl!(compare_exchange64, u64, AtomicU64);

impl Memory {
    /// A 128-bit read: two 64-bit halves when 8-byte aligned (each
    /// single-copy atomic, as Arm requires of a 128-bit SIMD load without
    /// FEAT_LSE2), else byte by byte.
    #[inline]
    pub fn read128(&self, addr: u64) -> Result<u128, MemFault> {
        if addr % 8 == 0 {
            let lo = self.read64(addr)?;
            let hi = self.read64(addr.wrapping_add(8))?;
            return Ok(u128::from(hi) << 64 | u128::from(lo));
        }
        let mut b = [0u8; 16];
        self.read_bytes(addr, &mut b)?;
        Ok(u128::from_le_bytes(b))
    }

    /// A 128-bit write, split as [`Memory::read128`] splits.
    #[inline]
    pub fn write128(&self, addr: u64, v: u128) -> Result<(), MemFault> {
        if addr % 8 == 0 && addr % PAGE_SIZE != PAGE_SIZE - 8 {
            self.write64(addr, v as u64)?;
            return self.write64(addr + 8, (v >> 64) as u64);
        }
        self.write_bytes(addr, &v.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::ARENA_ALIGN;

    const T: Tag = Tag { state: 5, attr: 0 };

    #[test]
    fn update_range_splits_and_keeps_contents() {
        let m = Memory::new();
        m.map_tagged(0x10000, 0x4000, Perm::RW, T, "heap").unwrap();
        m.write64(0x11ff8, 0x1122_3344_5566_7788).unwrap();
        m.update_range(0x11000, 0x2000, |p, t| {
            *p = Perm::R;
            t.attr = 8;
        })
        .unwrap();
        let blocks = m.range_regions(0x10000, 0x4000).unwrap();
        let shape: Vec<_> = blocks
            .iter()
            .map(|b| (b.base, b.size, b.perm, b.tag.attr))
            .collect();
        assert_eq!(
            shape,
            [
                (0x10000, 0x1000, Perm::RW, 0),
                (0x11000, 0x2000, Perm::R, 8),
                (0x13000, 0x1000, Perm::RW, 0),
            ]
        );
        // Contents survive the split; the read-only part rejects writes, and
        // a read straddling two regions still works.
        assert_eq!(m.read64(0x11ff8).unwrap(), 0x1122_3344_5566_7788);
        assert!(m.write8(0x11000, 1).is_err());
        m.write32(0x10ffc, 0xaabb_ccdd).unwrap();
        assert_eq!(m.read64(0x10ffc).unwrap(), 0x0000_0000_aabb_ccdd);
    }

    #[test]
    fn unmap_range_leaves_the_rest_and_reports_gaps() {
        let m = Memory::new();
        m.map_tagged(0x10000, 0x4000, Perm::RW, T, "heap").unwrap();
        m.write8(0x13000, 9).unwrap();
        m.unmap_range(0x11000, 0x1000).unwrap();
        assert!(!m.is_mapped(0x11800));
        assert_eq!(m.read8(0x13000).unwrap(), 9);
        assert_eq!(m.gap(0x11800), Some((0x11000, Some(0x12000))));
        assert_eq!(m.gap(0x20000), Some((0x14000, None)));
        assert_eq!(m.gap(0x100), Some((0, Some(0x10000))));
        assert_eq!(m.gap(0x10000), None);
        assert!(m.is_free(0x11000, 0x1000));
        assert!(!m.is_free(0x11000, 0x2000));
        // Ranges must be fully mapped and page aligned.
        assert!(m.unmap_range(0x10000, 0x3000).is_err());
        assert!(m.update_range(0x10000, 0x800, |_, _| {}).is_err());
        assert_eq!(m.region(0x10000).unwrap().size, 0x1000);
    }

    #[test]
    fn map_read_write() {
        let m = Memory::new();
        m.map(0x1000, 0x2000, Perm::RW, "test").unwrap();
        m.write64(0x1ffc, 0x1122_3344_5566_7788).unwrap();
        assert_eq!(m.read64(0x1ffc).unwrap(), 0x1122_3344_5566_7788);
        assert_eq!(m.read8(0x1ffc).unwrap(), 0x88);
        assert!(matches!(m.read8(0x3000), Err(MemFault::Unmapped { .. })));
    }

    #[test]
    fn overlap_rejected() {
        let m = Memory::new();
        m.map(0x1000, 0x2000, Perm::RW, "a").unwrap();
        assert!(m.map(0x2000, 0x1000, Perm::RW, "b").is_err());
        assert!(m.map(0x3000, 0x1000, Perm::RW, "c").is_ok());
    }

    #[test]
    fn cross_region_access() {
        let m = Memory::new();
        m.map(0x1000, 0x1000, Perm::RW, "a").unwrap();
        m.map(0x2000, 0x1000, Perm::RW, "b").unwrap();
        m.write32(0x1ffe, 0xdead_beef).unwrap();
        assert_eq!(m.read32(0x1ffe).unwrap(), 0xdead_beef);
    }

    #[test]
    fn permissions_enforced() {
        let m = Memory::new();
        m.map(0x1000, 0x1000, Perm::RX, "code").unwrap();
        assert!(matches!(
            m.write8(0x1000, 1),
            Err(MemFault::Permission { .. })
        ));
        m.poke_bytes(0x1000, &[0x1f, 0x20, 0x03, 0xd5]).unwrap();
        assert_eq!(m.fetch32(0x1000).unwrap(), 0xd503201f);
    }

    #[test]
    fn unmapped_frames_return_to_the_arena_and_come_back_zeroed() {
        let m = Memory::with_arena(Arc::new(Arena::new(ARENA_ALIGN).unwrap()));
        m.map(0x10000, 0x3000, Perm::RW, "a").unwrap();
        m.write64(0x11000, 0x55).unwrap();
        m.unmap_range(0x10000, 0x3000).unwrap();
        assert_eq!(m.arena().allocated(), 0);
        m.map(0x40000, 0x3000, Perm::RW, "b").unwrap();
        assert_eq!(m.read64(0x41000), Ok(0));
        // The arena is 2 MiB, of which 12 KiB are in use.
        assert_eq!(
            m.map(0x100000, ARENA_ALIGN, Perm::RW, "c"),
            Err(MemFault::OutOfMemory { addr: 0x100000 })
        );
        assert!(
            m.is_free(0x100000, ARENA_ALIGN),
            "a failed map leaves nothing"
        );
        assert!(m.unmap(0x40000));
        m.map(0x100000, ARENA_ALIGN, Perm::RW, "c").unwrap();
    }

    #[test]
    fn the_page_table_covers_39_bits() {
        let m = Memory::new();
        let top = 1u64 << ADDRESS_BITS;
        m.map(top - 0x1000, 0x1000, Perm::RW, "top").unwrap();
        m.write32(top - 4, 7).unwrap();
        assert_eq!(m.read32(top - 4), Ok(7));
        assert_eq!(
            m.read64(top - 4),
            Err(MemFault::Unmapped { addr: top }),
            "the bytes past the top are unmapped"
        );
        assert_eq!(
            m.map(top, 0x1000, Perm::RW, "above"),
            Err(MemFault::OutOfRange { addr: top })
        );
        assert_eq!(
            m.read8(u64::MAX),
            Err(MemFault::Unmapped { addr: u64::MAX })
        );
    }

    #[test]
    fn faults_name_the_first_page_that_fails() {
        let m = Memory::new();
        m.map(0x1000, 0x1000, Perm::RW, "a").unwrap();
        m.map(0x2000, 0x1000, Perm::R, "b").unwrap();
        // An unaligned write across the boundary writes the first page,
        // then faults on the second, as the regions did before the arena.
        assert_eq!(
            m.write32(0x1ffe, 0xaabb_ccdd),
            Err(MemFault::Permission {
                addr: 0x2000,
                needed: Perm::W
            })
        );
        assert_eq!(m.read16(0x1ffe), Ok(0xccdd));
        assert_eq!(m.read16(0x2000), Ok(0));
        assert_eq!(
            m.fetch32(0x1000),
            Err(MemFault::Permission {
                addr: 0x1000,
                needed: Perm::X
            })
        );
        assert_eq!(m.read128(0x2ff8), Err(MemFault::Unmapped { addr: 0x3000 }));
        m.protect(0x2000, Perm::RW);
        m.write128(0x1ff8, u128::MAX - 1).unwrap();
        assert_eq!(m.read128(0x1ff8), Ok(u128::MAX - 1));
        assert_eq!(m.read64(0x2000), Ok(u64::MAX));
    }

    #[test]
    fn regions_split_without_moving_their_pages() {
        let m = Memory::new();
        m.map(0x10000, 0x4000, Perm::RW, "heap").unwrap();
        m.write_bytes(0x10ffe, &[1, 2, 3, 4]).unwrap();
        m.update_range(0x11000, 0x1000, |p, _| *p = Perm::R)
            .unwrap();
        let names: Vec<_> = m.regions().collect();
        assert_eq!(
            names,
            [
                (0x10000, 0x1000, Perm::RW, "heap".to_string()),
                (0x11000, 0x1000, Perm::R, "heap".to_string()),
                (0x12000, 0x2000, Perm::RW, "heap".to_string()),
            ]
        );
        assert_eq!(m.read_vec(0x10ffe, 4).unwrap(), [1, 2, 3, 4]);
        let before = m.arena().allocated();
        m.unmap_range(0x11000, 0x1000).unwrap();
        assert_eq!(m.arena().allocated(), before - 0x1000);
    }

    #[test]
    fn memory_is_shared_between_host_threads() {
        fn shared<T: Send + Sync>() {}
        shared::<Memory>();
        shared::<Arena>();
    }

    #[test]
    fn backends_see_frames_permissions_and_changes() {
        let m = Memory::new();
        let e0 = m.epoch();
        m.map(0x10000, 0x2000, Perm::RX, "code").unwrap();
        let (frame, perm) = m.translate(0x11234).unwrap();
        assert_eq!((frame % PAGE_SIZE, perm), (0, Perm::RX));
        assert_ne!(m.translate(0x10000).unwrap().0, frame);
        // The frame holds what the guest sees at that page.
        m.poke_bytes(0x11234, &[0xab]).unwrap();
        let mut b = [0];
        unsafe { arena::load(m.arena().ptr(frame + 0x234), &mut b) };
        assert_eq!(b, [0xab]);
        let e1 = m.epoch();
        assert!(e1 > e0);
        m.update_range(0x11000, 0x1000, |p, _| *p = Perm::R)
            .unwrap();
        assert_eq!(m.translate(0x11000), Some((frame, Perm::R)));
        let e2 = m.epoch();
        assert!(e2 > e1);
        m.read8(0x11000).unwrap();
        assert_eq!(m.epoch(), e2, "accesses are not changes");
        m.unmap_range(0x10000, 0x2000).unwrap();
        assert_eq!(m.translate(0x11000), None);
        assert!(m.epoch() > e2);
    }

    #[test]
    fn compare_exchange_checks_alignment_and_permission() {
        let m = Memory::new();
        m.map(0x1000, 0x1000, Perm::RW, "data").unwrap();
        m.map(0x2000, 0x1000, Perm::R, "ro").unwrap();
        m.write64(0x1008, 5).unwrap();
        assert_eq!(m.compare_exchange64(0x1008, 5, 6), Ok(Ok(5)));
        assert_eq!(m.compare_exchange64(0x1008, 5, 7), Ok(Err(6)));
        assert_eq!(m.read64(0x1008), Ok(6));
        assert_eq!(m.compare_exchange16(0x100e, 0, 0xffff), Ok(Ok(0)));
        assert_eq!(m.read64(0x1008), Ok(0xffff_0000_0000_0006));
        assert_eq!(m.compare_exchange8(0x1009, 0, 1), Ok(Ok(0)));
        assert_eq!(
            m.compare_exchange32(0x100c, 0xffff_0000, 1),
            Ok(Ok(0xffff_0000))
        );
        assert_eq!(m.read64(0x1008), Ok(0x0000_0001_0000_0106));
        assert_eq!(
            m.compare_exchange32(0x1002, 0, 1),
            Err(MemFault::Unaligned { addr: 0x1002 })
        );
        assert_eq!(
            m.compare_exchange32(0x2000, 0, 1),
            Err(MemFault::Permission {
                addr: 0x2000,
                needed: Perm::RW
            })
        );
        assert_eq!(
            m.compare_exchange64(0x3000, 0, 1),
            Err(MemFault::Unmapped { addr: 0x3000 })
        );
    }

    /// Six host threads, one per Switch 2 core, on one Memory: atomic
    /// increments all land, and an aligned 64-bit value is never seen
    /// half written.
    #[test]
    fn six_host_threads_share_one_address_space() {
        const CORES: u64 = 6;
        const ROUNDS: u64 = 20_000;
        let m = Memory::new();
        m.map(0x1000, 0x1000, Perm::RW, "shared").unwrap();
        let (counter, word) = (0x1000, 0x1040);
        std::thread::scope(|s| {
            for core in 0..CORES {
                let m = &m;
                s.spawn(move || {
                    for i in 0..ROUNDS {
                        loop {
                            let v = m.read64(counter).unwrap();
                            if m.compare_exchange64(counter, v, v + 1).unwrap().is_ok() {
                                break;
                            }
                        }
                        if core % 2 == 0 {
                            let v = if i % 2 == 0 { 0 } else { u64::MAX };
                            m.write64(word, v).unwrap();
                        } else {
                            let w = m.read64(word).unwrap();
                            assert!(w == 0 || w == u64::MAX, "torn read {w:#x}");
                        }
                    }
                });
            }
        });
        assert_eq!(m.read64(counter), Ok(CORES * ROUNDS));
    }
}
