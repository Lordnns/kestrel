//! The service manager, "sm:" (Switchbrew, "Services API"): a client
//! registers its session, then asks for other services by name.

use super::{service_name, Call, IpcError, Out, SERVICES};
use crate::Process;

/// Results of the service manager (module 21), named as on Switchbrew's
/// "Error codes" page.
pub mod result {
    pub const INVALID_CLIENT: u32 = 0x415;
    pub const INVALID_SERVICE_NAME: u32 = 0xc15;
}

impl Process {
    /// RegisterClient (0) takes the process ID descriptor; GetServiceHandle
    /// (1) takes an 8-byte name and hands out a session. Asking before
    /// RegisterClient is InvalidClient (kestrel's choice: the "Error codes"
    /// name fits, and older libnx probed for 0x415 before registering).
    pub(super) fn sm(&mut self, call: &Call) -> Result<Out, IpcError> {
        match call.n {
            0 => {
                if !call.req.send_pid {
                    return Err(IpcError::Stop(
                        "sm: RegisterClient without the process ID descriptor it takes".into(),
                    ));
                }
                self.session(call.session).registered = true;
                Ok(Out::empty())
            }
            1 => {
                if !self.session(call.session).registered {
                    return Ok(Out::error(result::INVALID_CLIENT));
                }
                let raw: [u8; 8] = call.arg(0)?;
                let Some(name) = service_name(&raw) else {
                    return Ok(Out::error(result::INVALID_SERVICE_NAME));
                };
                match SERVICES.iter().find(|(n, _)| *n == name) {
                    Some(&(_, interface)) => Ok(Out::object(interface)),
                    None => Err(IpcError::Stop(format!("unimplemented service \"{name}\""))),
                }
            }
            _ => Err(call.unimplemented()),
        }
    }
}
