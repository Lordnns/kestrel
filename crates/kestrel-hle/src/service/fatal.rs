//! Fatal errors: "fatal:u" (Switchbrew, "Fatal services"), which libnx's
//! fatalThrow and fatalThrowWithPolicy send (`nx/source/services/fatal.c`).

use super::{Call, IpcError, Out};
use crate::Process;

impl Process {
    /// fatal:u ThrowFatalWithPolicy (Switchbrew, "Fatal services"): a u32
    /// result, a u32 FatalType and the process ID. ErrorReport (1) only
    /// files a report, so kestrel notes it and the program goes on; the
    /// other two show the error screen, which ends the program.
    pub(super) fn fatal(&mut self, call: &Call) -> Result<Out, IpcError> {
        match call.n {
            1 => {
                let result = u32::from_le_bytes(call.arg(0)?);
                let policy = u32::from_le_bytes(call.arg(4)?);
                call.u64(8)?;
                if !call.req.send_pid {
                    return Err(IpcError::Stop(
                        "fatal:u ThrowFatalWithPolicy without the process ID".into(),
                    ));
                }
                match policy {
                    1 => {
                        let what = crate::results::describe(result);
                        self.note(format!("fatal error report {what}"));
                        Ok(Out::empty())
                    }
                    0 | 2 => Err(IpcError::Fatal(result)),
                    _ => Err(IpcError::Stop(format!(
                        "fatal:u ThrowFatalWithPolicy with FatalType {policy}"
                    ))),
                }
            }
            _ => Err(call.unimplemented()),
        }
    }
}
