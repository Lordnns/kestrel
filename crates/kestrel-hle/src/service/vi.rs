//! "vi:u", the display (Switchbrew, "VI services" and "Nvnflinger
//! services"), and the buffer queue behind a layer. These are the requests
//! libnx's viInitialize, viOpenDefaultDisplay, viCreateLayer and its
//! native window send (`nx/source/services/vi.c`, `display/*.c`).
//!
//! A program opens the "Default" display and its layer, whose native
//! window names a binder object: a buffer queue that speaks Android's
//! IGraphicBufferProducer through IHOSBinderDriver's TransactParcel. The
//! program hands the queue its buffers (memory registered with nvmap),
//! then dequeues one, draws into it and queues it back. kestrel shows a
//! queued buffer at once (`display.rs`), and frees it at a vsync, 60 times
//! a second of guest time, when the buffer-free event is signalled: frames
//! take turns on the screen, each for its swap interval in vsyncs.

use super::nv;
use super::{Call, Interface, IpcError, Out};
use crate::display::Surface;
use crate::kernel::{ObjectId, PROCESS_ID};
use crate::Process;

/// vi's results (module 114): an interface the service does not give
/// ("VI services", IApplicationDisplayService).
pub mod result {
    pub const NOT_AVAILABLE: u32 = 0xa72;
}

/// The display kestrel has: "Default", as display ID 1 (kestrel's choice).
const DEFAULT_DISPLAY: u64 = 1;
/// GetDisplayResolution for the docked console: the "Default" display is
/// 1080p ("VI services", DisplayInfo).
const RESOLUTION: (u64, u64) = (1920, 1080);
/// The layer CreateManagedDisplayLayer makes, and the binder object of its
/// buffer queue (kestrel's choices).
pub const LAYER_ID: u64 = 1;
const BINDER_ID: i32 = 1;
/// The size the buffer queue reports before buffers set one (kestrel's
/// choice: the handheld screen, which libnx also asks for).
const DEFAULT_SIZE: (u32, u32) = (1280, 720);
/// GetNativeHandle's argument for the buffer-free event (libnx's
/// nwindowCreate asks for 0x0f).
const BUFFER_FREE_EVENT: u32 = 0x0f;
/// Guest ticks between vsyncs: 60 Hz of the 19.2 MHz clock.
pub const VSYNC_PERIOD: u64 = 19_200_000 / 60;
/// IGraphicBufferProducer's interface token.
const INTERFACE: &str = "android.gui.IGraphicBufferProducer";
/// Android status codes (negated errno values, `Errors.h`).
const BAD_VALUE: i32 = -22;
const WOULD_BLOCK: i32 = -11;
const NO_INIT: i32 = -19;
/// Buffer slots in a queue (Android's NUM_BUFFER_SLOTS on the Switch).
const SLOTS: usize = 64;

/// IGraphicBufferProducer's transaction codes, as libnx numbers them
/// (`display/buffer_producer.c`).
mod code {
    pub const REQUEST_BUFFER: u32 = 1;
    pub const DEQUEUE_BUFFER: u32 = 3;
    pub const QUEUE_BUFFER: u32 = 7;
    pub const CANCEL_BUFFER: u32 = 8;
    pub const QUERY: u32 = 9;
    pub const CONNECT: u32 = 10;
    pub const DISCONNECT: u32 = 11;
    pub const SET_PREALLOCATED_BUFFER: u32 = 14;
}

/// What QUERY asks for: Android's NATIVE_WINDOW_* query constants
/// (`system/window.h`, `vndk/window.h`, Apache 2.0).
mod query {
    pub const WIDTH: i32 = 0;
    pub const HEIGHT: i32 = 1;
    pub const FORMAT: i32 = 2;
    pub const DEFAULT_WIDTH: i32 = 6;
    pub const DEFAULT_HEIGHT: i32 = 7;
    pub const TRANSFORM_HINT: i32 = 8;
    pub const CONSUMER_RUNNING_BEHIND: i32 = 9;
}

