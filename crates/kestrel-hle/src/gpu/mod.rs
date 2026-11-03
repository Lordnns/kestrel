//! The GPU (phase 5B): the Tegra X1's GM20B (Maxwell) as homebrew drives it
//! through /dev/nvhost-gpu channels (`service/nvgpu.rs`).
//!
//! A channel's command lists are GPFIFO entries, each naming a stretch of
//! "pushbuffer" in the GPU address space; the pushbuffer is a stream of
//! method writes, each to a subchannel. Subchannels are bound to engine
//! objects (2D, 3D, compute, inline-to-memory, DMA copy) by SET_OBJECT; the
//! methods below 0x100 go to the channel itself (host methods: semaphores,
//! syncpoints, flushes). Sources: NVIDIA's open-gpu-doc class headers (MIT)
//! for the methods and the command format (`clb06f.h`, and `cla26f.h` for
//! the Tegra syncpoint methods), envytools' "DMA submission to FIFOs"
//! (`docs/hw/fifo/dma-pusher.rst`, MIT) for the pushbuffer and IB entry
//! layout.
//!
//! kestrel runs a channel's command lists when they are submitted, in
//! order, before the submission returns: there is no GPU running beside
//! the CPU, so waits on the GPU's work are always already satisfied when
//! the program makes them. Engines keep their method registers; methods
//! that act are executed or stop the program, named.

pub mod methods;
pub mod mme;
mod run;

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Whether a method only sets a register or acts when written
/// (scripts/gen-gpu-methods.py).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    State,
    Action,
}

/// An engine method as NVIDIA's header defines it: a scalar at `base`, or an
/// array of `count` (by `stride` bytes), each maybe an array of
/// `inner_count` (by `inner` bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Method {
    pub name: &'static str,
    pub base: u32,
    pub stride: u32,
    pub count: u32,
    pub inner: u32,
    pub inner_count: u32,
    pub kind: Kind,
}

/// The engine classes GM20B offers (GET_CHARACTERISTICS), and the engine
/// each is.
pub mod class {
    pub const TWOD: u32 = 0x902d;
    pub const THREED: u32 = 0xb197;
    pub const COMPUTE: u32 = 0xb1c0;
    pub const INLINE_TO_MEMORY: u32 = 0xa140;
    pub const DMA_COPY: u32 = 0xb0b5;
}

/// An engine's name, for messages.
pub fn engine_name(class: u32) -> &'static str {
    match class {
        class::TWOD => "2D",
        class::THREED => "3D",
        class::COMPUTE => "compute",
        class::INLINE_TO_MEMORY => "inline-to-memory",
        class::DMA_COPY => "DMA copy",
        _ => "unknown engine",
    }
}

fn table(class: u32) -> Option<&'static [Method]> {
    Some(match class {
        class::TWOD => methods::TWOD,
        class::THREED => methods::THREED,
        class::COMPUTE => methods::COMPUTE,
        class::INLINE_TO_MEMORY => methods::INLINE_TO_MEMORY,
        class::DMA_COPY => methods::DMA_COPY,
        _ => return None,
    })
}

/// Methods are word offsets below 0x4000 bytes.
pub const METHODS: usize = 0x1000;

/// For each method word of a class, the method it belongs to and its index
/// (outer * inner_count + inner), built once.
type Lookup = Vec<Option<(u16, u16)>>;

fn lookup_table(class: u32) -> Option<&'static Lookup> {
    static TABLES: OnceLock<BTreeMap<u32, Lookup>> = OnceLock::new();
    let tables = TABLES.get_or_init(|| {
        let mut all = BTreeMap::new();
        for c in [
            class::TWOD,
            class::THREED,
            class::COMPUTE,
            class::INLINE_TO_MEMORY,
            class::DMA_COPY,
        ] {
            let methods = table(c).expect("known class");
            let mut words: Lookup = vec![None; METHODS];
            // Scalars first, then arrays: where an inferred array reaches a
            // named method, the named method wins.
            let order = methods
                .iter()
                .enumerate()
                .filter(|(_, m)| m.stride == 0)
                .chain(methods.iter().enumerate().filter(|(_, m)| m.stride != 0));
            for (i, m) in order {
                for outer in 0..m.count {
                    for inner in 0..m.inner_count {
                        let at = m.base + outer * m.stride + inner * m.inner;
                        let w = (at / 4) as usize;
                        if w < METHODS && words[w].is_none() {
                            words[w] = Some((i as u16, (outer * m.inner_count + inner) as u16));
                        }
                    }
                }
            }
            all.insert(c, words);
        }
        all
    });
    tables.get(&class)
}

