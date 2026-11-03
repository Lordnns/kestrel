//! "nvdrv", the GPU driver's front (Switchbrew, "NV services"): a program
//! opens device files and sends them ioctls. These are the requests
//! libnx's nvInitialize, nvMapCreate and nvFenceInit send
//! (`nx/source/services/nv.c`, `nx/source/nvidia/map.c`, `fence.c`) when a
//! program sets up a framebuffer.
//!
//! Two devices so far: "/dev/nvmap" registers memory the program
//! allocated, which the display then reads frames from, and
//! "/dev/nvhost-ctrl" keeps the syncpoints (counters the GPU and the
//! display increment when work finishes; fences are syncpoint values)
//! and the events a program waits on for them (libnx's nvFenceWait). No
//! GPU runs yet, so only the program's own SYNCPT_INCR moves a
//! syncpoint.

use super::{Call, Interface, IpcError, Out};
use crate::kernel::{Kind, ObjectId};
use crate::Process;

/// NvError ("NV services").
pub mod error {
    pub const SUCCESS: u32 = 0;
    pub const NOT_SUPPORTED: u32 = 2;
    pub const NOT_INITIALIZED: u32 = 3;
    pub const BAD_PARAMETER: u32 = 4;
    pub const TIMEOUT: u32 = 5;
    /// libnx's nvConvertError names 13 AlreadyAllocated.
    pub const ALREADY_ALLOCATED: u32 = 13;
}

/// The /dev/nvhost-ctrl ioctls ("NV services", "/dev/nvhost-ctrl").
mod ctrl {
    pub const SYNCPT_READ: u32 = 0xc008_0014;
    pub const SYNCPT_INCR: u32 = 0x4004_0015;
    /// SYNCPT_INCR as libnx's nvioctlNvhostCtrl_SyncptIncr numbers it,
    /// with both direction bits (`_NV_IOWR`); "NV services" lists it as
    /// in only. kestrel's choice: both numbers mean SYNCPT_INCR.
    pub const SYNCPT_INCR_INOUT: u32 = 0xc004_0015;
    pub const SYNCPT_WAIT: u32 = 0xc00c_0016;
    pub const SYNCPT_WAITEX: u32 = 0xc010_0019;
    pub const SYNCPT_READ_MAX: u32 = 0xc008_001a;
    pub const SYNCPT_CLEAR_EVENT_WAIT: u32 = 0xc004_001c;
    pub const SYNCPT_WAIT_EVENT_EX: u32 = 0xc010_001e;
    pub const SYNCPT_ALLOC_EVENT: u32 = 0xc004_001f;
    pub const SYNCPT_FREE_EVENT: u32 = 0xc004_0020;
    pub const SYNCPT_FREE_EVENT_BATCH: u32 = 0x4008_0021;
}

/// How many syncpoints exist, IDs 0 to 191. "NV services" does not say;
/// kestrel's choice: the Tegra X1's host1x count (192, as Linux's host1x
/// driver lists for Tegra210). Other IDs get BadParameter.
pub const SYNCPOINTS: u32 = 192;

/// Syncpoint event slots, 0x00 to 0x3f ("NV services",
/// NVHOST_IOCTL_CTRL_SYNCPT_ALLOC_EVENT).
pub const EVENT_SLOTS: u32 = 64;

/// The event slot an event ID names. QueryEvent's ID for /dev/nvhost-ctrl
/// is `slot | (syncpoint & 0xfff) << 16 | valid << 28` ("NV services",
/// QueryEvent), and libnx's fences send CLEAR_EVENT_WAIT the same ID
/// (`0x10000000 | slot`). kestrel's choice: the slot is the low 16 bits;
/// the syncpoint and valid fields are not checked.
fn event_slot(id: u32) -> u32 {
    id & 0xffff
}

/// Whether a syncpoint at `value` has reached `threshold`: kestrel's
/// choice, a wrapping 32-bit compare (the value minus the threshold, as
/// signed, is not negative), so a fence keeps working when the counter
/// wraps.
pub fn reached(value: u32, threshold: u32) -> bool {
    value.wrapping_sub(threshold) as i32 >= 0
}

