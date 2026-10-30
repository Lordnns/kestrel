//! Kernel view of the address space: memory types and attributes per
//! mapping (Switchbrew, "SVC": MemoryType, MemoryAttribute) and the block
//! svcQueryMemory reports for any address.

use crate::kernel::{Kind, Object, ObjectId};
use crate::svc::result;
use crate::{layout, Process};
use kestrel_core::{Memory, Perm, Tag, PAGE_SIZE};

/// MemoryType values (Switchbrew, "SVC"; the 4.0.0+ values where they
/// changed).
pub mod state {
    pub const FREE: u32 = 0x0000_0000;
    pub const CODE: u32 = 0x00dc_7e03;
    pub const CODE_DATA: u32 = 0x03fe_bd04;
    pub const NORMAL: u32 = 0x037e_bd05;
    pub const STACK: u32 = 0x005c_3c0b;
    pub const THREAD_LOCAL: u32 = 0x0040_200c;
    pub const INACCESSIBLE: u32 = 0x0000_0010;
    pub const SHARED: u32 = 0x0040_2006;
    /// MemoryState bit 17, FlagCanTransfer: svcCreateTransferMemory may
    /// lend the memory.
    pub const CAN_TRANSFER: u32 = 1 << 17;
}

/// MemoryAttribute bits.
pub mod attr {
    pub const LOCKED: u32 = 1 << 0;
    pub const UNCACHED: u32 = 1 << 3;
}

pub const fn tag(state: u32) -> Tag {
    Tag { state, attr: 0 }
}

/// MemoryPermission bits (read 1, write 2, execute 4).
pub fn perm_bits(p: Perm) -> u32 {
    [(Perm::R, 1), (Perm::W, 2), (Perm::X, 4)]
        .iter()
        .filter(|(bit, _)| p.contains(*bit))
        .map(|(_, v)| v)
        .sum()
}

/// One block as svcQueryMemory reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemInfo {
    pub base: u64,
    pub size: u64,
    pub state: u32,
    pub attr: u32,
    pub perm: u32,
}

impl MemInfo {
    /// The 0x28-byte MemoryInfo layout; the IPC and device reference
    /// counts are zero, as nothing shares memory yet.
    pub fn to_bytes(self) -> [u8; 0x28] {
        let mut b = [0; 0x28];
        b[0..8].copy_from_slice(&self.base.to_le_bytes());
        b[8..16].copy_from_slice(&self.size.to_le_bytes());
        b[16..20].copy_from_slice(&self.state.to_le_bytes());
        b[20..24].copy_from_slice(&self.attr.to_le_bytes());
        b[24..28].copy_from_slice(&self.perm.to_le_bytes());
        b
    }
}

/// The block containing `addr`: the mapping with neighbours of the same
/// type, attribute and permission merged, or the unmapped gap around it.
/// A gap inside the address space is Free; outside it, Inaccessible.
pub fn query(mem: &Memory, addr: u64) -> MemInfo {
    if let Some(mut b) = mem.region(addr) {
        let same = |o: kestrel_core::Block| o.perm == b.perm && o.tag == b.tag;
        while let Some(prev) = b.base.checked_sub(1).and_then(|a| mem.region(a)) {
            if !same(prev) {
                break;
            }
            b.size += b.base - prev.base;
            b.base = prev.base;
        }
        while let Some(next) = mem.region(b.base.wrapping_add(b.size)) {
            if !same(next) || next.base == 0 {
                break;
            }
            b.size += next.size;
        }
        return MemInfo {
            base: b.base,
            size: b.size,
            state: b.tag.state,
            attr: b.tag.attr,
            perm: perm_bits(b.perm),
        };
    }
    let (start, end) = mem.gap(addr).expect("unmapped address has a gap");
    // The zone holding addr: below, inside or above the address space.
    // An end of 0 stands for 2^64.
    let (zone_start, zone_end, kind) = if addr < layout::ADDRESS_SPACE_BASE {
        (0, layout::ADDRESS_SPACE_BASE, state::INACCESSIBLE)
    } else if addr < layout::ADDRESS_SPACE_END {
        (
            layout::ADDRESS_SPACE_BASE,
            layout::ADDRESS_SPACE_END,
            state::FREE,
        )
    } else {
        (layout::ADDRESS_SPACE_END, 0, state::INACCESSIBLE)
    };
    let base = start.max(zone_start);
    let top = match (end, zone_end) {
        (Some(e), 0) => e,
        (Some(e), z) => e.min(z),
        (None, z) => z,
    };
    MemInfo {
        base,
        size: top.wrapping_sub(base),
        state: kind,
        attr: 0,
        perm: 0,
    }
}

/// Check a page-aligned, non-empty range; returns its end, or `None` if it
/// wraps around.
fn checked_range(addr: u64, size: u64) -> Result<Option<u64>, u64> {
    if addr % PAGE_SIZE != 0 {
        return Err(result::INVALID_ADDRESS);
    }
    if size == 0 || size % PAGE_SIZE != 0 {
        return Err(result::INVALID_SIZE);
    }
    Ok(addr.checked_add(size))
}

/// A range as (base, size).
type Part = (u64, u64);

/// Whether `[addr, end)` lies inside `(base, size)`.
fn within((base, size): layout::Region, addr: u64, end: u64) -> bool {
    base <= addr && end <= base + size
}

/// Whether `[addr, end)` and `(base, size)` share a byte.
fn overlaps((base, size): layout::Region, addr: u64, end: u64) -> bool {
    addr < base + size && base < end
}

/// Why svcMapSharedMemory mapped nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum MapError {
    Result(u64),
    Unsupported(&'static str),
}