/// The format QUERY answers before the program sets buffers: kestrel's
/// choice, RGBA 8888 (1), Android's BufferQueue default.
const DEFAULT_FORMAT: i32 = 1;

/// A QueueBufferInput as libnx's BqBufferInput lays it out (0x54 bytes):
/// a timestamp, isAutoTimestamp, the crop, scaling mode, transform,
/// sticky transform, a word libnx leaves 0, the swap interval at 0x2c,
/// then the NvMultiFence at 0x30 (a count, then up to four fences, each a
/// syncpoint ID and a value).
struct QueueInput {
    swap_interval: u32,
    fences: Vec<(u32, u32)>,
}

impl QueueInput {
    const SIZE: usize = 0x54;

    fn parse(b: &[u8]) -> Result<Self, IpcError> {
        if b.len() < Self::SIZE {
            return Err(IpcError::Stop(format!(
                "a QueueBufferInput of {:#x} bytes, fewer than its {:#x}",
                b.len(),
                Self::SIZE
            )));
        }
        let word = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().expect("four bytes"));
        let count = word(0x30);
        if count > 4 {
            return Err(IpcError::Stop(format!(
                "an NvMultiFence of {count} fences, more than its four"
            )));
        }
        let fences = (0..count as usize)
            .map(|i| (word(0x34 + 8 * i), word(0x38 + 8 * i)))
            .collect();
        Ok(QueueInput {
            swap_interval: word(0x2c),
            fences,
        })
    }
}

/// A slot's buffer: the GraphicBuffer the program set, flattened, and
/// what kestrel reads from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Buffer {
    pub flattened: Vec<u8>,
    pub nvmap_id: u32,
    pub offset: u32,
    pub surface: Surface,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// The program may dequeue it; `freed` orders the free slots, oldest
    /// first.
    Free {
        freed: u64,
    },
    Dequeued,
    /// Shown, freed at the vsync at `release`.
    Queued {
        release: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub buffer: Buffer,
    pub state: SlotState,
}

#[derive(Debug, Default)]
pub struct State {
    pub display_open: bool,
    /// CreateManagedDisplayLayer made the layer; OpenLayer opened it.
    pub layer_made: bool,
    pub layer_open: bool,
    pub connected: bool,
    pub slots: Vec<Option<Slot>>,
    /// Counts frees, to order the free slots.
    pub frees: u64,
    /// Signalled while a slot is free.
    pub buffer_free_event: Option<ObjectId>,
    /// Frames shown.
    pub frames: u64,
    /// The vsync that frees the last frame queued with a swap interval:
    /// the next frame's turn on the screen starts there.
    pub last_release: u64,
    /// GetDisplayVsyncEvent's event, signalled at every vsync after it
    /// was made.
    pub vsync_event: Option<ObjectId>,
    /// The last vsync seen, counted from 0 (the clock over the period).
    pub vsyncs: u64,
}

impl State {
    /// The next vsync that frees a queued buffer.
    pub fn next_release(&self) -> Option<u64> {
        self.slots
            .iter()
            .flatten()
            .filter_map(|s| match s.state {
                SlotState::Queued { release } => Some(release),
                _ => None,
            })
            .min()
    }
}

