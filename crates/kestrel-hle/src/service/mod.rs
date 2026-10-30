//! Named ports, client sessions and the services behind them.
//!
//! A client connects to a named port (svcConnectToNamedPort) or receives a
//! session handle from another service, then sends requests over the
//! session with svcSendSyncRequest (Switchbrew, "SVC"). The servers are
//! written in Rust and answer at once, so a request never blocks.
//!
//! The only named port is "sm:", the service manager, which hands out
//! sessions to the other services by name (Switchbrew, "Services API").
//! A port, service or command kestrel does not implement stops the process
//! with its name and number, never with a made-up answer.

use crate::ipc::{self, command_type, Reply, Request};
use crate::kernel::{Kind, Object, ObjectId};
use crate::svc::result;
use crate::Process;

pub mod set;
pub mod sm;

/// Longest port name read, with its NUL: kestrel's choice, as long as the
/// longest name a service can have (8 bytes) with room to spare.
const PORT_NAME_MAX: u64 = 12;

/// A service kestrel implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    /// "sm:", the service manager.
    Sm,
    /// "set:sys", system settings.
    SetSys,
    /// "test": command 0 echoes its arguments, command 1 fails with
    /// result 0x1234. For unit tests of the session path.
    #[cfg(test)]
    Test,
}

/// The services sm: hands out by name.
const SERVICES: &[Service] = &[
    Service::SetSys,
    #[cfg(test)]
    Service::Test,
];

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Service::Sm => "sm:",
            Service::SetSys => "set:sys",
            #[cfg(test)]
            Service::Test => "test",
        }
    }

    /// The service also takes TIPC requests. Switchbrew's "Services API":
    /// sm: is a TIPC interface from 12.0.0; libnx still sends it CMIF
    /// requests on every version, so it takes both.
    fn speaks_tipc(self) -> bool {
        self == Service::Sm
    }

    /// The name of command `n`, when it is known but not implemented.
    fn command_name(self, n: u32) -> Option<&'static str> {
        match (self, n) {
            (Service::Sm, 2) => Some("RegisterService"),
            (Service::Sm, 3) => Some("UnregisterService"),
            (Service::Sm, 4) => Some("DetachClient"),
            (Service::SetSys, 3) => Some("GetFirmwareVersion, removed on Switch 2"),
            _ => None,
        }
    }
}

/// A client session: the kernel object a session handle names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub service: Service,
    /// The client sent a close request; later requests fail.
    pub closed: bool,
    /// sm: only: RegisterClient came over this session.
    pub registered: bool,
}

impl Session {
    pub fn new(service: Service) -> Self {
        Session {
            service,
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
        let session = Object::Session(Session::new(Service::Sm));
        Ok(self.handles.insert(session, &[Kind::ClientSession])?[0])
    }

    fn session(&mut self, id: ObjectId) -> &mut Session {
        match self.handles.object_mut(id) {
            Object::Session(s) => s,
            o => unreachable!("a session handle names {o:?}"),
        }
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
        let service = session.service;
        let tlr = self.current_thread().tlr;
        let msg = self
            .mem
            .read_vec(tlr, ipc::MESSAGE_SIZE)
            .map_err(|e| IpcError::Stop(format!("cannot read the IPC message: {e}")))?;
        let stop = |what: String| IpcError::Stop(format!("{what}, sent to {}", service.name()));
        let req = ipc::parse(&msg).map_err(stop)?;
        if !req.copy_handles.is_empty() || !req.move_handles.is_empty() {
            return Err(stop("unsupported: handles in an IPC request".into()));
        }
        let reply = match req.command_type {
            command_type::CLOSE => {
                self.session(id).closed = true;
                return Ok(());
            }
            command_type::REQUEST | command_type::REQUEST_WITH_CONTEXT => {
                let cmif = ipc::cmif_request(&req).map_err(stop)?;
                let reply = self.serve(id, cmif.command, &req, &cmif.args)?;
                ipc::cmif_reply(&reply)
            }
            command_type::CONTROL | command_type::CONTROL_WITH_CONTEXT => {
                let n = ipc::cmif_request(&req).map_err(stop)?.command;
                return Err(stop(format!(
                    "unsupported: CMIF control command {n} ({})",
                    control_name(n)
                )));
            }
            t if t >= command_type::TIPC && service.speaks_tipc() => {
                let n = (t - command_type::TIPC) as u32;
                let reply = self.serve(id, n, &req, &req.raw)?;
                ipc::tipc_reply(&reply)
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
        }
        .map_err(stop)?;
        self.mem
            .write_bytes(tlr, &reply)
            .map_err(|e| IpcError::Stop(format!("cannot write the IPC reply: {e}")))
    }

    /// Answer command `n` on session `id`; `args` are the command's input
    /// arguments.
    fn serve(
        &mut self,
        id: ObjectId,
        n: u32,
        req: &Request,
        args: &[u8],
    ) -> Result<Reply, IpcError> {
        match self.session(id).service {
            Service::Sm => self.sm(id, n, req, args),
            Service::SetSys => self.set_sys(n, req),
            #[cfg(test)]
            Service::Test => match n {
                0 => Ok(Reply::ok(args.to_vec())),
                1 => Ok(Reply::error(0x1234)),
                _ => Err(unimplemented(Service::Test, n)),
            },
        }
    }
}

impl Process {
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

/// Stop for command `n` of `service`, which kestrel does not implement.
fn unimplemented(service: Service, n: u32) -> IpcError {
    let name = service
        .command_name(n)
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    IpcError::Stop(format!(
        "unimplemented: {} command {n}{name}",
        service.name()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{CMIF_IN_MAGIC, CMIF_OUT_MAGIC};
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
        // One copied handle before the raw data.
        let mut with_handle = words(&[4, 8 | 1 << 31, 1 << 1, 0x1234]);
        with_handle.extend(&cmif(4, false, 0, &[])[8..]);
        let e = send(&mut p, h, &with_handle).unwrap_err();
        assert!(e.contains("handles in an IPC request"), "{e}");
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

    #[test]
    fn only_session_handles_take_requests() {
        let mut p = setup();
        let main = p.main_thread as u64;
        assert_eq!(send(&mut p, main, &[]), Ok(result::INVALID_HANDLE));
        assert_eq!(send(&mut p, 0, &[]), Ok(result::INVALID_HANDLE));
    }
}
