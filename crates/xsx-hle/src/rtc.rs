//! Xbox Series X Real-Time Clock (RTC) & Win32 Calendar/Time subsystem.
//!
//! Provides exact Gregorian calendar conversion between Unix timestamps, Windows
//! `FILETIME` (100-ns intervals since 1601-01-01 UTC), and `SYSTEMTIME`, with support
//! for live host wall-clock time, signed RTC offsets (`--rtc-offset`), and fixed
//! deterministic timestamps for reproducible tests.

use std::time::{SystemTime as StdSystemTime, UNIX_EPOCH};

use xsx_abi::gdk::{FileTime, SystemTime, TimeZoneInformation};

/// 100-nanosecond ticks between 1601-01-01 00:00:00 UTC and 1970-01-01 00:00:00 UTC.
pub const WINDOWS_EPOCH_OFFSET_TICKS: u64 = 116_444_736_000_000_000;
const TICKS_PER_MS: u64 = 10_000;
const TICKS_PER_SEC: u64 = 10_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// Real-Time Clock configuration for `GuestRuntime`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RtcConfig {
    /// Signed offset in seconds added to live UTC time (e.g. for timezone/clock testing).
    pub offset_seconds: i64,
    /// Optional fixed Unix timestamp in milliseconds (deterministic RTC mode).
    pub fixed_unix_ms: Option<i64>,
    /// Local timezone bias in minutes (`UTC = local + bias`; 0 = UTC).
    pub tz_bias_minutes: i32,
}

impl RtcConfig {
    /// Current guest UTC time expressed as Windows `FILETIME` (100-ns ticks since 1601-01-01).
    pub fn now_filetime_utc(&self) -> FileTime {
        let unix_100ns: i128 = if let Some(fixed_ms) = self.fixed_unix_ms {
            (fixed_ms as i128) * (TICKS_PER_MS as i128)
        } else {
            let dur = StdSystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            (dur.as_nanos() / 100) as i128
        };
        let shifted = unix_100ns
            + (self.offset_seconds as i128) * (TICKS_PER_SEC as i128)
            + (WINDOWS_EPOCH_OFFSET_TICKS as i128);
        FileTime::from_u64(shifted.max(0) as u64)
    }

    pub fn now_systemtime_utc(&self) -> SystemTime {
        filetime_to_systemtime(self.now_filetime_utc())
    }

    pub fn now_systemtime_local(&self) -> SystemTime {
        let utc = self.now_filetime_utc().to_u64() as i128;
        let local_ticks = utc - (self.tz_bias_minutes as i128) * 60 * (TICKS_PER_SEC as i128);
        filetime_to_systemtime(FileTime::from_u64(local_ticks.max(0) as u64))
    }

    pub fn timezone_info(&self) -> TimeZoneInformation {
        let mut tzi = TimeZoneInformation {
            bias: self.tz_bias_minutes,
            ..TimeZoneInformation::default()
        };
        let name = if self.tz_bias_minutes == 0 {
            "UTC"
        } else {
            "Local"
        };
        for (i, u) in name.encode_utf16().take(31).enumerate() {
            tzi.standard_name[i] = u;
            tzi.daylight_name[i] = u;
        }
        tzi
    }

    pub fn format_utc_iso8601(&self) -> String {
        let st = self.now_systemtime_utc();
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            st.w_year,
            st.w_month,
            st.w_day,
            st.w_hour,
            st.w_minute,
            st.w_second,
            st.w_milliseconds
        )
    }
}

/// Convert Windows `FILETIME` to `SYSTEMTIME` using exact proleptic Gregorian calendar arithmetic.
pub fn filetime_to_systemtime(ft: FileTime) -> SystemTime {
    let ticks = ft.to_u64();
    let total_ms = ticks / TICKS_PER_MS;
    let ms = (total_ms % 1000) as u16;
    let total_secs = (ticks / TICKS_PER_SEC) as i64;
    // Days relative to Unix epoch (1970-01-01)
    let unix_secs = total_secs - (WINDOWS_EPOCH_OFFSET_TICKS / TICKS_PER_SEC) as i64;
    let days = unix_secs.div_euclid(SECS_PER_DAY);
    let secs_of_day = unix_secs.rem_euclid(SECS_PER_DAY);

    let hour = (secs_of_day / 3600) as u16;
    let minute = ((secs_of_day % 3600) / 60) as u16;
    let second = (secs_of_day % 60) as u16;

    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was Thursday (4); Sunday = 0
    let dow = (days + 4).rem_euclid(7) as u16;

    SystemTime {
        w_year: year.clamp(1601, 30827) as u16,
        w_month: month as u16,
        w_day_of_week: dow,
        w_day: day as u16,
        w_hour: hour,
        w_minute: minute,
        w_second: second,
        w_milliseconds: ms,
    }
}

/// Convert `SYSTEMTIME` to Windows `FILETIME`. Returns `None` if date/time fields are out of range.
pub fn systemtime_to_filetime(st: SystemTime) -> Option<FileTime> {
    if st.w_year < 1601
        || !(1..=12).contains(&st.w_month)
        || !(1..=31).contains(&st.w_day)
        || st.w_hour > 23
        || st.w_minute > 59
        || st.w_second > 59
        || st.w_milliseconds > 999
    {
        return None;
    }
    let days = days_from_civil(st.w_year as i64, st.w_month as u32, st.w_day as u32);
    let unix_secs =
        days * SECS_PER_DAY + (st.w_hour as i64) * 3600 + (st.w_minute as i64) * 60 + (st.w_second as i64);
    let win_secs = unix_secs + (WINDOWS_EPOCH_OFFSET_TICKS / TICKS_PER_SEC) as i64;
    if win_secs < 0 {
        return None;
    }
    let ticks = (win_secs as u64) * TICKS_PER_SEC + (st.w_milliseconds as u64) * TICKS_PER_MS;
    Some(FileTime::from_u64(ticks))
}

/// Howard Hinnant's exact O(1) civil-date-from-days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m as u32, d as u32)
}

/// Howard Hinnant's exact O(1) days-since-1970-01-01 from civil date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m_adj = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * m_adj + 2) / 5 + (d as u64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_epoch_and_leap_day_roundtrip() {
        let unix0 = filetime_to_systemtime(FileTime::from_u64(WINDOWS_EPOCH_OFFSET_TICKS));
        assert_eq!(
            (unix0.w_year, unix0.w_month, unix0.w_day, unix0.w_day_of_week),
            (1970, 1, 1, 4) // Thursday
        );

        let leap = SystemTime {
            w_year: 2024,
            w_month: 2,
            w_day_of_week: 4, // Thursday
            w_day: 29,
            w_hour: 15,
            w_minute: 42,
            w_second: 19,
            w_milliseconds: 500,
        };
        let ft = systemtime_to_filetime(leap).unwrap();
        let back = filetime_to_systemtime(ft);
        assert_eq!(back, leap);
    }

    #[test]
    fn deterministic_rtc_and_offset() {
        let rtc = RtcConfig {
            offset_seconds: 3600,
            fixed_unix_ms: Some(1_700_000_000_000), // 2023-11-14T22:13:20.000Z
            tz_bias_minutes: 0,
        };
        assert_eq!(rtc.format_utc_iso8601(), "2023-11-14T23:13:20.000Z");
    }
}