/// A syncpoint: its value, and the highest value work was promised for
/// (SYNCPT_READ_MAX). Both start at 0 (kestrel's choice).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Syncpoint {
    pub value: u32,
    pub max: u32,
}

/// An allocated event slot: its event, and the wait it was armed with
/// (SYNCPT_WAIT_EVENT_EX: a syncpoint and a threshold), until the
/// syncpoint reaches it or the wait is cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncptEvent {
    pub event: ObjectId,
    pub wait: Option<(u32, u32)>,
}

/// The /dev/nvmap ioctls ("NV services", "/dev/nvmap").
mod nvmap {
    pub const CREATE: u32 = 0xc008_0101;
    pub const CLAIM: u32 = 0x0000_0102;
    pub const FROM_ID: u32 = 0xc008_0103;
    pub const ALLOC: u32 = 0xc020_0104;
    pub const FREE: u32 = 0xc018_0105;
    pub const MMAP: u32 = 0xc028_0106;
    pub const WRITE: u32 = 0xc028_0107;
    pub const READ: u32 = 0xc028_0108;
    pub const PARAM: u32 = 0xc00c_0109;
    pub const PIN_MULT: u32 = 0xc010_010a;
    pub const UNPIN_MULT: u32 = 0xc010_010b;
    pub const CACHE: u32 = 0xc008_010c;
    pub const GET_IVC_ID: u32 = 0xc004_010d;
    pub const GET_ID: u32 = 0xc008_010e;
    pub const FROM_IVC_ID: u32 = 0xc004_010f;
    pub const SET_ALLOCATION_TAG_LABEL: u32 = 0x4004_0110;
    pub const RESERVE: u32 = 0x0000_0111;
    /// NVMAP_IOC_PARAM's HEAP answer, always this value.
    pub const HEAP: u32 = 0x4000_0000;
}

/// A device file a program opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    Nvmap,
    NvhostCtrl,
}

impl Device {
    fn path(self) -> &'static str {
        DEVICES
            .iter()
            .find(|(_, d)| *d == self)
            .map_or("?", |(p, _)| p)
    }
}

/// The device files kestrel has, by path.
const DEVICES: &[(&str, Device)] = &[
    ("/dev/nvmap", Device::Nvmap),
    ("/dev/nvhost-ctrl", Device::NvhostCtrl),
];

/// An nvmap object: memory registered with the driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapObject {
    pub size: u32,
    /// Set by NVMAP_IOC_ALLOC: where the memory is in the program, its
    /// alignment and its kind.
    pub addr: Option<u64>,
    pub align: u32,
    pub kind: u8,
    /// Handles naming it (CREATE and FROM_ID each add one, FREE takes one).
    pub refs: u32,
}

#[derive(Debug, Default)]
pub struct State {
    /// Initialize came.
    pub initialized: bool,
    /// Open device files by descriptor: descriptor n at n - 1 (kestrel's
    /// choice of numbers).
    pub files: Vec<Option<Device>>,
    /// nvmap objects: handle and ID n at n - 1 (kestrel's choice: one
    /// number names an object as both).
    pub maps: Vec<Option<MapObject>>,
    /// Syncpoints by ID, grown as they are touched (unlisted ones are at
    /// 0).
    pub syncpts: Vec<Syncpoint>,
    /// Syncpoint event slots, grown as they are allocated.
    pub events: Vec<Option<SyncptEvent>>,
}

impl State {
    /// Syncpoint `id`, if it exists.
    pub fn syncpoint(&self, id: u32) -> Option<Syncpoint> {
        if id >= SYNCPOINTS {
            return None;
        }
        Some(self.syncpts.get(id as usize).copied().unwrap_or_default())
    }

    fn syncpoint_mut(&mut self, id: u32) -> Option<&mut Syncpoint> {
        if id >= SYNCPOINTS {
            return None;
        }
        if self.syncpts.len() <= id as usize {
            self.syncpts.resize(id as usize + 1, Syncpoint::default());
        }
        Some(&mut self.syncpts[id as usize])
    }

    /// The event in slot `slot`, if allocated.
    pub fn event(&self, slot: u32) -> Option<SyncptEvent> {
        *self.events.get(slot as usize)?
    }