impl From<u64> for MapError {
    fn from(code: u64) -> Self {
        MapError::Result(code)
    }
}

impl Process {
    /// Check that `[addr, addr + size)` is mapped heap-like memory (Normal,
    /// not locked) inside the address space, as svcSetMemoryPermission and
    /// svcSetMemoryAttribute require.
    fn check_normal(&self, addr: u64, size: u64) -> Result<(), u64> {
        let end = checked_range(addr, size)?.ok_or(result::INVALID_CURRENT_MEMORY)?;
        if !within(layout::ASLR_REGION, addr, end) {
            return Err(result::INVALID_CURRENT_MEMORY);
        }
        let blocks = self
            .mem
            .range_regions(addr, size)
            .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        if blocks
            .iter()
            .any(|b| b.tag.state != state::NORMAL || b.tag.attr & attr::LOCKED != 0)
        {
            return Err(result::INVALID_CURRENT_MEMORY);
        }
        Ok(())
    }

    /// svcSetMemoryPermission (Switchbrew, "SVC"): move heap-like memory
    /// between ---, r-- and rw-.
    pub fn set_memory_permission(&mut self, addr: u64, size: u64, perm: u32) -> Result<(), u64> {
        checked_range(addr, size)?;
        let new = match perm {
            0 => Perm::NONE,
            1 => Perm::R,
            3 => Perm::RW,
            _ => return Err(result::INVALID_NEW_MEMORY_PERMISSION),
        };
        self.check_normal(addr, size)?;
        self.mem
            .update_range(addr, size, |p, _| *p = new)
            .map_err(|_| result::INVALID_CURRENT_MEMORY)
    }

    /// svcSetMemoryAttribute (Switchbrew, "SVC"): only the Uncached bit
    /// may be set or cleared.
    pub fn set_memory_attribute(
        &mut self,
        addr: u64,
        size: u64,
        mask: u32,
        value: u32,
    ) -> Result<(), u64> {
        checked_range(addr, size)?;
        if mask & !attr::UNCACHED != 0 || value & !mask != 0 {
            return Err(result::INVALID_COMBINATION);
        }
        self.check_normal(addr, size)?;
        self.mem
            .update_range(addr, size, |_, t| t.attr = (t.attr & !mask) | value)
            .map_err(|_| result::INVALID_CURRENT_MEMORY)
    }

    /// svcMapMemory (Switchbrew, "SVC"): make `size` bytes of read-write
    /// heap at `src` appear at `dst` in the stack region, as Stack memory.
    /// The source becomes inaccessible and Locked until unmapped. Its bytes
    /// move to the destination, which is equivalent while it is locked.
    pub fn map_memory(&mut self, dst: u64, src: u64, size: u64) -> Result<(), u64> {
        let dst_end = checked_range(dst, size)?.ok_or(result::INVALID_MEMORY_REGION)?;
        checked_range(src, size)?.ok_or(result::INVALID_CURRENT_MEMORY)?;
        if !within(layout::STACK_REGION, dst, dst_end) {
            return Err(result::INVALID_MEMORY_REGION);
        }
        if !self.mem.is_free(dst, size) {
            return Err(result::INVALID_CURRENT_MEMORY);
        }
        let blocks = self
            .mem
            .range_regions(src, size)
            .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        if blocks
            .iter()
            .any(|b| b.tag != tag(state::NORMAL) || b.perm != Perm::RW)
        {
            return Err(result::INVALID_CURRENT_MEMORY);
        }
        let bytes = self
            .mem
            .read_vec(src, size as usize)
            .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        let stack = tag(state::STACK);
        self.mem
            .map_tagged(dst, size, Perm::RW, stack, "mapped-memory")
            .and_then(|_| self.mem.poke_bytes(dst, &bytes))
            .and_then(|_| {
                self.mem.update_range(src, size, |p, t| {
                    *p = Perm::NONE;
                    t.attr |= attr::LOCKED;
                })
            })
            .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        self.mapped_memory.push((dst, src, size));
        Ok(())
    }

    /// svcUnmapMemory: undo all or part of an svcMapMemory with the same
    /// source and destination, moving the bytes back to the source.
    pub fn unmap_memory(&mut self, dst: u64, src: u64, size: u64) -> Result<(), u64> {
        let dst_end = checked_range(dst, size)?.ok_or(result::INVALID_MEMORY_REGION)?;
        checked_range(src, size)?.ok_or(result::INVALID_CURRENT_MEMORY)?;
        if !within(layout::STACK_REGION, dst, dst_end) {
            return Err(result::INVALID_MEMORY_REGION);
        }
        let i = self
            .mapped_memory
            .iter()
            .position(|&(d, s, n)| d <= dst && dst_end <= d + n && dst - d == src.wrapping_sub(s))
            .ok_or(result::INVALID_CURRENT_MEMORY)?;
        let bytes = self
            .mem
            .read_vec(dst, size as usize)
            .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        self.mem
            .unmap_range(dst, size)
            .and_then(|_| self.mem.poke_bytes(src, &bytes))
            .and_then(|_| {
                self.mem.update_range(src, size, |p, t| {
                    *p = Perm::RW;
                    t.attr &= !attr::LOCKED;
                })
            })
            .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        // Keep whatever is left of the mapping on either side.
        let (d, s, n) = self.mapped_memory.remove(i);
        if dst > d {
            self.mapped_memory.push((d, s, dst - d));
        }
        if dst_end < d + n {
            let off = dst_end - d;
            self.mapped_memory.push((dst_end, s + off, n - off));
        }
        Ok(())
    }

