//! The service manager, "sm:" (Switchbrew, "Services API"): a client
//! registers its session, then asks for other services by name.

use super::{service_name, unimplemented, IpcError, Service, Session, SERVICES};
use crate::ipc::{Reply, Request};
use crate::kernel::{Kind, Object, ObjectId};
use crate::Process;

/// Results of the service manager (module 21), named as on Switchbrew's
/// "Error codes" page.
pub mod result {
    pub const INVALID_CLIENT: u32 = 0x415;
    pub const INVALID_SERVICE_NAME: u32 = 0xc15;
}

impl Process {
    /// RegisterClient (0) takes the process ID descriptor; GetServiceHandle
    /// (1) takes an 8-byte name and moves a session handle back. Asking
    /// before RegisterClient is InvalidClient (kestrel's choice: the "Error
    /// codes" name fits, and older libnx probed for 0x415 before
    /// registering).
    pub(super) fn sm(
        &mut self,
        id: ObjectId,
        n: u32,
        req: &Request,
        args: &[u8],
    ) -> Result<Reply, IpcError> {
        match n {
            0 => {
                if !req.send_pid {
                    return Err(IpcError::Stop(
                        "sm: RegisterClient without the process ID descriptor it takes".into(),
                    ));
                }
                self.session(id).registered = true;
                Ok(Reply::ok(Vec::new()))
            }
            1 => {
                if !self.session(id).registered {
                    return Ok(Reply::error(result::INVALID_CLIENT));
                }
                let Some(raw) = args.get(..8) else {
                    return Err(IpcError::Stop(
                        "sm: GetServiceHandle without the service name it takes".into(),
                    ));
                };
                let Some(name) = service_name(raw) else {
                    return Ok(Reply::error(result::INVALID_SERVICE_NAME));
                };
                let Some(&service) = SERVICES.iter().find(|s| s.name() == name) else {
                    return Err(IpcError::Stop(format!("unimplemented service \"{name}\"")));
                };
                let session = Object::Session(Session::new(service));
                Ok(match self.handles.insert(session, &[Kind::ClientSession]) {
                    Ok(hs) => Reply {
                        move_handles: hs,
                        ..Reply::ok(Vec::new())
                    },
                    Err(code) => Reply::error(code as u32),
                })
            }
            _ => Err(unimplemented(Service::Sm, n)),
        }
    }
}
