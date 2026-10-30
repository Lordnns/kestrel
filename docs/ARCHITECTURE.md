# Architecture

```
kestrel-cli ──► kestrel-hle ──► kestrel-cpu ──► kestrel-core
                    │                             ▲
                    └──► kestrel-loader ──────────┘
```

| Crate            | Responsibility                                                    |
|------------------|-------------------------------------------------------------------|
| `kestrel-core`   | Guest address space (`Memory`), permissions, faults               |
| `kestrel-cpu`    | `CpuState`, the `CpuBackend` trait, the portable interpreter      |
| `kestrel-loader` | Executable loaders: ELF test programs, homebrew NRO               |
| `kestrel-hle`    | Process model and supervisor-call (SVC) handling                  |
| `kestrel-testsuite` | Guest test programs embedded in the binary, with expected output |
| `kestrel-cli`    | Command-line front end                                            |

## CPU backends

```rust
pub trait CpuBackend {
    fn run(&mut self, state: &mut CpuState, mem: &mut Memory, max_steps: u64) -> Exit;
    fn invalidate(&mut self, addr: u64, len: u64);
}
```

A backend runs guest code until something needs the OS (`Exit::Svc`), a
breakpoint, a fault, an instruction it does not support, or the step budget.
The HLE layer handles the exit and calls `run` again.

### Interpreter (all hosts)

`crates/kestrel-cpu/src/interp/` decodes by the top-level A64 encoding group
(bits 28..25), one file per group; Advanced SIMD has its own directory,
`interp/neon/`, with one set of lane helpers for every element size. Unsupported encodings return
`Exit::Undefined { pc, insn }` rather than guessing, so gaps are explicit.
Encodings that decode but use something not modelled yet (half precision,
for example) return `Exit::Unsupported` with an `Unmodelled` reason. Both
variants keep `Exit` at 24 bytes, because every interpreter step returns it.

### Floating point

`crates/kestrel-cpu/src/float.rs` implements the Arm floating-point
pseudocode on raw bit patterns, independent of any backend: NaN processing,
default NaN, flush-to-zero, FPRound, conversions, round-to-integral and the
reciprocal estimates. IEEE arithmetic runs on the host in round-to-nearest,
which matches Arm for finite results; everything around it (NaN choice,
host-generated NaNs, FZ) is applied here, so x86 and ARM hosts give the same
bits. NEON floating-point lanes call the same functions. Future backends
should call them too, or be differentially tested against them.

### Native (aarch64 hosts, planned)

Guest and host share the instruction set, so guest EL0 code can run directly
on the host CPU inside a hardware-virtualised sandbox:

- Linux: KVM (`/dev/kvm`)
- macOS: Hypervisor.framework
- Windows on ARM: Windows Hypervisor Platform

The VM runs only guest user code. SVCs trap to the host, which forwards them
to `kestrel-hle`. This requires `Memory` to become a host `mmap`'d arena that
can be mapped into the VM's stage-2 tables, which is the next refactor of
`kestrel-core`. macOS comes first, developed alongside the IPC and service
work once threads and synchronisation exist; then Linux and Windows.

### JIT (x86_64 hosts, planned)

Translate guest basic blocks to x86_64, cache by guest PC, invalidate on
writes to code pages (`CpuBackend::invalidate`). Exclusives and memory
ordering need care because x86 has a stronger memory model than ARM.

### Correctness

Faster backends are checked against the interpreter by running the same
program on both and comparing full `CpuState` at every SVC (and, in debug
builds, every N instructions).

## Memory

Today: a `BTreeMap` of regions with a one-entry lookup cache. Simple and
portable. Planned: one large reserved host range per process, guest pages
mapped at fixed offsets so address translation is an add (interpreter/JIT)
or a stage-2 mapping (native).

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

### Scheduling

Every thread runs on one host thread, so runs repeat exactly. Guest
threads are spread over four virtual cores; each core runs its
best-priority ready thread, and the cores take turns:

```
Process::run
  └─ backend.run(cpu, budget = min(steps left, slice left))
       ├─ Exit::Svc → svc::dispatch → schedule(rotate if the call yielded,
       │                                        slept or exited)
       └─ slice used up (19,200 instructions) → schedule(rotate)
schedule: wake sleepers and time out waiters whose time came; pick the
          best ready thread of this core (or of the next core with one,
          when rotating); if none is ready, jump the clock to the next
          deadline, or stop with a deadlock report if there is none
```

`CpuState::retired` doubles as the process clock (one instruction per
tick of the 19.2 MHz counter): a switch hands the clock to the incoming
thread and clears the outgoing thread's exclusive monitor. Idle jumps are
counted separately, so `Process::instructions` reports only executed
instructions.

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