    /// Check an svcMapPhysicalMemory or svcUnmapPhysicalMemory range and
    /// split it into (mapped, unmapped) parts. Mapped parts must be
    /// unlocked Normal memory.
    fn physical_parts(&self, addr: u64, size: u64) -> Result<(Vec<Part>, Vec<Part>), u64> {
        let end = checked_range(addr, size)?.ok_or(result::INVALID_MEMORY_REGION)?;
        if !within(layout::ALIAS_REGION, addr, end) {
            return Err(result::INVALID_MEMORY_REGION);
        }
        let (mut mapped, mut free) = (Vec::new(), Vec::new());
        let mut at = addr;
        while at < end {
            if let Some(b) = self.mem.region(at) {
                if b.tag != tag(state::NORMAL) {
                    return Err(result::INVALID_CURRENT_MEMORY);
                }
                let stop = (b.base + b.size).min(end);
                mapped.push((at, stop - at));
                at = stop;
            } else {
                let (_, next) = self.mem.gap(at).expect("unmapped address has a gap");
                let stop = next.map_or(end, |n| n.min(end));
                free.push((at, stop - at));
                at = stop;
            }
        }
        Ok((mapped, free))
    }

    /// svcMapPhysicalMemory (Switchbrew, "SVC"): heap-like memory at any
    /// page-aligned range of the alias region; pages already mapped there
    /// stay as they are.
    pub fn map_physical_memory(&mut self, addr: u64, size: u64) -> Result<(), u64> {
        let (_, free) = self.physical_parts(addr, size)?;
        let new: u64 = free.iter().map(|&(_, n)| n).sum();
        if self.used_memory() + new > layout::TOTAL_MEMORY {
            return Err(result::LIMIT_REACHED);
        }
        for (base, n) in free {
            self.mem
                .map_tagged(base, n, Perm::RW, tag(state::NORMAL), "physical")
                .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        }
        self.physical_size += new;
        Ok(())
    }

    /// svcUnmapPhysicalMemory: release whatever svcMapPhysicalMemory
    /// mapped in the range.
    pub fn unmap_physical_memory(&mut self, addr: u64, size: u64) -> Result<(), u64> {
        let (mapped, _) = self.physical_parts(addr, size)?;
        for &(base, n) in &mapped {
            self.mem
                .unmap_range(base, n)
                .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
            self.physical_size -= n;
        }
        Ok(())
    }

    /// Memory counted against TotalMemorySize: the image, the main stack,
    /// the heap (Switchbrew, "SVC", svcGetInfo UsedMemorySize) and
    /// physical memory mapped by the program.
    pub fn used_memory(&self) -> u64 {
        self.image_size + layout::STACK_SIZE + self.heap_size + self.physical_size
    }

    /// svcSetHeapSize (Switchbrew, "SVC"): grow or shrink the heap at the
    /// start of the heap region, keeping its contents. Returns the heap
    /// address, or a result code.
    pub fn set_heap_size(&mut self, size: u64) -> Result<u64, u64> {
        let (base, region_size) = layout::HEAP_REGION;
        if size % 0x20_0000 != 0 || size > 1 << 32 {
            return Err(result::INVALID_SIZE);
        }
        if size > region_size {
            return Err(result::OUT_OF_MEMORY);
        }
        let old = self.heap_size;
        if size > old {
            if self.used_memory() - old + size > layout::TOTAL_MEMORY {
                return Err(result::LIMIT_REACHED);
            }
            self.mem
                .map_tagged(base + old, size - old, Perm::RW, tag(state::NORMAL), "heap")
                .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        } else if size < old {
            // Only untouched heap can be released: Normal, read-write, no
            // attribute.
            let tail = self
                .mem
                .range_regions(base + size, old - size)
                .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
            if tail
                .iter()
                .any(|b| b.tag != tag(state::NORMAL) || b.perm != Perm::RW)
            {
                return Err(result::INVALID_CURRENT_MEMORY);
            }
            self.mem
                .unmap_range(base + size, old - size)
                .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        }
        self.heap_size = size;
        Ok(base)
    }

    /// svcCreateTransferMemory (Switchbrew, "SVC"): lend `size` bytes at
    /// `addr` through a TransferMemory handle. The range takes permission
    /// `perm` and the Locked attribute until the last handle closes.
    ///
    /// Switchbrew documents only that the execute bit is refused and that
    /// the memory needs FlagCanTransfer. Kestrel also refuses write-only
    /// (as svcSetMemoryPermission does) and memory with any attribute set,
    /// with the svcSetMemoryPermission result codes.
    pub fn create_transfer_memory(&mut self, addr: u64, size: u64, perm: u32) -> Result<u32, u64> {
        let end = checked_range(addr, size)?.ok_or(result::INVALID_CURRENT_MEMORY)?;
        let new = match perm {
            0 => Perm::NONE,
            1 => Perm::R,
            3 => Perm::RW,
            _ => return Err(result::INVALID_NEW_MEMORY_PERMISSION),
        };
        if !within(layout::ASLR_REGION, addr, end) {
            return Err(result::INVALID_CURRENT_MEMORY);
        }
        let blocks = self
            .mem
            .range_regions(addr, size)
            .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        if blocks
            .iter()
            .any(|b| b.tag.state & state::CAN_TRANSFER == 0 || b.tag.attr != 0)
        {
            return Err(result::INVALID_CURRENT_MEMORY);
        }
        let restore = blocks.iter().map(|b| (b.base, b.size, b.perm)).collect();
        let h = self
            .handles
            .insert(Object::TransferMemory { restore }, &[Kind::TransferMemory])?[0];
        self.mem
            .update_range(addr, size, |p, t| {
                *p = new;
                t.attr |= attr::LOCKED;
            })
            .expect("range checked above");
        Ok(h)
    }

