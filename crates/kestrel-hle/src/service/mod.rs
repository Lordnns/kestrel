//! Named ports, client sessions and the services behind them.
//!
//! A client connects to a named port (svcConnectToNamedPort) or receives a
//! session handle from another service, then sends requests over the
//! session with svcSendSyncRequest (Switchbrew, "SVC"). The servers are
//! written in Rust and answer at once, so a request never blocks.
//!
//! Each session serves one interface object. A command can hand out more
//! interfaces: as new sessions, or, once the client has converted the
//! session to a domain ("IPC Marshalling", ConvertCurrentObjectToDomain),
//! as numbered objects in that domain, which requests then address.
//!
//! The only named port is "sm:", the service manager, which hands out
//! sessions to the other services by name (Switchbrew, "Services API").
//! A port, service or command kestrel does not implement stops the process
//! with its name and number, never with a made-up answer.

pub mod am;
pub mod fatal;
pub mod fs;
pub mod hid;
pub mod nv;
pub mod psm;
pub mod set;
pub mod sm;
pub mod time;
pub mod vi;

use crate::ipc::{self, command_type, domain_kind, Reply, Request};
use crate::kernel::{Kind, Object, ObjectId};
use crate::svc::result;
use crate::Process;
use std::sync::{Arc, Mutex, MutexGuard};

/// Longest port name read, with its NUL: kestrel's choice, as long as the
/// longest name a service can have (8 bytes) with room to spare.
const PORT_NAME_MAX: u64 = 12;

/// The pointer buffer size every session reports (QueryPointerBufferSize).
/// Kestrel's choice: kestrel copies pointer data straight from the
/// client's memory, so any size works; 0x8000 holds every parcel and ioctl
/// libnx sends, which then travel as pointers.
pub const POINTER_BUFFER_SIZE: u16 = 0x8000;

/// An interface kestrel implements: a service's first object, or one a
/// command hands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interface {
    /// "sm:", the service manager.
    Sm,
    /// "set:sys", system settings.
    SetSys,
    /// "apm" and its IApmSession.
    Apm,
    ApmSession,
    /// "appletOE" and the interfaces an application reaches through it.
    AppletOE,
    ApplicationProxy,
    CommonStateGetter,
    SelfController,
    WindowController,
    AudioController,
    DisplayController,
    LibraryAppletCreator,
    ApplicationFunctions,
    DebugFunctions,
    /// "hid" and its IAppletResource.
    Hid,
    AppletResource,
    /// "time:u" and the clocks and time zone service it hands out.
    TimeU,
    SystemClock,
    SteadyClock,
    TimeZoneService,
    /// "fsp-srv", the SD card's IFileSystem, and the files and directories
    /// opened on it, numbered by fs.
    FileSystemProxy,
    SdCard,
    File(u32),
    Directory(u32),
    /// "psm" and its IPsmSession, power state.
    Psm,
    PsmSession,
    /// "fatal:u", fatal errors.
    FatalU,
    /// "nvdrv", the GPU driver.
    NvDrv,
    /// "vi:u", the display, its IApplicationDisplayService, and the
    /// IHOSBinderDriver relay to a layer's buffer queue.
    ViU,
    ApplicationDisplayService,
    HosBinderDriver,
    /// "test": command 0 echoes its arguments, command 1 fails with
    /// result 0x1234, command 2 hands out another test object. For unit
    /// tests of the session path.
    #[cfg(test)]
    Test,
}

/// The services sm: hands out by name, with the interface each opens.
const SERVICES: &[(&str, Interface)] = &[
    ("set:sys", Interface::SetSys),
    ("apm", Interface::Apm),
    ("appletOE", Interface::AppletOE),
    ("hid", Interface::Hid),
    ("time:u", Interface::TimeU),
    ("fsp-srv", Interface::FileSystemProxy),
    ("psm", Interface::Psm),
    ("fatal:u", Interface::FatalU),
    ("nvdrv", Interface::NvDrv),
    ("vi:u", Interface::ViU),
    #[cfg(test)]
    ("test", Interface::Test),
];

impl Interface {
    /// The service name, or the interface name for the ones a command
    /// hands out (Switchbrew's names, without the C++ namespaces).
    pub fn name(self) -> &'static str {
        use Interface::*;
        match self {
            Sm => "sm:",
            SetSys => "set:sys",
            Apm => "apm",
            ApmSession => "IApmSession",
            AppletOE => "appletOE",
            ApplicationProxy => "IApplicationProxy",
            CommonStateGetter => "ICommonStateGetter",
            SelfController => "ISelfController",
            WindowController => "IWindowController",
            AudioController => "IAudioController",
            DisplayController => "IDisplayController",
            LibraryAppletCreator => "ILibraryAppletCreator",
            ApplicationFunctions => "IApplicationFunctions",
            DebugFunctions => "IDebugFunctions",
            Hid => "hid",
            AppletResource => "IAppletResource",
            TimeU => "time:u",
            SystemClock => "ISystemClock",
            SteadyClock => "ISteadyClock",
            TimeZoneService => "ITimeZoneService",
            FileSystemProxy => "fsp-srv",
            SdCard => "IFileSystem",
            File(_) => "IFile",
            Directory(_) => "IDirectory",
            Psm => "psm",
            PsmSession => "IPsmSession",
            FatalU => "fatal:u",
            NvDrv => "nvdrv",
            ViU => "vi:u",
            ApplicationDisplayService => "IApplicationDisplayService",
            HosBinderDriver => "IHOSBinderDriver",
            #[cfg(test)]
            Test => "test",
        }
    }

    /// The interface also takes TIPC requests. Switchbrew's "Services API":
    /// sm: is a TIPC interface from 12.0.0; libnx still sends it CMIF
    /// requests on every version, so it takes both.
    fn speaks_tipc(self) -> bool {
        self == Interface::Sm
    }

    /// The name of command `n`, when it is known but not implemented.
    fn command_name(self, n: u32) -> Option<&'static str> {
        match (self, n) {
            (Interface::Sm, 2) => Some("RegisterService"),
            (Interface::Sm, 3) => Some("UnregisterService"),
            (Interface::Sm, 4) => Some("DetachClient"),
            (Interface::SetSys, 3) => Some("GetFirmwareVersion, removed on Switch 2"),
            _ => None,
        }
    }
}

/// A domain's objects, object ID n at n - 1. Sessions cloned from a
/// domain session share its domain.
#[derive(Clone, Debug, Default)]
pub struct Domain(Arc<Mutex<Vec<Option<Interface>>>>);

impl Domain {
    fn new(objects: Vec<Option<Interface>>) -> Self {
        Domain(Arc::new(Mutex::new(objects)))
    }

    /// The domain's objects. Only the kernel uses them, under its lock,
    /// so this lock is never contended.
    fn objects(&self) -> MutexGuard<'_, Vec<Option<Interface>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The sessions sharing the domain.
    fn sessions(&self) -> usize {
        Arc::strong_count(&self.0)
    }
}

/// Domains are equal when they hold the same objects, as before they were
/// shared between host threads.
impl PartialEq for Domain {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || *self.objects() == *other.objects()
    }
}

impl Eq for Domain {}

/// A client session: the kernel object a session handle names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// The object requests go to; in a domain, object 1.
    pub object: Interface,
    /// Once the session is a domain: its domain.
    pub domain: Option<Domain>,
    /// The client sent a close request; later requests fail.
    pub closed: bool,
    /// sm: only: RegisterClient came over this session.
    pub registered: bool,
}

impl Session {
    pub fn new(object: Interface) -> Self {
        Session {
            object,
            domain: None,
            closed: false,
            registered: false,
        }
    }
}

/// Why an IPC call gave no ordinary result.
#[derive(Debug, PartialEq, Eq)]
pub enum IpcError {
    /// Return this result code.
    Result(u64),
    /// Something kestrel does not implement or cannot read: stop.
    Stop(String),
    /// The program threw a fatal error with an error screen: it ends.
    Fatal(u32),
    /// Something kestrel does not implement, named with its identifiers:
    /// stop, and the frontend logs it.
    Unimplemented(String),
}

impl From<u64> for IpcError {
    fn from(code: u64) -> Self {
        IpcError::Result(code)
    }
}

/// What a command answers.
#[derive(Debug, Default)]
pub struct Out {
    pub result: u32,
    /// The output arguments.
    pub data: Vec<u8>,
    /// Interfaces handed out: new sessions, or new objects in the caller's
    /// domain.
    pub objects: Vec<Interface>,
    pub copy_handles: Vec<u32>,
}

impl Out {
    pub fn ok(data: Vec<u8>) -> Self {
        Out {
            data,
            ..Out::default()
        }
    }

    pub fn empty() -> Self {
        Out::default()
    }

    /// Failure: only the result is sent.
    pub fn error(result: u32) -> Self {
        Out {
            result,
            ..Out::default()
        }
    }

    pub fn object(interface: Interface) -> Self {
        Out {
            objects: vec![interface],
            ..Out::default()
        }
    }

    pub fn copy(handle: u32) -> Self {
        Out {
            copy_handles: vec![handle],
            ..Out::default()
        }
    }
}

/// A command being answered.
pub struct Call<'a> {
    /// The session it came over.
    pub session: ObjectId,
    pub object: Interface,
    pub n: u32,
    pub req: &'a Request,
    /// The input arguments.
    pub args: &'a [u8],
}

impl Call<'_> {
    /// `N` bytes of arguments at `at`; too few arguments stop.
    pub fn arg<const N: usize>(&self, at: usize) -> Result<[u8; N], IpcError> {
        self.args
            .get(at..at + N)
            .map(|b| b.try_into().unwrap())
            .ok_or_else(|| {
                IpcError::Stop(format!(
                    "{} command {} with {} bytes of arguments, fewer than the {} it takes",
                    self.object.name(),
                    self.n,
                    self.args.len(),
                    at + N
                ))
            })
    }

    pub fn u8(&self, at: usize) -> Result<u8, IpcError> {
        Ok(self.arg::<1>(at)?[0])
    }

    pub fn u64(&self, at: usize) -> Result<u64, IpcError> {
        Ok(u64::from_le_bytes(self.arg(at)?))
    }

    /// Stop: kestrel does not implement this command.
    pub fn unimplemented(&self) -> IpcError {
        let name = self
            .object
            .command_name(self.n)
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        IpcError::Unimplemented(format!("{} command {}{name}", self.object.name(), self.n))
    }
}

/// The names of the CMIF control commands ("IPC Marshalling").
fn control_name(n: u32) -> &'static str {
    match n {
        0 => "ConvertCurrentObjectToDomain",
        1 => "CopyFromCurrentDomain",
        2 => "CloneCurrentObject",
        3 => "QueryPointerBufferSize",
        4 => "CloneCurrentObjectEx",
        _ => "unknown",
    }
}

/// A service name from its 8-byte encoding: the characters, zero-padded.
/// None for an empty name or one with characters after the padding
/// (InvalidServiceName, kestrel's choice of when it applies).
fn service_name(raw: &[u8]) -> Option<String> {
    let len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    if len == 0 || raw[len..].iter().any(|&b| b != 0) {
        return None;
    }
    Some(raw[..len].escape_ascii().to_string())
}

/// Kernel objects the services keep for the process.
#[derive(Debug, Default)]
pub struct State {
    /// am: the event signalled while an applet message waits.
    pub applet_message_event: Option<ObjectId>,
    /// am: the event signalled when the suspended tick count changes.
    pub suspended_tick_event: Option<ObjectId>,
    pub hid: hid::State,
    /// time: the shared memory the clocks are read from.
    pub time_shared_memory: Option<ObjectId>,
    pub fs: fs::State,
    /// psm: the IPsmSession state-change event.
    pub psm_state_event: Option<ObjectId>,
    pub nv: nv::State,
    pub vi: vi::State,
}

impl Process {
    /// svcConnectToNamedPort: a new session to the port named by the
    /// string at `name`. An unreadable name is InvalidPointer and a name
    /// without a NUL in its first 12 bytes OutOfRange (kestrel's choices).
    pub fn connect_to_named_port(&mut self, name: u64) -> Result<u32, IpcError> {
        let mut bytes = Vec::new();
        for i in 0..PORT_NAME_MAX {
            match self.mem.read_vec(name.wrapping_add(i), 1) {
                Ok(b) if b[0] == 0 => break,
                Ok(b) => bytes.push(b[0]),
                Err(_) => return Err(result::INVALID_POINTER.into()),
            }
            if i == PORT_NAME_MAX - 1 {
                return Err(result::OUT_OF_RANGE.into());
            }
        }
        if bytes != b"sm:" {
            return Err(IpcError::Unimplemented(format!(
                "named port \"{}\" (kestrel has only \"sm:\")",
                bytes.escape_ascii()
            )));
        }
        Ok(self.open_session(Interface::Sm)?)
    }

    /// A new session serving `object`, as a handle for the client.
    fn open_session(&mut self, object: Interface) -> Result<u32, u64> {
        let session = Object::Session(Session::new(object));
        Ok(self.handles.insert(session, &[Kind::ClientSession])?[0])
    }

    fn session(&mut self, id: ObjectId) -> &mut Session {
        match self.handles.object_mut(id) {
            Object::Session(s) => s,
            o => unreachable!("a session handle names {o:?}"),
        }
    }

    /// A new event the services keep a reference to, with no handles yet.
    pub(crate) fn service_event(&mut self) -> ObjectId {
        self.handles
            .insert_unnamed(Object::Event { signaled: false })
    }

