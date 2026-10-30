# kestrel

An experimental, clean-room ARM64 console emulator written in Rust, aimed at
next-generation hybrid handheld consoles. It builds and runs on **x86_64 and
aarch64 hosts** (Linux, macOS, Windows) from day one.

> Status: early foundation. It runs freestanding ARM64 test programs. It does
> not run commercial software, and nothing here helps anyone obtain it.

## What works

- **Portable ARM64 interpreter** — integer data processing, branches, all
  load/store addressing modes, pairs, exclusives, acquire/release, CAS, ARMv8.1
  LSE atomics, user system registers, scalar single and double precision
  floating point (Arm NaN, default-NaN and flush-to-zero rules, bit-identical
  on every host), and Advanced SIMD (NEON) vector and scalar instructions
  with structure loads and stores. ~140 MIPS on a modern x86 core.
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
cargo run --release -p kestrel-cli -- selftest
```

## Self-test and test programs

`kestrel selftest` runs thirteen ARM64 programs compiled into the binary and
compares their output byte for byte with answers computed independently
by `scripts/gen-expected.py` (Python, hashlib, zlib), so testers need
nothing but the executable. Each integer C program is in the suite twice:
built integer-only, and built with floating point and NEON allowed
(`name-neon`), so auto-vectorised code must give the same output:

| Program      | Exercises                                                        |
|--------------|------------------------------------------------------------------|
| `hello`      | loader, entry, debug-print SVC, exit                             |
| `asm_checks` | 121 hand-written instruction checks: carries, bitfields, atomics, exclusives, jump tables, system registers, SIMD moves, NaNs, subnormals, flush-to-zero, saturation |
| `vm`         | a bytecode VM run with switch dispatch and with function-pointer dispatch |
| `bigint`     | 128-bit multiply, Miller-Rabin on 64-bit primes, SWAR popcount  |
| `algos`      | recursion, CRC32, prime sieve, heap allocation, sort            |
| `sha256`     | SHA-256 of three inputs (about 6.5M guest instructions)         |
| `fp`         | 127 scalar FP operations printed as bit patterns: NaN rules, flush-to-zero, default NaN, FPCR rounding modes |
| `intrinsics` | 168 NEON intrinsics across every instruction class, including structure loads and stores |

`kestrel extract <dir>` writes them out as `.elf` files for `kestrel run`.

## Packages for testers

Push a version tag and GitHub Actions builds and self-tests these, then
publishes them as a GitHub Release:

```
git tag v0.1.0 && git push origin v0.1.0
```

| Package                    | Built on                | Self-tested natively on     |
|----------------------------|-------------------------|-----------------------------|
| `kestrel-macos-universal`  | macOS 14, Apple Silicon | Apple Silicon and Intel Macs |
| `kestrel-linux-x86_64`     | Ubuntu x86_64           | Ubuntu x86_64               |
| `kestrel-linux-aarch64`    | Ubuntu ARM64            | Ubuntu ARM64                |
| `kestrel-windows-x86_64`   | Windows x86_64          | Windows x86_64              |
| `kestrel-windows-arm64`    | Windows 11 ARM64        | Windows 11 ARM64            |

### Download the latest release and self-test

These links always point at the newest release.

macOS (Apple Silicon or Intel):

```
curl -LO https://github.com/Lordnns/kestrel/releases/latest/download/kestrel-macos-universal.tar.gz
tar xzf kestrel-macos-universal.tar.gz && cd kestrel-macos-universal
xattr -dr com.apple.quarantine .
./kestrel selftest -v
```

Linux (`x86_64` or `aarch64`):

```
curl -LO https://github.com/Lordnns/kestrel/releases/latest/download/kestrel-linux-x86_64.tar.gz
tar xzf kestrel-linux-x86_64.tar.gz && cd kestrel-linux-x86_64
./kestrel selftest -v
```

Windows PowerShell (`x86_64` or `arm64`):

```
Invoke-WebRequest https://github.com/Lordnns/kestrel/releases/latest/download/kestrel-windows-x86_64.zip -OutFile kestrel.zip
Expand-Archive kestrel.zip -DestinationPath .; cd kestrel-windows-x86_64
.\kestrel.exe selftest -v
```

Each package has `TESTING.md` with step-by-step instructions. You can also
run the workflow by hand from the Actions tab to get the packages as
artifacts without making a release.

Rebuilding the guest test programs needs `clang` and `ld.lld` (they
cross-compile, so any host works): `./scripts/build-guest.sh`. On an ARM64
Linux or macOS machine, `./scripts/check-host.sh` builds the C programs
natively and checks that the real CPU prints exactly the expected output.

## How it runs on both architectures

| Host     | Today            | Fast path (planned)                                        |
|----------|------------------|------------------------------------------------------------|
| x86_64   | interpreter      | `jit`: ARM64 → x86_64 dynamic recompiler                   |
| aarch64  | interpreter      | `native`: guest code runs directly under KVM, Hypervisor.framework or WHP |

Every backend implements the same `CpuBackend` trait. The interpreter is the
reference: faster backends are differentially tested against it. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Roadmap

1. **Phase 1 tests**: NEON and floating-point test programs in the
   self-test (scalar FP and NEON are implemented).
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
