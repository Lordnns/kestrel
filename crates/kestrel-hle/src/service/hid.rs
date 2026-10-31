//! Input: "hid" (Switchbrew, "HID services"). The commands libnx's
//! hidInitialize and padConfigureInput send (`nx/source/services/hid.c`,
//! `nx/source/runtime/pad.c`): CreateAppletResource and the shared memory
//! it hands out, the activate commands, and the supported npad styles and
//! IDs.
//!
//! Input reaches the program through the 0x40000-byte shared memory
//! ("SharedMemoryFormat"). Without an input source (`Process::input`) it
//! stays zeroed: every ring buffer is empty and no controller is connected,
//! which libnx reads as no input. With one, hid samples the source every
//! `SAMPLE_PERIOD` of guest time and writes the controller's state into its
//! npad entry ("NpadSharedMemoryEntry"), as libnx's hidGetNpadStates and
//! padUpdate read it.

use super::{Call, Interface, IpcError, Out};
use crate::input::{button, Controller, Pad, STICK_MAX};
use crate::kernel::{Kind, ObjectId, PROCESS_ID};
use crate::Process;
use kestrel_core::Perm;

/// How often hid samples the controller: every 5 ms of guest time (96,000
/// ticks of the 19.2 MHz clock), kestrel's choice.
pub const SAMPLE_PERIOD: u64 = 96_000;

/// Where things are in the shared memory ("HID services").
pub mod layout {
    /// NpadSharedMemoryFormat: ten 0x5000-byte entries, No1 to No8, then
    /// Handheld (8) and Other (9).
    pub const NPAD: usize = 0x9a00;
    pub const NPAD_ENTRY: usize = 0x5000;
    pub const HANDHELD_ENTRY: usize = 8;
    // NpadInternalState fields.
    pub const STYLE_SET: usize = 0x0;
    pub const JOY_ASSIGNMENT_MODE: usize = 0x4;
    pub const FULL_KEY_COLOR: usize = 0x8;
    pub const JOY_COLOR: usize = 0x14;
    pub const FULL_KEY_LIFO: usize = 0x28;
    pub const HANDHELD_LIFO: usize = 0x378;
    pub const DEVICE_TYPE: usize = 0x4188;
    pub const SYSTEM_PROPERTIES: usize = 0x4190;
    pub const APPLET_FOOTER_UI_TYPE: usize = 0x41ac;
    /// A RingLifo: a 0x20-byte header (unused, BufferCount, Tail, Count),
    /// then 17 entries of a u64 sampling number and the state.
    pub const LIFO_ENTRIES: usize = 17;
    pub const LIFO_HEADER: usize = 0x20;
    /// An NpadFullKeyStateAtomicStorage: the sampling number and an
    /// NpadFullKeyState (sampling number, buttons, the two sticks,
    /// attributes, reserved).
    pub const LIFO_ENTRY: usize = 0x30;
}

/// NpadStyleTag bits for the two controllers.
const STYLE_FULL_KEY: u32 = 1 << 0;
const STYLE_HANDHELD: u32 = 1 << 1;
/// NpadAttribute: IsConnected, IsWired, IsLeftConnected, IsLeftWired,
/// IsRightConnected, IsRightWired.
const CONNECTED: u32 = 1;
const ATTACHED_JOY_CON: u32 = 0x3f;
/// A stick pushed past half way also presses its direction (the
/// StickL/StickR direction buttons), kestrel's choice of threshold.
const STICK_DIRECTION: i32 = STICK_MAX / 2;

/// The size of the shared memory ("SharedMemoryFormat").
pub const SHARED_MEMORY_SIZE: u64 = 0x40000;
/// At most this many NpadIdTypes in SetSupportedNpadIdType.
const MAX_NPAD_IDS: usize = 10;
/// The NpadIdType values: No1 to No8, Other, Handheld.
const NPAD_IDS: [u32; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 0x10, 0x20];
/// The ActivateNpadWithRevision revisions Switchbrew lists.
const REVISIONS: [i32; 5] = [0, 1, 2, 3, 5];

