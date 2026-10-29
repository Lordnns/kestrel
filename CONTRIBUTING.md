# Contributing

## Clean-room rules (non-negotiable)

Pull requests are rejected, and history rewritten if necessary, if they:

- include or link console firmware, encryption keys, certificates, BIOS or
  system files, or instructions for obtaining them;
- copy from leaked or NDA'd SDKs, documentation or source code;
- include commercial game code, assets or dumps, even as test fixtures;
- were written by someone who has read leaked SDK material for the same area.

Allowed sources: public documentation (for example Switchbrew), the Arm
Architecture Reference Manual, public talks and papers, open-source homebrew,
and observing behaviour of software you wrote yourself.

Test programs go in `guest/src` and must be original code.

## Workflow

1. `cargo test` and `cargo clippy --workspace --all-targets -- -D warnings` must pass.
2. New instructions need a test in `crates/kestrel-cpu/src/interp/tests.rs`
   whose encoding comes from a real assembler (`llvm-mc --triple=aarch64`),
   with the assembly in a comment.
3. Keep host-specific code behind `cfg(target_arch = ...)` so every host keeps building.
