//! The settings services (Switchbrew, "Settings services"). Only
//! set:sys's GetFirmwareVersion2 so far.

use super::{unimplemented, IpcError, Service};
use crate::firmware;
use crate::ipc::{Reply, Request};
use crate::Process;

/// The FirmwareVersion structure ("Settings services") for the version
/// kestrel reports. ReleaseNumber and Revision are 0; Platform,
/// DisplayVersion and DisplayName are kestrel's own text, so the answer
/// cannot be mistaken for a console's.
pub fn firmware_version() -> [u8; 0x100] {
    let mut v = [0u8; 0x100];
    v[..3].copy_from_slice(&[firmware::MAJOR, firmware::MINOR, firmware::MICRO]);
    let version = format!(
        "{}.{}.{}",
        firmware::MAJOR,
        firmware::MINOR,
        firmware::MICRO
    );
    let mut put = |at: usize, len: usize, text: &str| {
        // Each field keeps a NUL at its end.
        assert!(text.len() < len);
        v[at..at + text.len()].copy_from_slice(text.as_bytes());
    };
    put(0x08, 0x20, "kestrel");
    put(0x68, 0x18, &version);
    put(0x80, 0x80, &format!("kestrel {version}"));
    v
}

impl Process {
    /// set:sys. GetFirmwareVersion2 (4) writes the FirmwareVersion through
    /// its fixed-size output pointer (buffer type 0x1A). GetFirmwareVersion
    /// (3) was removed on Switch 2 ("Settings services"), so it stops.
    pub(super) fn set_sys(&mut self, n: u32, req: &Request) -> Result<Reply, IpcError> {
        match n {
            4 => {
                self.write_receive(req, 0, &firmware_version())?;
                Ok(Reply::ok(Vec::new()))
            }
            _ => Err(unimplemented(Service::SetSys, n)),
        }
    }
}