    /// svcSendSyncRequest: send the message in the current thread's local
    /// region over session `h` and write the reply in its place. A closed
    /// session gives SessionClosed (0xf601).
    pub fn send_sync_request(&mut self, h: u64) -> Result<(), IpcError> {
        let id = match self.handles.get(h as u32) {
            Some((Kind::ClientSession, id)) => id,
            _ => return Err(result::INVALID_HANDLE.into()),
        };
        let session = self.session(id).clone();
        if session.closed {
            return Err(result::SESSION_CLOSED.into());
        }
        let tlr = self.current_thread().tlr;
        let msg = self
            .mem
            .read_vec(tlr, ipc::MESSAGE_SIZE)
            .map_err(|e| IpcError::Stop(format!("cannot read the IPC message: {e}")))?;
        let name = session.object.name();
        let stop = |what: String| IpcError::Stop(format!("{what}, sent to {name}"));
        let req = ipc::parse(&msg).map_err(stop)?;
        if !req.move_handles.is_empty() {
            return Err(stop("unsupported: moved handles in an IPC request".into()));
        }
        let reply = match req.command_type {
            command_type::CLOSE => {
                self.session(id).closed = true;
                return Ok(());
            }
            command_type::REQUEST | command_type::REQUEST_WITH_CONTEXT
                if session.domain.is_some() =>
            {
                self.domain_request(id, &req).map_err(|e| match e {
                    IpcError::Stop(what) => stop(what),
                    e => e,
                })?
            }
            command_type::REQUEST | command_type::REQUEST_WITH_CONTEXT => {
                let cmif = ipc::cmif_request(&req).map_err(stop)?;
                let call = Call {
                    session: id,
                    object: session.object,
                    n: cmif.command,
                    req: &req,
                    args: &cmif.args,
                };
                let (reply, _) = self.answer(&call, false)?;
                ipc::cmif_reply(&reply, None).map_err(stop)?
            }
            command_type::CONTROL | command_type::CONTROL_WITH_CONTEXT => {
                let n = ipc::cmif_request(&req).map_err(stop)?.command;
                if n == 3 {
                    // QueryPointerBufferSize: a u16. libnx asks on every new
                    // session and sends auto-select buffers no larger than
                    // this as pointers.
                    let size = POINTER_BUFFER_SIZE.to_le_bytes().to_vec();
                    ipc::cmif_reply(&Reply::ok(size), None).map_err(stop)?
                } else if n == 2 || n == 4 {
                    // CloneCurrentObject, and CloneCurrentObjectEx, which
                    // also takes a u32 tag that kestrel does not use: a new
                    // session to the same object, moved to the client.
                    // libnx's session manager (`sf/sessionmgr.c`) clones a
                    // domain session and sends requests for the domain's
                    // objects over the clones, so a clone shares the domain.
                    let clone = Session {
                        object: session.object,
                        domain: session.domain.clone(),
                        closed: false,
                        registered: false,
                    };
                    let h = self
                        .handles
                        .insert(Object::Session(clone), &[Kind::ClientSession])?[0];
                    let reply = Reply {
                        move_handles: vec![h],
                        ..Reply::ok(Vec::new())
                    };
                    ipc::cmif_reply(&reply, None).map_err(stop)?
                } else if n != 0 {
                    return Err(IpcError::Unimplemented(format!(
                        "CMIF control command {n} ({}), sent to {name}",
                        control_name(n)
                    )));
                } else if session.domain.is_some() {
                    return Err(stop(
                        "unsupported: ConvertCurrentObjectToDomain on a domain".into(),
                    ));
                } else {
                    // ConvertCurrentObjectToDomain: the session's object
                    // becomes object 1, kestrel's choice of ID.
                    let domain = Domain::new(vec![Some(session.object)]);
                    self.session(id).domain = Some(domain);
                    ipc::cmif_reply(&Reply::ok(1u32.to_le_bytes().to_vec()), None).map_err(stop)?
                }
            }
            t if t >= command_type::TIPC && session.object.speaks_tipc() => {
                let call = Call {
                    session: id,
                    object: session.object,
                    n: (t - command_type::TIPC) as u32,
                    req: &req,
                    args: &req.raw,
                };
                let (reply, _) = self.answer(&call, false)?;
                ipc::tipc_reply(&reply).map_err(stop)?
            }
            t => {
                let kind = match t {
                    command_type::INVALID => "Invalid",
                    command_type::LEGACY_REQUEST => "LegacyRequest",
                    command_type::LEGACY_CONTROL => "LegacyControl",
                    _ => "TIPC",
                };
                return Err(stop(format!("unsupported: IPC command type {t} ({kind})")));
            }
        };
        self.mem
            .write_bytes(tlr, &reply)
            .map_err(|e| IpcError::Stop(format!("cannot write the IPC reply: {e}")))
    }

    /// Answer a request to an object in session `id`'s domain.
    fn domain_request(&mut self, id: ObjectId, req: &Request) -> Result<Vec<u8>, IpcError> {
        let d = ipc::domain_request(req).map_err(IpcError::Stop)?;
        let slot = d.object_id.checked_sub(1).map(|i| i as usize);
        let domain = self.session(id).domain.clone().expect("a domain");
        let mut domain = domain.objects();
        let Some(object) = slot.and_then(|i| domain.get(i).copied().flatten()) else {
            return Err(IpcError::Stop(format!(
                "a request to domain object {}, which does not exist",
                d.object_id
            )));
        };
        if d.kind == domain_kind::CLOSE {
            domain[slot.unwrap()] = None;
            drop(domain);
            self.interface_released(object);
            return ipc::cmif_reply(&Reply::ok(Vec::new()), Some(&[])).map_err(IpcError::Stop);
        }
        if !d.in_objects.is_empty() {
            return Err(IpcError::Stop(format!(
                "unsupported: domain objects as arguments to {}",
                object.name()
            )));
        }
        drop(domain);
        let cmif = d.cmif.expect("SendMessage has a command");
        let call = Call {
            session: id,
            object,
            n: cmif.command,
            req,
            args: &cmif.args,
        };
        let (reply, ids) = self.answer(&call, true)?;
        ipc::cmif_reply(&reply, Some(&ids)).map_err(IpcError::Stop)
    }

    /// Run `call` and turn its interfaces into new sessions (moved handles)
    /// or, for a domain, new objects in it (their IDs).
    fn answer(&mut self, call: &Call, in_domain: bool) -> Result<(Reply, Vec<u32>), IpcError> {
        let out = self.serve(call)?;
        let mut reply = Reply {
            result: out.result,
            copy_handles: out.copy_handles,
            move_handles: Vec::new(),
            data: out.data,
        };
        let mut ids = Vec::new();
        for object in out.objects {
            if in_domain {
                let domain = self.session(call.session).domain.clone().expect("a domain");
                let mut domain = domain.objects();
                let free = domain.iter().position(Option::is_none);
                let i = free.unwrap_or_else(|| {
                    domain.push(None);
                    domain.len() - 1
                });
                domain[i] = Some(object);
                ids.push(i as u32 + 1);
            } else {
                reply.move_handles.push(self.open_session(object)?);
            }
        }
        Ok((reply, ids))
    }

    /// Answer `call` with the code of the interface it is for.
    fn serve(&mut self, call: &Call) -> Result<Out, IpcError> {
        use Interface::*;
        match call.object {
            Sm => self.sm(call),
            SetSys => self.set_sys(call),
            Apm | ApmSession => self.apm(call),
            AppletOE | ApplicationProxy | CommonStateGetter | SelfController | WindowController
            | AudioController | DisplayController | LibraryAppletCreator | ApplicationFunctions
            | DebugFunctions => self.am(call),
            Hid | AppletResource => self.hid(call),
            TimeU | SystemClock | SteadyClock | TimeZoneService => self.time(call),
            FileSystemProxy | SdCard | File(_) | Directory(_) => self.fs(call),
            Psm | PsmSession => self.psm(call),
            FatalU => self.fatal(call),
            NvDrv => self.nvdrv(call),
            ViU | ApplicationDisplayService | HosBinderDriver => self.vi(call),
            #[cfg(test)]
            Test => match call.n {
                0 => Ok(Out::ok(call.args.to_vec())),
                1 => Ok(Out::error(0x1234)),
                2 => Ok(Out::object(Test)),
                _ => Err(call.unimplemented()),
            },
        }
    }

    /// The bytes the client sends through pointer descriptor `index` (an
    /// X buffer), as the kernel copies them to the server.
    fn read_pointer(&self, req: &Request, index: u8) -> Result<Vec<u8>, IpcError> {
        let Some(x) = req.pointers.iter().find(|x| x.index == index) else {
            return Err(IpcError::Stop(format!(
                "a request without pointer buffer {index} for its input"
            )));
        };
        self.mem.read_vec(x.addr, x.size as usize).map_err(|e| {
            IpcError::Stop(format!(
                "cannot read pointer buffer {index} at {:#x}: {e}",
                x.addr
            ))
        })
    }

    /// An interface is gone: a domain object closed, or the last session
    /// serving it.
    fn interface_released(&mut self, object: Interface) {
        self.fs_released(object);
    }

    /// The kernel freed a session: its object goes, or, for the last
    /// session of a domain, every object in the domain.
    pub(crate) fn session_released(&mut self, session: Session) {
        match session.domain {
            Some(d) if d.sessions() == 1 => {
                let all = std::mem::take(&mut *d.objects());
                for object in all.into_iter().flatten() {
                    self.interface_released(object);
                }
            }
            Some(_) => {}
            None => self.interface_released(session.object),
        }
    }

    /// `size` bytes from the client's send buffer (A) `i`. A missing or
    /// smaller buffer stops.
    fn read_send(&self, req: &Request, i: usize, size: u64) -> Result<Vec<u8>, IpcError> {
        let Some(a) = req.sends.get(i).filter(|a| a.size >= size) else {
            return Err(IpcError::Stop(format!(
                "a request without a send buffer {i} of {size:#x} bytes"
            )));
        };
        self.mem.read_vec(a.addr, size as usize).map_err(|e| {
            IpcError::Stop(format!("cannot read send buffer {i} at {:#x}: {e}", a.addr))
        })
    }

    /// The bytes of auto-select input buffer `i`. libnx's
    /// cmifRequestInAutoBuffer sends it as pointer `i` (X) and send buffer
    /// `i` (A), with the one not used empty.
    fn read_auto(&self, req: &Request, i: usize) -> Result<Vec<u8>, IpcError> {
        let pointer = req.pointers.iter().find(|x| x.index as usize == i);
        match (pointer, req.sends.get(i)) {
            (Some(x), _) if x.size > 0 => self.read_pointer(req, i as u8),
            (_, Some(a)) => self.read_send(req, i, a.size),
            (Some(_), None) => Ok(Vec::new()),
            (None, None) => Err(IpcError::Stop(format!(
                "a request without auto-select input buffer {i}"
            ))),
        }
    }

    /// Room in auto-select output buffer `i`: receive list entry `i` (C)
    /// or receive buffer `i` (B), the one libnx's cmifRequestOutAutoBuffer
    /// did not leave empty.
    fn auto_room(req: &Request, i: usize) -> Result<u64, IpcError> {
        match (req.receive_list.get(i), req.receives.get(i)) {
            (Some(c), _) if c.size > 0 => Ok(c.size),
            (_, Some(b)) => Ok(b.size),
            (Some(_), None) => Ok(0),
            (None, None) => Err(IpcError::Stop(format!(
                "a request without auto-select output buffer {i}"
            ))),
        }
    }

    /// Write `data` into auto-select output buffer `i`, which must hold
    /// it.
    fn write_auto(&mut self, req: &Request, i: usize, data: &[u8]) -> Result<(), IpcError> {
        match req.receive_list.get(i) {
            Some(c) if c.size > 0 => self.write_receive(req, i, data),
            _ => self.write_buffer(req, i, data.len() as u64, data),
        }
    }

    /// Write `data` at the start of the client's receive buffer (B) `i`,
    /// which must hold `size` bytes. A missing or smaller buffer stops.
    fn write_buffer(
        &mut self,
        req: &Request,
        i: usize,
        size: u64,
        data: &[u8],
    ) -> Result<(), IpcError> {
        let Some(b) = req.receives.get(i).filter(|b| b.size >= size) else {
            return Err(IpcError::Stop(format!(
                "a request without a receive buffer {i} of {size:#x} bytes"
            )));
        };
        self.mem.write_bytes(b.addr, data).map_err(|e| {
            IpcError::Stop(format!(
                "cannot write receive buffer {i} at {:#x}: {e}",
                b.addr
            ))
        })
    }

