//! The environment a homebrew loader hands to an NRO.
//!
//! From Switchbrew's public "Homebrew ABI" page: the entry point receives
//! X0 = pointer to a list of loader config entries and X1 = all ones. Each
//! entry is `{u32 key, u32 flags, u64 value[2]}`; the list ends with key 0.
//! The program arguments are one string, split on spaces with double quotes
//! grouping, whose first word is argv[0].

use crate::{layout, Process};
use kestrel_core::{Perm, PAGE_SIZE};

/// Config entry keys used here.
pub mod key {
    pub const END_OF_LIST: u32 = 0;
    pub const MAIN_THREAD_HANDLE: u32 = 1;
    pub const ARGV: u32 = 5;
    pub const APPLET_TYPE: u32 = 7;
    pub const RANDOM_SEED: u32 = 14;
}

/// AppletType "Application".
pub const APPLET_TYPE_APPLICATION: u64 = 0;
/// A fixed random seed, so runs are reproducible.
pub const RANDOM_SEED: [u64; 2] = [0x6b65_7374_7265_6c00, 0x9e37_79b9_7f4a_7c15];

const ENTRY_SIZE: usize = 24;
const ENTRIES: usize = 5;

/// Join `argv` into the loader's argument string, quoting words that are
/// empty or contain spaces. A double quote cannot be carried.
pub fn argv_string(argv: &[String]) -> Result<String, String> {
    let mut words = Vec::new();
    for a in argv {
        if a.contains('"') {
            return Err(format!(
                "argument {a:?} contains a double quote, which the homebrew argument string cannot carry"
            ));
        }
        if a.is_empty() || a.contains(char::is_whitespace) {
            words.push(format!("\"{a}\""));
        } else {
            words.push(a.clone());
        }
    }
    Ok(words.join(" "))
}

impl Process {
    /// Set up `self` to start as a homebrew NRO with arguments `argv`
    /// (argv[0] first): write the loader config entries and the strings
    /// they point to at `layout::HOMEBREW_CONFIG`, and set X0 and X1.
    pub fn start_homebrew(&mut self, argv: &[String]) -> Result<(), String> {
        let base = layout::HOMEBREW_CONFIG;
        let info = concat!("kestrel ", env!("CARGO_PKG_VERSION")).as_bytes();
        let mut args = argv_string(argv)?.into_bytes();
        args.push(0);
        let entries_at = base;
        let info_at = entries_at + (ENTRIES * ENTRY_SIZE) as u64;
        let args_at = info_at + info.len() as u64;
        let end = args_at + args.len() as u64;
        let entries: [(u32, u64, u64); ENTRIES] = [
            (key::MAIN_THREAD_HANDLE, self.main_thread as u64, 0),
            (key::APPLET_TYPE, APPLET_TYPE_APPLICATION, 0),
            (key::ARGV, 0, args_at),
            (key::RANDOM_SEED, RANDOM_SEED[0], RANDOM_SEED[1]),
            (key::END_OF_LIST, info_at, info.len() as u64),
        ];
        let mut bytes = Vec::with_capacity((end - base) as usize);
        for (k, v0, v1) in entries {
            bytes.extend(k.to_le_bytes());
            bytes.extend(0u32.to_le_bytes()); // flags
            bytes.extend(v0.to_le_bytes());
            bytes.extend(v1.to_le_bytes());
        }
        bytes.extend(info);
        bytes.extend(&args);
        let size = (end - base).div_ceil(PAGE_SIZE) * PAGE_SIZE;
        self.mem
            .map_tagged(
                base,
                size,
                Perm::RW,
                crate::memory::tag(crate::memory::state::NORMAL),
                "homebrew-config",
            )
            .map_err(|e| e.to_string())?;
        self.mem
            .write_bytes(base, &bytes)
            .map_err(|e| e.to_string())?;
        self.cpu.x[0] = entries_at;
        self.cpu.x[1] = u64::MAX;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_core::Memory;
    use kestrel_loader::Loaded;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn argument_string_quotes_only_when_needed() {
        let s = argv_string(&strings(&["hb.nro", "-v", "two words", ""])).unwrap();
        assert_eq!(s, r#"hb.nro -v "two words" """#);
        assert!(argv_string(&strings(&["a\"b"])).is_err());
    }

    #[test]
    fn entry_registers_and_config_list_follow_the_abi() {
        let image = Loaded {
            entry: 0x800_0000,
            image_start: 0x800_0000,
            image_end: 0x800_1000,
        };
        let mut p = Process::new(Memory::new(), &image).unwrap();
        p.start_homebrew(&strings(&["hb.nro", "x y"])).unwrap();
        assert_eq!(p.cpu.x[1], u64::MAX);
        assert_eq!(p.cpu.pc, 0x800_0000);
        // Walk the list as a homebrew program would.
        let mut at = p.cpu.x[0];
        let mut seen = Vec::new();
        loop {
            let k = p.mem.read32(at).unwrap();
            let flags = p.mem.read32(at + 4).unwrap();
            let v = [
                p.mem.read64(at + 8).unwrap(),
                p.mem.read64(at + 16).unwrap(),
            ];
            assert_eq!(flags, 0);
            seen.push(k);
            match k {
                1 => {
                    assert_eq!(p.thread_id(v[0]), Ok(crate::kernel::MAIN_THREAD_ID));
                    assert_eq!(v[1], 0);
                }
                7 => assert_eq!(v, [0, 0]),
                5 => {
                    let s = p.mem.read_vec(v[1], 13).unwrap();
                    assert_eq!(s, b"hb.nro \"x y\"\0");
                }
                14 => {}
                0 => {
                    let s = p.mem.read_vec(v[0], v[1] as usize).unwrap();
                    assert!(s.starts_with(b"kestrel "));
                    break;
                }
                other => panic!("unexpected key {other}"),
            }
            at += 24;
        }
        assert_eq!(seen, [1, 7, 5, 14, 0]);
    }
}
