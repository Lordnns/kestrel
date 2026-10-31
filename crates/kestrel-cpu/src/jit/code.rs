//! Executable memory for translated blocks, never writable and executable
//! at once (the owner's rule of 2026-10-31): a page being written is
//! read-write, and becomes read-execute before any code in it runs.
//!
//! Blocks are appended one after another. Writing a block makes the pages
//! it lands on read-write, copies it, then makes them read-execute again;
//! a page may hold the end of an earlier block, which cannot run meanwhile
//! because the one thread using this memory is the one writing it.
//!
//! The OS calls are declared here directly, as `kestrel_core`'s host
//! module does, so the crate still uses the standard library only:
//! - Linux: `mmap`, `mprotect`, `munmap` (the values from Linux's
//!   asm-generic headers).
//! - Windows: `VirtualAlloc`, `VirtualProtect`, `VirtualFree` and
//!   `FlushInstructionCache` (kernel32), as Microsoft documents them.

use std::io;

/// Host page size on the x86_64 hosts the recompiler runs on.
const PAGE: usize = 4096;

pub struct CodeMemory {
    base: *mut u8,
    size: usize,
    used: usize,
}

impl CodeMemory {
    /// Reserve `size` bytes (a multiple of the page size), read-write.
    pub fn new(size: usize) -> io::Result<Self> {
        let base = os::alloc(size)?;
        Ok(Self {
            base,
            size,
            used: 0,
        })
    }

    /// Copy `code` in and return its address, executable; `None` when it
    /// does not fit.
    pub fn add(&mut self, code: &[u8]) -> io::Result<Option<*const u8>> {
        // Blocks start 16-byte aligned, as compilers align functions.
        let at = self.used.next_multiple_of(16);
        if at + code.len() > self.size {
            return Ok(None);
        }
        let first = at / PAGE * PAGE;
        let end = (at + code.len()).next_multiple_of(PAGE);
        // SAFETY: [first, end) lies inside the allocation, and no code in
        // it runs while it is writable (one thread uses this memory).
        unsafe {
            let pages = self.base.add(first);
            os::protect(pages, end - first, false)?;
            std::ptr::copy_nonoverlapping(code.as_ptr(), self.base.add(at), code.len());
            os::protect(pages, end - first, true)?;
            os::flush(self.base.add(at), code.len());
        }
        self.used = at + code.len();
        Ok(Some(unsafe { self.base.add(at) }))
    }

    /// Forget every block: the memory is reused from the start.
    pub fn clear(&mut self) {
        self.used = 0;
    }

    /// Bytes of code written.
    pub fn used(&self) -> usize {
        self.used
    }
}

impl Drop for CodeMemory {
    fn drop(&mut self) {
        // SAFETY: base and size are what alloc returned.
        unsafe { os::free(self.base, self.size) }
    }
}

#[cfg(target_os = "linux")]
mod os {
    use std::ffi::{c_int, c_void};
    use std::io;

    const PROT_READ: c_int = 1;
    const PROT_WRITE: c_int = 2;
    const PROT_EXEC: c_int = 4;
    const MAP_PRIVATE: c_int = 0x02;
    const MAP_ANONYMOUS: c_int = 0x20;
    const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

    extern "C" {
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
    }

    pub fn alloc(len: usize) -> io::Result<*mut u8> {
        // SAFETY: a new anonymous mapping touches no existing memory.
        let p = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
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
    /// `[p, p + len)` must be whole pages of an allocation.
    pub unsafe fn protect(p: *mut u8, len: usize, exec: bool) -> io::Result<()> {
        let prot = if exec {
            PROT_READ | PROT_EXEC
        } else {
            PROT_READ | PROT_WRITE
        };
        if mprotect(p.cast(), len, prot) == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// x86 keeps instruction fetch coherent with stores.
    pub unsafe fn flush(_p: *mut u8, _len: usize) {}

    /// # Safety
    /// `p` and `len` must be an allocation from `alloc`.
    pub unsafe fn free(p: *mut u8, len: usize) {
        munmap(p.cast(), len);
    }
}

#[cfg(windows)]
mod os {
    use std::ffi::c_void;
    use std::io;

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_RELEASE: u32 = 0x8000;
    const PAGE_READWRITE: u32 = 0x04;
    const PAGE_EXECUTE_READ: u32 = 0x20;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
        fn VirtualProtect(addr: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
        fn VirtualFree(addr: *mut c_void, size: usize, kind: u32) -> i32;
        fn FlushInstructionCache(process: *mut c_void, addr: *const c_void, size: usize) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
    }

    pub fn alloc(len: usize) -> io::Result<*mut u8> {
        // SAFETY: a new allocation touches no existing memory.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        if p.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(p.cast())
        }
    }

    /// # Safety
    /// `[p, p + len)` must be whole pages of an allocation.
    pub unsafe fn protect(p: *mut u8, len: usize, exec: bool) -> io::Result<()> {
        let prot = if exec {
            PAGE_EXECUTE_READ
        } else {
            PAGE_READWRITE
        };
        let mut old = 0;
        if VirtualProtect(p.cast(), len, prot, &mut old) != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Windows asks for it after writing code ("FlushInstructionCache").
    pub unsafe fn flush(p: *mut u8, len: usize) {
        FlushInstructionCache(GetCurrentProcess(), p.cast(), len);
    }

    /// # Safety
    /// `p` must be an allocation from `alloc`.
    pub unsafe fn free(p: *mut u8, _len: usize) {
        VirtualFree(p.cast(), 0, MEM_RELEASE);
    }
}
