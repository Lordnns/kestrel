//! The applet manager (Switchbrew, "Applet Manager services"): "appletOE",
//! through which an application reaches its applet interfaces, and "apm",
//! performance modes. These are the commands libnx's appletInitialize sends
//! for an application (`nx/source/services/applet.c`, `apm.c`).
//!
//! kestrel runs one application that is always in focus and never
//! suspended, so no applet message ever arrives and the events stay
//! unsignaled.

use super::{Call, Interface, IpcError, Out, State};
use crate::kernel::{Kind, ObjectId, PROCESS_ID};
use crate::svc::handle;
use crate::Process;

/// OperationMode Console (docked): kestrel's choice for a desktop.
pub const OPERATION_MODE: u8 = 1;
/// PerformanceMode Boost, the mode that goes with Console.
pub const PERFORMANCE_MODE: u32 = 1;
/// FocusState InFocus.
pub const FOCUS_STATE: u8 = 1;
/// ReceiveMessage with no message waiting ("Applet Manager services").
pub const NO_MESSAGE: u32 = 0x680;

impl Process {
    /// A copy handle to the event `slot` holds, made on first use.
    pub(super) fn event_handle(&mut self, slot: fn(&mut State) -> &mut Option<ObjectId>) -> Out {
        let id = match *slot(&mut self.services) {
            Some(id) => id,
            None => {
                let id = self.service_event();
                *slot(&mut self.services) = Some(id);
                id
            }
        };
        match self.handles.add_handle(id, Kind::ReadableEvent) {
            Ok(h) => Out::copy(h),
            Err(code) => Out::error(code as u32),
        }
    }

    /// apm: OpenSession (0) and GetPerformanceMode (1). Switchbrew notes
    /// that official Switch 2 software no longer uses apm; libnx opens it
    /// for every application.
    pub(super) fn apm(&mut self, call: &Call) -> Result<Out, IpcError> {
        match (call.object, call.n) {
            (Interface::Apm, 0) => Ok(Out::object(Interface::ApmSession)),
            (Interface::Apm, 1) => Ok(Out::ok(PERFORMANCE_MODE.to_le_bytes().to_vec())),
            _ => Err(call.unimplemented()),
        }
    }

    pub(super) fn am(&mut self, call: &Call) -> Result<Out, IpcError> {
        use Interface::*;
        let ok = || Ok(Out::empty());
        match (call.object, call.n) {
            // OpenApplicationProxy: a reserved u64, the process ID and a
            // copy of the current process handle.
            (AppletOE, 0) => {
                call.u64(0)?;
                let process = call.req.copy_handles.first().map(|&h| h as u64);
                if !call.req.send_pid || process != Some(handle::CURRENT_PROCESS) {
                    return Err(IpcError::Stop(
                        "appletOE OpenApplicationProxy without the process ID and the \
                         current-process handle it takes"
                            .into(),
                    ));
                }
                Ok(Out::object(ApplicationProxy))
            }
            (ApplicationProxy, n) => {
                let object = match n {
                    0 => CommonStateGetter,
                    1 => SelfController,
                    2 => WindowController,
                    3 => AudioController,
                    4 => DisplayController,
                    11 => LibraryAppletCreator,
                    20 => ApplicationFunctions,
                    1000 => DebugFunctions,
                    _ => return Err(call.unimplemented()),
                };
                Ok(Out::object(object))
            }
            (CommonStateGetter, 0) => Ok(self.event_handle(|s| &mut s.applet_message_event)),
            (CommonStateGetter, 1) => Ok(Out::error(NO_MESSAGE)),
            (CommonStateGetter, 5) => Ok(Out::ok(vec![OPERATION_MODE])),
            (CommonStateGetter, 6) => Ok(Out::ok(PERFORMANCE_MODE.to_le_bytes().to_vec())),
            (CommonStateGetter, 9) => Ok(Out::ok(vec![FOCUS_STATE])),
            // SetOperationModeChangedNotification,
            // SetPerformanceModeChangedNotification,
            // SetOutOfFocusSuspendingEnabled: a u8 flag. Neither mode nor
            // focus ever changes, so there is nothing to notify.
            (SelfController, 11 | 12 | 16) => call.u8(0).and_then(|_| ok()),
            // SetFocusHandlingMode: three u8 flags.
            (SelfController, 13) => call.arg::<3>(0).and_then(|_| ok()),
            (SelfController, 91) => Ok(self.event_handle(|s| &mut s.suspended_tick_event)),
            // GetAppletResourceUserId: the process ID, kestrel's choice.
            (WindowController, 1) => Ok(Out::ok(PROCESS_ID.to_le_bytes().to_vec())),
            (WindowController, 10) => ok(), // AcquireForegroundRights
            // NotifyRunning: official software ignores the u8; kestrel
            // answers 0.
            (ApplicationFunctions, 40) => Ok(Out::ok(vec![0])),
            _ => Err(call.unimplemented()),
        }
    }
}