/// What the process told hid.
#[derive(Debug, Default)]
pub struct State {
    pub shared_memory: Option<ObjectId>,
    /// SetSupportedNpadStyleSet's NpadStyleTag bits.
    pub style_set: u32,
    /// SetSupportedNpadIdType's list.
    pub npad_ids: Vec<u32>,
    /// ActivateNpad (revision 0) or ActivateNpadWithRevision.
    pub npad_revision: Option<i32>,
    /// The guest clock at which hid samples the controller next.
    pub next_sample: u64,
    /// Samples written to the shared memory so far.
    pub samples: u64,
    /// The program does not accept the controller; noted once.
    pub refused: bool,
}

/// The fields a connected controller's npad entry holds, and the ring it
/// is sampled into, as (NpadIdType, entry index, style, ring offset,
/// attributes, DeviceType, AppletFooterUiType). Switchbrew gives the
/// values; which ones go with the two controllers is kestrel's reading:
/// a Pro Controller is wireless player 1 (DeviceType FullKey, footer
/// SwitchProController); the attached Joy-Con are wired (DeviceType
/// HandheldLeft and HandheldRight, footer HandheldJoyConLeftJoyConRight).
fn npad(c: Controller) -> (u32, usize, u32, usize, u32, u32, u8) {
    use layout::*;
    match c {
        Controller::ProController => (0, 0, STYLE_FULL_KEY, FULL_KEY_LIFO, CONNECTED, 1, 12),
        Controller::Handheld => (
            0x20,
            HANDHELD_ENTRY,
            STYLE_HANDHELD,
            HANDHELD_LIFO,
            ATTACHED_JOY_CON,
            1 << 2 | 1 << 3,
            4,
        ),
    }
}

/// The stick direction buttons for `pad`'s sticks, added to its buttons.
pub fn with_stick_directions(pad: &Pad) -> u64 {
    let mut b = pad.buttons & button::PRESSABLE;
    for ((x, y), first) in [
        (pad.left, button::STICK_L_LEFT),
        (pad.right, button::STICK_R_LEFT),
    ] {
        // Left, Up, Right, Down.
        for (i, on) in [
            x < -STICK_DIRECTION,
            y > STICK_DIRECTION,
            x > STICK_DIRECTION,
            y < -STICK_DIRECTION,
        ]
        .into_iter()
        .enumerate()
        {
            if on {
                b |= first << i;
            }
        }
    }
    b
}

/// The bytes of one ring entry: the sampling number, then the state.
pub fn lifo_entry(n: u64, pad: &Pad, attributes: u32) -> [u8; layout::LIFO_ENTRY] {
    let mut e = [0u8; layout::LIFO_ENTRY];
    let clamp = |v: i32| v.clamp(-STICK_MAX, STICK_MAX);
    e[..8].copy_from_slice(&n.to_le_bytes());
    e[8..0x10].copy_from_slice(&n.to_le_bytes());
    e[0x10..0x18].copy_from_slice(&with_stick_directions(pad).to_le_bytes());
    for (at, v) in [
        (0x18, pad.left.0),
        (0x1c, pad.left.1),
        (0x20, pad.right.0),
        (0x24, pad.right.1),
    ] {
        e[at..at + 4].copy_from_slice(&clamp(v).to_le_bytes());
    }
    e[0x28..0x2c].copy_from_slice(&attributes.to_le_bytes());
    e
}

impl Process {
    /// Sample the input source into the shared memory once for every
    /// sampling period that has passed; the run loop calls this between
    /// slices. When the clock jumped (every thread slept), hid catches up
    /// with at most a ring's worth of samples, as a console keeps sampling
    /// while a program sleeps.
    pub(crate) fn hid_tick(&mut self) {
        let now = self.now();
        let hid = &mut self.services.hid;
        let behind = layout::LIFO_ENTRIES as u64 * SAMPLE_PERIOD;
        if now >= hid.next_sample + behind {
            hid.next_sample = now - (behind - SAMPLE_PERIOD);
        }
        while now >= self.services.hid.next_sample {
            self.services.hid.next_sample += SAMPLE_PERIOD;
            self.hid_sample();
        }
    }