    /// A new shared memory object of `size` zeroed bytes that the process
    /// may map with permission `perm`, held by the kernel for the service
    /// that made it.
    pub(crate) fn create_shared_memory(&mut self, size: u64, perm: Perm) -> ObjectId {
        self.handles.insert_unnamed(Object::SharedMemory {
            perm,
            contents: vec![0; size as usize],
            mapped: None,
        })
    }

    /// Write `data` at `offset` in shared memory object `id`: into the
    /// process's mapping when it is mapped, else into the object.
    pub(crate) fn write_shared_memory(&mut self, id: ObjectId, offset: usize, data: &[u8]) {
        match self.handles.object_mut(id) {
            Object::SharedMemory {
                mapped: Some(addr), ..
            } => {
                let addr = *addr + offset as u64;
                self.mem
                    .poke_bytes(addr, data)
                    .expect("a shared memory mapping covers the object");
            }
            Object::SharedMemory { contents, .. } => {
                contents[offset..offset + data.len()].copy_from_slice(data);
            }
            o => unreachable!("shared memory {id} is {o:?}"),
        }
    }

    /// The shared memory object `h` names.
    fn shared_memory(&mut self, h: u64) -> Result<ObjectId, u64> {
        match self.handles.get(h as u32) {
            Some((Kind::SharedMemory, id)) => Ok(id),
            _ => Err(result::INVALID_HANDLE),
        }
    }

    /// svcMapSharedMemory (Switchbrew, "SVC"): map the whole object at
    /// `addr` as Shared memory. The mapping holds a reference to the
    /// object.
    ///
    /// Switchbrew gives the arguments and the reference count only, so
    /// kestrel chose the checks, with the result codes of the other memory
    /// calls: the range page-aligned (InvalidAddress, InvalidSize), the
    /// size the object's (InvalidSize), inside the address space and
    /// outside the heap and alias regions, which hold their own kinds of
    /// memory (InvalidMemoryRegion), free (InvalidCurrentMemory), and the
    /// permission the one the object allows the process
    /// (InvalidNewMemoryPermission). A second mapping of one object stops
    /// as unsupported.
    pub fn map_shared_memory(
        &mut self,
        h: u64,
        addr: u64,
        size: u64,
        perm: u32,
    ) -> Result<(), MapError> {
        let id = self.shared_memory(h)?;
        let end = checked_range(addr, size)?.ok_or(result::INVALID_MEMORY_REGION)?;
        let Object::SharedMemory {
            perm: allowed,
            contents,
            mapped,
        } = self.handles.object(id)
        else {
            unreachable!("a shared memory handle names another object")
        };
        if size != contents.len() as u64 {
            return Err(result::INVALID_SIZE.into());
        }
        if !within(layout::ASLR_REGION, addr, end)
            || overlaps(layout::HEAP_REGION, addr, end)
            || overlaps(layout::ALIAS_REGION, addr, end)
        {
            return Err(result::INVALID_MEMORY_REGION.into());
        }
        if !self.mem.is_free(addr, size) {
            return Err(result::INVALID_CURRENT_MEMORY.into());
        }
        if perm != perm_bits(*allowed) {
            return Err(result::INVALID_NEW_MEMORY_PERMISSION.into());
        }
        if mapped.is_some() {
            return Err(MapError::Unsupported(
                "svcMapSharedMemory of a shared memory object that is already mapped",
            ));
        }
        let (allowed, bytes) = (*allowed, contents.clone());
        self.mem
            .map_tagged(addr, size, allowed, tag(state::SHARED), "shared-memory")
            .and_then(|_| self.mem.poke_bytes(addr, &bytes))
            .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        self.handles.retain(id);
        if let Object::SharedMemory { mapped, .. } = self.handles.object_mut(id) {
            *mapped = Some(addr);
        }
        Ok(())
    }

    /// svcUnmapSharedMemory: undo an svcMapSharedMemory of the same object,
    /// address and size; anything else is InvalidCurrentMemory (kestrel's
    /// choice). The bytes go back to the object.
    pub fn unmap_shared_memory(&mut self, h: u64, addr: u64, size: u64) -> Result<(), u64> {
        let id = self.shared_memory(h)?;
        checked_range(addr, size)?;
        let Object::SharedMemory {
            contents, mapped, ..
        } = self.handles.object(id)
        else {
            unreachable!("a shared memory handle names another object")
        };
        if *mapped != Some(addr) || size != contents.len() as u64 {
            return Err(result::INVALID_CURRENT_MEMORY);
        }
        let bytes = self
            .mem
            .read_vec(addr, size as usize)
            .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        self.mem
            .unmap_range(addr, size)
            .map_err(|_| result::INVALID_CURRENT_MEMORY)?;
        if let Object::SharedMemory {
            contents, mapped, ..
        } = self.handles.object_mut(id)
        {
            *contents = bytes;
            *mapped = None;
        }
        self.release_object(id);
        Ok(())
    }

