//! A 128-bit compare-and-swap on host memory, for the guest's 128-bit
//! exclusive pairs (LDXP/STXP of two X registers) and CASP of two X
//! registers when guest cores run on several host threads. Rust has no
//! stable 128-bit atomic, so each host gets its own instruction sequence:
//!
//! - x86_64: `LOCK CMPXCHG16B` (Intel SDM, "CMPXCHG8B/CMPXCHG16B"). Every
//!   CPU that runs 64-bit Windows 8.1 or later has it, as do the x86_64
//!   CPUs of every supported Linux and macOS host.
//! - aarch64: a loop of LDAXP and STLXP (Arm ARM, "Load-Exclusive and
//!   Store-Exclusive instructions"), which every Armv8-A CPU has; CASP
//!   would need FEAT_LSE, which older Linux boards lack. When the compare
//!   fails, the value read is stored back, so the read itself is
//!   single-copy atomic.
//! - anything else: one process-wide lock around a compare and two 64-bit
//!   stores (kestrel's choice). Every 128-bit compare-and-swap is then
//!   atomic with every other, but not with a plain store racing it.

/// Replace the 16 bytes at `p` with `new` if they hold `current`:
/// `Ok(current)` if replaced, `Err(seen)` with what was found if not.
///
/// # Safety
/// `p` must be valid for reads and writes of 16 bytes and aligned to 16.
#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn compare_exchange(p: *mut u8, current: u128, new: u128) -> Result<u128, u128> {
    let (mut lo, mut hi) = (current as u64, (current >> 64) as u64);
    let ok: u64;
    // RBX belongs to LLVM and cannot be named, so the low half of `new`
    // goes in through another register and is swapped in and out around
    // the instruction. LLVM may still pick RBX for a register operand
    // (it does on Windows), so nothing that must survive the swap is left
    // to it: the address is in RDI and the result in RCX's low byte.
    // SAFETY: the caller guarantees `p`.
    unsafe {
        std::arch::asm!(
            "xchg {new_lo}, rbx",
            "lock cmpxchg16b xmmword ptr [rdi]",
            "sete cl",
            "mov rbx, {new_lo}",
            in("rdi") p,
            new_lo = inout(reg) new as u64 => _,
            inout("rcx") (new >> 64) as u64 => ok,
            inout("rax") lo,
            inout("rdx") hi,
            options(nostack),
        );
    }
    let seen = u128::from(hi) << 64 | u128::from(lo);
    if ok as u8 != 0 {
        Ok(seen)
    } else {
        Err(seen)
    }
}

/// See the x86_64 version.
///
/// # Safety
/// `p` must be valid for reads and writes of 16 bytes and aligned to 16.
#[cfg(target_arch = "aarch64")]
pub(crate) unsafe fn compare_exchange(p: *mut u8, current: u128, new: u128) -> Result<u128, u128> {
    let (lo, hi): (u64, u64);
    // SAFETY: the caller guarantees `p`.
    unsafe {
        std::arch::asm!(
            "2:",
            "ldaxp {lo}, {hi}, [{p}]",
            "cmp {lo}, {cur_lo}",
            "ccmp {hi}, {cur_hi}, #0, eq",
            "b.ne 3f",
            "stlxp {status:w}, {new_lo}, {new_hi}, [{p}]",
            "cbnz {status:w}, 2b",
            "b 4f",
            "3:",
            "stlxp {status:w}, {lo}, {hi}, [{p}]",
            "cbnz {status:w}, 2b",
            "4:",
            p = in(reg) p,
            cur_lo = in(reg) current as u64,
            cur_hi = in(reg) (current >> 64) as u64,
            new_lo = in(reg) new as u64,
            new_hi = in(reg) (new >> 64) as u64,
            lo = out(reg) lo,
            hi = out(reg) hi,
            status = out(reg) _,
            options(nostack),
        );
    }
    let seen = u128::from(hi) << 64 | u128::from(lo);
    if seen == current {
        Ok(seen)
    } else {
        Err(seen)
    }
}

/// See the x86_64 version.
///
/// # Safety
/// `p` must be valid for reads and writes of 16 bytes and aligned to 16.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(crate) unsafe fn compare_exchange(p: *mut u8, current: u128, new: u128) -> Result<u128, u128> {
    use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the caller guarantees `p`; both halves are aligned to 8.
    let (lo, hi) = unsafe {
        (
            AtomicU64::from_ptr(p.cast()),
            AtomicU64::from_ptr(p.add(8).cast()),
        )
    };
    // Guest memory is little-endian on every host.
    let half = |a: &AtomicU64| u128::from(u64::from_le(a.load(SeqCst)));
    let seen = half(hi) << 64 | half(lo);
    if seen != current {
        return Err(seen);
    }
    lo.store((new as u64).to_le(), SeqCst);
    hi.store(((new >> 64) as u64).to_le(), SeqCst);
    Ok(seen)
}
