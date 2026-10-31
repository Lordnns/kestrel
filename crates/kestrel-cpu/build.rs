//! Sets `jit_host` where the x86_64 recompiler is built: Windows and Linux
//! on x86_64 (`PLAN.md` section 3, "Platform priority"). Other hosts build
//! without it and are told it is not supported there yet.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(jit_host)");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if arch == "x86_64" && (os == "windows" || os == "linux") {
        println!("cargo::rustc-cfg=jit_host");
    }
}