/// The method at byte offset `offset` of `class`, and its array index.
pub fn method(class: u32, offset: u32) -> Option<(&'static Method, u32)> {
    let (i, index) = (*lookup_table(class)?.get((offset / 4) as usize)?)?;
    Some((&table(class)?[i as usize], index as u32))
}

/// One method write from a pushbuffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call {
    pub subchannel: u32,
    /// The method's byte offset.
    pub method: u32,
    pub value: u32,
}

/// A GPFIFO entry (`clb06f.h`, NVB06F_GP_ENTRY0/1; envytools "IB mode"):
/// the pushbuffer's GPU address (bits 2 to 39), its length in words, and
/// whether it is a control entry (length 0, with an opcode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub address: u64,
    pub words: u32,
    pub opcode: u32,
}

impl Entry {
    pub fn decode(raw: u64) -> Entry {
        let (w0, w1) = (raw as u32, (raw >> 32) as u32);
        Entry {
            address: (w0 & !3) as u64 | ((w1 & 0xff) as u64) << 32,
            words: (w1 >> 10) & 0x1f_ffff,
            opcode: w1 & 0xff,
        }
    }
}

/// Decode a pushbuffer segment into method writes, by the command formats
/// of `clb06f.h` (NVB06F_DMA_*) and envytools' "GF100 commands": SEC_OP 1
/// incrementing, 3 non-incrementing, 4 immediate data, 5 increment once;
/// SEC_OP 0 and 2 with TERT_OP 0, the old incrementing and non-incrementing
/// forms; 7 ends the segment. The rest (subdevice masks, reserved) are
/// errors, named.
pub fn decode(words: &[u32]) -> Result<Vec<Call>, String> {
    let mut calls = Vec::new();
    let mut at = 0;
    while at < words.len() {
        let h = words[at];
        at += 1;
        let sub = (h >> 13) & 7;
        let addr = h & 0xfff;
        let (first, count, step_after) = match h >> 29 {
            // Incrementing: each word to the next method.
            1 => (addr, (h >> 16) & 0x1fff, Step::Every),
            // Non-incrementing: every word to the same method.
            3 => (addr, (h >> 16) & 0x1fff, Step::Never),
            // Immediate data: the 13-bit value in the header itself.
            4 => {
                calls.push(Call {
                    subchannel: sub,
                    method: addr * 4,
                    value: (h >> 16) & 0x1fff,
                });
                continue;
            }
            // Increment once: the first word to the method, the rest to the
            // next one.
            5 => (addr, (h >> 16) & 0x1fff, Step::Once),
            // The old forms: method bits 12..2, count bits 28..18.
            0 if (h >> 16) & 3 == 0 => ((h >> 2) & 0x7ff, (h >> 18) & 0x7ff, Step::Every),
            2 if (h >> 16) & 3 == 0 => ((h >> 2) & 0x7ff, (h >> 18) & 0x7ff, Step::Never),
            // End of the pushbuffer segment.
            7 => break,
            op => {
                return Err(format!(
                    "pushbuffer command {h:#010x} (SEC_OP {op}, TERT_OP {}), which kestrel does \
                     not decode",
                    (h >> 16) & 3
                ))
            }
        };
        let count = count as usize;
        if at + count > words.len() {
            return Err(format!(
                "pushbuffer command {h:#010x} with {count} data words, past the segment's end"
            ));
        }
        for k in 0..count {
            let offset = match step_after {
                Step::Every => k as u32,
                Step::Never => 0,
                Step::Once => (k > 0) as u32,
            };
            calls.push(Call {
                subchannel: sub,
                method: (first + offset) * 4,
                value: words[at + k],
            });
        }
        at += count;
    }
    Ok(calls)
}

enum Step {
    Every,
    Never,
    Once,
}

/// An engine object's state: its method registers, and for the 3D engine
/// the macro (MME) memories its LOAD_MME_* methods fill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Engine {
    pub class: u32,
    pub regs: Vec<u32>,
    pub mme_code: Vec<u32>,
    pub mme_code_pointer: u32,
    pub mme_start: Vec<u32>,
    pub mme_start_pointer: u32,
    /// Constant buffers bound to shader stages (BIND_GROUP_CONSTANT_BUFFER):
    /// (stage, slot) to (GPU address, size).
    pub constant_buffers: BTreeMap<(u32, u32), (u64, u32)>,
}

impl Engine {
    pub fn new(class: u32) -> Engine {
        Engine {
            class,
            regs: vec![0; METHODS],
            mme_code: Vec::new(),
            mme_code_pointer: 0,
            mme_start: Vec::new(),
            mme_start_pointer: 0,
            constant_buffers: BTreeMap::new(),
        }
    }

    pub fn reg(&self, offset: u32) -> u32 {
        self.regs[(offset / 4) as usize]
    }
}

