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
pub mod hid;
pub mod set;
pub mod sm;

use crate::ipc::{self, command_type, domain_kind, Reply, Request};
use crate::kernel::{Kind, Object, ObjectId};
use crate::svc::result;
use crate::Process;

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

/// A client session: the kernel object a session handle names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// The object requests go to; in a domain, object 1.
    pub object: Interface,
    /// Once the session is a domain: its objects, object ID n at n - 1.
    pub domain: Option<Vec<Option<Interface>>>,
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
        IpcError::Stop(format!(
            "unimplemented: {} command {}{name}",
            self.object.name(),
            self.n
        ))
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
            return Err(IpcError::Stop(format!(
                "unsupported: named port \"{}\" (kestrel has only \"sm:\")",
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
                if n != 0 || session.domain.is_some() {
                    return Err(stop(format!(
                        "unsupported: CMIF control command {n} ({}){}",
                        control_name(n),
                        if session.domain.is_some() {
                            " on a domain"
                        } else {
                            ""
                        }
                    )));
                }
                // ConvertCurrentObjectToDomain: the session's object becomes
                // object 1, kestrel's choice of ID.
                self.session(id).domain = Some(vec![Some(session.object)]);
                ipc::cmif_reply(&Reply::ok(1u32.to_le_bytes().to_vec()), None).map_err(stop)?
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
        let domain = self.session(id).domain.as_mut().expect("a domain");
        let Some(object) = slot.and_then(|i| domain.get(i).copied().flatten()) else {
            return Err(IpcError::Stop(format!(
                "a request to domain object {}, which does not exist",
                d.object_id
            )));
        };
        if d.kind == domain_kind::CLOSE {
            domain[slot.unwrap()] = None;
            return ipc::cmif_reply(&Reply::ok(Vec::new()), Some(&[])).map_err(IpcError::Stop);
        }
        if !d.in_objects.is_empty() {
            return Err(IpcError::Stop(format!(
                "unsupported: domain objects as arguments to {}",
                object.name()
            )));
        }
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
                let domain = self
                    .session(call.session)
                    .domain
                    .as_mut()
                    .expect("a domain");
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
        assert!(e.starts_with("unimplemented service \"nosuch\""), "{e}");
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
            e.contains("control command 0 (ConvertCurrentObjectToDomain) on a domain"),
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
    fn only_session_handles_take_requests() {
        let mut p = setup();
        let main = p.main_thread as u64;
        assert_eq!(send(&mut p, main, &[]), Ok(result::INVALID_HANDLE));
        assert_eq!(send(&mut p, 0, &[]), Ok(result::INVALID_HANDLE));
    }
}
