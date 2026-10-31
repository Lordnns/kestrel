# Architecture

```
kestrel-cli ──► kestrel-hle ──► kestrel-cpu ──► kestrel-core
                    │                             ▲
                    └──► kestrel-loader ──────────┘
```

| Crate            | Responsibility                                                    |
|------------------|-------------------------------------------------------------------|
| `kestrel-core`   | Guest memory: the host arena (`Arena`), the address space (`Memory`), permissions, faults |
| `kestrel-cpu`    | `CpuState`, the `CpuBackend` trait, the portable interpreter      |
| `kestrel-loader` | Executable loaders: ELF test programs, homebrew NRO               |
| `kestrel-hle`    | Process model and supervisor-call (SVC) handling                  |
| `kestrel-testsuite` | Guest test programs embedded in the binary, with expected output |
| `kestrel-cli`    | Command-line front end                                            |

## CPU backends

```rust
pub trait CpuBackend: Send {
    fn run(&mut self, state: &mut CpuState, mem: &Memory, max_steps: u64) -> Exit;
    fn invalidate(&mut self, addr: u64, len: u64);
}
```

A backend runs one guest core until something needs the OS (`Exit::Svc`), a
breakpoint, a fault, an instruction it does not support, or the step budget.
The HLE layer handles the exit and calls `run` again. With the cores on
host threads ("Threaded cores" below) each core has its own backend on its
own host thread, hence `Send`, and all of them run on one shared `Memory`
at once, hence `&Memory`: a backend's guest atomics, exclusives and
barriers must hold between host threads (the interpreter's do, below).

### Interpreter (all hosts)

`crates/kestrel-cpu/src/interp/` decodes by the top-level A64 encoding group
(bits 28..25), one file per group; Advanced SIMD has its own directory,
`interp/neon/`, with one set of lane helpers for every element size. Unsupported encodings return
`Exit::Undefined { pc, insn }` rather than guessing, so gaps are explicit.
Encodings that decode but use something not modelled yet (a half-precision
conversion under FPCR.AHP, for example) return `Exit::Unsupported` with an
`Unmodelled` reason. Both
variants keep `Exit` at 24 bytes, because every interpreter step returns it.

Guest atomics reach memory through the host's compare-and-swap
(`interp/ldst.rs`), so they hold when other cores run on other host
threads: CAS, CASP and the LSE atomics are compare-and-swap loops, and
the exclusive monitor remembers the address and the value a
load-exclusive read, the store-exclusive succeeding only if a
compare-and-swap from that value does (a store by another core or the
kernel in between fails it; one writing back the same value does not,
which Arm permits). Pairs of X registers use the 128-bit compare-and-swap.
DMB and DSB are full host fences, a load-acquire is followed by an
acquire fence, and a store-release has a release fence before it and a
full fence after it, so Arm's ordering holds on an aarch64 host too. A
thread switch still clears the monitor.

### Floating point

`crates/kestrel-cpu/src/float.rs` implements the Arm floating-point
pseudocode on raw bit patterns, independent of any backend: NaN processing,
default NaN, flush-to-zero, FPRound, conversions, round-to-integral and the
reciprocal estimates. Single- and double-precision IEEE arithmetic runs on
the host in round-to-nearest, which matches Arm for finite results;
everything around it (NaN choice, host-generated NaNs, FZ) is applied here,
so x86 and ARM hosts give the same bits. Half precision (FEAT_FP16) is
computed exactly in integers and rounded here, in every rounding mode, with
FPCR.FZ16 as its flush-to-zero control. NEON floating-point lanes call the same functions. Future backends
should call them too, or be differentially tested against them.

### Native (aarch64 hosts, planned)

Guest and host share the instruction set, so guest EL0 code can run directly
on the host CPU inside a hardware-virtualised sandbox:

- Linux: KVM (`/dev/kvm`)
- macOS: Hypervisor.framework
- Windows on ARM: Windows Hypervisor Platform

The VM runs only guest user code. SVCs trap to the host, which forwards them
to `kestrel-hle`. Guest memory is already the arena such a VM needs ("Memory"
below, with what the backend must do with it). macOS comes first, on the
owner's Mac; then Linux and Windows.

### JIT (x86_64 hosts, planned)

Translate guest basic blocks to x86_64, cache by guest PC, invalidate on
writes to code pages (`CpuBackend::invalidate`). Exclusives and memory
ordering need care because x86 has a stronger memory model than ARM. Guest
memory accesses go to the arena directly ("Memory" below).

### Correctness

Faster backends are checked against the interpreter by running the same
program on both and comparing full `CpuState` at every SVC (and, in debug
builds, every N instructions).

## Memory

Guest memory is one arena of host memory shared by every backend and by
every host thread running a guest core (`PLAN.md` phase 3; done on Linux on
2026-10-30, before phase 3 on macOS, by the owner's decision):

```
guest address ──page table──► frame offset ──+ arena base──► host address
 (39 bits, 4 KiB pages)          (Memory)       (Arena: guest physical memory)
```

- **The arena** (`kestrel_core::arena::Arena`) is the guest's physical
  memory: one range of host address space, reserved whole (16 GiB by
  default, `DEFAULT_ARENA_SIZE`: address space only), its start aligned to
  2 MiB (`ARENA_ALIGN`) and committed from the start in 2 MiB steps as
  4 KiB frames are first handed out. The OS calls (`mmap` and `mprotect`
  on Linux and macOS, `VirtualAlloc` on Windows, `std::alloc` elsewhere)
  are declared in `kestrel_core::host`, so the core crates still use std
  only. A freed frame goes back to a free list and is zeroed when handed
  out again; committed memory stays committed while the arena lives, so a
  late access from another core never touches memory the host took back.
- **The address space** (`kestrel_core::Memory`) maps guest pages to
  frames with a two-level page table (a top table of 2^18 leaves, each
  mapping 512 pages, 2 MiB): an entry is the frame's offset in the arena,
  the page's permission and a present bit. Beside it are the regions the
  OS layer works with (permission, memory type and attribute, name), as
  the memory calls create, split and change them. Splitting a region
  moves no bytes. Addresses above 39 bits (`ADDRESS_BITS`, the largest
  Switch address space) cannot be mapped.
- **Concurrency.** Every `Memory` method takes `&self`, and `Memory` and
  `Arena` are `Send + Sync`. Reads, writes and fetches take no lock: the
  page table's entries and leaf pointers are atomics, and every byte of
  the arena is read and written with host atomics (relaxed; plain loads
  and stores on x86_64 and aarch64), so racing guest cores are defined
  behaviour on the host as on the console. An aligned access is one host
  access, which gives Arm's single-copy atomicity; an unaligned one goes
  byte by byte; a 128-bit access is two 64-bit halves, as Armv8.0
  defines it. Mapping and permission changes are serialised by a lock;
  each publishes the zeroed frames before the entries that point at them.
- **Faults.** An access spanning pages goes page by page and stops at the
  first page that is unmapped or lacks the permission, naming its first
  byte; the pages before it keep what a write put there.
- **For the fast backends:** `compare_exchange8/16/32/64/128` (guest
  atomics as host atomics; the 128-bit one is `LOCK CMPXCHG16B` on
  x86_64 and an LDAXP/STLXP loop on aarch64, as stable Rust has no
  `AtomicU128`, and a process-wide lock elsewhere), `translate` (a page's
  frame and permission) and `epoch` (moves on with every mapping or
  permission change).

Not changed by the arena: svcMapMemory still moves the bytes to the new
address, and a shared memory object keeps its bytes while unmapped and
copies them in and out on mapping. The arena allows real sharing (two
pages, one frame) when a service needs it.

The `CpuBackend` trait passes `&Memory`: every core's host thread shares
the one `Memory` (`kestrel_hle::Process::mem` is an `Arc<Memory>`).

### What the native backend (phase 3) must do with it

None of this can be tested off an ARM host with a hypervisor; it is what
phase 3 builds on, and what to check there.

- Map the arena into the VM once, whole: guest physical (IPA) range
  `[0, size)` to `[host_base, host_base + size)` (`hv_vm_map`,
  `KVM_SET_USER_MEMORY_REGION`, `WHvMapGpaRange`). Both ends are multiples
  of 2 MiB, so of every host page size (4 KiB, 16 KiB on Apple Silicon,
  64 KiB on some aarch64 Linux kernels), and stage 2 can use 2 MiB blocks.
  The default arena needs a 34-bit IPA space; check each hypervisor's
  limit (Hypervisor.framework's IPA size, `KVM_CAP_ARM_VM_IPA_SIZE`).
- The guest's own page tables in the VM (stage 1, EL1, which kestrel owns)
  use a 4 KiB granule: each guest page points at its frame's offset with
  its permission (AP, UXN, PXN), built from `Memory::translate`. This is
  how 4 KiB guest pages run on a 16 KiB host: the host's page size only
  constrains stage 2, which maps the arena as one block.
- After any exit that may have changed mappings, compare `Memory::epoch`
  with the value the tables were built at; rebuild what changed and
  invalidate the VM's TLB before re-entering.
- Only committed memory is backed. Frames handed out are always committed,
  but the arena's tail is reserved only (`PROT_NONE`, `MEM_RESERVE`) until
  used. Check on each OS whether a VM mapping may cover reserved pages; if
  not, map `[0, Arena::committed())` and extend the mapping as commitment
  grows, in 2 MiB steps, or commit the whole arena up front on that host.
- One vCPU per guest core, each on its own host thread, all on the one
  mapping (`PLAN.md` section 3). Guest loads, stores, exclusives and
  atomics are then the hardware's, on the same memory the OS layer reads
  and writes through `Memory`'s atomics, which the VM sees as stores from
  another core.

### What the recompiler (phase 4) must do with it

- Always available, on any host page size: generated code walks the page
  table as `Memory` does (top table, leaf, entry; entry layout in
  `memory.rs`) and accesses `host_base + frame + offset`.
- The fast path, with guard pages: reserve 2^39 bytes of host address
  space (plus a guard tail beyond it, so base + offset cannot escape) and
  map each guest page there as a second view of its frame, with the
  host's permission; unmapped pages stay inaccessible. A guest access is
  then one host access at `view + address`, and a host fault (a signal on
  Unix, a vectored exception on Windows) goes to the slow path or becomes
  a guest fault. This needs:
  - the arena backed by a shareable memory object, so a frame can be
    mapped twice: `memfd_create` on Linux, a Mach memory entry on macOS,
    a pagefile-backed section and `MapViewOfFile3` with placeholders on
    Windows. That change is inside `kestrel_core::host`; the API stays.
  - host pages no larger than guest pages (4 KiB): true on x86_64 hosts,
    where the recompiler runs. On 16 KiB hosts it takes the table path.
  - on Windows, checking the alignment `MapViewOfFile3` requires of a
    view's offset within the section (if it is the 64 KiB allocation
    granularity, 4 KiB views of arbitrary frames need another way).
  - rebuilding views when `Memory::epoch` moves, and invalidating
    translations on writes to code pages.
- Guest atomics: host compare-and-swap (`Memory::compare_exchange*`, or
  the same instructions in generated code); LDXR/STXR as a compare-and-swap
  of the value LDXR saw. That misses a store of the same value between the
  two (which should clear the monitor), an accepted approximation for a
  recompiler, and what the interpreter does; the native backend uses the
  hardware's monitor. The 128-bit pairs (LDXP/STXP, CASP) use
  `Memory::compare_exchange128`, or `cmpxchg16b` in generated code.
- One host thread per guest core: `Process::run_threaded` ("Threaded
  cores" below) takes one backend per core and calls `run` on each from
  its own host thread with a budget of at most `thread::SLICE`
  instructions; a recompiler needs nothing more from the OS layer.

## OS emulation (HLE)

Guest software talks to the OS through SVCs and, above that, IPC to system
services. kestrel answers both in Rust from public documentation. The process
layout is one fixed, documented 39-bit layout (`kestrel_hle::layout`),
reported to guests through `svcGetInfo`; the console randomises placement,
kestrel does not, so runs are reproducible. Behaviour learned by observation
rather than documentation is described in `docs/SWITCH2_NOTES.md` before it
is implemented.

`kestrel_hle::svc` decodes each SVC's registers and calls a `Process`
method; the methods live next to the state they change:

- `kestrel_hle::memory`: memory types and attributes per mapping, the
  memory calls, transfer memory, and shared memory objects with their
  mappings.
- `kestrel_hle::kernel`: the handle table and kernel objects (threads,
  events, transfer memory). An object lives while a handle names it;
  closing the last handle hands the object back so its owner can release
  what it holds (transfer memory gets its permissions back). The
  pseudo-handles 0xffff8000 and 0xffff8001 name the current thread and
  process without occupying a slot.
- `kestrel_hle::thread`: thread records and the scheduler. `Process::cpu`
  holds the running thread's registers; the others' are saved in their
  records.
- `kestrel_hle::sync`: waiting on objects, mutex and condition variable
  arbitration, address arbitration. A blocked thread records what it
  waits for (`thread::Wait`) and an optional deadline; whatever ends the
  wait (a signal, a hand-over, the deadline, a cancel) writes the result
  into that thread's saved registers.
- `kestrel_hle::ipc`: the IPC message format. A request is parsed from
  the start of the caller's thread local region; replies are encoded
  for CMIF ("SFCO" header) or TIPC (result word first) and written back
  in its place.
- `kestrel_hle::service`: named ports, client sessions and the services
  behind them, one file per service. A client session is a kernel object
  serving one interface object; svcSendSyncRequest parses the message,
  hands the command to that interface's Rust code and writes the reply,
  without blocking. Commands that return interfaces hand out new
  sessions, or new objects once the client made the session a CMIF
  domain. The service manager, sm:, is reached through the "sm:" named
  port and hands out sessions to the other services by name. Events a
  service signals later are kernel objects it holds a reference to. A
  port, service or command kestrel does not implement stops the process
  with its name and number. `kestrel_hle::firmware` holds the system
  version every service answers as, and the time the clocks start at and
  the time zone. Sessions cloned from a domain session share the domain.
  The SD card is a host folder the frontend chooses (`fs::State::sd`);
  fsp-srv checks every path so none leads outside it.
- Kestrel's own notes about a program (`Process::note`, kept in
  `Process::log` and echoed to stderr by `kestrel run`) are apart from the
  program's output. `kestrel_hle::results` turns a result code into the
  console's "2345-0033" form with its module's name. A program that aborts
  (svcBreak) or throws a fatal error with an error screen stops with
  `Stop::Abort` or `Stop::Fatal`.
- Controller input: `kestrel_hle::input::Source` gives hid one
  controller's state; hid writes it into its shared memory every 5 ms of
  guest time (`hid_tick`, called from the run loop). The front end's
  window (`kestrel-cli/src/window.rs`, winit, softbuffer, gilrs, behind
  the default `window` feature) runs the event loop on the main thread
  and the program on a worker thread, and maps keys and gamepads as
  `user/config/input.cfg` says (`input_config.rs`). The self-test uses a
  scripted Source instead.
- The display: a program registers memory with nvdrv's /dev/nvmap
  (`hle/service/nv.rs`) and hands it to its layer's buffer queue through
  vi:u's binder relay (`hle/service/vi.rs`). A queued buffer is read from
  the program's memory and turned into pixels (`hle/display.rs`: block
  linear, pixel formats) for `Process::screen`, a
  `kestrel_hle::display::Screen`: the window paints it, the self-test
  records a CRC-32 of it. Buffers are freed at a 60 Hz vsync of guest
  time, which the scheduler wakes for.
- Unattended runs: `kestrel-cli/src/unattended.rs` gives a program a
  scripted controller (`--press`) and saves its last frame as a PNG
  (`--save-frame`); `scripts/run-examples.sh` uses both to run the
  standard homebrew examples, and `check-examples.py` reads the frames
  back as text with libnx's font.
- Whatever kestrel does not implement (an SVC, a named port, a service,
  a command or a CMIF control command) stops the program with
  `Stop::Unimplemented`, which names it with its identifiers; services
  return `IpcError::Unimplemented` for it. `kestrel run` appends each one
  to `user/logs/unimplemented.log`. SVC names come from
  `kestrel_hle::svc_names`, generated from Switchbrew's SVC table.

### Scheduling

By default every guest thread runs on one host thread, so runs on the
counted clock repeat exactly: the self-test, unattended runs and the
differential tester. Guest threads are spread over `thread::CORES` (four)
virtual cores; each core runs its best-priority ready thread, and the
cores take turns (with `--threaded`, each core runs on its own host
thread instead: "Threaded cores" below):

```
Process::run
  └─ backend.run(cpu, budget = min(steps left, slice left))
       ├─ Exit::Svc → svc::dispatch → schedule(rotate if the call yielded,
       │                                        slept or exited)
       └─ slice used up (19,200 instructions) → schedule(rotate)
schedule: wake sleepers and time out waiters whose time came; pick the
          best ready thread of this core (or of the next core with one,
          when rotating); if none is ready, jump a counted clock to
          the next deadline (on the host clock, set idle_until and let
          Process::run wait on the host), or stop with a deadlock report
          if there is none
```

The process clock is the 19.2 MHz system counter, `CpuState::counter`,
which the interpreter's MRS CNTPCT_EL0 and the OS layer both read. Its
source is `CpuState::clock` (the owner's decision, `PLAN.md` section 3):
`Clock::Counted` gives one tick every 52 retired instructions (998.4 MHz)
plus the ticks skipped while every thread slept, so the self-test and
headless runs repeat; `Clock::Host` is the host's monotonic clock, set by
`kestrel run` when it opens a window, so guest time is real time. A
switch hands the instruction count and the clock to the incoming thread
and clears the outgoing thread's exclusive monitor.
`Process::instructions` counts only executed instructions.

### Threaded cores

`Process::run_threaded` (`kestrel_hle::cores`; `kestrel run --threaded`,
`kestrel selftest --threaded`) runs each of the `CORES` guest cores on its
own host thread, with its own `CpuBackend`, on the host's clock, as the
fast backends will for real time (`PLAN.md` section 3):

```
core thread k (one per guest core)
  loop:
    lock the OS layer
      the last run ended in an SVC → svc::dispatch with that thread's
        registers as Process::cpu (as on one host thread), notify idle cores
      interrupted, out of budget, or another core stopped → stop
      wake due sleepers and timed-out waits, hid_tick, vsync_tick
      pick the best ready thread of core k (priority, then turn) that no
        other core runs; none → wait on the condition variable until
        another core changes the threads or the next wake-up (≤ 10 ms),
        or stop if nothing can ever run (exited, or a deadlock)
    unlock
    backend.run(thread's registers, &mem, ≤ SLICE instructions), no lock
```

- **One lock for the OS layer.** Kernel objects, the handle table, the
  scheduler, synchronisation, IPC and services sit behind one mutex that
  a system call and every look at the scheduler take. Calls from
  different cores are serialised, so the OS layer is safe to call from
  every core with no locks of its own, and the system calls are the same
  code on both schedulers (`Process` is `Send`; outside a call no thread
  is current, `thread::NO_THREAD`). Guest code runs without it. Finer
  locks can follow where a game shows contention.
- **Guest words the kernel changes.** Other cores change mutex and
  arbitration words with their own atomics while the kernel looks, so the
  kernel's read-then-write changes are compare-and-swaps (`sync.rs`).
- **Latency.** A core sees a newly ready thread of better priority, a
  thread moved to it, sleepers, timeouts, hid samples and vsync at its
  next call or within `SLICE` (19,200) instructions.
- **Stopping.** The first stop on any core wins; the other cores finish
  their slice. The registers kept are those of the thread that stopped
  the process.

Programs whose output depends on doing something within a span of real
time (hbdisplay, hbinput: `kestrel_testsuite::HOST_SPEED`) need a host
that keeps up; the others give the same output as on one host thread.

Planned: the remaining system services behind sm:, which will also
bring buffer transfers and shared memory.

## Graphics (planned)

Two levels, both needed (`PLAN.md` phase 5):

- **GPU level.** GPU driver services, GPU address space, command-stream
  processing for the 3D, compute and copy engines, and a shader translator.
  Shaders go through one intermediate representation with a Maxwell decoder
  (Switch 1 and homebrew) and an Ampere decoder (Switch 2), both emitting
  SPIR-V for Vulkan. Works for any guest software.
- **API level.** Commercial games link Nintendo's NVN graphics API into their
  own code. Intercepting those calls and translating them directly to Vulkan
  skips command-stream emulation. Applies only to commercial software and
  depends on owner approval (`PLAN.md` section 7).

Both levels render through one Vulkan renderer (MoltenVK on macOS) with a
cross-platform window and presentation layer, fed by the display services
and buffer queue.