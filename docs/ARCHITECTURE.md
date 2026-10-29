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
| `kestrel-loader` | Executable loaders (ELF now, homebrew format next)                |
| `kestrel-hle`    | Process model and supervisor-call (SVC) handling                  |
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
(bits 28..25), one file per group. Unsupported encodings return
`Exit::Undefined { pc, insn }` rather than guessing, so gaps are explicit.

### Native (aarch64 hosts, planned)

Guest and host share the instruction set, so guest EL0 code can run directly
on the host CPU inside a hardware-virtualised sandbox:

- Linux: KVM (`/dev/kvm`)
- macOS: Hypervisor.framework
- Windows on ARM: Windows Hypervisor Platform

The VM runs only guest user code. SVCs trap to the host, which forwards them
to `kestrel-hle`. This requires `Memory` to become a host `mmap`'d arena that
can be mapped into the VM's stage-2 tables, which is the next refactor of
`kestrel-core`.

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
services. kestrel answers both in Rust from public documentation. The current
process layout uses fixed addresses (`kestrel_hle::layout`); real titles get
their region addresses through `svcGetInfo`, which is on the roadmap.