    fn event_mut(&mut self, slot: u32) -> Option<&mut Option<SyncptEvent>> {
        if slot >= EVENT_SLOTS {
            return None;
        }
        if self.events.len() <= slot as usize {
            self.events.resize(slot as usize + 1, None);
        }
        Some(&mut self.events[slot as usize])
    }

    /// The nvmap object with ID `id`.
    pub fn map(&self, id: u32) -> Option<&MapObject> {
        self.maps.get((id as usize).checked_sub(1)?)?.as_ref()
    }

    fn map_mut(&mut self, id: u32) -> Option<&mut MapObject> {
        self.maps.get_mut((id as usize).checked_sub(1)?)?.as_mut()
    }

    fn file(&self, fd: u32) -> Option<Device> {
        *self.files.get((fd as usize).checked_sub(1)?)?
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("four bytes"))
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// An NvError as the u32 output most commands end with.
fn nv(err: u32) -> Out {
    Out::ok(err.to_le_bytes().to_vec())
}

impl Process {
    pub(super) fn nvdrv(&mut self, call: &Call) -> Result<Out, IpcError> {
        if call.object != Interface::NvDrv {
            return Err(call.unimplemented());
        }
        match call.n {
            // Open: the path in send buffer 0, then the descriptor and an
            // NvError.
            0 => {
                let size = call.req.sends.first().map_or(0, |a| a.size);
                let path = self.read_send(call.req, 0, size)?;
                let len = path.iter().position(|&b| b == 0).unwrap_or(path.len());
                let path = String::from_utf8_lossy(&path[..len]).into_owned();
                let Some(&(_, device)) = DEVICES.iter().find(|(p, _)| *p == path) else {
                    return Err(IpcError::Unimplemented(format!(
                        "nvdrv device \"{}\"",
                        path.escape_default()
                    )));
                };
                let nv_state = &mut self.services.nv;
                if !nv_state.initialized {
                    return Ok(Out::ok(
                        [0, error::NOT_INITIALIZED].map(u32::to_le_bytes).concat(),
                    ));
                }
                let free = nv_state.files.iter().position(Option::is_none);
                let i = free.unwrap_or_else(|| {
                    nv_state.files.push(None);
                    nv_state.files.len() - 1
                });
                nv_state.files[i] = Some(device);
                let fd = i as u32 + 1;
                Ok(Out::ok([fd, error::SUCCESS].map(u32::to_le_bytes).concat()))
            }
            1 => self.nv_ioctl(call),
            // Close: a descriptor.
            2 => {
                let fd = u32::from_le_bytes(call.arg(0)?);
                match self
                    .services
                    .nv
                    .files
                    .get_mut((fd as usize).wrapping_sub(1))
                {
                    Some(f @ Some(_)) => {
                        *f = None;
                        Ok(nv(error::SUCCESS))
                    }
                    _ => Ok(nv(error::BAD_PARAMETER)),
                }
            }
            // Initialize: the process and a transfer memory as copy
            // handles, and the transfer memory's size. kestrel does not use
            // the memory and keeps no reference to it.
            3 => {
                call.arg::<4>(0)?;
                let hs = &call.req.copy_handles;
                let kinds: Vec<_> = hs
                    .iter()
                    .map(|&h| self.handles.get(h).map(|k| k.0))
                    .collect();
                if hs.len() != 2
                    || hs[0] as u64 != crate::svc::handle::CURRENT_PROCESS
                    || kinds[1] != Some(Kind::TransferMemory)
                {
                    return Err(IpcError::Stop(
                        "nvdrv Initialize without the current-process and transfer-memory \
                         handles it takes"
                            .into(),
                    ));
                }
                self.services.nv.initialized = true;
                Ok(nv(error::SUCCESS))
            }
            // QueryEvent: a descriptor and an event ID; an NvError and the
            // event as a copy handle.
            4 => {
                let fd = u32::from_le_bytes(call.arg(0)?);
                let id = u32::from_le_bytes(call.arg(4)?);
                match self.services.nv.file(fd) {
                    None => Ok(nv(error::BAD_PARAMETER)),
                    Some(Device::NvhostCtrl) => {
                        let Some(e) = self.services.nv.event(event_slot(id)) else {
                            return Ok(nv(error::BAD_PARAMETER));
                        };
                        match self.handles.add_handle(e.event, Kind::ReadableEvent) {
                            Ok(h) => Ok(Out {
                                copy_handles: vec![h],
                                ..nv(error::SUCCESS)
                            }),
                            Err(code) => Ok(Out::error(code as u32)),
                        }
                    }
                    Some(device) => Err(IpcError::Unimplemented(format!(
                        "nvdrv QueryEvent {id:#x} on {}",
                        device.path()
                    ))),
                }
            }
            // SetAruid: the process ID and the AppletResourceUserId.
            8 => {
                call.u64(0)?;
                Ok(nv(error::SUCCESS))
            }
            _ => Err(call.unimplemented()),
        }
    }