    /// Give back memory lent by svcCreateTransferMemory once its last
    /// handle is closed: the old permissions return and Locked clears.
    pub(crate) fn release_transfer_memory(&mut self, restore: &[(u64, u64, Perm)]) {
        for &(base, size, perm) in restore {
            // Locked memory cannot be unmapped, so the range is still there.
            self.mem
                .update_range(base, size, |p, t| {
                    *p = perm;
                    t.attr &= !attr::LOCKED;
                })
                .expect("lent memory stays mapped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_loader::Loaded;
    use layout::{ADDRESS_SPACE_BASE as LO, ADDRESS_SPACE_END as HI};

    const MB2: u64 = 0x20_0000;

    fn process() -> Process {
        let image = Loaded {
            entry: 0,
            image_start: 0,
            image_end: 0,
        };
        Process::new(Memory::new(), &image).unwrap()
    }

    #[test]
    fn transfer_memory_locks_the_range_until_its_handle_closes() {
        let mut p = process();
        let heap = p.set_heap_size(MB2).unwrap();
        p.mem.write64(heap + 0x1000, 0xabcd).unwrap();
        let h = p.create_transfer_memory(heap, 0x2000, 1).unwrap();
        let lent = query(&p.mem, heap);
        assert_eq!((lent.size, lent.attr, lent.perm), (0x2000, attr::LOCKED, 1));
        assert_eq!(lent.state, state::NORMAL);
        assert_eq!(p.mem.read64(heap + 0x1000).unwrap(), 0xabcd);
        assert!(
            p.mem.write64(heap + 0x1000, 0).is_err(),
            "read-only while lent"
        );
        // Locked memory cannot be lent again, reprotected or freed.
        let busy = Some(result::INVALID_CURRENT_MEMORY);
        assert_eq!(p.create_transfer_memory(heap, 0x1000, 0).err(), busy);
        assert_eq!(p.set_memory_permission(heap, 0x1000, 3).err(), busy);
        assert_eq!(p.set_heap_size(0).err(), busy);
        p.close_handle(h as u64).unwrap();
        let back = query(&p.mem, heap);
        assert_eq!((back.size, back.attr, back.perm), (MB2, 0, 3));
        p.mem.write64(heap + 0x1000, 1).unwrap();
        assert_eq!(p.set_heap_size(0), Ok(heap));
    }

    #[test]
    fn transfer_memory_restores_each_blocks_own_permission() {
        let mut p = process();
        let heap = p.set_heap_size(MB2).unwrap();
        p.set_memory_permission(heap, 0x1000, 1).unwrap();
        let h = p.create_transfer_memory(heap, 0x2000, 0).unwrap();
        assert_eq!(query(&p.mem, heap).size, 0x2000, "one block while lent");
        p.close_handle(h as u64).unwrap();
        assert_eq!(
            (query(&p.mem, heap).size, query(&p.mem, heap).perm),
            (0x1000, 1)
        );
        assert_eq!(query(&p.mem, heap + 0x1000).perm, 3);
    }

    #[test]
    fn transfer_memory_checks_its_arguments() {
        let mut p = process();
        let heap = p.set_heap_size(MB2).unwrap();
        let tm = |p: &mut Process, addr, size, perm| p.create_transfer_memory(addr, size, perm);
        assert_eq!(
            tm(&mut p, heap + 1, 0x1000, 0),
            Err(result::INVALID_ADDRESS)
        );
        assert_eq!(tm(&mut p, heap, 0x800, 0), Err(result::INVALID_SIZE));
        assert_eq!(tm(&mut p, heap, 0, 0), Err(result::INVALID_SIZE));
        for perm in [2, 4, 5, 7] {
            let r = tm(&mut p, heap, 0x1000, perm);
            assert_eq!(r, Err(result::INVALID_NEW_MEMORY_PERMISSION), "perm {perm}");
        }
        // The stack lacks FlagCanTransfer; free memory and memory past the
        // heap are not lendable.
        let busy = Err(result::INVALID_CURRENT_MEMORY);
        let stack = layout::STACK_TOP - layout::STACK_SIZE;
        assert_eq!(tm(&mut p, stack, 0x1000, 0), busy);
        assert_eq!(tm(&mut p, heap + MB2 - 0x1000, 0x2000, 0), busy);
        assert_eq!(tm(&mut p, HI - 0x1000, 0x2000, 0), busy);
        // Uncached memory has an attribute set.
        p.set_memory_attribute(heap, 0x1000, 8, 8).unwrap();
        assert_eq!(tm(&mut p, heap, 0x1000, 0), busy);
        assert_eq!(p.handles.free(), crate::kernel::MAX_HANDLES - 1);
    }

    #[test]
    fn the_heap_grows_and_shrinks_in_place() {
        let mut p = process();
        let base = p.set_heap_size(MB2).unwrap();
        assert_eq!(base, layout::HEAP_REGION.0);
        p.mem.write64(base + 8, 0x1234).unwrap();
        assert_eq!(p.set_heap_size(3 * MB2), Ok(base));
        assert_eq!(
            p.mem.read64(base + 8).unwrap(),
            0x1234,
            "growing keeps contents"
        );
        p.mem.write8(base + 3 * MB2 - 1, 7).unwrap();
        assert_eq!(p.set_heap_size(MB2), Ok(base));
        assert!(!p.mem.is_mapped(base + MB2));
        assert_eq!(
            p.mem.read64(base + 8).unwrap(),
            0x1234,
            "shrinking keeps the rest"
        );
        assert_eq!(p.set_heap_size(0), Ok(base));
        assert!(!p.mem.is_mapped(base));
        assert_eq!(p.heap_size, 0);
    }

    #[test]
    fn heap_permissions_move_between_none_read_and_read_write() {
        let mut p = process();
        let base = p.set_heap_size(MB2).unwrap();
        p.mem.write8(base + 0x1000, 5).unwrap();
        assert_eq!(p.set_memory_permission(base + 0x1000, 0x1000, 1), Ok(()));
        assert!(p.mem.write8(base + 0x1000, 6).is_err());
        assert_eq!(p.mem.read8(base + 0x1000).unwrap(), 5);
        assert_eq!(query(&p.mem, base + 0x1000).perm, 1);
        assert_eq!(p.set_memory_permission(base + 0x1000, 0x1000, 0), Ok(()));
        assert!(p.mem.read8(base + 0x1000).is_err());
        assert_eq!(p.set_memory_permission(base, MB2, 3), Ok(()));
        assert_eq!(p.mem.read8(base + 0x1000).unwrap(), 5);
        // Back to one block once every page agrees again.
        let q = query(&p.mem, base);
        assert_eq!((q.base, q.size, q.perm), (base, MB2, 3));
    }

    #[test]
    fn permission_and_attribute_changes_are_checked_as_documented() {
        let mut p = process();
        let base = p.set_heap_size(MB2).unwrap();
        assert_eq!(
            p.set_memory_permission(base + 1, 0x1000, 1),
            Err(result::INVALID_ADDRESS)
        );
        assert_eq!(
            p.set_memory_permission(base, 0, 1),
            Err(result::INVALID_SIZE)
        );
        assert_eq!(
            p.set_memory_permission(base, 0x800, 1),
            Err(result::INVALID_SIZE)
        );
        for bad in [2, 4, 5, 7] {
            let r = p.set_memory_permission(base, 0x1000, bad);
            assert_eq!(
                r,
                Err(result::INVALID_NEW_MEMORY_PERMISSION),
                "permission {bad}"
            );
        }
        // Past the end of the heap, and memory that is not heap.
        assert_eq!(
            p.set_memory_permission(base, 2 * MB2, 1),
            Err(result::INVALID_CURRENT_MEMORY)
        );
        let stack = layout::STACK_TOP - layout::STACK_SIZE;
        assert_eq!(
            p.set_memory_permission(stack, 0x1000, 1),
            Err(result::INVALID_CURRENT_MEMORY)
        );
        // Only Uncached (bit 3) can change.
        assert_eq!(p.set_memory_attribute(base, 0x1000, 8, 8), Ok(()));
        assert_eq!(query(&p.mem, base).attr, attr::UNCACHED);
        assert_eq!(p.set_memory_attribute(base, 0x1000, 8, 0), Ok(()));
        assert_eq!(query(&p.mem, base).attr, 0);
        assert_eq!(
            p.set_memory_attribute(base, 0x1000, 1, 1),
            Err(result::INVALID_COMBINATION)
        );
        assert_eq!(
            p.set_memory_attribute(base, 0x1000, 0, 8),
            Err(result::INVALID_COMBINATION)
        );
    }

    #[test]
    fn mapped_memory_moves_to_the_stack_region_and_back() {
        let mut p = process();
        let heap = p.set_heap_size(MB2).unwrap();
        let dst = layout::STACK_REGION.0;
        p.mem.write64(heap + 0x1000, 0xabcd).unwrap();
        assert_eq!(p.map_memory(dst, heap, 0x3000), Ok(()));
        // The destination is Stack memory with the source's bytes; the
        // source is locked and inaccessible.
        assert_eq!(p.mem.read64(dst + 0x1000).unwrap(), 0xabcd);
        let d = query(&p.mem, dst);
        assert_eq!(
            (d.base, d.size, d.state, d.perm),
            (dst, 0x3000, state::STACK, 3)
        );
        let s = query(&p.mem, heap);
        assert_eq!(
            (s.size, s.state, s.attr, s.perm),
            (0x3000, state::NORMAL, attr::LOCKED, 0)
        );
        assert!(p.mem.read8(heap).is_err());
        // A locked heap page cannot be released or reprotected.
        assert_eq!(p.set_heap_size(0), Err(result::INVALID_CURRENT_MEMORY));
        assert_eq!(
            p.set_memory_permission(heap, 0x1000, 1),
            Err(result::INVALID_CURRENT_MEMORY)
        );
        // Unmap the middle page, then the rest; writes come back.
        p.mem.write64(dst + 0x1000, 0x5555).unwrap();
        assert_eq!(p.unmap_memory(dst + 0x1000, heap + 0x1000, 0x1000), Ok(()));
        assert_eq!(p.mem.read64(heap + 0x1000).unwrap(), 0x5555);
        assert!(!p.mem.is_mapped(dst + 0x1000));
        assert!(p.mem.read8(heap).is_err(), "the outer pages stay mapped");
        assert_eq!(p.unmap_memory(dst, heap, 0x1000), Ok(()));
        assert_eq!(p.unmap_memory(dst + 0x2000, heap + 0x2000, 0x1000), Ok(()));
        assert!(p.mapped_memory.is_empty());
        let s = query(&p.mem, heap);
        assert_eq!((s.size, s.attr, s.perm), (MB2, 0, 3));
        assert_eq!(p.set_heap_size(0), Ok(heap));
    }

    #[test]
    fn map_memory_is_checked_as_documented() {
        let mut p = process();
        let heap = p.set_heap_size(MB2).unwrap();
        let dst = layout::STACK_REGION.0;
        assert_eq!(
            p.map_memory(dst + 4, heap, 0x1000),
            Err(result::INVALID_ADDRESS)
        );
        assert_eq!(p.map_memory(dst, heap, 0), Err(result::INVALID_SIZE));
        // Only into the stack region, only over free pages, only from heap.
        let alias = layout::ALIAS_REGION.0;
        assert_eq!(
            p.map_memory(alias, heap, 0x1000),
            Err(result::INVALID_MEMORY_REGION)
        );
        let main_stack = layout::STACK_TOP - layout::STACK_SIZE;
        assert_eq!(
            p.map_memory(main_stack, heap, 0x1000),
            Err(result::INVALID_CURRENT_MEMORY)
        );
        assert_eq!(
            p.map_memory(dst, main_stack, 0x1000),
            Err(result::INVALID_CURRENT_MEMORY)
        );
        assert_eq!(
            p.map_memory(dst, heap + MB2, 0x1000),
            Err(result::INVALID_CURRENT_MEMORY)
        );
        // Unmapping needs the pairing that was mapped.
        assert_eq!(p.map_memory(dst, heap, 0x2000), Ok(()));
        assert_eq!(
            p.unmap_memory(dst, heap + 0x1000, 0x1000),
            Err(result::INVALID_CURRENT_MEMORY)
        );
        assert_eq!(
            p.unmap_memory(dst, heap, 0x3000),
            Err(result::INVALID_CURRENT_MEMORY)
        );
        assert_eq!(p.unmap_memory(dst, heap, 0x2000), Ok(()));
    }

    #[test]
    fn physical_memory_fills_only_the_gaps_of_the_alias_range() {
        let mut p = process();
        let a = layout::ALIAS_REGION.0;
        let used = p.used_memory();
        assert_eq!(p.map_physical_memory(a + 0x1000, 0x2000), Ok(()));
        p.mem.write64(a + 0x1000, 77).unwrap();
        let q = query(&p.mem, a + 0x1000);
        assert_eq!(
            (q.base, q.size, q.state, q.perm),
            (a + 0x1000, 0x2000, state::NORMAL, 3)
        );
        // Mapping a larger range adds only the missing pages.
        assert_eq!(p.map_physical_memory(a, 0x5000), Ok(()));
        assert_eq!(p.used_memory(), used + 0x5000);
        assert_eq!(p.mem.read64(a + 0x1000).unwrap(), 77);
        assert_eq!(query(&p.mem, a).size, 0x5000);
        // Unmapping releases what is mapped, gaps included.
        assert_eq!(p.unmap_physical_memory(a + 0x1000, 0x1000), Ok(()));
        assert_eq!(p.unmap_physical_memory(a, 0x6000), Ok(()));
        assert_eq!(p.used_memory(), used);
        assert!(p.mem.is_free(a, 0x6000));
    }

    #[test]
    fn physical_memory_is_checked_as_documented() {
        let mut p = process();
        let a = layout::ALIAS_REGION.0;
        assert_eq!(
            p.map_physical_memory(a + 1, 0x1000),
            Err(result::INVALID_ADDRESS)
        );
        assert_eq!(p.map_physical_memory(a, 0), Err(result::INVALID_SIZE));
        assert_eq!(p.map_physical_memory(a, 0x1800), Err(result::INVALID_SIZE));
        let heap = layout::HEAP_REGION.0;
        assert_eq!(
            p.map_physical_memory(heap, 0x1000),
            Err(result::INVALID_MEMORY_REGION)
        );
        let wrap = 0u64.wrapping_sub(0x1000);
        assert_eq!(
            p.map_physical_memory(wrap, 0x2000),
            Err(result::INVALID_MEMORY_REGION)
        );
        let too_much = layout::TOTAL_MEMORY;
        assert_eq!(
            p.map_physical_memory(a, too_much),
            Err(result::LIMIT_REACHED)
        );
        assert!(p.mem.is_free(a, 0x1000));
    }

    #[test]
    fn heap_sizes_are_checked_as_documented() {
        let mut p = process();
        assert_eq!(p.set_heap_size(0x1000), Err(result::INVALID_SIZE));
        assert_eq!(p.set_heap_size((1 << 32) + MB2), Err(result::INVALID_SIZE));
        // Within 4 GiB and the heap region, but over the memory total.
        let over = (layout::TOTAL_MEMORY + MB2) & !(MB2 - 1);
        assert_eq!(p.set_heap_size(over), Err(result::LIMIT_REACHED));
        assert_eq!(p.heap_size, 0);
        // A reprotected page cannot be released.
        let base = p.set_heap_size(2 * MB2).unwrap();
        p.mem
            .update_range(base + MB2, 0x1000, |perm, _| *perm = Perm::R)
            .unwrap();
        assert_eq!(p.set_heap_size(MB2), Err(result::INVALID_CURRENT_MEMORY));
        assert_eq!(p.heap_size, 2 * MB2);
    }

    #[test]
    fn neighbours_of_the_same_kind_merge() {
        let mut m = Memory::new();
        m.map_tagged(LO, 0x2000, Perm::RX, tag(state::CODE), "a")
            .unwrap();
        m.map_tagged(LO + 0x2000, 0x1000, Perm::RX, tag(state::CODE), "b")
            .unwrap();
        m.map_tagged(LO + 0x3000, 0x1000, Perm::R, tag(state::CODE), "c")
            .unwrap();
        let q = query(&m, LO + 0x2800);
        assert_eq!(
            (q.base, q.size, q.state, q.perm),
            (LO, 0x3000, state::CODE, 5)
        );
        let q = query(&m, LO + 0x3000);
        assert_eq!((q.base, q.size, q.perm), (LO + 0x3000, 0x1000, 1));
    }

    #[test]
    fn gaps_are_free_inside_the_address_space_and_inaccessible_outside() {
        let mut m = Memory::new();
        m.map_tagged(0x20_0000, 0x1000, Perm::RW, tag(state::CODE_DATA), "elf")
            .unwrap();
        m.map_tagged(LO + 0x10000, 0x1000, Perm::RW, tag(state::NORMAL), "x")
            .unwrap();
        let q = query(&m, LO);
        assert_eq!((q.base, q.size, q.state), (LO, 0x10000, state::FREE));
        let q = query(&m, LO + 0x20000);
        assert_eq!(
            (q.base, q.size, q.state),
            (LO + 0x11000, HI - LO - 0x11000, state::FREE)
        );
        let q = query(&m, 0x1000);
        assert_eq!(
            (q.base, q.size, q.state),
            (0, 0x20_0000, state::INACCESSIBLE)
        );
        let q = query(&m, 0x30_0000);
        assert_eq!((q.base, q.size), (0x20_1000, LO - 0x20_1000));
        let q = query(&m, u64::MAX);
        assert_eq!(
            (q.base, q.size, q.state),
            (HI, 0u64.wrapping_sub(HI), state::INACCESSIBLE)
        );
    }

    #[test]
    fn memory_info_layout() {
        let i = MemInfo {
            base: 1,
            size: 2,
            state: 3,
            attr: 4,
            perm: 5,
        };
        let b = i.to_bytes();
        assert_eq!(b[0], 1);
        assert_eq!(b[8], 2);
        assert_eq!(b[0x10], 3);
        assert_eq!(b[0x14], 4);
        assert_eq!(b[0x18], 5);
        assert!(b[0x1c..].iter().all(|&x| x == 0));
    }

    /// svcMapSharedMemory or svcUnmapSharedMemory; the result code.
    fn shared(p: &mut Process, n: u16, args: &[u64]) -> u64 {
        p.cpu.x[..args.len()].copy_from_slice(args);
        assert_eq!(crate::svc::dispatch(p, n), None);
        p.cpu.x[0]
    }

    #[test]
    fn shared_memory_maps_whole_with_its_permission_outside_heap_and_alias() {
        use crate::svc::nr::{MAP_SHARED_MEMORY as MAP, UNMAP_SHARED_MEMORY as UNMAP};
        let mut p = process();
        let id = p.create_shared_memory(0x2000, Perm::R);
        if let Object::SharedMemory { contents, .. } = p.handles.object_mut(id) {
            contents[0x1ff8..].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        }
        let h = p.handles.add_handle(id, Kind::SharedMemory).unwrap() as u64;
        let at = 0x4000_0000;
        let bad = |p: &mut Process, h, addr, size, perm| shared(p, MAP, &[h, addr, size, perm]);
        let main = p.main_thread as u64;
        assert_eq!(bad(&mut p, main, at, 0x2000, 1), result::INVALID_HANDLE);
        assert_eq!(bad(&mut p, h, at + 8, 0x2000, 1), result::INVALID_ADDRESS);
        assert_eq!(bad(&mut p, h, at, 0x1000, 1), result::INVALID_SIZE);
        assert_eq!(
            bad(&mut p, h, at, 0x2000, 3),
            result::INVALID_NEW_MEMORY_PERMISSION
        );
        let heap = layout::HEAP_REGION.0 + 0x10_0000;
        let alias = layout::ALIAS_REGION.0;
        assert_eq!(
            bad(&mut p, h, heap, 0x2000, 1),
            result::INVALID_MEMORY_REGION
        );
        assert_eq!(
            bad(&mut p, h, alias, 0x2000, 1),
            result::INVALID_MEMORY_REGION
        );
        assert_eq!(
            bad(&mut p, h, HI - 0x1000, 0x2000, 1),
            result::INVALID_MEMORY_REGION
        );
        p.mem
            .map(at + 0x1000, 0x1000, Perm::RW, "in the way")
            .unwrap();
        assert_eq!(
            bad(&mut p, h, at, 0x2000, 1),
            result::INVALID_CURRENT_MEMORY
        );
        p.mem.unmap_range(at + 0x1000, 0x1000).unwrap();
        assert_eq!(shared(&mut p, MAP, &[h, at, 0x2000, 1]), 0);
        let info = query(&p.mem, at);
        assert_eq!(
            (info.base, info.size, info.state, info.perm),
            (at, 0x2000, state::SHARED, 1)
        );
        assert_eq!(p.mem.read64(at + 0x1ff8), Ok(0x1122_3344_5566_7788));
        // One mapping at a time.
        p.cpu.x[..4].copy_from_slice(&[h, at + 0x10_0000, 0x2000, 1]);
        match crate::svc::dispatch(&mut p, MAP) {
            Some(crate::Stop::Error(e)) => assert!(e.contains("already mapped"), "{e}"),
            other => panic!("expected a stop, got {other:?}"),
        }
        // The mapping keeps the object alive after its handles close.
        let h2 = p.handles.add_handle(id, Kind::SharedMemory).unwrap() as u64;
        assert_eq!(shared(&mut p, crate::svc::nr::CLOSE_HANDLE, &[h]), 0);
        assert_eq!(
            shared(&mut p, UNMAP, &[h2, at + 0x1000, 0x1000]),
            result::INVALID_CURRENT_MEMORY
        );
        assert_eq!(shared(&mut p, UNMAP, &[h2, at, 0x2000]), 0);
        assert_eq!(query(&p.mem, at).state, state::FREE);
        assert_eq!(
            shared(&mut p, UNMAP, &[h2, at, 0x2000]),
            result::INVALID_CURRENT_MEMORY
        );
        // Mapped again, the bytes are still there.
        assert_eq!(shared(&mut p, MAP, &[h2, at, 0x2000, 1]), 0);
        assert_eq!(p.mem.read64(at + 0x1ff8), Ok(0x1122_3344_5566_7788));
        assert_eq!(shared(&mut p, UNMAP, &[h2, at, 0x2000]), 0);
        // The service's reference remains after the last handle closes.
        assert_eq!(shared(&mut p, crate::svc::nr::CLOSE_HANDLE, &[h2]), 0);
        assert!(matches!(p.handles.object(id), Object::SharedMemory { .. }));
    }
}
