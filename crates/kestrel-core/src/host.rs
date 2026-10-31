//! Host virtual memory for the guest memory arena: reserve one range of
//! address space, then commit pages in it as they are needed.
//!
//! Reserved pages take address space only and fault on access; committed
//! pages are readable, writable and start zeroed. A reservation is released
//! as a whole when dropped. The core crates use only the standard library,
//! so the few OS calls are declared here directly:
//!
//! - Linux and macOS: `mmap` with `PROT_NONE` to reserve, `mprotect` to
//!   commit (POSIX; the flag values differ between the two, see below).
//! - Windows: `VirtualAlloc` with `MEM_RESERVE`, then `MEM_COMMIT`.
//! - Anything else: one zeroed allocation from `std::alloc`, committed
//!   from the start. No supported target uses it; it keeps others building.

use std::io;

/// One reserved range of host address space.
pub struct Reservation {
    base: *mut u8,
    len: usize,
}

// The reservation is plain host memory; who may touch which bytes is the
// arena's business.
unsafe impl Send for Reservation {}
unsafe impl Sync for Reservation {}

impl Reservation {
    /// Reserve `len` bytes (a multiple of the host page size).
    pub fn new(len: usize) -> io::Result<Self> {
        let base = os::reserve(len)?;
        Ok(Self { base, len })
    }

    pub fn base(&self) -> *mut u8 {
        self.base
    }

    /// Make `[offset, offset + len)` readable and writable. Both must be
    /// multiples of the host page size; pages committed for the first time
    /// read as zero.
    pub fn commit(&self, offset: usize, len: usize) -> io::Result<()> {
        assert!(offset.checked_add(len).is_some_and(|end| end <= self.len));
        // SAFETY: the range lies inside this reservation.
        unsafe { os::commit(self.base.add(offset), len) }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // SAFETY: base and len are exactly what reserve returned.
        unsafe { os::release(self.base, self.len) }
    }
}

/// The host's page size: the unit of `commit`, and of what a hypervisor
/// maps. 4 KiB on x86_64 and Windows; 16 KiB on Apple Silicon (macOS and
/// Asahi Linux); some aarch64 Linux kernels use 64 KiB.
pub fn page_size() -> usize {
    os::page_size()
}

#[cfg(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
mod os {
    use std::ffi::{c_int, c_long, c_void};
    use std::io;

    const PROT_NONE: c_int = 0;
    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const MAP_PRIVATE: c_int = 0x02;
    /// MAP_ANONYMOUS: 0x20 in Linux's asm-generic and x86 headers, MAP_ANON
    /// 0x1000 in macOS's <sys/mman.h>.
    #[cfg(target_os = "linux")]
    const MAP_ANONYMOUS: c_int = 0x20;
    #[cfg(target_os = "macos")]
    const MAP_ANONYMOUS: c_int = 0x1000;
    /// _SC_PAGESIZE: 30 in glibc and musl, 29 in macOS's <unistd.h>.
    #[cfg(target_os = "linux")]
    const SC_PAGESIZE: c_int = 30;
    #[cfg(target_os = "macos")]
    const SC_PAGESIZE: c_int = 29;
    const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

    /// Reserved pages cost no memory.
    #[cfg(test)]
    pub const LAZY: bool = true;

    extern "C" {
        // off_t is 64-bit on every target this module is built for.
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
        fn sysconf(name: c_int) -> c_long;
    }

    pub fn reserve(len: usize) -> io::Result<*mut u8> {
        // PROT_NONE pages are not counted against the host's commit limit,
        // so reserving more than the host has is fine.
        // SAFETY: a new anonymous mapping touches no existing memory.
        let p = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_NONE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == MAP_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(p.cast())
        }
    }

    /// # Safety
    /// `[p, p + len)` must lie inside a reservation.
    pub unsafe fn commit(p: *mut u8, len: usize) -> io::Result<()> {
        if mprotect(p.cast(), len, PROT_READ | PROT_WRITE) == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// # Safety
    /// `p` and `len` must be a whole reservation, no longer used.
    pub unsafe fn release(p: *mut u8, len: usize) {
        munmap(p.cast(), len);
    }

    pub fn page_size() -> usize {
        // SAFETY: sysconf has no preconditions.
        let n = unsafe { sysconf(SC_PAGESIZE) };
        if n > 0 {
            n as usize
        } else {
            4096
        }
    }
}