/// How large kestrel lets the macro memories grow (kestrel's choice: no
/// source gives GM20B's sizes; past this a load stops, named).
pub const MME_CODE_WORDS: u32 = 0x1_0000;
pub const MME_START_WORDS: u32 = 0x1000;

/// A channel's GPU state: what each subchannel is bound to, the engines'
/// state (one object per class), the host methods' registers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChannelState {
    pub subchannels: [Option<u32>; 8],
    pub engines: BTreeMap<u32, Engine>,
    /// Host methods' registers (byte offsets below 0x100), by word.
    pub host: Vec<u32>,
    /// Method writes executed, for tests and for phase 5B's debugging.
    pub methods_run: u64,
    /// Privileged GPU registers the firmware calls wrote, by address
    /// (SET_FALCON04, `gpu/run.rs`).
    pub hw_regs: BTreeMap<u32, u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_are_found_by_offset() {
        let (m, i) = method(class::THREED, 0x2c8).unwrap();
        assert_eq!(
            (m.name, i, m.kind),
            ("INCREMENT_SYNC_POINT", 0, Kind::Action)
        );
        // SET_COLOR_TARGET_A(3) at 0x800 + 3 * 64.
        let (m, i) = method(class::THREED, 0x8c0).unwrap();
        assert_eq!((m.name, i, m.kind), ("SET_COLOR_TARGET_A", 3, Kind::State));
        // SET_STREAM_OUT_LAYOUT_SELECT(1, 2) at 0x2800 + 128 + 8.
        let (m, i) = method(class::THREED, 0x2888).unwrap();
        assert_eq!((m.name, i), ("SET_STREAM_OUT_LAYOUT_SELECT", 32 + 2));
        let (m, _) = method(class::THREED, 0x1b0c).unwrap();
        assert_eq!((m.name, m.kind), ("SET_REPORT_SEMAPHORE_D", Kind::Action));
        assert_eq!(
            method(class::INLINE_TO_MEMORY, 0x1b4).unwrap().0.name,
            "LOAD_INLINE_DATA"
        );
        assert_eq!(method(class::DMA_COPY, 0x300).unwrap().0.name, "LAUNCH_DMA");
        // A stream-out buffer past the fourth: no method (envytools gives
        // four).
        assert_eq!(method(class::THREED, 0x380 + 4 * 32), None);
        assert_eq!(method(0x1234, 0x100), None);
    }

    #[test]
    fn pushbuffer_commands_decode_to_method_writes() {
        // Incrementing (SEC_OP 1): 2 words to 3D method 0x2c8 and 0x2cc on
        // subchannel 0 (header: 1 << 29 | count << 16 | subch << 13 |
        // method / 4).
        let inc = 1 << 29 | 2 << 16 | (0x2c8 >> 2);
        // Non-incrementing (3) on subchannel 2, immediate (4), increment
        // once (5), the old incrementing form (SEC_OP 0) and an end.
        let non = 3 << 29 | 2 << 16 | 2 << 13 | (0x1b4 >> 2);
        let imm = 4 << 29 | 0x123 << 16 | 1 << 13 | (0x110 >> 2);
        let once = 5 << 29 | 3 << 16 | (0x118 >> 2);
        let old = 1 << 18 | 0x8;
        let words = [
            inc,
            10,
            11,
            non,
            20,
            21,
            imm,
            once,
            30,
            31,
            32,
            old,
            40,
            7 << 29,
            99,
        ];
        let calls = decode(&words).unwrap();
        let c = |subchannel, method, value| Call {
            subchannel,
            method,
            value,
        };
        assert_eq!(
            calls,
            [
                c(0, 0x2c8, 10),
                c(0, 0x2cc, 11),
                c(2, 0x1b4, 20),
                c(2, 0x1b4, 21),
                c(1, 0x110, 0x123),
                c(0, 0x118, 30),
                c(0, 0x11c, 31),
                c(0, 0x11c, 32),
                c(0, 0x8, 40),
            ]
        );
        // A command past the end, and SLI masks, are errors.
        assert!(decode(&[1 << 29 | 3 << 16, 1])
            .unwrap_err()
            .contains("past"));
        assert!(decode(&[1 << 16]).unwrap_err().contains("TERT_OP 1"));
    }

    #[test]
    fn gpfifo_entries_name_their_pushbuffer() {
        // 0x12_3456_7890, 0x40 words.
        let raw = 0x12u64 << 32 | (0x40 << 10) << 32 | 0x3456_7890;
        assert_eq!(
            Entry::decode(raw),
            Entry {
                address: 0x12_3456_7890,
                words: 0x40,
                opcode: 0x12
            }
        );
    }
}