/// A parcel's payload, read front to back as Android's Parcel reads it:
/// 32-bit words, everything padded to four bytes.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> Result<&[u8], IpcError> {
        let end = self.at + n.next_multiple_of(4);
        let b = self
            .data
            .get(self.at..self.at + n)
            .filter(|_| end <= self.data.len());
        let b = b.ok_or_else(|| IpcError::Stop("a parcel shorter than its contents".into()))?;
        self.at = end;
        Ok(b)
    }

    fn i32(&mut self) -> Result<i32, IpcError> {
        Ok(i32::from_le_bytes(
            self.bytes(4)?.try_into().expect("four bytes"),
        ))
    }

    /// A flattened object: its length, its file descriptor count (always
    /// 0 on the Switch), then its bytes.
    fn flattened(&mut self) -> Result<Vec<u8>, IpcError> {
        let len = self.i32()?;
        let fds = self.i32()?;
        if len < 0 || fds != 0 {
            return Err(IpcError::Stop(format!(
                "a flattened object of {len} bytes with {fds} file descriptors"
            )));
        }
        Ok(self.bytes(len as usize)?.to_vec())
    }

    /// writeInterfaceToken: a strict-mode policy word, then the
    /// interface name as a String16 (its length, then UTF-16 with a NUL).
    fn interface_token(&mut self) -> Result<String, IpcError> {
        self.i32()?;
        let len = self.i32()?;
        if !(0..=0x100).contains(&len) {
            return Err(IpcError::Stop(format!(
                "an interface token of {len} characters"
            )));
        }
        let raw = self.bytes((len as usize + 1) * 2)?;
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take(len as usize)
            .collect();
        Ok(String::from_utf16_lossy(&units))
    }
}

/// A reply parcel: the payload words.
#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend(v.to_le_bytes());
        self
    }

    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend(v.to_le_bytes());
        self
    }

    fn flattened(&mut self, b: &[u8]) -> &mut Self {
        self.i32(b.len() as i32).i32(0);
        self.0.extend(b);
        self.0.resize(self.0.len().next_multiple_of(4), 0);
        self
    }

    /// The parcel: its header ("VI services", Parcel) and the payload,
    /// with no objects.
    fn parcel(&self) -> Vec<u8> {
        let size = self.0.len() as u32;
        let mut p = [size, 0x10, 0, 0x10 + size].map(u32::to_le_bytes).concat();
        p.extend(&self.0);
        p
    }
}

/// The parcel's payload: the bytes its header points at.
fn payload(parcel: &[u8]) -> Result<&[u8], IpcError> {
    let word = |at: usize| {
        parcel
            .get(at..at + 4)
            .map(|w| u32::from_le_bytes(w.try_into().expect("four bytes")) as usize)
    };
    let (Some(size), Some(off)) = (word(0), word(4)) else {
        return Err(IpcError::Stop("a parcel without its header".into()));
    };
    parcel.get(off..off.saturating_add(size)).ok_or_else(|| {
        IpcError::Stop(format!(
            "a parcel of {size} bytes at {off:#x}, past its end"
        ))
    })
}

/// The output of CONNECT and QUEUE_BUFFER (libnx's BqBufferOutput):
/// width, height, transform hint, buffers waiting to be shown.
fn buffer_output(w: &mut Writer, size: (u32, u32), pending: u32) {
    w.u32(size.0).u32(size.1).u32(0).u32(pending);
}

/// An empty NvMultiFence (no fences), flattened: libnx's dequeue reads
/// 0x24 bytes and waits on nothing.
const NO_FENCE: [u8; 0x24] = [0; 0x24];

/// What kestrel reads from a GraphicBuffer: the flattened header ("GBFR"
/// magic, width, height, stride, format, usage, pid, refcount, fd and int
/// counts) and the ints that follow, the NvGraphicBuffer after its
/// NativeHandle header (libnx's `nvidia/graphic_buffer.h`): the nvmap ID
/// at 4, then plane 0 from 0x34, its layout at 0x44, pitch 0x48, offset
/// 0x50 and block height 0x58.
fn parse_buffer(flat: &[u8]) -> Result<Buffer, String> {
    let word = |at: usize| {
        flat.get(at..at + 4)
            .map(|w| u32::from_le_bytes(w.try_into().expect("four bytes")))
            .ok_or_else(|| format!("a graphic buffer of {:#x} bytes", flat.len()))
    };
    if word(0)? != 0x4742_4652 {
        return Err(format!("a graphic buffer with magic {:#x}", word(0)?));
    }
    let ints = 40;
    let int = |at: usize| word(ints + at);
    if (word(36)? as usize) < 0x5c / 4 {
        return Err(format!("a graphic buffer with {} ints", word(36)?));
    }
    let surface = Surface {
        width: word(4)?,
        height: word(8)?,
        format: word(16)?,
        layout: int(0x44)?,
        pitch: int(0x48)?,
        block_height_log2: int(0x58)?,
    };
    surface.size()?;
    Ok(Buffer {
        flattened: flat.to_vec(),
        nvmap_id: int(4)?,
        offset: int(0x50)?,
        surface,
    })
}