    /// Write `data` through the client's receive list entry `i` (a C
    /// buffer), as the kernel copies a reply's pointer data back. A
    /// missing or short entry stops: the client did not ask as the
    /// command's documentation says.
    fn write_receive(&mut self, req: &Request, i: usize, data: &[u8]) -> Result<(), IpcError> {
        let Some(r) = req.receive_list.get(i) else {
            return Err(IpcError::Stop(format!(
                "a request without receive buffer {i} for its output"
            )));
        };
        if r.size < data.len() as u64 {
            return Err(IpcError::Stop(format!(
                "receive buffer {i} holds {:#x} bytes; the output needs {:#x}",
                r.size,
                data.len()
            )));
        }
        self.mem.write_bytes(r.addr, data).map_err(|e| {
            IpcError::Stop(format!(
                "cannot write receive buffer {i} at {:#x}: {e}",
                r.addr
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{CMIF_IN_MAGIC, CMIF_OUT_MAGIC};
    use crate::kernel::PROCESS_ID;
    use crate::svc::handle;
    use crate::svc::{dispatch, nr};
    use crate::testutil::{process, svc};
    use crate::Stop;
    use kestrel_core::Perm;

    const STRINGS: u64 = 0x10_0000;

    fn words(ws: &[u32]) -> Vec<u8> {
        ws.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// A request as libnx's cmifMakeRequest builds it: no handles or
    /// buffers, an optional process ID, `args` after the "SFCI" header.
    fn cmif(kind: u16, pid: bool, cmd: u32, args: &[u8]) -> Vec<u8> {
        let size = (16 + 16 + args.len() + 1) & !1;
        let mut m = words(&[kind as u32, (size.div_ceil(4) as u32) | (pid as u32) << 31]);
        if pid {
            m.extend(words(&[1, 0, 0]));
        }
        m.resize(m.len().next_multiple_of(16), 0);
        m.extend(words(&[CMIF_IN_MAGIC, 0, cmd, 0]));
        m.extend(args);
        m
    }

    /// A TIPC request as libnx's tipcMakeRequest builds it.
    fn tipc(cmd: u32, pid: bool, args: &[u8]) -> Vec<u8> {
        let mut m = words(&[16 + cmd, args.len().div_ceil(4) as u32 | (pid as u32) << 31]);
        if pid {
            m.extend(words(&[1, 0, 0]));
        }
        m.extend(args);
        m
    }

    /// Send `msg` over `h`: the call's result, or the stop message.
    fn send(p: &mut Process, h: u64, msg: &[u8]) -> Result<u64, String> {
        let tlr = p.current_thread().tlr;
        p.mem.write_bytes(tlr, &[0xee; ipc::MESSAGE_SIZE]).unwrap();
        p.mem.write_bytes(tlr, msg).unwrap();
        p.cpu.x[0] = h;
        match dispatch(p, nr::SEND_SYNC_REQUEST) {
            None => Ok(p.cpu.x[0]),
            Some(Stop::Error(e)) => Err(e),
            Some(Stop::Unimplemented { what, .. }) => Err(format!("unimplemented: {what}")),
            Some(other) => panic!("{other:?}"),
        }
    }

    /// The reply in the thread local region as (result, moved handles,
    /// data after the result), for CMIF or TIPC.
    fn reply(p: &Process, is_cmif: bool) -> (u32, Vec<u32>, Vec<u8>) {
        let m = p
            .mem
            .read_vec(p.current_thread().tlr, ipc::MESSAGE_SIZE)
            .unwrap();
        let w = |at: usize| u32::from_le_bytes(m[at..at + 4].try_into().unwrap());
        let raw_words = (w(4) & 0x3ff) as usize;
        let mut at = 8;
        let mut moved = Vec::new();
        if w(4) >> 31 != 0 {
            let special = w(8);
            assert_eq!(special & 0x1f, 0, "no process ID or copied handles");
            at = 12;
            for _ in 0..special >> 5 & 0xf {
                moved.push(w(at));
                at += 4;
            }
        }
        let end = at + raw_words * 4;
        if is_cmif {
            let start = at.next_multiple_of(16);
            assert_eq!(w(start), CMIF_OUT_MAGIC);
            // The 16 bytes of padding not used before "SFCO" end the data.
            let data_end = end - (16 - (start - at));
            (w(start + 8), moved, m[start + 16..data_end].to_vec())
        } else {
            (w(at), moved, m[at + 4..end].to_vec())
        }
    }

    fn connect(p: &mut Process, name: &[u8]) -> Result<(u64, u64), String> {
        p.mem.write_bytes(STRINGS, name).unwrap();
        p.cpu.x[1] = STRINGS;
        match dispatch(p, nr::CONNECT_TO_NAMED_PORT) {
            None => Ok((p.cpu.x[0], p.cpu.x[1])),
            Some(Stop::Error(e)) => Err(e),
            Some(Stop::Unimplemented { what, .. }) => Err(format!("unimplemented: {what}")),
            Some(other) => panic!("{other:?}"),
        }
    }

    fn setup() -> Process {
        let p = process();
        p.mem.map(STRINGS, 0x1000, Perm::RW, "strings").unwrap();
        p
    }

    /// A session to sm: that has registered.
    fn sm(p: &mut Process) -> u64 {
        let (rc, h) = connect(p, b"sm:\0").unwrap();
        assert_eq!(rc, 0);
        assert_eq!(send(p, h, &cmif(4, true, 0, &[0; 8])), Ok(0));
        assert_eq!(reply(p, true), (0, vec![], vec![]));
        h
    }

    fn name(s: &[u8]) -> Vec<u8> {
        let mut n = s.to_vec();
        n.resize(8, 0);
        n
    }

    #[test]
    fn named_ports_need_a_readable_name_with_a_nul_in_12_bytes() {
        let mut p = setup();
        p.cpu.x[1] = 0x20_0000;
        assert_eq!(dispatch(&mut p, nr::CONNECT_TO_NAMED_PORT), None);
        assert_eq!(p.cpu.x[0], result::INVALID_POINTER);
        let long = connect(&mut p, b"abcdefghijkl").unwrap();
        assert_eq!(long.0, result::OUT_OF_RANGE);
        // 11 characters and the NUL are read; the name is then unknown.
        let e = connect(&mut p, b"abcdefghijk\0").unwrap_err();
        assert!(e.contains("named port \"abcdefghijk\""), "{e}");
        let (rc, h) = connect(&mut p, b"sm:\0").unwrap();
        assert_eq!(rc, 0);
        assert_eq!(h >> 30, 0);
        // A name ending at the end of a mapping is read up to its NUL.
        p.mem.write_bytes(STRINGS + 0xffc, b"sm:\0").unwrap();
        p.cpu.x[1] = STRINGS + 0xffc;
        assert_eq!(dispatch(&mut p, nr::CONNECT_TO_NAMED_PORT), None);
        assert_eq!(p.cpu.x[0], 0);
    }

    #[test]
    fn sm_hands_out_sessions_by_name_after_register_client() {
        let mut p = setup();
        let (_, h) = connect(&mut p, b"sm:\0").unwrap();
        let get = |n: &[u8]| cmif(4, false, 1, &name(n));
        assert_eq!(send(&mut p, h, &get(b"test")), Ok(0));
        assert_eq!(reply(&p, true).0, sm::result::INVALID_CLIENT);
        let e = send(&mut p, h, &cmif(4, false, 0, &[0; 8])).unwrap_err();
        assert!(e.contains("process ID"), "{e}");
        let h = sm(&mut p);
        for bad in [&b""[..], b"a\0b"] {
            assert_eq!(send(&mut p, h, &get(bad)), Ok(0));
            assert_eq!(reply(&p, true).0, sm::result::INVALID_SERVICE_NAME);
        }
        let e = send(&mut p, h, &get(b"nosuch")).unwrap_err();
        assert!(e.starts_with("unimplemented: service \"nosuch\""), "{e}");
        // A session to the test service, used through its handle.
        assert_eq!(send(&mut p, h, &get(b"test")), Ok(0));
        let (rc, moved, _) = reply(&p, true);
        assert_eq!((rc, moved.len()), (0, 1));
        let t = moved[0] as u64;
        assert_eq!(send(&mut p, t, &cmif(4, false, 0, &[1, 2, 3, 4])), Ok(0));
        let (rc, _, data) = reply(&p, true);
        assert_eq!((rc, &data[..4]), (0, &[1, 2, 3, 4][..]));
        // RequestWithContext is answered the same way; a failure sends
        // only its result.
        assert_eq!(send(&mut p, t, &cmif(6, false, 1, &[])), Ok(0));
        assert_eq!(reply(&p, true), (0x1234, vec![], vec![]));
        let e = send(&mut p, t, &cmif(4, false, 7, &[])).unwrap_err();
        assert!(e.starts_with("unimplemented: test command 7"), "{e}");
        let e = send(&mut p, t, &tipc(0, false, &[])).unwrap_err();
        assert!(e.contains("command type 16 (TIPC), sent to test"), "{e}");
    }

    #[test]
    fn sm_takes_tipc_requests_too() {
        let mut p = setup();
        let (_, h) = connect(&mut p, b"sm:\0").unwrap();
        assert_eq!(send(&mut p, h, &tipc(1, false, &name(b"test"))), Ok(0));
        assert_eq!(reply(&p, false).0, sm::result::INVALID_CLIENT);
        assert_eq!(send(&mut p, h, &tipc(0, true, &[])), Ok(0));
        assert_eq!(reply(&p, false), (0, vec![], vec![]));
        assert_eq!(send(&mut p, h, &tipc(1, false, &name(b"test"))), Ok(0));
        let (rc, moved, _) = reply(&p, false);
        assert_eq!((rc, moved.len()), (0, 1));
        let e = send(&mut p, h, &tipc(2, false, &[])).unwrap_err();
        assert!(
            e.starts_with("unimplemented: sm: command 2 (RegisterService)"),
            "{e}"
        );
    }

    #[test]
    fn a_closed_session_refuses_requests_until_its_handle_closes() {
        let mut p = setup();
        let h = sm(&mut p);
        let close = words(&[2, 0]);
        assert_eq!(send(&mut p, h, &close), Ok(0));
        let register = cmif(4, true, 0, &[0; 8]);
        assert_eq!(send(&mut p, h, &register), Ok(result::SESSION_CLOSED));
        assert_eq!(send(&mut p, h, &close), Ok(result::SESSION_CLOSED));
        p.cpu.x[0] = h;
        assert_eq!(dispatch(&mut p, nr::CLOSE_HANDLE), None);
        assert_eq!(p.cpu.x[0], 0);
        assert_eq!(send(&mut p, h, &close), Ok(result::INVALID_HANDLE));
    }

    /// Send `msg` over `h` and return the stop it causes.
    fn stop_from(p: &mut Process, h: u64, msg: &[u8]) -> Option<Stop> {
        let tlr = p.current_thread().tlr;
        p.mem.write_bytes(tlr, msg).unwrap();
        p.cpu.x[0] = h;
        dispatch(p, nr::SEND_SYNC_REQUEST)
    }

    #[test]
    fn what_kestrel_lacks_stops_as_unimplemented_with_its_identifiers() {
        let mut p = setup();
        let pc = p.cpu.pc.wrapping_sub(4);
        let unimplemented = |what: &str| {
            Some(Stop::Unimplemented {
                what: what.into(),
                pc,
            })
        };
        let h = sm(&mut p);
        let get = cmif(4, false, 1, &name(b"nosuch"));
        assert_eq!(
            stop_from(&mut p, h, &get),
            unimplemented("service \"nosuch\"")
        );
        let t = open(&mut p, b"test");
        assert_eq!(
            stop_from(&mut p, t, &cmif(4, false, 7, &[])),
            unimplemented("test command 7")
        );
        assert_eq!(
            stop_from(&mut p, t, &cmif(5, false, 1, &[0; 4])),
            unimplemented("CMIF control command 1 (CopyFromCurrentDomain), sent to test")
        );
        // Inside a domain too.
        assert_eq!(convert(&mut p, t), Ok(1));
        assert_eq!(
            stop_from(&mut p, t, &request(false, &[], Some(1), 7, &[])),
            unimplemented("test command 7")
        );
        p.mem.write_bytes(STRINGS, b"nope\0").unwrap();
        p.cpu.x[1] = STRINGS;
        assert_eq!(
            dispatch(&mut p, nr::CONNECT_TO_NAMED_PORT),
            unimplemented("named port \"nope\" (kestrel has only \"sm:\")")
        );
    }

    #[test]
    fn requests_kestrel_cannot_answer_stop_with_a_reason() {
        let mut p = setup();
        let h = sm(&mut p);
        let e = send(&mut p, h, &cmif(5, false, 1, &[0; 4])).unwrap_err();
        assert!(
            e.contains("control command 1 (CopyFromCurrentDomain)"),
            "{e}"
        );
        let e = send(&mut p, h, &cmif(1, false, 0, &[])).unwrap_err();
        assert!(
            e.contains("command type 1 (LegacyRequest), sent to sm:"),
            "{e}"
        );
        // One moved handle before the raw data.
        let mut with_handle = words(&[4, 8 | 1 << 31, 1 << 5, 0x1234]);
        with_handle.extend(&cmif(4, false, 0, &[])[8..]);
        let e = send(&mut p, h, &with_handle).unwrap_err();
        assert!(e.contains("moved handles in an IPC request"), "{e}");
        let e = send(&mut p, h, &words(&[4, 0x3ff])).unwrap_err();
        assert!(e.contains("longer than"), "{e}");
        let pc = p.cpu.pc.wrapping_sub(4);
        assert!(e.ends_with(&format!("at pc {pc:#x}")), "{e}");
    }

    /// Add one receive list entry (a C buffer) after `msg`'s raw data, as
    /// cmifRequestOutFixedPointer does. `msg` has no special header.
    fn with_receive(mut msg: Vec<u8>, addr: u64, size: u64) -> Vec<u8> {
        let w1 = u32::from_le_bytes(msg[4..8].try_into().unwrap());
        msg[4..8].copy_from_slice(&(w1 | 3 << 10).to_le_bytes());
        msg.resize(8 + (w1 & 0x3ff) as usize * 4, 0);
        msg.extend(words(&[
            addr as u32,
            (addr >> 32) as u32 | (size as u32) << 16,
        ]));
        msg
    }

    /// A session to `service`, through a registered sm: session.
    fn open(p: &mut Process, service: &[u8]) -> u64 {
        let h = sm(p);
        assert_eq!(send(p, h, &cmif(4, false, 1, &name(service))), Ok(0));
        let (rc, moved, _) = reply(p, true);
        assert_eq!((rc, moved.len()), (0, 1));
        moved[0] as u64
    }

    #[test]
    fn set_sys_writes_the_firmware_version_through_a_c_buffer() {
        let mut p = setup();
        let h = open(&mut p, b"set:sys");
        let out = STRINGS + 0x100;
        let get = |size| with_receive(cmif(4, false, 4, &[]), out, size);
        assert_eq!(send(&mut p, h, &get(0x100)), Ok(0));
        assert_eq!(reply(&p, true).0, 0);
        let v = p.mem.read_vec(out, 0x100).unwrap();
        assert_eq!(&v[..8], &[20, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&v[0x08..0x10], b"kestrel\0");
        assert_eq!(&v[0x68..0x6f], b"20.0.0\0");
        assert!(v[0x28..0x68].iter().all(|&b| b == 0));
        let e = send(&mut p, h, &get(0x80)).unwrap_err();
        assert!(
            e.contains("holds 0x80 bytes; the output needs 0x100"),
            "{e}"
        );
        let e = send(&mut p, h, &cmif(4, false, 4, &[])).unwrap_err();
        assert!(e.contains("without receive buffer 0"), "{e}");
        let old = with_receive(cmif(4, false, 3, &[]), out, 0x100);
        let e = send(&mut p, h, &old).unwrap_err();
        assert!(
            e.contains("set:sys command 3 (GetFirmwareVersion, removed on Switch 2)"),
            "{e}"
        );
    }

    /// A request as cmifMakeRequest builds it, with a process ID, copied
    /// handles and, for a domain, the domain header naming `object`.
    fn request(pid: bool, copy: &[u32], object: Option<u32>, cmd: u32, args: &[u8]) -> Vec<u8> {
        let domain = if object.is_some() { 16 } else { 0 };
        let size = (16 + domain + 16 + args.len() + 1) & !1;
        let special = pid || !copy.is_empty();
        let mut m = words(&[4, size.div_ceil(4) as u32 | (special as u32) << 31]);
        if special {
            m.extend(words(&[pid as u32 | (copy.len() as u32) << 1]));
            if pid {
                m.extend(words(&[0, 0]));
            }
            m.extend(words(copy));
        }
        m.resize(m.len().next_multiple_of(16), 0);
        if let Some(id) = object {
            m.extend(words(&[1 | ((16 + args.len()) as u32) << 16, id, 0, 0]));
        }
        m.extend(words(&[CMIF_IN_MAGIC, 0, cmd, 0]));
        m.extend(args);
        m
    }

    /// A reply as libnx's cmifParseResponse reads it, expecting `size`
    /// bytes of data: (result, copied handles, moved handles, data, domain
    /// object IDs).
    fn answer(
        p: &Process,
        domain: bool,
        size: usize,
    ) -> (u32, Vec<u32>, Vec<u32>, Vec<u8>, Vec<u32>) {
        let m = p
            .mem
            .read_vec(p.current_thread().tlr, ipc::MESSAGE_SIZE)
            .unwrap();
        let w = |at: usize| u32::from_le_bytes(m[at..at + 4].try_into().unwrap());
        let (mut copied, mut moved) = (Vec::new(), Vec::new());
        let mut at = 8;
        if w(4) >> 31 != 0 {
            let special = w(8);
            at = 12;
            for i in 0..(special >> 1 & 0xf) + (special >> 5 & 0xf) {
                let list = if i < (special >> 1 & 0xf) {
                    &mut copied
                } else {
                    &mut moved
                };
                list.push(w(at));
                at += 4;
            }
        }
        let mut start = at.next_multiple_of(16);
        let n_objects = if domain {
            start += 16;
            w(start - 16) as usize
        } else {
            0
        };
        assert_eq!(w(start), CMIF_OUT_MAGIC);
        let data = m[start + 16..start + 16 + size].to_vec();
        let objects = (0..n_objects)
            .map(|i| w(start + 16 + size + 4 * i))
            .collect();
        (w(start + 8), copied, moved, data, objects)
    }

    /// Convert session `h` to a domain; returns the session's object ID.
    fn convert(p: &mut Process, h: u64) -> Result<u32, String> {
        send(p, h, &cmif(5, false, 0, &[]))?;
        let (rc, _, _, data, _) = answer(p, false, 4);
        assert_eq!(rc, 0);
        Ok(u32::from_le_bytes(data.try_into().unwrap()))
    }

    #[test]
    fn a_domain_hands_out_objects_by_id() {
        let mut p = setup();
        let h = open(&mut p, b"test");
        assert_eq!(convert(&mut p, h), Ok(1));
        // The session's own object is object 1; a new one gets ID 2.
        assert_eq!(
            send(&mut p, h, &request(false, &[], Some(1), 2, &[])),
            Ok(0)
        );
        let (rc, copied, moved, _, objects) = answer(&p, true, 0);
        assert_eq!((rc, copied, moved, objects), (0, vec![], vec![], vec![2]));
        assert_eq!(
            send(&mut p, h, &request(false, &[], Some(2), 0, &[7; 4])),
            Ok(0)
        );
        assert_eq!(answer(&p, true, 4).3, vec![7; 4]);
        // Closing object 2 frees its ID for the next object.
        let close = words(&[4, 8, 0, 0, 2, 2, 0, 0]);
        assert_eq!(send(&mut p, h, &close), Ok(0));
        let e = send(&mut p, h, &request(false, &[], Some(2), 0, &[])).unwrap_err();
        assert!(
            e.contains("domain object 2, which does not exist, sent to test"),
            "{e}"
        );
        assert_eq!(
            send(&mut p, h, &request(false, &[], Some(1), 2, &[])),
            Ok(0)
        );
        assert_eq!(answer(&p, true, 0).4, vec![2]);
        let e = convert(&mut p, h).unwrap_err();
        assert!(
            e.contains("ConvertCurrentObjectToDomain on a domain, sent to test"),
            "{e}"
        );
        // Outside a domain, a new object is a new session.
        let t = open(&mut p, b"test");
        assert_eq!(send(&mut p, t, &cmif(4, false, 2, &[])), Ok(0));
        let (_, _, moved, _, _) = answer(&p, false, 0);
        assert_eq!(moved.len(), 1);
        assert_eq!(
            p.handles.get(moved[0]).map(|(k, _)| k),
            Some(Kind::ClientSession)
        );
    }

    /// CloneCurrentObject over `h`: the moved handle.
    fn clone(p: &mut Process, h: u64) -> u64 {
        assert_eq!(send(p, h, &cmif(5, false, 2, &[])), Ok(0));
        let (rc, _, moved, data, _) = answer(p, false, 0);
        assert_eq!((rc, moved.len(), data), (0, 1, vec![]));
        moved[0] as u64
    }

    #[test]
    fn a_clone_of_a_domain_session_shares_its_domain() {
        let mut p = setup();
        let h = open(&mut p, b"test");
        assert_eq!(convert(&mut p, h), Ok(1));
        let c = clone(&mut p, h);
        assert_ne!(c, h);
        // An object made through one session is reached through the other.
        assert_eq!(
            send(&mut p, h, &request(false, &[], Some(1), 2, &[])),
            Ok(0)
        );
        assert_eq!(answer(&p, true, 0).4, vec![2]);
        assert_eq!(
            send(&mut p, c, &request(false, &[], Some(2), 0, &[5; 4])),
            Ok(0)
        );
        assert_eq!(answer(&p, true, 4).3, vec![5; 4]);
        // Closing it through the clone closes it for both.
        let close = words(&[4, 8, 0, 0, 2, 2, 0, 0]);
        assert_eq!(send(&mut p, c, &close), Ok(0));
        let e = send(&mut p, h, &request(false, &[], Some(2), 0, &[])).unwrap_err();
        assert!(e.contains("domain object 2, which does not exist"), "{e}");
        // Closing the clone's session leaves the first one working.
        assert_eq!(send(&mut p, c, &words(&[2, 0])), Ok(0));
        assert_eq!(
            send(&mut p, h, &request(false, &[], Some(1), 0, &[6; 4])),
            Ok(0)
        );
        assert_eq!(answer(&p, true, 4).3, vec![6; 4]);
        // Outside a domain, a clone is another session to the same object.
        let t = open(&mut p, b"test");
        let u = clone(&mut p, t);
        assert_eq!(send(&mut p, u, &cmif(4, false, 0, &[7; 4])), Ok(0));
        assert_eq!(answer(&p, false, 4).3, vec![7; 4]);
        // CloneCurrentObjectEx takes a tag and clones the same way, as
        // libnx's nvInitialize sends it.
        assert_eq!(send(&mut p, t, &cmif(5, false, 4, &[1, 0, 0, 0])), Ok(0));
        let (rc, _, moved, data, _) = answer(&p, false, 0);
        assert_eq!((rc, moved.len(), data), (0, 1, vec![]));
        let v = moved[0] as u64;
        assert_eq!(send(&mut p, v, &cmif(4, false, 0, &[8; 4])), Ok(0));
        assert_eq!(answer(&p, false, 4).3, vec![8; 4]);
        let e = send(&mut p, t, &cmif(5, false, 1, &[0; 4])).unwrap_err();
        assert!(
            e.contains("control command 1 (CopyFromCurrentDomain)"),
            "{e}"
        );
    }

    #[test]
    fn every_session_reports_its_pointer_buffer_size() {
        // cmifQueryPointerBufferSize: control command 3, a u16 back.
        let mut p = setup();
        let h = open(&mut p, b"test");
        assert_eq!(send(&mut p, h, &cmif(5, false, 3, &[])), Ok(0));
        let (rc, _, _, data, _) = answer(&p, false, 2);
        assert_eq!((rc, data), (0, vec![0x00, 0x80]));
    }

    #[test]
    fn an_application_starts_the_way_libnx_starts_it() {
        let mut p = setup();
        // apmInitialize: apm, OpenSession.
        let apm = open(&mut p, b"apm");
        assert_eq!(send(&mut p, apm, &cmif(4, false, 0, &[])), Ok(0));
        assert_eq!(answer(&p, false, 0).2.len(), 1);
        assert_eq!(send(&mut p, apm, &cmif(4, false, 1, &[])), Ok(0));
        assert_eq!(answer(&p, false, 4).3, 1u32.to_le_bytes());
        // appletOE as a domain, then OpenApplicationProxy with the process
        // ID and the current-process handle.
        let am = open(&mut p, b"appletOE");
        assert_eq!(convert(&mut p, am), Ok(1));
        let cur = handle::CURRENT_PROCESS as u32;
        let e = send(&mut p, am, &request(true, &[], Some(1), 0, &[0; 8])).unwrap_err();
        assert!(e.contains("current-process handle"), "{e}");
        assert_eq!(
            send(&mut p, am, &request(true, &[cur], Some(1), 0, &[0; 8])),
            Ok(0)
        );
        let proxy = answer(&p, true, 0).4[0];
        let get = |p: &mut Process, n: u32| {
            assert_eq!(
                send(p, am, &request(false, &[], Some(proxy), n, &[])),
                Ok(0)
            );
            answer(p, true, 0).4[0]
        };
        let functions = get(&mut p, 20);
        let _creator = get(&mut p, 11);
        let state = get(&mut p, 0);
        let selfc = get(&mut p, 1);
        let window = get(&mut p, 2);
        let audio = get(&mut p, 3);
        // (object, command, arguments, output size) -> (result, data)
        let call = |p: &mut Process, object: u32, n: u32, args: &[u8], size: usize| {
            assert_eq!(
                send(p, am, &request(false, &[], Some(object), n, args)),
                Ok(0)
            );
            let (rc, copied, _, data, _) = answer(p, true, size);
            (rc, copied, data)
        };
        assert_eq!(call(&mut p, window, 1, &[], 8).2, PROCESS_ID.to_le_bytes());
        let (rc, tick_event, _) = call(&mut p, selfc, 91, &[], 0);
        assert_eq!((rc, tick_event.len()), (0, 1));
        let (rc, message_event, _) = call(&mut p, state, 0, &[], 0);
        assert_eq!((rc, message_event.len()), (0, 1));
        assert_eq!(
            call(&mut p, state, 9, &[], 1),
            (0, vec![], vec![am::FOCUS_STATE])
        );
        assert_eq!(call(&mut p, state, 1, &[], 0).0, am::NO_MESSAGE);
        assert_eq!(call(&mut p, window, 10, &[], 0).0, 0);
        assert_eq!(call(&mut p, selfc, 13, &[0, 0, 1], 0).0, 0);
        assert_eq!(call(&mut p, selfc, 16, &[0], 0).0, 0);
        assert_eq!(call(&mut p, functions, 40, &[], 1), (0, vec![], vec![0]));
        assert_eq!(call(&mut p, state, 5, &[], 1).2, [am::OPERATION_MODE]);
        assert_eq!(
            call(&mut p, state, 6, &[], 4).2,
            am::PERFORMANCE_MODE.to_le_bytes()
        );
        assert_eq!(call(&mut p, selfc, 11, &[1], 0).0, 0);
        assert_eq!(call(&mut p, selfc, 12, &[1], 0).0, 0);
        // CreateManagedDisplayLayer: the layer vi's OpenLayer opens.
        assert!(!p.services.vi.layer_made);
        assert_eq!(
            call(&mut p, selfc, 40, &[], 8),
            (0, vec![], vi::LAYER_ID.to_le_bytes().to_vec())
        );
        assert!(p.services.vi.layer_made);
        // The events are readable events that nothing signals; asking
        // again names the same event.
        let (_, again, _) = call(&mut p, state, 0, &[], 0);
        let object = |h: u32| p.handles.get(h).unwrap();
        assert_eq!(object(again[0]), object(message_event[0]));
        assert_eq!(object(message_event[0]).0, Kind::ReadableEvent);
        assert_ne!(object(tick_event[0]), object(message_event[0]));
        let e = send(&mut p, am, &request(false, &[], Some(selfc), 13, &[0])).unwrap_err();
        assert!(
            e.contains("ISelfController command 13 with 1 bytes of arguments, fewer than the 3"),
            "{e}"
        );
        let e = send(&mut p, am, &request(false, &[], Some(audio), 0, &[])).unwrap_err();
        assert!(
            e.starts_with("unimplemented: IAudioController command 0"),
            "{e}"
        );
    }

    /// Add pointer descriptor (X buffer) 0 to `msg`, built by request()
    /// with a process ID and no handles, as cmifRequestInPointer does.
    fn with_pointer(mut msg: Vec<u8>, addr: u64, size: u16) -> Vec<u8> {
        let w0 = u32::from_le_bytes(msg[0..4].try_into().unwrap());
        msg[0..4].copy_from_slice(&(w0 | 1 << 16).to_le_bytes());
        // The descriptor goes after the special header and process ID; the
        // CMIF data then moves to the next 16-byte boundary.
        let head = 8 + 4 + 8;
        let mut out = msg[..head].to_vec();
        out.extend(words(&[
            ((addr >> 36) as u32) << 6 | ((addr >> 32) as u32 & 0xf) << 12 | (size as u32) << 16,
            addr as u32,
        ]));
        let data = msg[head.next_multiple_of(16)..].to_vec();
        out.resize(out.len().next_multiple_of(16), 0);
        out.extend(data);
        out
    }

    #[test]
    fn hid_hands_out_its_shared_memory_and_takes_the_npad_setup() {
        let mut p = setup();
        let hid = open(&mut p, b"hid");
        let aruid = PROCESS_ID.to_le_bytes();
        let e = send(&mut p, hid, &request(true, &[], None, 0, &[1; 8])).unwrap_err();
        assert!(
            e.contains("applet resource user ID 0x80 (got 0x101010101010101)"),
            "{e}"
        );
        let e = send(&mut p, hid, &request(false, &[], None, 0, &aruid)).unwrap_err();
        assert!(e.contains("without the process ID"), "{e}");
        assert_eq!(
            send(&mut p, hid, &request(true, &[], None, 0, &aruid)),
            Ok(0)
        );
        let resource = answer(&p, false, 0).2[0] as u64;
        assert_eq!(send(&mut p, resource, &cmif(4, false, 0, &[])), Ok(0));
        let (rc, copied, _, _, _) = answer(&p, false, 0);
        assert_eq!((rc, copied.len()), (0, 1));
        let (kind, id) = p.handles.get(copied[0]).unwrap();
        assert_eq!(kind, Kind::SharedMemory);
        match p.handles.object(id) {
            Object::SharedMemory {
                perm,
                contents,
                mapped,
            } => {
                assert_eq!((*perm, contents.len(), *mapped), (Perm::R, 0x40000, None));
                assert!(contents.iter().all(|&b| b == 0));
            }
            o => panic!("{o:?}"),
        }
        // Asking again gives another handle to the same memory.
        assert_eq!(send(&mut p, resource, &cmif(4, false, 0, &[])), Ok(0));
        let again = answer(&p, false, 0).1[0];
        assert_eq!(p.handles.get(again).unwrap().1, id);
        // padConfigureInput: ActivateNpadWithRevision 5, the IDs, the styles.
        let mut rev = 5u32.to_le_bytes().to_vec();
        rev.extend([0; 4]);
        rev.extend(aruid);
        assert_eq!(
            send(&mut p, hid, &request(true, &[], None, 109, &rev)),
            Ok(0)
        );
        assert_eq!(answer(&p, false, 0).0, 0);
        assert_eq!(p.services.hid.npad_revision, Some(5));
        let ids = words(&[0, 1, 2, 3, 4, 5, 6, 7, 0x20]);
        p.mem.write_bytes(STRINGS + 0x200, &ids).unwrap();
        let set_ids = with_pointer(request(true, &[], None, 102, &aruid), STRINGS + 0x200, 36);
        assert_eq!(send(&mut p, hid, &set_ids), Ok(0));
        assert_eq!(answer(&p, false, 0).0, 0);
        assert_eq!(p.services.hid.npad_ids, [0, 1, 2, 3, 4, 5, 6, 7, 0x20]);
        let mut styles = 0x1fu32.to_le_bytes().to_vec();
        styles.extend([0; 4]);
        styles.extend(aruid);
        assert_eq!(
            send(&mut p, hid, &request(true, &[], None, 100, &styles)),
            Ok(0)
        );
        assert_eq!(
            send(&mut p, hid, &request(true, &[], None, 101, &aruid)),
            Ok(0)
        );
        assert_eq!(answer(&p, false, 4).3, 0x1fu32.to_le_bytes());
        for n in [11, 21, 31, 103] {
            assert_eq!(
                send(&mut p, hid, &request(true, &[], None, n, &aruid)),
                Ok(0)
            );
            assert_eq!(answer(&p, false, 0).0, 0);
        }
        assert_eq!(p.services.hid.npad_revision, Some(0));
        // Out-of-range input stops with the reason.
        p.mem.write_bytes(STRINGS + 0x200, &words(&[8])).unwrap();
        let bad = with_pointer(request(true, &[], None, 102, &aruid), STRINGS + 0x200, 4);
        let e = send(&mut p, hid, &bad).unwrap_err();
        assert!(e.contains("SetSupportedNpadIdType with 0x4 bytes"), "{e}");
        let e = send(&mut p, hid, &request(true, &[], None, 102, &aruid)).unwrap_err();
        assert!(e.contains("without pointer buffer 0"), "{e}");
        rev[0] = 4;
        let e = send(&mut p, hid, &request(true, &[], None, 109, &rev)).unwrap_err();
        assert!(
            e.contains("revision 4, which Switchbrew does not list"),
            "{e}"
        );
    }

    #[test]
    fn time_hands_out_the_clocks_and_their_shared_memory() {
        let mut p = setup();
        let t = open(&mut p, b"time:u");
        let get = |p: &mut Process, n: u32| {
            assert_eq!(send(p, t, &cmif(4, false, n, &[])), Ok(0));
            let (rc, _, moved, _, _) = answer(p, false, 0);
            assert_eq!((rc, moved.len()), (0, 1));
            moved[0] as u64
        };
        let user = get(&mut p, 0);
        let network = get(&mut p, 1);
        let steady = get(&mut p, 2);
        let zone = get(&mut p, 3);
        let local = get(&mut p, 4);
        assert_eq!(send(&mut p, t, &cmif(4, false, 20, &[])), Ok(0));
        let (rc, copied, _, _, _) = answer(&p, false, 0);
        assert_eq!((rc, copied.len()), (0, 1));
        let (kind, id) = p.handles.get(copied[0]).unwrap();
        assert_eq!(kind, Kind::SharedMemory);
        match p.handles.object(id) {
            Object::SharedMemory { perm, contents, .. } => {
                assert_eq!((*perm, contents.len()), (Perm::R, 0x1000));
                let boot = crate::firmware::BOOT_TIME.to_le_bytes();
                // Steady clock: counter, base time 0, source ID.
                assert!(contents[..0x10].iter().all(|&b| b == 0));
                assert_eq!(contents[0x10..0x20], time::SOURCE_ID);
                // Local (and user) and network clock contexts.
                for at in [0x40, 0x88] {
                    assert_eq!(contents[at - 8..at], [0; 8]);
                    assert_eq!(contents[at..at + 8], boot);
                    assert_eq!(contents[at + 8..at + 0x10], [0; 8]);
                    assert_eq!(contents[at + 0x10..at + 0x20], time::SOURCE_ID);
                }
            }
            o => panic!("{o:?}"),
        }
        // Three seconds and a bit of the process clock later.
        p.cpu.clock.skip(3 * 19_200_000 + 5);
        let now = (crate::firmware::BOOT_TIME + 3).to_le_bytes();
        for clock in [user, network, local] {
            assert_eq!(send(&mut p, clock, &cmif(4, false, 0, &[])), Ok(0));
            assert_eq!(answer(&p, false, 8).3, now);
        }
        assert_eq!(send(&mut p, user, &cmif(4, false, 2, &[])), Ok(0));
        let context = answer(&p, false, 0x20).3;
        assert_eq!(context[..8], crate::firmware::BOOT_TIME.to_le_bytes());
        assert_eq!(context[8..0x10], [0; 8]);
        assert_eq!(send(&mut p, steady, &cmif(4, false, 0, &[])), Ok(0));
        let point = answer(&p, false, 0x18).3;
        assert_eq!(point[..8], 3i64.to_le_bytes());
        assert_eq!(point[8..], time::SOURCE_ID);
        assert_eq!(send(&mut p, zone, &cmif(4, false, 0, &[])), Ok(0));
        let name = answer(&p, false, 0x24).3;
        assert_eq!(
            (&name[..4], name[4..].iter().all(|&b| b == 0)),
            (&b"UTC\0"[..], true)
        );
        // ToCalendarTimeWithMyRule: (time, y, m, d, h, min, s, wday, yday).
        let cases: [(i64, u16, [u8; 5], u32, u32); 4] = [
            (crate::firmware::BOOT_TIME, 2025, [6, 5, 0, 0, 0], 4, 155),
            (-1, 1969, [12, 31, 23, 59, 59], 3, 364),
            (951_825_845, 2000, [2, 29, 12, 4, 5], 2, 59),
            (253_402_300_799, 9999, [12, 31, 23, 59, 59], 5, 364),
        ];
        for (t, year, date, wday, yday) in cases {
            assert_eq!(
                send(&mut p, zone, &cmif(4, false, 101, &t.to_le_bytes())),
                Ok(0)
            );
            let (rc, _, _, out, _) = answer(&p, false, 0x20);
            assert_eq!(rc, 0);
            assert_eq!(out[..2], year.to_le_bytes(), "{t}");
            assert_eq!(out[2..8], [date[0], date[1], date[2], date[3], date[4], 0]);
            assert_eq!(out[8..0xc], wday.to_le_bytes(), "{t}");
            assert_eq!(out[0xc..0x10], yday.to_le_bytes(), "{t}");
            assert_eq!(out[0x10..0x18], *b"UTC\0\0\0\0\0");
            assert_eq!(out[0x18..], [0; 8]);
        }
        let far = cmif(4, false, 101, &i64::MAX.to_le_bytes());
        let e = send(&mut p, zone, &far).unwrap_err();
        assert!(e.contains("does not fit CalendarTime's u16"), "{e}");
        let e = send(&mut p, t, &cmif(4, false, 5, &[])).unwrap_err();
        assert!(e.starts_with("unimplemented: time:u command 5"), "{e}");
    }

    /// A domain request to `object` with pointer (X), send (A) and receive
    /// (B) buffers as (address, size), laid out as libnx's hipcMakeRequest
    /// and cmifMakeRequest lay them out.
    fn buffers(
        object: u32,
        cmd: u32,
        args: &[u8],
        x: &[(u64, u64)],
        a: &[(u64, u64)],
        b: &[(u64, u64)],
    ) -> Vec<u8> {
        let size = (16 + 16 + 16 + args.len() + 1) & !1;
        let w0 = 4 | (x.len() as u32) << 16 | (a.len() as u32) << 20 | (b.len() as u32) << 24;
        let mut m = words(&[w0, size.div_ceil(4) as u32]);
        for (i, &(addr, size)) in x.iter().enumerate() {
            m.extend(words(&[
                i as u32
                    | ((addr >> 36) as u32 & 0x3f) << 6
                    | ((addr >> 32) as u32 & 0xf) << 12
                    | (size as u32) << 16,
                addr as u32,
            ]));
        }
        for &(addr, size) in a.iter().chain(b) {
            m.extend(words(&[
                size as u32,
                addr as u32,
                ((addr >> 36) as u32 & 0x3f_ffff) << 2
                    | ((size >> 32) as u32 & 0xf) << 24
                    | ((addr >> 32) as u32 & 0xf) << 28,
            ]));
        }
        m.resize(m.len().next_multiple_of(16), 0);
        m.extend(words(&[1 | ((16 + args.len()) as u32) << 16, object, 0, 0]));
        m.extend(words(&[CMIF_IN_MAGIC, 0, cmd, 0]));
        m.extend(args);
        m
    }

    /// A request to a session (not a domain) as libnx's cmifMakeRequest
    /// lays it out, with a process ID, copied handles, and pointer (X),
    /// send (A), receive (B) and receive list (C) buffers as (address,
    /// size). Each C entry is an out pointer with its size in the table
    /// after the arguments.
    #[allow(clippy::too_many_arguments)]
    fn hipc(
        pid: bool,
        copy: &[u32],
        cmd: u32,
        args: &[u8],
        x: &[(u64, u64)],
        a: &[(u64, u64)],
        b: &[(u64, u64)],
        c: &[(u64, u64)],
    ) -> Vec<u8> {
        let table = (16 + 16 + args.len() + 1) & !1;
        let size = table + 2 * c.len();
        let special = pid || !copy.is_empty();
        let w0 = 4 | (x.len() as u32) << 16 | (a.len() as u32) << 20 | (b.len() as u32) << 24;
        let statics = if c.is_empty() { 0 } else { 2 + c.len() as u32 };
        let w1 = size.div_ceil(4) as u32 | statics << 10 | (special as u32) << 31;
        let mut m = words(&[w0, w1]);
        if special {
            m.extend(words(&[pid as u32 | (copy.len() as u32) << 1]));
            if pid {
                m.extend(words(&[0, 0]));
            }
            m.extend(words(copy));
        }
        for (i, &(addr, size)) in x.iter().enumerate() {
            m.extend(words(&[
                i as u32
                    | ((addr >> 36) as u32 & 0x3f) << 6
                    | ((addr >> 32) as u32 & 0xf) << 12
                    | (size as u32) << 16,
                addr as u32,
            ]));
        }
        for &(addr, size) in a.iter().chain(b) {
            m.extend(words(&[
                size as u32,
                addr as u32,
                ((addr >> 36) as u32 & 0x3f_ffff) << 2
                    | ((size >> 32) as u32 & 0xf) << 24
                    | ((addr >> 32) as u32 & 0xf) << 28,
            ]));
        }
        let data = m.len();
        m.resize(m.len().next_multiple_of(16), 0);
        m.extend(words(&[CMIF_IN_MAGIC, 0, cmd, 0]));
        m.extend(args);
        m.resize(data + table, 0);
        for &(_, size) in c {
            m.extend((size as u16).to_le_bytes());
        }
        m.resize(data + size.next_multiple_of(4), 0);
        for &(addr, size) in c {
            m.extend(words(&[
                addr as u32,
                (addr >> 32) as u32 & 0xffff | (size as u32) << 16,
            ]));
        }
        m
    }

    /// The u32s of a reply's data.
    fn u32s(data: &[u8]) -> Vec<u32> {
        data.chunks_exact(4)
            .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn nvdrv_opens_nvmap_and_registers_memory() {
        let mut p = setup();
        let nv = open(&mut p, b"nvdrv");
        let (input, output) = (STRINGS + 0x200, STRINGS + 0x300);
        // Open: the path in send buffer 0; then (descriptor, NvError).
        let open_dev = |p: &mut Process, path: &[u8]| {
            p.mem.write_bytes(STRINGS, path).unwrap();
            let msg = hipc(
                false,
                &[],
                0,
                &[],
                &[],
                &[(STRINGS, path.len() as u64)],
                &[],
                &[],
            );
            send(p, nv, &msg).map(|_| u32s(&answer(p, false, 8).3))
        };
        assert_eq!(
            open_dev(&mut p, b"/dev/nvmap"),
            Ok(vec![0, 3]),
            "NotInitialized"
        );
        // Initialize, as nvInitialize sends it: the process and transfer
        // memory handles and the memory's size.
        let heap = p.set_heap_size(0x20_0000).unwrap();
        let tmem = p.create_transfer_memory(heap, 0x1000, 0).unwrap();
        let init =
            |copy: &[u32]| hipc(false, copy, 3, &0x1000u32.to_le_bytes(), &[], &[], &[], &[]);
        let process = handle::CURRENT_PROCESS as u32;
        let e = send(&mut p, nv, &init(&[process])).unwrap_err();
        assert!(e.contains("Initialize without"), "{e}");
        assert_eq!(send(&mut p, nv, &init(&[process, tmem])), Ok(0));
        assert_eq!(u32s(&answer(&p, false, 4).3), vec![0]);
        // SetAruid: the process ID and the AppletResourceUserId.
        let aruid = PROCESS_ID.to_le_bytes();
        assert_eq!(
            send(&mut p, nv, &hipc(true, &[], 8, &aruid, &[], &[], &[], &[])),
            Ok(0)
        );
        assert_eq!(u32s(&answer(&p, false, 4).3), vec![0]);
        assert_eq!(open_dev(&mut p, b"/dev/nvmap"), Ok(vec![1, 0]));
        assert_eq!(open_dev(&mut p, b"/dev/nvhost-ctrl"), Ok(vec![2, 0]));
        let e = open_dev(&mut p, b"/dev/nvhost-gpu").unwrap_err();
        assert!(
            e.contains("unimplemented: nvdrv device \"/dev/nvhost-gpu\""),
            "{e}"
        );

        // Ioctl over pointers (X in, C out), as libnx sends small ones.
        let ioctl = |p: &mut Process, fd: u32, code: u32, arg: &[u8]| {
            p.mem.write_bytes(input, arg).unwrap();
            let size = arg.len() as u64;
            let args = [fd.to_le_bytes(), code.to_le_bytes()].concat();
            let (x, c) = ([(input, size)], [(output, size)]);
            let msg = hipc(false, &[], 1, &args, &x, &[(0, 0)], &[(0, 0)], &c);
            send(p, nv, &msg).map(|_| {
                let err = u32s(&answer(p, false, 4).3)[0];
                (err, p.mem.read_vec(output, arg.len()).unwrap())
            })
        };
        // CREATE: size in, handle out.
        let (err, out) = ioctl(&mut p, 1, 0xc008_0101, &words(&[0x3000, 0])).unwrap();
        assert_eq!((err, u32s(&out)), (0, vec![0x3000, 1]));
        assert_eq!(
            ioctl(&mut p, 1, 0xc008_0101, &[0; 8]).unwrap().0,
            4,
            "size 0"
        );
        // ALLOC through mapped buffers (A in, B out), as libnx sends ioctls
        // larger than the pointer buffer.
        let mut alloc = words(&[1, 0, 1, 0x1000]);
        alloc.extend([0xfe, 0, 0, 0, 0, 0, 0, 0]);
        alloc.extend(0x8000_0000u64.to_le_bytes());
        p.mem.write_bytes(input, &alloc).unwrap();
        let args = [1u32.to_le_bytes(), 0xc020_0104u32.to_le_bytes()].concat();
        let msg = hipc(
            false,
            &[],
            1,
            &args,
            &[(0, 0)],
            &[(input, 0x20)],
            &[(output, 0x20)],
            &[(0, 0)],
        );
        assert_eq!(send(&mut p, nv, &msg), Ok(0));
        assert_eq!(u32s(&answer(&p, false, 4).3), vec![0]);
        assert_eq!(p.mem.read_vec(output, 0x20).unwrap(), alloc);
        let m = p.services.nv.map(1).unwrap();
        assert_eq!(
            (m.size, m.addr, m.align, m.kind),
            (0x3000, Some(0x8000_0000), 0x1000, 0xfe)
        );
        // GET_ID: ID out first, handle in second.
        let (err, out) = ioctl(&mut p, 1, 0xc008_010e, &words(&[0, 1])).unwrap();
        assert_eq!((err, u32s(&out)), (0, vec![1, 1]));
        // PARAM: size, alignment, heap, kind; BASE is refused.
        for (param, value) in [(1, 0x3000), (2, 0x1000), (4, 0x4000_0000), (5, 0xfe)] {
            let (err, out) = ioctl(&mut p, 1, 0xc00c_0109, &words(&[1, param, 0])).unwrap();
            assert_eq!((err, u32s(&out)[2]), (0, value), "param {param}");
        }
        assert_eq!(
            ioctl(&mut p, 1, 0xc00c_0109, &words(&[1, 3, 0])).unwrap().0,
            4
        );
        // FROM_ID adds a handle; the first FREE leaves the memory.
        let (err, out) = ioctl(&mut p, 1, 0xc008_0103, &words(&[1, 0])).unwrap();
        assert_eq!((err, u32s(&out)), (0, vec![1, 1]));
        let free = |p: &mut Process| {
            let (err, out) = ioctl(p, 1, 0xc018_0105, &[0; 24]).unwrap();
            (
                err,
                u64::from_le_bytes(out[8..16].try_into().unwrap()),
                u32s(&out[16..]),
            )
        };
        let free_one = |p: &mut Process| {
            let mut arg = [0u8; 24];
            arg[0] = 1;
            let (err, out) = ioctl(p, 1, 0xc018_0105, &arg).unwrap();
            (
                err,
                u64::from_le_bytes(out[8..16].try_into().unwrap()),
                u32s(&out[16..]),
            )
        };
        assert_eq!(free_one(&mut p), (0, 0, vec![0x3000, 0]));
        assert_eq!(free_one(&mut p), (0, 0x8000_0000, vec![0x3000, 0]));
        assert_eq!(p.services.nv.map(1), None);
        assert_eq!(free(&mut p).0, 4, "handle 0");
        // Listed as NotSupported; an unknown ioctl stops.
        assert_eq!(ioctl(&mut p, 1, 0xc008_010c, &[0; 8]).unwrap().0, 2);
        let e = ioctl(&mut p, 1, 0xc008_01ff, &[0; 8]).unwrap_err();
        assert!(
            e.contains("unimplemented: nvdrv /dev/nvmap ioctl 0xc00801ff"),
            "{e}"
        );
        let e = ioctl(&mut p, 2, 0xc008_0014, &[0; 8]).unwrap_err();
        assert!(e.contains("/dev/nvhost-ctrl ioctl 0xc0080014"), "{e}");
        // A closed descriptor takes no ioctls.
        let close = hipc(false, &[], 2, &1u32.to_le_bytes(), &[], &[], &[], &[]);
        assert_eq!(send(&mut p, nv, &close), Ok(0));
        assert_eq!(u32s(&answer(&p, false, 4).3), vec![0]);
        assert_eq!(ioctl(&mut p, 1, 0xc008_0101, &[0; 8]).unwrap().0, 4);
        assert_eq!(send(&mut p, nv, &close), Ok(0));
        assert_eq!(u32s(&answer(&p, false, 4).3), vec![4]);
    }

    /// A parcel as libnx's parcelTransact sends it: the header, then
    /// IGraphicBufferProducer's interface token and `args`.
    fn parcel(args: &[u8]) -> Vec<u8> {
        let token = "android.gui.IGraphicBufferProducer";
        let mut payload = words(&[0x100, token.len() as u32]);
        for c in token.encode_utf16().chain([0]) {
            payload.extend(c.to_le_bytes());
        }
        payload.resize(payload.len().next_multiple_of(4), 0);
        payload.extend(args);
        let size = payload.len() as u32;
        let mut p = words(&[size, 0x10, 0, 0x10 + size]);
        p.extend(payload);
        p
    }

    /// A flattened object: its length, no file descriptors, its bytes.
    fn flattened(b: &[u8]) -> Vec<u8> {
        let mut v = words(&[b.len() as u32, 0]);
        v.extend(b);
        v
    }

    /// The GraphicBuffer libnx's framebufferCreate and
    /// bqSetPreallocatedBuffer send for a 32x8 block-linear RGB 565
    /// surface (blocks one GOB high) at `offset` in nvmap object `nvmap`:
    /// the "GBFR" header, then the NvGraphicBuffer after its NativeHandle
    /// header, as ints.
    fn graphic_buffer(nvmap: u32, offset: u32) -> Vec<u8> {
        let mut ints = vec![0u32; 0x51];
        ints[0] = u32::MAX;
        ints[1] = nvmap;
        ints[3] = 0xdaff_caff;
        ints[0x34 / 4] = 32;
        ints[0x38 / 4] = 8;
        ints[0x44 / 4] = 3;
        ints[0x48 / 4] = 64;
        ints[0x50 / 4] = offset;
        let mut v = words(&[0x4742_4652, 32, 8, 32, 4, 0xb00, 42, 0, 0, 0x51]);
        v.extend(words(&ints));
        v
    }

    /// Frames a test's screen was shown.
    struct Keep(Arc<Mutex<Vec<crate::display::Frame>>>);

    impl crate::display::Screen for Keep {
        fn present(&mut self, frame: &crate::display::Frame) {
            self.0.lock().unwrap().push(frame.clone());
        }
    }

    #[test]
    fn vi_shows_the_frames_a_layer_queues_and_frees_them_at_vsync() {
        let mut p = setup();
        let frames = Arc::new(Mutex::new(Vec::new()));
        p.screen = Some(Box::new(Keep(frames.clone())));
        // viInitialize asks for vi:m, then vi:s, then vi:u.
        let h = sm(&mut p);
        for refused in [&b"vi:m"[..], b"vi:s"] {
            assert_eq!(send(&mut p, h, &cmif(4, false, 1, &name(refused))), Ok(0));
            assert_eq!(reply(&p, true).0, sm::result::NOT_ALLOWED);
        }
        let vi = open(&mut p, b"vi:u");
        let get = |p: &mut Process, h: u64, n: u32, args: &[u8]| {
            assert_eq!(send(p, h, &cmif(4, false, n, args)), Ok(0));
            let (rc, moved, _) = reply(p, true);
            (rc, moved.first().map(|&m| m as u64))
        };
        assert_eq!(
            get(&mut p, vi, 0, &[1, 0, 0, 0]),
            (vi::result::NOT_AVAILABLE, None)
        );
        let ads = get(&mut p, vi, 0, &[0; 4]).1.unwrap();
        let relay = get(&mut p, ads, 100, &[]).1.unwrap();
        assert_eq!(
            get(&mut p, ads, 101, &[]),
            (vi::result::NOT_AVAILABLE, None)
        );
        // OpenDisplay "Default", then its resolution.
        let mut display = [0u8; 0x40];
        display[..7].copy_from_slice(b"Default");
        assert_eq!(send(&mut p, ads, &cmif(4, false, 1010, &display)), Ok(0));
        assert_eq!(answer(&p, false, 8).3, 1u64.to_le_bytes());
        let resolution = cmif(4, false, 1102, &1u64.to_le_bytes());
        assert_eq!(send(&mut p, ads, &resolution), Ok(0));
        assert_eq!(answer(&p, false, 16).3, words(&[1920, 0, 1080, 0]));
        let mut external = [0u8; 0x40];
        external[..8].copy_from_slice(b"External");
        let e = send(&mut p, ads, &cmif(4, false, 1010, &external)).unwrap_err();
        assert!(e.contains("unimplemented: vi display \"External\""), "{e}");
        // OpenLayer: the layer CreateManagedDisplayLayer made, the native
        // window through a receive buffer.
        let (window, input, output) = (STRINGS + 0x100, STRINGS + 0x200, STRINGS + 0x600);
        let mut args = display.to_vec();
        args.extend(vi::LAYER_ID.to_le_bytes());
        args.extend(PROCESS_ID.to_le_bytes());
        let open_layer = hipc(true, &[], 2020, &args, &[], &[], &[(window, 0x100)], &[]);
        let e = send(&mut p, ads, &open_layer).unwrap_err();
        assert!(
            e.contains("which the program has not made and opened"),
            "{e}"
        );
        p.services.vi.layer_made = true;
        assert_eq!(send(&mut p, ads, &open_layer), Ok(0));
        assert_eq!(answer(&p, false, 8).3, 0x3cu64.to_le_bytes());
        let w = p.mem.read_vec(window, 0x3c).unwrap();
        assert_eq!(
            u32s(&w[..0x28]),
            [0x28, 0x10, 4, 0x38, 2, PROCESS_ID as u32, 1, 0, 0, 0]
        );
        assert_eq!(&w[0x28..0x30], b"dispdrv\0");
        assert!(w[0x30..].iter().all(|&b| b == 0));
        // SetLayerScalingMode: FitToLayer (2), padding, the layer.
        let mut scaling = words(&[2, 0]);
        scaling.extend(vi::LAYER_ID.to_le_bytes());
        assert_eq!(get(&mut p, ads, 2101, &scaling), (0, None));
        // The relay: references, then the buffer-free event.
        assert_eq!(get(&mut p, relay, 1, &words(&[1, 1, 0])), (0, None));
        let e = send(&mut p, relay, &cmif(4, false, 1, &words(&[2, 1, 0]))).unwrap_err();
        assert!(e.contains("binder object 2, which is not open"), "{e}");
        let native = cmif(4, false, 2, &words(&[1, 0x0f]));
        assert_eq!(send(&mut p, relay, &native), Ok(0));
        let (rc, event, _, _, _) = answer(&p, false, 0);
        assert_eq!((rc, event.len()), (0, 1));
        let event = event[0];
        let signaled = |p: &Process| match p.handles.object(p.handles.get(event).unwrap().1) {
            Object::Event { signaled } => *signaled,
            o => panic!("{o:?}"),
        };
        // Transactions as TransactParcelAuto with pointers: the reply's
        // payload.
        let transact = |p: &mut Process, code: u32, args: &[u8]| {
            let req = parcel(args);
            p.mem.write_bytes(input, &req).unwrap();
            let (x, c) = ([(input, req.len() as u64)], [(output, 0x200)]);
            let a = words(&[1, code, 0]);
            let msg = hipc(false, &[], 3, &a, &x, &[(0, 0)], &[(0, 0)], &c);
            send(p, relay, &msg).map(|_| {
                assert_eq!(answer(p, false, 0).0, 0);
                let r = p.mem.read_vec(output, 0x200).unwrap();
                let (size, off) = (u32s(&r[..4])[0] as usize, u32s(&r[4..8])[0] as usize);
                r[off..off + size].to_vec()
            })
        };
        // CONNECT: no listener, the CPU API, not controlled by the app.
        let connect = words(&[0, 2, 0]);
        assert_eq!(
            u32s(&transact(&mut p, 10, &connect).unwrap()),
            [1280, 720, 0, 0, 0]
        );
        let again = transact(&mut p, 10, &connect).unwrap();
        assert_eq!(u32s(&again)[4] as i32, -22);
        // Two buffers in one nvmap object, 0x400 bytes each.
        let fb = 0x50_0000;
        p.mem.map(fb, 0x1000, Perm::RW, "fb").unwrap();
        p.services.nv.maps.push(Some(nv::MapObject {
            size: 0x800,
            addr: Some(fb),
            align: 0x1000,
            kind: 0,
            refs: 1,
        }));
        for slot in 0..2u32 {
            let mut args = words(&[slot, 1]);
            args.extend(flattened(&graphic_buffer(1, slot * 0x400)));
            assert_eq!(transact(&mut p, 14, &args), Ok(vec![]));
        }
        assert!(signaled(&p), "free buffers");
        // DEQUEUE_BUFFER: the slot, an empty fence, the status.
        let dequeue_args = words(&[1, 32, 8, 4, 0xb00]);
        let dequeue = |p: &mut Process| transact(p, 3, &dequeue_args).map(|r| u32s(&r));
        let mut first = vec![0, 1, 0x24, 0];
        first.extend([0; 9]);
        first.push(0);
        assert_eq!(dequeue(&mut p), Ok(first));
        // REQUEST_BUFFER: the GraphicBuffer the slot was given.
        let mut buffer = words(&[1]);
        buffer.extend(flattened(&graphic_buffer(1, 0)));
        buffer.extend(words(&[0]));
        assert_eq!(transact(&mut p, 1, &words(&[0])), Ok(buffer));
        // Light pixel (3, 5): byte 6 of row 5, in sector 4's second row.
        p.mem
            .write_bytes(fb + 4 * 32 + 16 + 6, &[0xff, 0xff])
            .unwrap();
        // QUEUE_BUFFER: the slot and a QueueBufferInput of 0x54 bytes.
        let mut queue = words(&[0]);
        queue.extend(flattened(&[0; 0x54]));
        p.cpu.clock.skip(1000);
        assert_eq!(
            u32s(&transact(&mut p, 7, &queue).unwrap()),
            [32, 8, 0, 1, 0]
        );
        let shown = frames.lock().unwrap().last().cloned().unwrap();
        assert_eq!((shown.width, shown.height), (32, 8));
        let lit: Vec<usize> = (0..256).filter(|&i| shown.pixels[i] != 0).collect();
        assert_eq!(lit, vec![5 * 32 + 3]);
        assert_eq!(shown.pixels[5 * 32 + 3], 0xffffff);
        // The second buffer, then none: WOULD_BLOCK, the event clear.
        assert_eq!(dequeue(&mut p).unwrap()[0], 1);
        assert_eq!(dequeue(&mut p), Ok(vec![u32::MAX, 0, -11i32 as u32]));
        assert!(!signaled(&p));
        queue[0] = 1;
        let pending = u32s(&transact(&mut p, 7, &queue).unwrap())[3];
        assert_eq!(pending, 2);
        // The thread waits on the event: the clock goes to the vsync that
        // frees the buffers, and the wait ends.
        let list = STRINGS + 0xf00;
        p.mem.write_bytes(list, &event.to_le_bytes()).unwrap();
        let wait = [0, list, 1, u64::MAX];
        svc(&mut p, nr::WAIT_SYNCHRONIZATION, &wait);
        // The wake-up wrote the result and the index.
        assert_eq!((p.cpu.x[0], p.cpu.x[1]), (0, 0));
        assert_eq!(p.now(), vi::VSYNC_PERIOD);
        assert_eq!(frames.lock().unwrap().len(), 2);
        // Freed oldest first; CANCEL_BUFFER frees a dequeued buffer.
        assert_eq!(dequeue(&mut p).unwrap()[0], 0);
        let mut cancel = words(&[0]);
        cancel.extend(flattened(&[0; 0x24]));
        assert_eq!(transact(&mut p, 8, &cancel), Ok(vec![]));
        assert_eq!(dequeue(&mut p).unwrap()[0], 1);
        // DISCONNECT forgets the buffers.
        assert_eq!(transact(&mut p, 11, &words(&[2])), Ok(words(&[0])));
        assert_eq!(dequeue(&mut p).unwrap()[2] as i32, -11);
        // What kestrel does not know stops.
        let e = transact(&mut p, 9, &words(&[0])).unwrap_err();
        assert!(
            e.contains("unimplemented: IGraphicBufferProducer transaction 9"),
            "{e}"
        );
        let mut bad = parcel(&[]);
        bad[0x10 + 8] = b'X';
        p.mem.write_bytes(input, &bad).unwrap();
        let (x, c) = ([(input, bad.len() as u64)], [(output, 0x200)]);
        let msg = hipc(
            false,
            &[],
            3,
            &words(&[1, 10, 0]),
            &x,
            &[(0, 0)],
            &[(0, 0)],
            &c,
        );
        let e = send(&mut p, relay, &msg).unwrap_err();
        assert!(e.contains("a binder transaction for \"Xndroid.gui"), "{e}");
        // TransactParcel (0) takes mapped buffers.
        let req = parcel(&connect);
        p.mem.write_bytes(input, &req).unwrap();
        let (a, b) = ([(input, req.len() as u64)], [(output, 0x200)]);
        let msg = hipc(false, &[], 0, &words(&[1, 10, 0]), &[], &a, &b, &[]);
        assert_eq!(send(&mut p, relay, &msg), Ok(0));
        let r = p.mem.read_vec(output, 0x24).unwrap();
        assert_eq!(u32s(&r), [0x14, 0x10, 0, 0x24, 1280, 720, 0, 0, 0]);
    }

    #[test]
    fn a_programs_own_file_shows_read_only_on_the_card() {
        // The program lives in a library folder outside the card's folder,
        // and shows at /switch/game.nro.
        let mut p = setup();
        let sd = sd_folder("fs-program");
        let library = sd_folder("fs-program-library");
        std::fs::write(library.join("game.nro"), b"NRO0 bytes").unwrap();
        p.services.fs.sd = Some(sd.clone());
        p.services.fs.program = Some(fs::Program {
            parts: vec!["switch".into(), "game.nro".into()],
            host: library.join("game.nro"),
        });
        const BUF: u64 = 0x20_0000;
        p.mem.map(BUF, 0x4000, Perm::RW, "fs").unwrap();
        let put = |p: &mut Process, at: u64, s: &str| {
            let mut b = s.as_bytes().to_vec();
            b.resize(0x301, 0);
            p.mem.write_bytes(at, &b).unwrap();
        };
        let x1 = [(BUF, 0x301)];
        let x2 = [(BUF, 0x301), (BUF + 0x400, 0x301)];
        let h = open(&mut p, b"fsp-srv");
        assert_eq!(convert(&mut p, h), Ok(1));
        let set = request(true, &[], Some(1), 1, &[0; 8]);
        assert_eq!(send(&mut p, h, &set), Ok(0));
        let sd_open = request(false, &[], Some(1), 18, &[]);
        assert_eq!(send(&mut p, h, &sd_open), Ok(0));
        let sdfs = answer(&p, true, 0).4[0];
        let call =
            |p: &mut Process, object, n, args: &[u8], x: &[(u64, u64)], b: &[(u64, u64)], size| {
                assert_eq!(send(p, h, &buffers(object, n, args, x, &[], b)), Ok(0));
                let (rc, _, _, data, objects) = answer(p, true, size);
                (rc, data, objects)
            };
        let read_dir = |p: &mut Process, path: &str| {
            put(p, BUF, path);
            let (rc, _, objects) = call(p, sdfs, 9, &3u32.to_le_bytes(), &x1, &[], 0);
            assert_eq!(rc, 0, "{path}");
            let b = [(BUF + 0x1000, 3 * fs::ENTRY_SIZE as u64)];
            let (_, n, _) = call(p, objects[0], 0, &[], &[], &b, 8);
            let n = u64::from_le_bytes(n.try_into().unwrap()) as usize;
            let raw = p.mem.read_vec(BUF + 0x1000, n * fs::ENTRY_SIZE).unwrap();
            let close = words(&[4, 8, 0, 0, 2, objects[0], 0, 0]);
            assert_eq!(send(p, h, &close), Ok(0));
            raw.chunks(fs::ENTRY_SIZE)
                .map(|e| {
                    let len = e.iter().position(|&b| b == 0).unwrap();
                    let size = u64::from_le_bytes(e[0x308..].try_into().unwrap());
                    (
                        String::from_utf8(e[..len].to_vec()).unwrap(),
                        e[0x304],
                        size,
                    )
                })
                .collect::<Vec<_>>()
        };
        // The card's root shows /switch, and /switch the program.
        assert_eq!(read_dir(&mut p, "/"), [("switch".into(), 0, 0)]);
        assert_eq!(read_dir(&mut p, "/switch/"), [("game.nro".into(), 1, 10)]);
        for (path, kind) in [("/switch", 0u32), ("/switch/game.nro", 1)] {
            put(&mut p, BUF, path);
            assert_eq!(
                call(&mut p, sdfs, 7, &[], &x1, &[], 4).1,
                kind.to_le_bytes()
            );
        }
        // It opens for reading; writing it is refused.
        put(&mut p, BUF, "/switch/game.nro");
        let (rc, _, file) = call(&mut p, sdfs, 8, &1u32.to_le_bytes(), &x1, &[], 0);
        assert_eq!(rc, 0);
        let mut read = vec![0u8; 24];
        read[16..].copy_from_slice(&4u64.to_le_bytes());
        let b = [(BUF + 0x1000, 4)];
        assert_eq!(
            call(&mut p, file[0], 0, &read, &[], &b, 8).1,
            4u64.to_le_bytes()
        );
        assert_eq!(p.mem.read_vec(BUF + 0x1000, 4).unwrap(), b"NRO0");
        assert_eq!(
            send(&mut p, h, &words(&[4, 8, 0, 0, 2, file[0], 0, 0])),
            Ok(0)
        );
        assert_eq!(
            call(&mut p, sdfs, 8, &3u32.to_le_bytes(), &x1, &[], 0).0,
            fs::result::UNSUPPORTED_WRITE_FOR_READ_ONLY_FILE
        );
        assert_eq!(
            call(&mut p, sdfs, 1, &[], &x1, &[], 0).0,
            fs::result::UNSUPPORTED_WRITE_FOR_READ_ONLY_FILE_SYSTEM
        );
        put(&mut p, BUF + 0x400, "/other.nro");
        assert_eq!(
            call(&mut p, sdfs, 5, &[], &x2, &[], 0).0,
            fs::result::UNSUPPORTED_WRITE_FOR_READ_ONLY_FILE_SYSTEM
        );
        put(&mut p, BUF, "/switch");
        assert_eq!(
            call(&mut p, sdfs, 4, &[], &x1, &[], 0).0,
            fs::result::UNSUPPORTED_WRITE_FOR_READ_ONLY_FILE_SYSTEM
        );
        assert_eq!(
            call(&mut p, sdfs, 0, &[0; 16], &x1, &[], 0).0,
            fs::result::PATH_ALREADY_EXISTS
        );
        // What the program writes goes to the card's folder, making
        // /switch there; the program still shows beside it.
        assert!(!sd.join("switch").exists());
        put(&mut p, BUF, "/switch/save.dat");
        assert_eq!(call(&mut p, sdfs, 0, &[0; 16], &x1, &[], 0).0, 0);
        assert!(sd.join("switch").join("save.dat").is_file());
        assert!(!library.join("save.dat").exists());
        assert_eq!(
            read_dir(&mut p, "/switch"),
            [("game.nro".into(), 1, 10), ("save.dat".into(), 1, 0)]
        );
        // A real file at the program's path wins.
        std::fs::write(sd.join("switch").join("game.nro"), b"mine").unwrap();
        assert_eq!(
            read_dir(&mut p, "/switch"),
            [("game.nro".into(), 1, 4), ("save.dat".into(), 1, 0)]
        );
        put(&mut p, BUF, "/switch/game.nro");
        assert_eq!(call(&mut p, sdfs, 1, &[], &x1, &[], 0).0, 0);
        assert_eq!(
            std::fs::read(library.join("game.nro")).unwrap(),
            b"NRO0 bytes"
        );
        std::fs::remove_dir_all(&sd).unwrap();
        std::fs::remove_dir_all(&library).unwrap();
    }

    /// A fresh, empty folder for one test's SD card.
    fn sd_folder(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kestrel-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fs_serves_the_sd_folder() {
        let mut p = setup();
        let sd = sd_folder("fs-unit");
        p.services.fs.sd = Some(sd.clone());
        const BUF: u64 = 0x20_0000;
        p.mem.map(BUF, 0x4000, Perm::RW, "fs").unwrap();
        // Paths go at BUF and BUF + 0x400, padded to 0x301 bytes; data at
        // BUF + 0x800, read buffers at BUF + 0x1000.
        let put = |p: &mut Process, at: u64, s: &str| {
            let mut b = s.as_bytes().to_vec();
            b.resize(0x301, 0);
            p.mem.write_bytes(at, &b).unwrap();
        };
        let x1 = [(BUF, 0x301)];
        let x2 = [(BUF, 0x301), (BUF + 0x400, 0x301)];

        // fsInitialize: a domain, SetCurrentProcess, two clones.
        let h = open(&mut p, b"fsp-srv");
        assert_eq!(convert(&mut p, h), Ok(1));
        let e = send(&mut p, h, &request(false, &[], Some(1), 18, &[])).unwrap_err();
        assert!(e.contains("before SetCurrentProcess"), "{e}");
        let e = send(&mut p, h, &request(false, &[], Some(1), 1, &[0; 8])).unwrap_err();
        assert!(e.contains("without the process ID"), "{e}");
        assert_eq!(
            send(&mut p, h, &request(true, &[], Some(1), 1, &[0; 8])),
            Ok(0)
        );
        assert_eq!(answer(&p, true, 0).0, 0);
        let c = clone(&mut p, h);
        assert_eq!(
            send(&mut p, h, &request(false, &[], Some(1), 18, &[])),
            Ok(0)
        );
        let (rc, _, _, _, objects) = answer(&p, true, 0);
        assert_eq!((rc, objects.len()), (0, 1));
        let sdfs = objects[0];

        // (object, command, args, X, A, B, output size) over the clone.
        let call = |p: &mut Process,
                    object,
                    n,
                    args: &[u8],
                    x: &[(u64, u64)],
                    a: &[(u64, u64)],
                    b: &[(u64, u64)],
                    size| {
            assert_eq!(send(p, c, &buffers(object, n, args, x, a, b)), Ok(0));
            let (rc, _, _, data, objects) = answer(p, true, size);
            (rc, data, objects)
        };
        let stop = |p: &mut Process,
                    object,
                    n,
                    args: &[u8],
                    x: &[(u64, u64)],
                    a: &[(u64, u64)],
                    b: &[(u64, u64)]| {
            send(p, c, &buffers(object, n, args, x, a, b)).unwrap_err()
        };
        let create_file = [0u8; 16];
        put(&mut p, BUF, "/d");
        assert_eq!(call(&mut p, sdfs, 2, &[], &x1, &[], &[], 0).0, 0);
        assert_eq!(
            call(&mut p, sdfs, 2, &[], &x1, &[], &[], 0).0,
            fs::result::PATH_ALREADY_EXISTS
        );
        assert!(sd.join("d").is_dir());
        put(&mut p, BUF, "/d/f");
        assert_eq!(call(&mut p, sdfs, 0, &create_file, &x1, &[], &[], 0).0, 0);
        put(&mut p, BUF, "/nope/f");
        assert_eq!(
            call(&mut p, sdfs, 0, &create_file, &x1, &[], &[], 0).0,
            fs::result::PATH_NOT_FOUND
        );
        // GetEntryType.
        for (path, want) in [
            ("/d", (0, 0u32)),
            ("/d/f", (0, 1)),
            ("/", (0, 0)),
            ("/x", (fs::result::PATH_NOT_FOUND, 0)),
        ] {
            put(&mut p, BUF, path);
            let (rc, data, _) = call(&mut p, sdfs, 7, &[], &x1, &[], &[], 4);
            let ty = if rc == 0 {
                u32::from_le_bytes(data.try_into().unwrap())
            } else {
                0
            };
            assert_eq!((rc, ty), want, "{path}");
        }
        // Paths kestrel refuses.
        for (path, rc) in [
            ("/d/../d", fs::result::NOT_NORMALIZED),
            ("/a:b", fs::result::INVALID_CHARACTER),
            ("d", fs::result::INVALID_PATH_FORMAT),
        ] {
            put(&mut p, BUF, path);
            assert_eq!(call(&mut p, sdfs, 7, &[], &x1, &[], &[], 0).0, rc, "{path}");
        }

        // Write through a write+append handle, then a write-only one.
        put(&mut p, BUF, "/d/f");
        let open_file = |p: &mut Process, m: u32| {
            let (rc, _, objects) = call(p, sdfs, 8, &m.to_le_bytes(), &x1, &[], &[], 0);
            assert_eq!((rc, objects.len()), (0, 1));
            objects[0]
        };
        let io = |offset: u64, size: u64| {
            let mut a = vec![0u8; 8];
            a.extend(offset.to_le_bytes());
            a.extend(size.to_le_bytes());
            a
        };
        let appender = open_file(&mut p, 6);
        p.mem.write_bytes(BUF + 0x800, b"hello").unwrap();
        let data = [(BUF + 0x800, 5)];
        assert_eq!(
            call(&mut p, appender, 1, &io(0, 5), &[], &data, &[], 0).0,
            0
        );
        let writer = open_file(&mut p, 2);
        assert_eq!(
            call(&mut p, writer, 1, &io(3, 5), &[], &data, &[], 0).0,
            fs::result::FILE_EXTENSION_WITHOUT_OPEN_MODE_ALLOW_APPEND
        );
        p.mem.write_bytes(BUF + 0x800, b"HE").unwrap();
        assert_eq!(
            call(
                &mut p,
                writer,
                1,
                &io(0, 2),
                &[],
                &[(BUF + 0x800, 2)],
                &[],
                0
            )
            .0,
            0
        );
        let e = stop(&mut p, writer, 0, &io(0, 2), &[], &[], &[(BUF + 0x1000, 2)]);
        assert!(
            e.contains("IFile Read on a file opened with mode 0x2"),
            "{e}"
        );
        // Read it back: five bytes of a 16-byte read, then none past the end.
        let reader = open_file(&mut p, 1);
        let (rc, data, _) = call(
            &mut p,
            reader,
            0,
            &io(0, 16),
            &[],
            &[],
            &[(BUF + 0x1000, 16)],
            8,
        );
        assert_eq!((rc, u64::from_le_bytes(data.try_into().unwrap())), (0, 5));
        assert_eq!(p.mem.read_vec(BUF + 0x1000, 5).unwrap(), b"HEllo");
        let (_, data, _) = call(
            &mut p,
            reader,
            0,
            &io(9, 4),
            &[],
            &[],
            &[(BUF + 0x1000, 4)],
            8,
        );
        assert_eq!(data, 0u64.to_le_bytes());
        assert_eq!(
            call(&mut p, reader, 4, &[], &[], &[], &[], 8).1,
            5u64.to_le_bytes()
        );
        assert_eq!(std::fs::read(sd.join("d/f")).unwrap(), b"HEllo");
        // An open file cannot be deleted or renamed.
        assert_eq!(
            call(&mut p, sdfs, 1, &[], &x1, &[], &[], 0).0,
            fs::result::TARGET_LOCKED
        );
        put(&mut p, BUF + 0x400, "/d/g");
        assert_eq!(
            call(&mut p, sdfs, 5, &[], &x2, &[], &[], 0).0,
            fs::result::TARGET_LOCKED
        );

        // List /d: one file of five bytes.
        put(&mut p, BUF, "/d");
        let (rc, _, objects) = call(&mut p, sdfs, 9, &3u64.to_le_bytes(), &x1, &[], &[], 0);
        assert_eq!(rc, 0);
        // libnx sends the filter as a u32.
        let (rc, _, libnx) = call(&mut p, sdfs, 9, &3u32.to_le_bytes(), &x1, &[], &[], 0);
        assert_eq!((rc, libnx.len()), (0, 1));
        let close = words(&[4, 8, 0, 0, 2, libnx[0], 0, 0]);
        assert_eq!(send(&mut p, c, &close), Ok(0));
        let dir = objects[0];
        assert_eq!(
            call(&mut p, dir, 1, &[], &[], &[], &[], 8).1,
            1u64.to_le_bytes()
        );
        let b = [(BUF + 0x1000, 2 * fs::ENTRY_SIZE as u64)];
        assert_eq!(
            call(&mut p, dir, 0, &[], &[], &[], &b, 8).1,
            1u64.to_le_bytes()
        );
        let e = p.mem.read_vec(BUF + 0x1000, fs::ENTRY_SIZE).unwrap();
        assert_eq!((&e[..2], e[0x301], e[0x304]), (&b"f\0"[..], 0, 1));
        assert_eq!(e[0x308..], 5u64.to_le_bytes());
        assert_eq!(
            call(&mut p, dir, 0, &[], &[], &[], &b, 8).1,
            0u64.to_le_bytes()
        );

        // Closing the objects unlocks the file.
        assert_eq!(p.services.fs.open_objects(), 4);
        for object in [appender, writer, reader, dir] {
            assert_eq!(
                send(&mut p, c, &words(&[4, 8, 0, 0, 2, object, 0, 0])),
                Ok(0)
            );
        }
        assert_eq!(p.services.fs.open_objects(), 0);
        put(&mut p, BUF, "/d/f");
        assert_eq!(call(&mut p, sdfs, 5, &[], &x2, &[], &[], 0).0, 0);
        assert!(sd.join("d/g").is_file() && !sd.join("d/f").exists());
        put(&mut p, BUF, "/d");
        let e = stop(&mut p, sdfs, 3, &[], &x1, &[], &[]);
        assert!(
            e.contains("DeleteDirectory of /d, which is not empty"),
            "{e}"
        );
        assert_eq!(call(&mut p, sdfs, 4, &[], &x1, &[], &[], 0).0, 0);
        assert!(!sd.join("d").exists());

        // A file left open goes with the domain's last session.
        put(&mut p, BUF, "/f");
        assert_eq!(call(&mut p, sdfs, 0, &create_file, &x1, &[], &[], 0).0, 0);
        open_file(&mut p, 1);
        assert_eq!(p.services.fs.open_objects(), 1);
        for s in [c, h] {
            p.cpu.x[0] = s;
            assert_eq!(dispatch(&mut p, nr::CLOSE_HANDLE), None);
        }
        assert_eq!(p.services.fs.open_objects(), 0);
        std::fs::remove_dir_all(&sd).unwrap();

        // Without a card, OpenSdCardFileSystem fails.
        let mut p = setup();
        let h = open(&mut p, b"fsp-srv");
        assert_eq!(
            send(&mut p, h, &request(true, &[], None, 1, &[0; 8])),
            Ok(0)
        );
        assert_eq!(send(&mut p, h, &request(false, &[], None, 18, &[])), Ok(0));
        assert_eq!(answer(&p, false, 0).0, fs::result::PORT_SD_CARD_NO_DEVICE);
    }

    #[test]
    fn psm_reports_a_docked_console_on_its_charger() {
        let mut p = setup();
        let psm = open(&mut p, b"psm");
        let mut get = |n: u32, size: usize| {
            assert_eq!(send(&mut p, psm, &cmif(4, false, n, &[])), Ok(0));
            let (rc, _, _, data, _) = answer(&p, false, size);
            assert_eq!(rc, 0);
            data
        };
        assert_eq!(get(0, 4), 100u32.to_le_bytes());
        assert_eq!(get(1, 4), 1u32.to_le_bytes());
        assert_eq!(get(4, 1), [1]);
        assert_eq!(get(12, 4), 3u32.to_le_bytes());
        assert_eq!(get(13, 8), 100f64.to_le_bytes());
        assert_eq!(get(14, 1), [1]);
        assert_eq!(get(15, 8), 100f64.to_le_bytes());
        assert_eq!(send(&mut p, psm, &cmif(4, false, 7, &[])), Ok(0));
        let session = answer(&p, false, 0).2[0] as u64;
        assert_eq!(send(&mut p, session, &cmif(4, false, 0, &[])), Ok(0));
        let (rc, copied, _, _, _) = answer(&p, false, 0);
        assert_eq!((rc, copied.len()), (0, 1));
        assert_eq!(
            p.handles.get(copied[0]).map(|(k, _)| k),
            Some(Kind::ReadableEvent)
        );
        for n in [2, 3, 4] {
            assert_eq!(send(&mut p, session, &cmif(4, false, n, &[1])), Ok(0));
            assert_eq!(answer(&p, false, 0).0, 0);
        }
        assert_eq!(send(&mut p, session, &cmif(4, false, 1, &[])), Ok(0));
        let e = send(&mut p, psm, &cmif(4, false, 2, &[])).unwrap_err();
        assert!(e.starts_with("unimplemented: psm command 2"), "{e}");
    }

    /// A scripted controller: A for the first three samples, then B; the
    /// left stick fully right, the right stick a quarter down.
    struct Script(crate::input::Controller);

    impl crate::input::Source for Script {
        fn controller(&self) -> crate::input::Controller {
            self.0
        }
        fn sample(&mut self, n: u64) -> crate::input::Pad {
            use crate::input::button;
            crate::input::Pad {
                buttons: if n < 3 { button::A } else { button::B },
                left: (0x7fff, 0),
                right: (0, -0x2000),
            }
        }
    }

    /// Open hid and its shared memory as hidInitialize does; with `setup`,
    /// also configure it as padConfigureInput(1, standard styles) does.
    fn hid_with_input(p: &mut Process, c: crate::input::Controller, setup: bool) -> ObjectId {
        p.input = Some(Box::new(Script(c)));
        let hid = open(p, b"hid");
        let aruid = PROCESS_ID.to_le_bytes();
        assert_eq!(send(p, hid, &request(true, &[], None, 0, &aruid)), Ok(0));
        let resource = answer(p, false, 0).2[0] as u64;
        assert_eq!(send(p, resource, &cmif(4, false, 0, &[])), Ok(0));
        let shm = answer(p, false, 0).1[0];
        if setup {
            let mut rev = 5u64.to_le_bytes().to_vec();
            rev.extend(aruid);
            assert_eq!(send(p, hid, &request(true, &[], None, 109, &rev)), Ok(0));
            p.mem
                .write_bytes(STRINGS + 0x200, &words(&[0, 1, 2, 3, 4, 5, 6, 7, 0x20]))
                .unwrap();
            let ids = with_pointer(request(true, &[], None, 102, &aruid), STRINGS + 0x200, 36);
            assert_eq!(send(p, hid, &ids), Ok(0));
            let mut styles = 0x1fu64.to_le_bytes().to_vec();
            styles.extend(aruid);
            assert_eq!(send(p, hid, &request(true, &[], None, 100, &styles)), Ok(0));
        }
        // Sample next one period from now.
        p.services.hid.next_sample = p.now() + hid::SAMPLE_PERIOD;
        p.handles.get(shm).unwrap().1
    }

    /// Advance the guest clock by one sampling period and let hid sample.
    fn sample(p: &mut Process) {
        p.cpu.clock.skip(hid::SAMPLE_PERIOD);
        p.hid_tick();
    }

    fn shared_bytes(p: &Process, id: ObjectId, at: usize, len: usize) -> Vec<u8> {
        match p.handles.object(id) {
            Object::SharedMemory {
                contents,
                mapped: None,
                ..
            } => contents[at..at + len].to_vec(),
            Object::SharedMemory {
                mapped: Some(addr), ..
            } => p.mem.read_vec(addr + at as u64, len).unwrap(),
            o => panic!("{o:?}"),
        }
    }

    fn u64_at(p: &Process, id: ObjectId, at: usize) -> u64 {
        u64::from_le_bytes(shared_bytes(p, id, at, 8).try_into().unwrap())
    }

    #[test]
    fn hid_writes_the_controller_into_its_shared_memory() {
        use crate::input::{button, Controller};
        use hid::layout::*;
        let mut p = setup();
        let shm = hid_with_input(&mut p, Controller::ProController, true);
        let entry = NPAD; // No1
        let lifo = entry + FULL_KEY_LIFO;
        for _ in 0..3 {
            sample(&mut p);
        }
        assert_eq!(u64_at(&p, shm, entry + STYLE_SET), 1); // FullKey, Dual
        assert_eq!(shared_bytes(&p, shm, entry + DEVICE_TYPE, 4), [1, 0, 0, 0]);
        assert_eq!(
            u64_at(&p, shm, entry + SYSTEM_PROPERTIES),
            1 << 11 | 1 << 13 | 1 << 14 | 1 << 15
        );
        assert_eq!(
            shared_bytes(&p, shm, entry + APPLET_FOOTER_UI_TYPE, 1),
            [12]
        );
        // FullKeyColor Ok, JoyColor NoController.
        assert_eq!(shared_bytes(&p, shm, entry + FULL_KEY_COLOR, 4), [0; 4]);
        assert_eq!(shared_bytes(&p, shm, entry + JOY_COLOR, 4), [2, 0, 0, 0]);
        // The ring: BufferCount 17, Tail 2, Count 3.
        assert_eq!(
            [8, 0x10, 0x18].map(|at| u64_at(&p, shm, lifo + at)),
            [17, 2, 3]
        );
        let newest = lifo + LIFO_HEADER + 2 * LIFO_ENTRY;
        let e = shared_bytes(&p, shm, newest, LIFO_ENTRY);
        assert_eq!(e[..8], 2u64.to_le_bytes());
        assert_eq!(e[8..0x10], 2u64.to_le_bytes());
        // A, StickLRight; the right stick at a quarter presses nothing.
        assert_eq!(e[0x10..0x18], (button::A | 1 << 18).to_le_bytes());
        assert_eq!(e[0x18..0x1c], 0x7fffi32.to_le_bytes());
        assert_eq!(e[0x24..0x28], (-0x2000i32).to_le_bytes());
        assert_eq!(e[0x28..0x2c], 1u32.to_le_bytes()); // IsConnected
                                                       // The ring wraps: after 20 samples Tail is 19 % 17 and Count 16.
        for _ in 3..20 {
            sample(&mut p);
        }
        assert_eq!([0x10, 0x18].map(|at| u64_at(&p, shm, lifo + at)), [2, 16]);
        assert_eq!(u64_at(&p, shm, newest), 19);
        assert_eq!(
            shared_bytes(&p, shm, newest + 0x10, 8),
            (button::B | 1 << 18).to_le_bytes()
        );
        // Samples land in the program's mapping once it is mapped.
        let h = p.handles.add_handle(shm, Kind::SharedMemory).unwrap() as u64;
        assert!(p
            .map_shared_memory(h, 0x4000_0000, hid::SHARED_MEMORY_SIZE, 1)
            .is_ok());
        sample(&mut p);
        assert_eq!(u64_at(&p, shm, lifo + 0x10), 3);
        assert_eq!(u64_at(&p, shm, lifo + LIFO_HEADER + 3 * LIFO_ENTRY), 20);
        // After a long sleep hid catches up with a ring's worth of samples.
        p.cpu.clock.skip(40 * hid::SAMPLE_PERIOD);
        p.hid_tick();
        assert_eq!(p.services.hid.samples, 21 + 17);
        assert_eq!(u64_at(&p, shm, lifo + 0x18), 16);
    }

    #[test]
    fn handheld_joy_con_connect_as_the_handheld_npad() {
        use crate::input::Controller;
        use hid::layout::*;
        let mut p = setup();
        let shm = hid_with_input(&mut p, Controller::Handheld, true);
        sample(&mut p);
        let entry = NPAD + HANDHELD_ENTRY * NPAD_ENTRY;
        assert_eq!(u64_at(&p, shm, entry + STYLE_SET), 2);
        assert_eq!(
            shared_bytes(&p, shm, entry + DEVICE_TYPE, 4),
            [0xc, 0, 0, 0]
        );
        assert_eq!(shared_bytes(&p, shm, entry + APPLET_FOOTER_UI_TYPE, 1), [4]);
        assert_eq!(shared_bytes(&p, shm, entry + JOY_COLOR, 4), [0; 4]);
        let e = shared_bytes(&p, shm, entry + HANDHELD_LIFO + LIFO_HEADER, LIFO_ENTRY);
        assert_eq!(e[0x28..0x2c], 0x3fu32.to_le_bytes());
        // Player 1's entry stays empty.
        assert_eq!(u64_at(&p, shm, NPAD + STYLE_SET), 0);
    }

    #[test]
    fn no_controller_until_the_program_accepts_it() {
        use crate::input::Controller;
        let mut p = setup();
        let shm = hid_with_input(&mut p, Controller::ProController, false);
        sample(&mut p);
        assert!(shared_bytes(&p, shm, 0, hid::SHARED_MEMORY_SIZE as usize)
            .iter()
            .all(|&b| b == 0));
        assert!(p.log.is_empty());
        // Activated, nothing supported yet: not connected, but not refused
        // either, as the next call may set what the program supports.
        let hid = open(&mut p, b"hid");
        let aruid = PROCESS_ID.to_le_bytes();
        let mut rev = 5u64.to_le_bytes().to_vec();
        rev.extend(aruid);
        assert_eq!(
            send(&mut p, hid, &request(true, &[], None, 109, &rev)),
            Ok(0)
        );
        sample(&mut p);
        assert!(p.log.is_empty(), "{:?}", p.log);
        // Only Handheld supported: refused, noted once.
        let mut styles = 2u64.to_le_bytes().to_vec();
        styles.extend(aruid);
        assert_eq!(
            send(&mut p, hid, &request(true, &[], None, 100, &styles)),
            Ok(0)
        );
        sample(&mut p);
        sample(&mut p);
        assert!(shared_bytes(&p, shm, 0, hid::SHARED_MEMORY_SIZE as usize)
            .iter()
            .all(|&b| b == 0));
        assert_eq!(p.log.len(), 1);
        assert!(
            p.log[0].contains("does not accept the emulated ProController"),
            "{}",
            p.log[0]
        );
    }

    #[test]
    fn fatal_errors_are_reported_or_end_the_program() {
        let mut p = setup();
        let fatal = open(&mut p, b"fatal:u");
        // libnx's MAKERESULT(Module_Libnx, LibnxError_InitFail_FS).
        let throw = |policy: u32| {
            let mut args = (345u32 | 33 << 9).to_le_bytes().to_vec();
            args.extend(policy.to_le_bytes());
            args.extend([0; 8]);
            request(true, &[], None, 1, &args)
        };
        assert_eq!(send(&mut p, fatal, &throw(1)), Ok(0));
        assert_eq!(answer(&p, false, 0).0, 0);
        assert_eq!(p.log, ["fatal error report 2345-0033 (Libnx: InitFail_FS)"]);
        let tlr = p.current_thread().tlr;
        p.mem.write_bytes(tlr, &throw(2)).unwrap();
        p.cpu.x[0] = fatal;
        assert_eq!(
            dispatch(&mut p, nr::SEND_SYNC_REQUEST),
            Some(Stop::Fatal {
                result: 345 | 33 << 9
            })
        );
    }

    #[test]
    fn svc_break_notifies_or_aborts_with_libnx_result() {
        let mut p = setup();
        p.mem.write_bytes(STRINGS, &0x202u32.to_le_bytes()).unwrap();
        // Panic with the notification-only flag: noted, returns 0.
        p.cpu.x[..3].copy_from_slice(&[0x8000_0000, STRINGS, 4]);
        assert_eq!(dispatch(&mut p, nr::BREAK), None);
        assert_eq!(p.cpu.x[0], 0);
        assert_eq!(
            p.log,
            ["svcBreak notification (Panic), result 2002-0001 (Fs)"]
        );
        // diagAbortWithResult: Panic, the result's address and size.
        p.cpu.x[..3].copy_from_slice(&[0, STRINGS, 4]);
        let pc = p.cpu.pc - 4;
        assert_eq!(
            dispatch(&mut p, nr::BREAK),
            Some(Stop::Abort {
                pc,
                reason: 0,
                result: Some(0x202)
            })
        );
        // Any other size passes no result.
        p.cpu.x[..3].copy_from_slice(&[2, STRINGS, 8]);
        assert!(matches!(
            dispatch(&mut p, nr::BREAK),
            Some(Stop::Abort {
                reason: 2,
                result: None,
                ..
            })
        ));
    }

    #[test]
    fn only_session_handles_take_requests() {
        let mut p = setup();
        let main = p.main_thread as u64;
        assert_eq!(send(&mut p, main, &[]), Ok(result::INVALID_HANDLE));
        assert_eq!(send(&mut p, 0, &[]), Ok(result::INVALID_HANDLE));
    }
}