    /// Ioctl: a descriptor and an ioctl number, the ioctl's input in
    /// auto-select input buffer 0 and its output in auto-select output
    /// buffer 0, then an NvError.
    fn nv_ioctl(&mut self, call: &Call) -> Result<Out, IpcError> {
        let fd = u32::from_le_bytes(call.arg(0)?);
        let code = u32::from_le_bytes(call.arg(4)?);
        let Some(device) = self.services.nv.file(fd) else {
            return Ok(nv(error::BAD_PARAMETER));
        };
        // The ioctl number holds its argument's size and direction, as
        // Linux's _IOC does: bit 30 in (written by the program), bit 31
        // out.
        let size = (code >> 16 & 0x3fff) as usize;
        let mut arg = vec![0u8; size];
        if code & 1 << 30 != 0 {
            let input = self.read_auto(call.req, 0)?;
            if input.len() < size {
                return Err(IpcError::Stop(format!(
                    "nvdrv ioctl {code:#010x} with {:#x} bytes of input, fewer than its {size:#x}",
                    input.len()
                )));
            }
            arg.copy_from_slice(&input[..size]);
        }
        let err = match device {
            Device::Nvmap => self.nvmap_ioctl(code, &mut arg)?,
            Device::NvhostCtrl => self.nvhost_ctrl_ioctl(code, &mut arg)?,
        };
        if code & 1 << 31 != 0 && size > 0 {
            if Self::auto_room(call.req, 0)? < size as u64 {
                return Err(IpcError::Stop(format!(
                    "nvdrv ioctl {code:#010x} without room for its {size:#x} bytes of output"
                )));
            }
            self.write_auto(call.req, 0, &arg)?;
        }
        Ok(nv(err))
    }