#[cfg(windows)]
mod os {
    use std::ffi::c_void;
    use std::io;

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_RELEASE: u32 = 0x8000;
    const PAGE_NOACCESS: u32 = 0x01;
    const PAGE_READWRITE: u32 = 0x04;

    /// Reserved pages cost no memory.
    #[cfg(test)]
    pub const LAZY: bool = true;

    /// SYSTEM_INFO from <sysinfoapi.h>.
    #[repr(C)]
    struct SystemInfo {
        processor_architecture: u16,
        reserved: u16,
        page_size: u32,
        minimum_application_address: *mut c_void,
        maximum_application_address: *mut c_void,
        active_processor_mask: usize,
        number_of_processors: u32,
        processor_type: u32,
        allocation_granularity: u32,
        processor_level: u16,
        processor_revision: u16,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
        fn VirtualFree(addr: *mut c_void, size: usize, kind: u32) -> i32;
        fn GetSystemInfo(info: *mut SystemInfo);
    }

    pub fn reserve(len: usize) -> io::Result<*mut u8> {
        // Reserved pages count against neither memory nor the commit limit.
        // SAFETY: a new reservation touches no existing memory.
        let p = unsafe { VirtualAlloc(std::ptr::null_mut(), len, MEM_RESERVE, PAGE_NOACCESS) };
        if p.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(p.cast())
        }
    }

    /// # Safety
    /// `[p, p + len)` must lie inside a reservation.
    pub unsafe fn commit(p: *mut u8, len: usize) -> io::Result<()> {
        if VirtualAlloc(p.cast(), len, MEM_COMMIT, PAGE_READWRITE).is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// # Safety
    /// `p` must be a whole reservation, no longer used.
    pub unsafe fn release(p: *mut u8, _len: usize) {
        VirtualFree(p.cast(), 0, MEM_RELEASE);
    }

    pub fn page_size() -> usize {
        // SAFETY: GetSystemInfo fills the structure and cannot fail; zeroed
        // is a valid SystemInfo.
        let mut info: SystemInfo = unsafe { std::mem::zeroed() };
        unsafe { GetSystemInfo(&mut info) };
        if info.page_size > 0 {
            info.page_size as usize
        } else {
            4096
        }
    }
}

#[cfg(not(any(
    windows,
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
)))]
mod os {
    use std::alloc::{alloc_zeroed, dealloc, Layout};
    use std::io;

    const ALIGN: usize = 64 << 10;

    /// Everything is allocated up front.
    #[cfg(test)]
    pub const LAZY: bool = false;

    pub fn reserve(len: usize) -> io::Result<*mut u8> {
        let layout = Layout::from_size_align(len, ALIGN)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: the layout is non-zero-sized (the arena never reserves 0).
        let p = unsafe { alloc_zeroed(layout) };
        if p.is_null() {
            Err(io::ErrorKind::OutOfMemory.into())
        } else {
            Ok(p)
        }
    }

    /// # Safety
    /// Always safe here: everything is committed from the start.
    pub unsafe fn commit(_p: *mut u8, _len: usize) -> io::Result<()> {
        Ok(())
    }

    /// # Safety
    /// `p` and `len` must be a whole reservation, no longer used.
    pub unsafe fn release(p: *mut u8, len: usize) {
        dealloc(p, Layout::from_size_align_unchecked(len, ALIGN));
    }

    pub fn page_size() -> usize {
        4096
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_pages_are_zero_and_writable() {
        let page = page_size();
        assert!(page.is_power_of_two() && page >= 4096);
        let r = Reservation::new(64 * page).unwrap();
        assert_eq!(r.base() as usize % page, 0);
        r.commit(8 * page, 2 * page).unwrap();
        // SAFETY: those two pages are committed.
        unsafe {
            let p = r.base().add(8 * page);
            assert_eq!(*p.add(2 * page - 1), 0);
            *p.add(page + 3) = 7;
            assert_eq!(*p.add(page + 3), 7);
        }
        // Committing again keeps the contents.
        r.commit(8 * page, 2 * page).unwrap();
        assert_eq!(unsafe { *r.base().add(9 * page + 3) }, 7);
    }

    #[test]
    fn reserving_more_than_the_host_has_costs_only_address_space() {
        // 64 GiB: more than a CI runner's memory, so this only works if
        // nothing is committed up front.
        if cfg!(target_pointer_width = "64") && os::LAZY {
            let r = Reservation::new(64 << 30).unwrap();
            r.commit(0, page_size()).unwrap();
        }
    }
}
