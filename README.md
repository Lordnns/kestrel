# kestrel

An experimental, clean-room ARM64 console emulator written in Rust, aimed at
next-generation hybrid handheld consoles. It builds and runs on **x86_64 and
aarch64 hosts** (Linux, macOS, Windows) from day one.

> Status: early foundation. It runs freestanding ARM64 test programs. It does
> not run commercial software, and nothing here helps anyone obtain it.

## What works

- **Portable ARM64 interpreter** — integer data processing, branches, all
  load/store addressing modes, pairs, exclusives, acquire/release, CAS, ARMv8.1
  LSE atomics, user system registers, and the SIMD register moves compilers
  emit for integer code. ~45 MIPS on a modern x86 core.
- **Guest address space** with permissions and fault reporting.
- **ELF loader** for the test programs in `guest/`.
- **HLE kernel stub** answering a first set of supervisor calls
  (heap, exit, sleep, tick, debug output).
- **Backend selection** that reports which fast path the host will get.

```
$ cargo run --release -p kestrel-cli -- run guest/bin/algos.elf --stats
fib(25) = 75025
crc32 = 0x00000000414fa339
primes <= 100000 = 9592
sort ok
signed div/mod = 97 99
[interp] 4890955 instructions, 17 SVCs in 0.106s (46.1 MIPS)
```

## Building

```
cargo build --release
cargo test
cargo run --release -p kestrel-cli -- info
```

Rebuilding the guest test programs needs `clang` and `ld.lld` (they
cross-compile, so any host works): `./scripts/build-guest.sh`.

## How it runs on both architectures

| Host     | Today            | Fast path (planned)                                        |
|----------|------------------|------------------------------------------------------------|
| x86_64   | interpreter      | `jit`: ARM64 → x86_64 dynamic recompiler                   |
| aarch64  | interpreter      | `native`: guest code runs directly under KVM, Hypervisor.framework or WHP |

Every backend implements the same `CpuBackend` trait. The interpreter is the
reference: faster backends are differentially tested against it. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Roadmap

1. **NEON and floating point** in the interpreter (first gap hit by compiled
   code: `cmgt v3.2d, v3.2d, v6.2d`).
2. Homebrew executable loader, threads, handles and synchronisation SVCs.
3. IPC and the first OS services, from public documentation.
4. `native` backend on aarch64 hosts; `jit` backend on x86_64.
5. GPU command processing and shader translation to Vulkan.

## Legal and project rules

- Clean-room only. No console firmware, keys, SDKs, leaked material or
  commercial game code may be committed, linked or referenced. See
  [CONTRIBUTING.md](CONTRIBUTING.md).
- The project ships nothing derived from a console. Users who want to run
  their own software are responsible for dumping it from hardware they own.
- Not affiliated with or endorsed by any console manufacturer.

## License

MIT. See [LICENSE](LICENSE).