    /// One /dev/nvhost-ctrl ioctl on `arg`, laid out as "NV services"
    /// shows ("Identical to Linux driver" for the syncpoint ones; the
    /// event ones are the Switch's own).
    fn nvhost_ctrl_ioctl(&mut self, code: u32, arg: &mut [u8]) -> Result<u32, IpcError> {
        let s = &mut self.services.nv;
        let id = u32_at(arg, 0);
        Ok(match code {
            // SYNCPT_READ and SYNCPT_READ_MAX: ID in, value out.
            ctrl::SYNCPT_READ | ctrl::SYNCPT_READ_MAX => {
                let Some(sp) = s.syncpoint(id) else {
                    return Ok(error::BAD_PARAMETER);
                };
                let value = if code == ctrl::SYNCPT_READ {
                    sp.value
                } else {
                    sp.max
                };
                put_u32(arg, 4, value);
                error::SUCCESS
            }
            // SYNCPT_INCR: ID in. The value and the max move together;
            // events waiting for the new value are signalled.
            ctrl::SYNCPT_INCR | ctrl::SYNCPT_INCR_INOUT => {
                if self.syncpoint_incr(id).is_none() {
                    return Ok(error::BAD_PARAMETER);
                }
                error::SUCCESS
            }
            // SYNCPT_WAIT: ID, threshold and timeout in; SYNCPT_WAITEX
            // also the value out.
            ctrl::SYNCPT_WAIT | ctrl::SYNCPT_WAITEX => {
                let (threshold, timeout) = (u32_at(arg, 4), u32_at(arg, 8) as i32);
                let Some(sp) = s.syncpoint(id) else {
                    return Ok(error::BAD_PARAMETER);
                };
                if reached(sp.value, threshold) {
                    if code == ctrl::SYNCPT_WAITEX {
                        put_u32(arg, 12, sp.value);
                    }
                    error::SUCCESS
                } else if timeout == 0 {
                    error::TIMEOUT
                } else {
                    // A console blocks here until the syncpoint moves or the
                    // time runs out; kestrel's services answer at once.
                    return Err(IpcError::Unimplemented(format!(
                        "nvdrv /dev/nvhost-ctrl ioctl {code:#010x} waiting {timeout} for \
                         syncpoint {id} to reach {threshold} (it is at {}): a request \
                         that blocks",
                        sp.value
                    )));
                }
            }
            // SYNCPT_WAIT_EVENT_EX: ID, threshold, timeout and an event slot
            // in. Reached: the value out. Not reached: the slot's event is
            // armed and signalled when the syncpoint gets there, and the
            // answer is Timeout with `slot | id << 4` out ("NV services").
            // kestrel's choice: the timeout is not used (libnx waits on the
            // event with its own, then clears the wait).
            ctrl::SYNCPT_WAIT_EVENT_EX => {
                let (threshold, slot) = (u32_at(arg, 4), u32_at(arg, 12));
                let Some(sp) = s.syncpoint(id) else {
                    return Ok(error::BAD_PARAMETER);
                };
                let Some(Some(e)) = s.event_mut(slot) else {
                    return Ok(error::BAD_PARAMETER);
                };
                if reached(sp.value, threshold) {
                    put_u32(arg, 12, sp.value);
                    return Ok(error::SUCCESS);
                }
                e.wait = Some((id, threshold));
                let event = e.event;
                self.set_event(event, false);
                put_u32(arg, 12, slot | id << 4);
                error::TIMEOUT
            }
            // SYNCPT_CLEAR_EVENT_WAIT: the slot's wait is dropped and its
            // event cleared.
            ctrl::SYNCPT_CLEAR_EVENT_WAIT => {
                let Some(Some(e)) = s.event_mut(event_slot(id)) else {
                    return Ok(error::BAD_PARAMETER);
                };
                e.wait = None;
                let event = e.event;
                self.set_event(event, false);
                error::SUCCESS
            }
            // SYNCPT_ALLOC_EVENT: a slot in. kestrel's choice: a slot already
            // allocated gives AlreadyAllocated.
            ctrl::SYNCPT_ALLOC_EVENT => match s.event_mut(id) {
                None => error::BAD_PARAMETER,
                Some(Some(_)) => error::ALREADY_ALLOCATED,
                Some(None) => {
                    let event = self.service_event();
                    *self.services.nv.event_mut(id).expect("checked") =
                        Some(SyncptEvent { event, wait: None });
                    error::SUCCESS
                }
            },
            // SYNCPT_FREE_EVENT: a slot in, which must be allocated
            // (kestrel's choice: BadParameter otherwise). Handles the
            // program holds to its event stay valid.
            ctrl::SYNCPT_FREE_EVENT => match s.event_mut(id) {
                Some(e @ Some(_)) => {
                    *e = None;
                    error::SUCCESS
                }
                _ => error::BAD_PARAMETER,
            },
            // SYNCPT_FREE_EVENT_BATCH: a mask of slots in. kestrel's choice:
            // bits for slots not allocated are ignored.
            ctrl::SYNCPT_FREE_EVENT_BATCH => {
                let mask = u64::from_le_bytes(arg[0..8].try_into().expect("eight bytes"));
                for slot in 0..EVENT_SLOTS {
                    if mask >> slot & 1 != 0 {
                        if let Some(e) = s.event_mut(slot) {
                            *e = None;
                        }
                    }
                }
                error::SUCCESS
            }
            _ => {
                return Err(IpcError::Unimplemented(format!(
                    "nvdrv /dev/nvhost-ctrl ioctl {code:#010x}"
                )))
            }
        })
    }

