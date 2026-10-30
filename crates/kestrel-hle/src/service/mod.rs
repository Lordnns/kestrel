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
pub mod psm;
pub mod set;
pub mod sm;
pub mod time;

use crate::ipc::{self, command_type, domain_kind, Reply, Request};
use crate::kernel::{Kind, Object, ObjectId};
use crate::svc::result;
use crate::Process;
use std::cell::RefCell;
use std::rc::Rc;

/// Longest port name read, with its NUL: kestrel's choice, as long as the
/// longest name a service can have (8 bytes) with room to spare.
const PORT_NAME_MAX: u64 = 12;

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
pub type Domain = Rc<RefCell<Vec<Option<Interface>>>>;

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
                if n == 2 {
                    // CloneCurrentObject: a new session to the same object,
                    // moved to the client. libnx's session manager
                    // (`sf/sessionmgr.c`) clones a domain session and sends
                    // requests for the domain's objects over the clones, so
                    // a clone shares the domain.
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
                    let domain = Rc::new(RefCell::new(vec![Some(session.object)]));
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
        let mut domain = domain.borrow_mut();
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
                let mut domain = domain.borrow_mut();
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
            Some(d) if Rc::strong_count(&d) == 1 => {
                for object in d.take().into_iter().flatten() {
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
    use crate::testutil::process;
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
        let mut p = process();
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
            stop_from(&mut p, t, &cmif(5, false, 3, &[])),
            unimplemented("CMIF control command 3 (QueryPointerBufferSize), sent to test")
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
        let e = send(&mut p, h, &cmif(5, false, 3, &[])).unwrap_err();
        assert!(
            e.contains("control command 3 (QueryPointerBufferSize)"),
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
        let e = send(&mut p, t, &cmif(5, false, 4, &[0; 4])).unwrap_err();
        assert!(
            e.contains("control command 4 (CloneCurrentObjectEx)"),
            "{e}"
        );
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
        p.cpu.retired = 3 * 19_200_000 + 5;
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
        p.services.hid.next_sample = p.cpu.retired + hid::SAMPLE_PERIOD;
        p.handles.get(shm).unwrap().1
    }

    /// Advance the guest clock by one sampling period and let hid sample.
    fn sample(p: &mut Process) {
        p.cpu.retired += hid::SAMPLE_PERIOD;
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
        p.cpu.retired += 40 * hid::SAMPLE_PERIOD;
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
        // Activated, but only Handheld supported: refused, noted once.
        let hid = open(&mut p, b"hid");
        let aruid = PROCESS_ID.to_le_bytes();
        let mut rev = 5u64.to_le_bytes().to_vec();
        rev.extend(aruid);
        assert_eq!(
            send(&mut p, hid, &request(true, &[], None, 109, &rev)),
            Ok(0)
        );
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
