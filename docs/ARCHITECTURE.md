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

Today the process has one thread and no handles beyond the current-thread
pseudo-handle; memory calls live in `kestrel_hle::memory`. Planned, in
order: a per-process handle table with kernel objects (threads, events,
shared and transfer memory, later sessions), a cooperative scheduler on one
host thread, synchronisation, then IPC and the system services.

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