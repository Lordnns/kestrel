//! Power state: "psm" (Switchbrew, "PTM services"), the battery and charger
//! state libnx's psm functions ask for (`nx/source/services/psm.c`).
//!
//! kestrel runs docked (am's OperationMode Console), so it reports what a
//! docked console on its charger reports, kestrel's choice: a full battery,
//! a charger with enough power, charging enabled, a good battery voltage.
//! Nothing changes, so the state-change event is never signaled.

use super::{Call, Interface, IpcError, Out};
use crate::Process;

/// GetBatteryChargePercentage: full.
pub const CHARGE_PERCENT: u32 = 100;
/// ChargerType EnoughPower.
pub const CHARGER_TYPE: u32 = 1;
/// BatteryVoltageState Good.
pub const VOLTAGE_STATE: u32 = 3;

impl Process {
    pub(super) fn psm(&mut self, call: &Call) -> Result<Out, IpcError> {
        use Interface::*;
        let ok = |v: &[u8]| Ok(Out::ok(v.to_vec()));
        match (call.object, call.n) {
            (Psm, 0) => ok(&CHARGE_PERCENT.to_le_bytes()),
            (Psm, 1) => ok(&CHARGER_TYPE.to_le_bytes()),
            // IsBatteryChargingEnabled, IsEnoughPowerSupplied.
            (Psm, 4 | 14) => ok(&[1]),
            (Psm, 7) => Ok(Out::object(PsmSession)),
            (Psm, 12) => ok(&VOLTAGE_STATE.to_le_bytes()),
            // GetRawBatteryChargePercentage, GetBatteryAgePercentage: a
            // double; a new, full battery.
            (Psm, 13 | 15) => ok(&100f64.to_le_bytes()),
            // BindStateChangeEvent: an event nothing signals.
            (PsmSession, 0) => Ok(self.event_handle(|s| &mut s.psm_state_event)),
            // UnbindStateChangeEvent, and the three bools that choose which
            // changes signal it.
            (PsmSession, 1) => Ok(Out::empty()),
            (PsmSession, 2..=4) => {
                call.u8(0)?;
                Ok(Out::empty())
            }
            _ => Err(call.unimplemented()),
        }
    }
}