    /// Write one sample of the controller, if one is connected.
    fn hid_sample(&mut self) {
        let (Some(memory), Some(source)) = (self.services.hid.shared_memory, &self.input) else {
            return;
        };
        let controller = source.controller();
        let (id, entry, style, lifo, attributes, device_type, footer) = npad(controller);
        // The controller connects once the program has activated npads and
        // accepts its style and ID (padConfigureInput sets both), as a
        // console only reports controllers a program supports.
        let hid = &self.services.hid;
        if hid.npad_revision.is_none() {
            return;
        }
        if hid.style_set & style == 0 || !hid.npad_ids.contains(&id) {
            // Only a list the program set can refuse the controller: one
            // still empty may be set by its next call (padConfigureInput
            // sends two), which another guest core may not have reached
            // when this one samples.
            let refused = (hid.style_set != 0 && hid.style_set & style == 0)
                || (!hid.npad_ids.is_empty() && !hid.npad_ids.contains(&id));
            if refused && !hid.refused {
                let what = format!(
                    "the program does not accept the emulated {controller:?} (style {style:#x}, \
                     NpadIdType {id:#x}); it supports styles {:#x} and IDs {:x?}",
                    hid.style_set, hid.npad_ids
                );
                self.services.hid.refused = true;
                self.note(what);
            }
            return;
        }
        let n = self.services.hid.samples;
        let pad = self.input.as_mut().expect("checked above").sample(n);
        let base = layout::NPAD + entry * layout::NPAD_ENTRY;
        let put = |p: &mut Process, at: usize, bytes: &[u8]| {
            p.write_shared_memory(memory, base + at, bytes);
        };
        if n == 0 {
            self.connect_npad(memory, base, controller, style, device_type, footer);
        }
        // The next ring entry, then the header pointing at it. The count
        // stops at 16, so the entry written next is never one a reader may
        // still count (kestrel's choice).
        let slot = (n as usize) % layout::LIFO_ENTRIES;
        let at = lifo + layout::LIFO_HEADER + slot * layout::LIFO_ENTRY;
        put(self, at, &lifo_entry(n, &pad, attributes));
        let count = (n + 1).min(layout::LIFO_ENTRIES as u64 - 1);
        let mut header = [0u8; 0x18];
        header[..8].copy_from_slice(&(layout::LIFO_ENTRIES as u64).to_le_bytes());
        header[8..0x10].copy_from_slice(&(slot as u64).to_le_bytes());
        header[0x10..].copy_from_slice(&count.to_le_bytes());
        put(self, lifo + 8, &header);
        self.services.hid.samples = n + 1;
    }

    /// The fields that say which controller is connected: the style, Dual
    /// assignment, the colours, DeviceType, the system properties
    /// (IsAbxyButtonOriented, IsPlusAvailable, IsMinusAvailable,
    /// IsDirectionalButtonsAvailable) and the footer type. The colours are
    /// kestrel's: a dark grey body with light grey buttons; the other
    /// colour block is NoController.
    fn connect_npad(
        &mut self,
        memory: ObjectId,
        base: usize,
        controller: Controller,
        style: u32,
        device_type: u32,
        footer: u8,
    ) {
        use layout::*;
        const OK: u32 = 0;
        const NO_CONTROLLER: u32 = 2;
        const BODY: u32 = 0xff2d_2d2d;
        const BUTTONS: u32 = 0xffe6_e6e6;
        let words = |ws: &[u32]| -> Vec<u8> { ws.iter().flat_map(|w| w.to_le_bytes()).collect() };
        let (full_key, joy) = match controller {
            Controller::ProController => (
                words(&[OK, BODY, BUTTONS]),
                words(&[NO_CONTROLLER, 0, 0, 0, 0]),
            ),
            Controller::Handheld => (
                words(&[NO_CONTROLLER, 0, 0]),
                words(&[OK, BODY, BUTTONS, BODY, BUTTONS]),
            ),
        };
        let properties: u64 = 1 << 11 | 1 << 13 | 1 << 14 | 1 << 15;
        for (at, bytes) in [
            (STYLE_SET, words(&[style, 0])),
            (FULL_KEY_COLOR, full_key),
            (JOY_COLOR, joy),
            (DEVICE_TYPE, device_type.to_le_bytes().to_vec()),
            (SYSTEM_PROPERTIES, properties.to_le_bytes().to_vec()),
            (APPLET_FOOTER_UI_TYPE, vec![footer]),
        ] {
            self.write_shared_memory(memory, base + at, &bytes);
        }
        debug_assert_eq!(JOY_ASSIGNMENT_MODE, STYLE_SET + 4);
    }