impl Process {
    pub(super) fn vi(&mut self, call: &Call) -> Result<Out, IpcError> {
        use Interface::*;
        let ok = || Ok(Out::empty());
        match (call.object, call.n) {
            // GetDisplayService: 0 for an application; 1 asks for the
            // system's privileges, which vi:u refuses.
            (ViU, 0) => match u32::from_le_bytes(call.arg(0)?) {
                0 => Ok(Out::object(ApplicationDisplayService)),
                _ => Ok(Out::error(result::NOT_AVAILABLE)),
            },
            (ApplicationDisplayService, 100) => Ok(Out::object(HosBinderDriver)),
            // GetSystemDisplayService, GetManagerDisplayService,
            // GetIndirectDisplayTransactionService: not through vi:u.
            (ApplicationDisplayService, 101..=103) => Ok(Out::error(result::NOT_AVAILABLE)),
            // OpenDisplay: a DisplayName.
            (ApplicationDisplayService, 1010) => {
                let name = display_name(&call.arg::<0x40>(0)?);
                if name != "Default" {
                    return Err(IpcError::Unimplemented(format!(
                        "vi display \"{}\"",
                        name.escape_default()
                    )));
                }
                self.services.vi.display_open = true;
                Ok(Out::ok(DEFAULT_DISPLAY.to_le_bytes().to_vec()))
            }
            (ApplicationDisplayService, 1020) => {
                self.display_id(call, 0)?;
                self.services.vi.display_open = false;
                ok()
            }
            (ApplicationDisplayService, 1102) => {
                self.display_id(call, 0)?;
                Ok(Out::ok(
                    [RESOLUTION.0, RESOLUTION.1].map(u64::to_le_bytes).concat(),
                ))
            }
            // OpenLayer: the process ID, the native window's receive
            // buffer, a DisplayName, the layer and the
            // AppletResourceUserId. Answers the native window's size.
            (ApplicationDisplayService, 2020) => {
                let name = display_name(&call.arg::<0x40>(0)?);
                let (layer, aruid) = (call.u64(0x40)?, call.u64(0x48)?);
                if !call.req.send_pid || aruid != PROCESS_ID {
                    return Err(IpcError::Stop(
                        "vi OpenLayer without the process ID and the program's \
                         AppletResourceUserId"
                            .into(),
                    ));
                }
                let vi = &self.services.vi;
                if name != "Default" || !vi.display_open || layer != LAYER_ID || !vi.layer_made {
                    return Err(IpcError::Stop(format!(
                        "vi OpenLayer of layer {layer} on display \"{}\", which the program \
                         has not made and opened",
                        name.escape_default()
                    )));
                }
                let window = native_window();
                self.write_buffer(call.req, 0, 0x100, &window)?;
                self.services.vi.layer_open = true;
                Ok(Out::ok((window.len() as u64).to_le_bytes().to_vec()))
            }
            (ApplicationDisplayService, 2021) => {
                if call.u64(0)? != LAYER_ID || !self.services.vi.layer_open {
                    return Err(IpcError::Stop("vi CloseLayer of a layer not open".into()));
                }
                self.services.vi.layer_open = false;
                ok()
            }
            // SetLayerScalingMode: the mode (libnx sends a u32 and padding)
            // and the layer. kestrel scales the frame to the window.
            (ApplicationDisplayService, 2101) => {
                call.u64(8)?;
                ok()
            }
            // GetDisplayVsyncEvent: the display ID; an event signalled at
            // every vsync ("VI services"). kestrel's choices: each call
            // copies a handle to the same event, and its first signal is
            // the first vsync after the first call.
            (ApplicationDisplayService, 5202) => {
                self.display_id(call, 0)?;
                let fresh = self.services.vi.vsync_event.is_none();
                let out = self.event_handle(|s| &mut s.vi.vsync_event);
                if fresh {
                    self.services.vi.vsyncs = self.now() / VSYNC_PERIOD;
                }
                Ok(out)
            }
            (HosBinderDriver, 0 | 3) => self.transact_parcel(call),
            // AdjustRefcount: the binder ID, an amount and which count.
            (HosBinderDriver, 1) => {
                self.binder_id(call)?;
                call.arg::<8>(4)?;
                ok()
            }
            // GetNativeHandle: the binder ID and which handle.
            (HosBinderDriver, 2) => {
                self.binder_id(call)?;
                match u32::from_le_bytes(call.arg(4)?) {
                    BUFFER_FREE_EVENT => Ok(self.event_handle(|s| &mut s.vi.buffer_free_event)),
                    n => Err(IpcError::Unimplemented(format!(
                        "IHOSBinderDriver GetNativeHandle {n:#x}"
                    ))),
                }
            }
            _ => Err(call.unimplemented()),
        }
    }

