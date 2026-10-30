//! Time: "time:u" (Switchbrew, "Glue services", time:a, time:r, time:u).
//! The commands libnx's timeInitialize and __libnx_init_time send
//! (`nx/source/services/time.c`, `nx/source/runtime/newlib.c`): the four
//! clocks and the time zone service, the shared memory the clocks are read
//! from, and the conversion of the boot time to a calendar time.
//!
//! What time it is was the owner's choice (`firmware::BOOT_TIME`,
//! `firmware::LOCATION_NAME`): the system clocks start at a fixed time and
//! run with the process clock, and the time zone is UTC.

use super::{Call, Interface, IpcError, Out};
use crate::firmware;
use crate::kernel::{Kind, Object};
use crate::Process;
use kestrel_core::Perm;

/// The size of the shared memory, as libnx maps it.
pub const SHARED_MEMORY_SIZE: u64 = 0x1000;
/// The process clock's ticks in a second ("SVC", GetSystemTick).
const TICKS_PER_SECOND: u64 = 19_200_000;
/// The clock source ID every SteadyClockTimePoint carries: kestrel's
/// choice of UUID, its name, so it cannot be mistaken for a console's.
pub const SOURCE_ID: [u8; 16] = *b"kestrel\0\0\0\0\0\0\0\0\0";
/// A LocationName holds up to 0x24 bytes ("Glue services").
const LOCATION_NAME_SIZE: usize = 0x24;

/// The steady clock's time point: whole seconds of the process clock. The
/// steady clock starts at 0 when the process starts (kestrel's choice).
fn steady_seconds(p: &Process) -> i64 {
    (p.cpu.retired / TICKS_PER_SECOND) as i64
}

/// A SystemClockContext: the clock's epoch, then the SteadyClockTimePoint
/// it is counted from. Every system clock has the same one: BOOT_TIME at
/// steady time 0.
fn system_clock_context() -> [u8; 0x20] {
    let mut c = [0u8; 0x20];
    c[..8].copy_from_slice(&firmware::BOOT_TIME.to_le_bytes());
    c[0x10..].copy_from_slice(&SOURCE_ID);
    c
}

/// The shared memory's contents. Switchbrew does not describe them; this
/// is the layout libnx reads (`_timeReadClockFromSharedMem`): at 0x00 the
/// steady clock (a base time in nanoseconds and the source ID), at 0x38
/// the context of the local system clock, which libnx also reads for the
/// user clock, and at 0x80 the network system clock's. Each is a u32
/// counter, 4 bytes of padding and two copies of the value; libnx reads
/// the copy the counter's low bit picks. kestrel never changes them, so
/// the counter stays 0 and only the first copy is written.
pub fn shared_memory_contents() -> Vec<u8> {
    let mut m = vec![0u8; SHARED_MEMORY_SIZE as usize];
    // The steady clock's base time is 0: it counts from process start.
    m[0x10..0x20].copy_from_slice(&SOURCE_ID);
    m[0x40..0x60].copy_from_slice(&system_clock_context());
    m[0x88..0xa8].copy_from_slice(&system_clock_context());
    m
}

/// The UTC calendar time of `t`, as ToCalendarTime's output: a
/// CalendarTime (year, month, day, hour, minute, second, padding) and a
/// CalendarAdditionalInfo (day of the week and of the year, both 0-based,
/// the time zone's name, DST and its offset from UTC). Switchbrew calls
/// only the last two 0-based; month and day count from 1, as calendar
/// dates do. None for a year CalendarTime's u16 cannot hold.
pub fn calendar_time(t: i64) -> Option<[u8; 0x20]> {
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    // Days since 1970-01-01 to a date (the proleptic Gregorian calendar,
    // in 400-year eras that start on March 1).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    let year = u16::try_from(year).ok()?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    const BEFORE: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let yday = BEFORE[month as usize - 1] + (leap && month > 2) as i64 + day - 1;
    // 1970-01-01 was a Thursday; Sunday is day 0.
    let wday = (days + 4).rem_euclid(7);

    let mut out = [0u8; 0x20];
    out[..2].copy_from_slice(&year.to_le_bytes());
    out[2..7].copy_from_slice(&[
        month as u8,
        day as u8,
        (secs / 3600) as u8,
        (secs / 60 % 60) as u8,
        (secs % 60) as u8,
    ]);
    out[8..0xc].copy_from_slice(&(wday as u32).to_le_bytes());
    out[0xc..0x10].copy_from_slice(&(yday as u32).to_le_bytes());
    // The time zone name (8 bytes), then DST 0 and offset 0 stay zero.
    out[0x10..0x10 + firmware::LOCATION_NAME.len()]
        .copy_from_slice(firmware::LOCATION_NAME.as_bytes());
    Some(out)
}

impl Process {
    pub(super) fn time(&mut self, call: &Call) -> Result<Out, IpcError> {
        use Interface::*;
        match (call.object, call.n) {
            // GetStandardUserSystemClock, GetStandardNetworkSystemClock,
            // GetStandardSteadyClock, GetTimeZoneService,
            // GetStandardLocalSystemClock.
            (TimeU, 0 | 1 | 4) => Ok(Out::object(SystemClock)),
            (TimeU, 2) => Ok(Out::object(SteadyClock)),
            (TimeU, 3) => Ok(Out::object(TimeZoneService)),
            // GetSharedMemoryNativeHandle: a copy handle to the one shared
            // memory, read-only for the process.
            (TimeU, 20) => {
                let id = match self.services.time_shared_memory {
                    Some(id) => id,
                    None => {
                        let id = self.create_shared_memory(SHARED_MEMORY_SIZE, Perm::R);
                        if let Object::SharedMemory { contents, .. } = self.handles.object_mut(id) {
                            *contents = shared_memory_contents();
                        }
                        self.services.time_shared_memory = Some(id);
                        id
                    }
                };
                Ok(match self.handles.add_handle(id, Kind::SharedMemory) {
                    Ok(h) => Out::copy(h),
                    Err(code) => Out::error(code as u32),
                })
            }
            // GetCurrentTime: the context's epoch plus the steady clock, as
            // libnx computes it from the shared memory.
            (SystemClock, 0) => {
                let now = firmware::BOOT_TIME + steady_seconds(self);
                Ok(Out::ok(now.to_le_bytes().to_vec()))
            }
            (SystemClock, 2) => Ok(Out::ok(system_clock_context().to_vec())),
            // GetCurrentTimePoint: a SteadyClockTimePoint.
            (SteadyClock, 0) => {
                let mut point = steady_seconds(self).to_le_bytes().to_vec();
                point.extend(SOURCE_ID);
                Ok(Out::ok(point))
            }
            (TimeZoneService, 0) => {
                let mut name = firmware::LOCATION_NAME.as_bytes().to_vec();
                name.resize(LOCATION_NAME_SIZE, 0);
                Ok(Out::ok(name))
            }
            // ToCalendarTimeWithMyRule: the device's rule is UTC.
            (TimeZoneService, 101) => {
                let t = call.u64(0)? as i64;
                let out = calendar_time(t).ok_or_else(|| {
                    IpcError::Stop(format!(
                        "ITimeZoneService ToCalendarTimeWithMyRule of {t}: the year does not \
                         fit CalendarTime's u16"
                    ))
                })?;
                Ok(Out::ok(out.to_vec()))
            }
            _ => Err(call.unimplemented()),
        }
    }
}
