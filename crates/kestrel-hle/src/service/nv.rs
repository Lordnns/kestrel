//! "nvdrv", the GPU driver's front (Switchbrew, "NV services"): a program
//! opens device files and sends them ioctls. These are the requests
//! libnx's nvInitialize, nvMapCreate and nvFenceInit send
//! (`nx/source/services/nv.c`, `nx/source/nvidia/map.c`, `fence.c`) when a
//! program sets up a framebuffer.
//!
//! Only memory handles exist so far: "/dev/nvmap" registers memory the
//! program allocated, which the display then reads frames from, and
//! "/dev/nvhost-ctrl" opens but takes no ioctls yet (libnx's fences only
//! need it once the GPU runs, in phase 5).

use super::{Call, Interface, IpcError, Out};
use crate::kernel::Kind;
use crate::Process;

/// NvError ("NV services").
pub mod error {
    pub const SUCCESS: u32 = 0;
    pub const NOT_SUPPORTED: u32 = 2;
    pub const NOT_INITIALIZED: u32 = 3;
    pub const BAD_PARAMETER: u32 = 4;
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
}

impl State {
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
            Device::NvhostCtrl => {
                return Err(IpcError::Unimplemented(format!(
                    "nvdrv /dev/nvhost-ctrl ioctl {code:#010x}"
                )))
            }
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