    fn display_id(&self, call: &Call, at: usize) -> Result<(), IpcError> {
        match call.u64(at)? {
            DEFAULT_DISPLAY if self.services.vi.display_open => Ok(()),
            id => Err(IpcError::Stop(format!(
                "vi display {id}, which is not open"
            ))),
        }
    }

    fn binder_id(&self, call: &Call) -> Result<(), IpcError> {
        match i32::from_le_bytes(call.arg(0)?) {
            BINDER_ID if self.services.vi.layer_open => Ok(()),
            id => Err(IpcError::Stop(format!(
                "binder object {id}, which is not open"
            ))),
        }
    }

    /// TransactParcel (0, mapped buffers) and TransactParcelAuto (3,
    /// auto-select buffers): the binder ID, the transaction code and
    /// flags, the request parcel in input buffer 0 and the reply parcel
    /// in output buffer 0.
    fn transact_parcel(&mut self, call: &Call) -> Result<Out, IpcError> {
        self.binder_id(call)?;
        let tx = u32::from_le_bytes(call.arg(4)?);
        call.arg::<4>(8)?;
        let auto = call.n == 3;
        let parcel = if auto {
            self.read_auto(call.req, 0)?
        } else {
            let size = call.req.sends.first().map_or(0, |a| a.size);
            self.read_send(call.req, 0, size)?
        };
        let mut r = Reader {
            data: payload(&parcel)?,
            at: 0,
        };
        let token = r.interface_token()?;
        if token != INTERFACE {
            return Err(IpcError::Stop(format!(
                "a binder transaction for \"{}\"",
                token.escape_default()
            )));
        }
        let reply = self.buffer_queue(tx, &mut r)?.parcel();
        let room = if auto {
            Self::auto_room(call.req, 0)?
        } else {
            call.req.receives.first().map_or(0, |b| b.size)
        };
        if room < reply.len() as u64 {
            return Err(IpcError::Stop(format!(
                "a reply parcel of {:#x} bytes, more than the {room:#x} the program gave",
                reply.len()
            )));
        }
        if auto {
            self.write_auto(call.req, 0, &reply)?;
        } else {
            self.write_buffer(call.req, 0, reply.len() as u64, &reply)?;
        }
        Ok(Out::empty())
    }