    /// Check the process ID descriptor and the AppletResourceUserId at
    /// `at`: only the one appletOE hands out (the process ID) is known.
    fn aruid(call: &Call, at: usize) -> Result<(), IpcError> {
        let aruid = call.u64(at)?;
        if !call.req.send_pid || aruid != PROCESS_ID {
            return Err(IpcError::Stop(format!(
                "{} command {} without the process ID and the applet resource user ID \
                 {PROCESS_ID:#x} (got {aruid:#x})",
                call.object.name(),
                call.n
            )));
        }
        Ok(())
    }

    pub(super) fn hid(&mut self, call: &Call) -> Result<Out, IpcError> {
        use Interface::*;
        match (call.object, call.n) {
            (Hid, 0) => {
                Self::aruid(call, 0)?;
                Ok(Out::object(AppletResource))
            }
            // ActivateTouchScreen, ActivateMouse, ActivateKeyboard,
            // ActivateNpad.
            (Hid, 11 | 21 | 31 | 103) => {
                Self::aruid(call, 0)?;
                if call.n == 103 {
                    self.services.hid.npad_revision = Some(0);
                }
                Ok(Out::empty())
            }
            // SetSupportedNpadStyleSet: u32 styles, padding, ARUID.
            (Hid, 100) => {
                Self::aruid(call, 8)?;
                self.services.hid.style_set = u32::from_le_bytes(call.arg(0)?);
                Ok(Out::empty())
            }
            (Hid, 101) => {
                Self::aruid(call, 0)?;
                Ok(Out::ok(self.services.hid.style_set.to_le_bytes().to_vec()))
            }
            // SetSupportedNpadIdType: the IDs in pointer buffer 0.
            (Hid, 102) => {
                Self::aruid(call, 0)?;
                let bytes = self.read_pointer(call.req, 0)?;
                let ids: Vec<u32> = bytes
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                    .collect();
                if bytes.len() % 4 != 0
                    || ids.len() > MAX_NPAD_IDS
                    || ids.iter().any(|id| !NPAD_IDS.contains(id))
                {
                    return Err(IpcError::Stop(format!(
                        "hid SetSupportedNpadIdType with {:#x} bytes {:02x?}: Switchbrew \
                         allows up to {MAX_NPAD_IDS} NpadIdType values",
                        bytes.len(),
                        bytes
                    )));
                }
                self.services.hid.npad_ids = ids;
                Ok(Out::empty())
            }
            // ActivateNpadWithRevision: s32 revision, padding, ARUID.
            (Hid, 109) => {
                Self::aruid(call, 8)?;
                let revision = i32::from_le_bytes(call.arg(0)?);
                if !REVISIONS.contains(&revision) {
                    return Err(IpcError::Stop(format!(
                        "hid ActivateNpadWithRevision with revision {revision}, \
                         which Switchbrew does not list"
                    )));
                }
                self.services.hid.npad_revision = Some(revision);
                Ok(Out::empty())
            }
            // GetSharedMemoryHandle: a copy of the one shared memory.
            (AppletResource, 0) => {
                let id = match self.services.hid.shared_memory {
                    Some(id) => id,
                    None => {
                        let id = self.create_shared_memory(SHARED_MEMORY_SIZE, Perm::R);
                        self.services.hid.shared_memory = Some(id);
                        id
                    }
                };
                Ok(match self.handles.add_handle(id, Kind::SharedMemory) {
                    Ok(h) => Out::copy(h),
                    Err(code) => Out::error(code as u32),
                })
            }
            _ => Err(call.unimplemented()),
        }
    }
}
