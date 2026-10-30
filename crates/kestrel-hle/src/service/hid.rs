//! Input: "hid" (Switchbrew, "HID services"). The commands libnx's
//! hidInitialize and padConfigureInput send (`nx/source/services/hid.c`,
//! `nx/source/runtime/pad.c`): CreateAppletResource and the shared memory
//! it hands out, the activate commands, and the supported npad styles and
//! IDs.
//!
//! kestrel has no host input yet, so the 0x40000-byte shared memory
//! ("SharedMemoryFormat") stays zeroed: every ring buffer is empty and no
//! controller is connected, which libnx reads as no input.

use super::{Call, Interface, IpcError, Out};
use crate::kernel::{Kind, ObjectId, PROCESS_ID};
use crate::Process;
use kestrel_core::Perm;

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
}

impl Process {
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