    /// One IGraphicBufferProducer transaction; the reply's payload.
    fn buffer_queue(&mut self, tx: u32, r: &mut Reader) -> Result<Writer, IpcError> {
        let mut w = Writer::default();
        let size = self.queue_size();
        match tx {
            // CONNECT: a listener (none), the API, producerControlledByApp.
            code::CONNECT => {
                let (listener, _api, _by_app) = (r.i32()?, r.i32()?, r.i32()?);
                if listener != 0 {
                    return Err(IpcError::Stop("a buffer queue listener".into()));
                }
                let vi = &mut self.services.vi;
                let status = if vi.connected { BAD_VALUE } else { 0 };
                vi.connected = true;
                buffer_output(&mut w, size, 0);
                w.i32(status);
            }
            // DISCONNECT: the API. The queue forgets its buffers.
            code::DISCONNECT => {
                r.i32()?;
                let vi = &mut self.services.vi;
                let status = if vi.connected { 0 } else { NO_INIT };
                vi.connected = false;
                vi.slots.clear();
                vi.last_release = 0;
                self.update_buffer_free();
                w.i32(status);
            }
            // SET_PREALLOCATED_BUFFER: the slot, whether a buffer follows,
            // the flattened GraphicBuffer. No reply.
            code::SET_PREALLOCATED_BUFFER => {
                let slot = r.i32()?;
                let has = r.i32()?;
                let slot = usize::try_from(slot)
                    .ok()
                    .filter(|&s| s < SLOTS)
                    .ok_or_else(|| IpcError::Stop(format!("buffer slot {slot}")))?;
                let vi = &mut self.services.vi;
                if vi.slots.len() <= slot {
                    vi.slots.resize(slot + 1, None);
                }
                vi.slots[slot] = if has != 0 {
                    let buffer = parse_buffer(&r.flattened()?).map_err(IpcError::Stop)?;
                    vi.frees += 1;
                    Some(Slot {
                        buffer,
                        state: SlotState::Free { freed: vi.frees },
                    })
                } else {
                    None
                };
                self.update_buffer_free();
            }
            // DEQUEUE_BUFFER: async, width, height, format, usage. Answers
            // the free slot freed longest ago and an empty fence, or
            // WOULD_BLOCK.
            code::DEQUEUE_BUFFER => {
                for _ in 0..5 {
                    r.i32()?;
                }
                let free = self
                    .services
                    .vi
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(|(i, s)| match s.as_ref()?.state {
                        SlotState::Free { freed } => Some((freed, i)),
                        _ => None,
                    });
                match free.min() {
                    Some((_, i)) => {
                        self.services.vi.slots[i].as_mut().expect("free").state =
                            SlotState::Dequeued;
                        self.update_buffer_free();
                        w.i32(i as i32).i32(1).flattened(&NO_FENCE).i32(0);
                    }
                    None => {
                        w.i32(-1).i32(0).i32(WOULD_BLOCK);
                    }
                }
            }
            // REQUEST_BUFFER: the slot. Answers its GraphicBuffer.
            code::REQUEST_BUFFER => {
                let slot = r.i32()?;
                match self.slot(slot) {
                    Some(s) => {
                        let flat = s.buffer.flattened.clone();
                        w.i32(1).flattened(&flat).i32(0);
                    }
                    None => {
                        w.i32(0).i32(BAD_VALUE);
                    }
                }
            }
            // QUEUE_BUFFER: the slot and its QueueBufferInput. The frame is
            // shown now, once its fences are reached, and the buffer freed
            // when its turn on the screen ends (`release_at`).
            code::QUEUE_BUFFER => {
                let slot = r.i32()?;
                let input = QueueInput::parse(&r.flattened()?)?;
                let status = match self.slot(slot) {
                    Some(s) if s.state == SlotState::Dequeued => {
                        let buffer = s.buffer.clone();
                        self.check_fences(&input.fences)?;
                        self.present(&buffer)?;
                        let release = self.release_at(input.swap_interval);
                        let s = self.services.vi.slots[slot as usize]
                            .as_mut()
                            .expect("slot");
                        s.state = SlotState::Queued { release };
                        0
                    }
                    _ => BAD_VALUE,
                };
                buffer_output(&mut w, size, self.pending());
                w.i32(status);
            }
            // QUERY: what to ask (Android's NATIVE_WINDOW_* constants);
            // the value and a status. Only what kestrel knows is answered;
            // the rest stops, named.
            code::QUERY => {
                let what = r.i32()?;
                let vi = &self.services.vi;
                let value = match what {
                    query::WIDTH | query::DEFAULT_WIDTH => size.0 as i32,
                    query::HEIGHT | query::DEFAULT_HEIGHT => size.1 as i32,
                    query::FORMAT => vi
                        .slots
                        .iter()
                        .flatten()
                        .next()
                        .map_or(DEFAULT_FORMAT, |s| s.buffer.surface.format as i32),
                    // As CONNECT and QUEUE_BUFFER answer it.
                    query::TRANSFORM_HINT => 0,
                    // "More than one buffer behind": more than one queued
                    // and not yet freed, as libnx reads numPendingBuffers.
                    query::CONSUMER_RUNNING_BEHIND => (self.pending() > 1) as i32,
                    what => {
                        return Err(IpcError::Unimplemented(format!(
                            "IGraphicBufferProducer QUERY {what}"
                        )))
                    }
                };
                w.i32(value).i32(0);
            }
            // CANCEL_BUFFER: the slot and a fence. No reply.
            code::CANCEL_BUFFER => {
                let slot = r.i32()?;
                r.flattened()?;
                if self
                    .slot(slot)
                    .is_some_and(|s| s.state == SlotState::Dequeued)
                {
                    self.free_slot(slot as usize);
                }
            }
            tx => {
                return Err(IpcError::Unimplemented(format!(
                    "IGraphicBufferProducer transaction {tx}"
                )))
            }
        }
        Ok(w)
    }