    /// Increment syncpoint `id` (and its max), and signal the events whose
    /// waits it reaches. None if there is no such syncpoint.
    pub(crate) fn syncpoint_incr(&mut self, id: u32) -> Option<()> {
        let s = &mut self.services.nv;
        let sp = s.syncpoint_mut(id)?;
        sp.value = sp.value.wrapping_add(1);
        sp.max = sp.max.wrapping_add(1);
        let value = sp.value;
        let mut done = Vec::new();
        for e in s.events.iter_mut().flatten() {
            if let Some((sid, threshold)) = e.wait {
                if sid == id && reached(value, threshold) {
                    e.wait = None;
                    done.push(e.event);
                }
            }
        }
        for event in done {
            self.set_event(event, true);
        }
        Some(())
    }

    /// One /dev/nvmap ioctl on `arg`, laid out as "NV services" shows.
    fn nvmap_ioctl(&mut self, code: u32, arg: &mut [u8]) -> Result<u32, IpcError> {
        let s = &mut self.services.nv;
        Ok(match code {
            // CREATE: size in, handle out.
            nvmap::CREATE => {
                let size = u32_at(arg, 0);
                if size == 0 {
                    return Ok(error::BAD_PARAMETER);
                }
                s.maps.push(Some(MapObject {
                    size,
                    addr: None,
                    align: 0,
                    kind: 0,
                    refs: 1,
                }));
                put_u32(arg, 4, s.maps.len() as u32);
                error::SUCCESS
            }
            // FROM_ID: ID in, handle out.
            nvmap::FROM_ID => match s.map_mut(u32_at(arg, 0)) {
                Some(m) => {
                    m.refs += 1;
                    let id = u32_at(arg, 0);
                    put_u32(arg, 4, id);
                    error::SUCCESS
                }
                None => error::BAD_PARAMETER,
            },
            // ALLOC: handle, heap mask, flags, alignment, kind, address.
            nvmap::ALLOC => {
                let (align, kind) = (u32_at(arg, 12), arg[16]);
                let addr = u64::from_le_bytes(arg[24..32].try_into().expect("eight bytes"));
                match s.map_mut(u32_at(arg, 0)) {
                    Some(m) if m.addr.is_none() => {
                        (m.addr, m.align, m.kind) = (Some(addr), align, kind);
                        error::SUCCESS
                    }
                    _ => error::BAD_PARAMETER,
                }
            }
            // FREE: handle in; address (0 while other handles remain),
            // size and flags out.
            nvmap::FREE => {
                let handle = u32_at(arg, 0);
                let Some(m) = s.map_mut(handle) else {
                    return Ok(error::BAD_PARAMETER);
                };
                m.refs -= 1;
                let (size, gone) = (m.size, m.refs == 0);
                let addr = if gone { m.addr.unwrap_or(0) } else { 0 };
                if gone {
                    s.maps[handle as usize - 1] = None;
                }
                arg[8..16].copy_from_slice(&addr.to_le_bytes());
                put_u32(arg, 16, size);
                put_u32(arg, 20, 0);
                error::SUCCESS
            }
            // PARAM: handle and parameter in, its value out.
            nvmap::PARAM => {
                let Some(m) = s.map(u32_at(arg, 0)) else {
                    return Ok(error::BAD_PARAMETER);
                };
                let value = match u32_at(arg, 4) {
                    1 => m.size,
                    2 => m.align,
                    4 => nvmap::HEAP,
                    5 => m.kind as u32,
                    _ => return Ok(error::BAD_PARAMETER),
                };
                put_u32(arg, 8, value);
                error::SUCCESS
            }
            // GET_ID: ID out first, handle in second.
            nvmap::GET_ID => {
                let handle = u32_at(arg, 4);
                if s.map(handle).is_none() {
                    return Ok(error::BAD_PARAMETER);
                }
                put_u32(arg, 0, handle);
                error::SUCCESS
            }
            // "NV services" lists these as returning NotSupported.
            nvmap::CLAIM
            | nvmap::MMAP
            | nvmap::WRITE
            | nvmap::READ
            | nvmap::PIN_MULT
            | nvmap::UNPIN_MULT
            | nvmap::CACHE
            | nvmap::GET_IVC_ID
            | nvmap::FROM_IVC_ID
            | nvmap::SET_ALLOCATION_TAG_LABEL
            | nvmap::RESERVE => error::NOT_SUPPORTED,
            _ => {
                return Err(IpcError::Unimplemented(format!(
                    "nvdrv /dev/nvmap ioctl {code:#010x}"
                )))
            }
        })
    }
}
