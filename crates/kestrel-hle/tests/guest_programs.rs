//! End-to-end tests: load the prebuilt ARM64 programs from `guest/bin`
//! (built by scripts/build-guest.sh) and check what they print.
//! The binaries are committed so these tests run on any host without clang.

use kestrel_core::Memory;
use kestrel_cpu::{create_backend, BackendKind};
use kestrel_hle::{Process, Stop};
use std::path::PathBuf;

fn run_guest(name: &str) -> (Stop, String) {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "..",
        "guest",
        "bin",
        &format!("{name}.elf"),
    ]
    .iter()
    .collect();
    let file = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut mem = Memory::new();
    let image = kestrel_loader::load_elf(&file, &mut mem).expect("load");
    let mut proc = Process::new(mem, &image).expect("process");
    let (mut backend, _) = create_backend(BackendKind::Auto).expect("backend");
    let stop = proc.run(backend.as_mut(), 200_000_000);
    (stop, proc.debug_output_str())
}

#[test]
fn hello() {
    let (stop, out) = run_guest("hello");
    assert_eq!(stop, Stop::Exited);
    assert_eq!(out, "Hello from guest ARM64 code!\n");
}

#[test]
fn algorithms() {
    let (stop, out) = run_guest("algos");
    assert_eq!(stop, Stop::Exited, "output so far:\n{out}");
    let expected = "\
fib(25) = 75025
crc32 = 0x00000000414fa339
primes <= 100000 = 9592
sort ok
signed div/mod = 97 99
";
    assert_eq!(out, expected);
}