    /// Buffers queued and not yet freed.
    fn pending(&self) -> u32 {
        let slots = self.services.vi.slots.iter().flatten();
        slots
            .filter(|s| matches!(s.state, SlotState::Queued { .. }))
            .count() as u32
    }

    /// The vsync that frees a buffer queued now with swap interval `n`.
    /// The swap interval is the minimum number of vsyncs a frame stays on
    /// the screen (EGL's eglSwapInterval, Android's setSwapInterval), so
    /// frames take turns. kestrel's choices: a frame's turn starts at the
    /// start of the vsync period it is queued in (kestrel shows it at
    /// once) or when the previous frame's turn ends, whichever is later,
    /// and lasts `n` vsyncs; its buffer is freed when the turn ends. With
    /// 0 it is freed at the next vsync, without waiting for its turn.
    fn release_at(&mut self, n: u32) -> u64 {
        let next = (self.now() / VSYNC_PERIOD + 1) * VSYNC_PERIOD;
        if n == 0 {
            return next;
        }
        let vi = &mut self.services.vi;
        let n = n as u64 * VSYNC_PERIOD;
        let release = (next + n - VSYNC_PERIOD).max(vi.last_release + n);
        vi.last_release = release;
        release
    }

    /// A queued buffer's fences: each a syncpoint and a value the GPU must
    /// reach before the frame is shown. With no GPU yet, one not reached
    /// would never be: it stops, named.
    fn check_fences(&self, fences: &[(u32, u32)]) -> Result<(), IpcError> {
        for &(id, value) in fences {
            match self.services.nv.syncpoint(id) {
                None => {
                    return Err(IpcError::Stop(format!(
                        "a queued buffer's fence on syncpoint {id}, which does not exist"
                    )))
                }
                Some(sp) if !nv::reached(sp.value, value) => {
                    return Err(IpcError::Unimplemented(format!(
                        "a queued buffer waiting for syncpoint {id} to reach {value} (it is \
                         at {}): no GPU increments it yet",
                        sp.value
                    )))
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    fn slot(&self, slot: i32) -> Option<&Slot> {
        self.services
            .vi
            .slots
            .get(usize::try_from(slot).ok()?)?
            .as_ref()
    }

    /// The size the queue reports: its buffers', once set.
    fn queue_size(&self) -> (u32, u32) {
        let first = self.services.vi.slots.iter().flatten().next();
        first.map_or(DEFAULT_SIZE, |s| {
            (s.buffer.surface.width, s.buffer.surface.height)
        })
    }

    fn free_slot(&mut self, i: usize) {
        let vi = &mut self.services.vi;
        vi.frees += 1;
        if let Some(s) = vi.slots[i].as_mut() {
            s.state = SlotState::Free { freed: vi.frees };
        }
        self.update_buffer_free();
    }

    /// The buffer-free event is signalled while a slot is free.
    fn update_buffer_free(&mut self) {
        let vi = &self.services.vi;
        let free = vi
            .slots
            .iter()
            .flatten()
            .any(|s| matches!(s.state, SlotState::Free { .. }));
        if let Some(id) = vi.buffer_free_event {
            self.set_event(id, free);
        }
    }

    /// Show `buffer`: read it from the memory its nvmap object names.
    fn present(&mut self, buffer: &Buffer) -> Result<(), IpcError> {
        let stop = |what: String| IpcError::Stop(format!("a queued buffer {what}"));
        let Some(map) = self.services.nv.map(buffer.nvmap_id) else {
            return Err(stop(format!(
                "in nvmap object {}, which does not exist",
                buffer.nvmap_id
            )));
        };
        let size = buffer.surface.size().map_err(stop)?;
        let Some(addr) = map
            .addr
            .filter(|_| buffer.offset as usize + size <= map.size as usize)
        else {
            return Err(stop(format!(
                "of {size:#x} bytes at {:#x} in nvmap object {}, which holds {:#x}",
                buffer.offset, buffer.nvmap_id, map.size
            )));
        };
        let memory = self
            .mem
            .read_vec(addr + buffer.offset as u64, size)
            .map_err(|e| stop(format!("in memory kestrel cannot read: {e}")))?;
        let frame = buffer.surface.frame(&memory);
        self.services.vi.frames += 1;
        if let Some(screen) = self.screen.as_mut() {
            screen.present(&frame);
        }
        Ok(())
    }

    /// Free the buffers whose vsync has come, and signal the vsync event
    /// at each new vsync. Called from the run loop and the scheduler, which
    /// wakes for the next one.
    pub(crate) fn vsync_tick(&mut self) {
        let now = self.now();
        let vsync = now / VSYNC_PERIOD;
        if vsync > self.services.vi.vsyncs {
            self.services.vi.vsyncs = vsync;
            if let Some(id) = self.services.vi.vsync_event {
                self.set_event(id, true);
            }
        }
        let due: Vec<usize> = (self.services.vi.slots.iter().enumerate())
            .filter(|(_, s)| {
                matches!(s, Some(Slot { state: SlotState::Queued { release }, .. }) if *release <= now)
            })
            .map(|(i, _)| i)
            .collect();
        for i in due {
            self.free_slot(i);
        }
    }
}

/// The NUL-terminated name in a 0x40-byte DisplayName.
fn display_name(raw: &[u8; 0x40]) -> String {
    let len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..len]).into_owned()
}

/// The native window OpenLayer writes ("VI services", NativeWindow and
/// ParcelData): a parcel whose payload is 2, the process ID, the binder
/// ID, 12 zero bytes, "dispdrv" and 8 zero bytes, and whose objects are
/// one zero u32.
fn native_window() -> Vec<u8> {
    let mut data = [2, PROCESS_ID as u32, BINDER_ID as u32, 0, 0, 0]
        .map(u32::to_le_bytes)
        .concat();
    data.extend(b"dispdrv\0");
    data.extend([0; 8]);
    let size = data.len() as u32;
    let mut p = [size, 0x10, 4, 0x10 + size].map(u32::to_le_bytes).concat();
    p.extend(data);
    p.extend([0; 4]);
    p
}
