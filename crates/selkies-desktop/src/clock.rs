//! Local time for the panel clock and the calendar, through the C library so
//! the container's time zone and locale are honored.

use std::ffi::{CStr, CString};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Broken-down local time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    /// Full year.
    pub year: i32,
    /// Month, 1 to 12.
    pub month: u32,
    /// Day of the month, 1 to 31.
    pub day: u32,
    /// Day of the week, 0 being Sunday.
    pub weekday: u32,
    /// Hour, 0 to 23.
    pub hour: u32,
    /// Minute.
    pub minute: u32,
    /// Second.
    pub second: u32,
}

unsafe extern "C" {
    fn tzset();
}

/// The current local time.
pub fn now() -> LocalTime {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    local(secs)
}

/// A Unix time as the C library's `time_t`, which libc deprecates naming on
/// musl while the alias moves to 64 bits; the cast is what either width needs.
#[allow(deprecated)]
fn time_t(secs: i64) -> libc::time_t {
    secs as libc::time_t
}

/// Breaks a Unix time down in the local time zone.
///
/// # Arguments
/// * `secs` - Seconds since the epoch.
pub fn local(secs: i64) -> LocalTime {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        tzset();
        let secs = time_t(secs);
        libc::localtime_r(&secs, &mut tm);
    }
    LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        weekday: tm.tm_wday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    }
}

/// Formats the current local time with a strftime pattern.
///
/// # Arguments
/// * `format` - The strftime pattern.
pub fn format_now(format: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_time(secs, format)
}

/// Formats a Unix time in the local time zone with a strftime pattern.
///
/// # Arguments
/// * `secs` - Seconds since the epoch.
/// * `format` - The strftime pattern.
pub fn format_time(secs: i64, format: &str) -> String {
    let Ok(c_format) = CString::new(format) else {
        return String::new();
    };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let mut buf = [0u8; 256];
    let written = unsafe {
        tzset();
        let secs = time_t(secs);
        libc::localtime_r(&secs, &mut tm);
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            c_format.as_ptr(),
            &tm,
        )
    };
    if written == 0 {
        return String::new();
    }
    CStr::from_bytes_with_nul(&buf[..=written])
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// The instant the wall clock next reaches a whole minute.
///
/// # Arguments
/// * `now` - The current instant, paired with the current wall clock.
pub fn next_minute(now: Instant) -> Instant {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let into_minute = Duration::from_nanos((since_epoch.as_nanos() % 60_000_000_000) as u64);
    now + (Duration::from_secs(60) - into_minute) + Duration::from_millis(20)
}

/// Days in a month of a year.
///
/// # Arguments
/// * `year` - Full year.
/// * `month` - Month, 1 to 12.
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Day of the week of a date, 0 being Sunday.
///
/// # Arguments
/// * `year` - Full year.
/// * `month` - Month, 1 to 12.
/// * `day` - Day of the month.
pub fn weekday(year: i32, month: u32, day: u32) -> u32 {
    let (y, m) = if month < 3 {
        (year - 1, month as i32 + 12)
    } else {
        (year, month as i32)
    };
    let k = y.rem_euclid(100);
    let j = y.div_euclid(100);
    let h = (day as i32 + (13 * (m + 1)) / 5 + k + k / 4 + j / 4 + 5 * j).rem_euclid(7);
    ((h + 6) % 7) as u32
}

/// Localized full month name.
///
/// # Arguments
/// * `month` - Month, 1 to 12.
pub fn month_name(month: u32) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_mon = (month.clamp(1, 12) - 1) as i32;
    tm.tm_mday = 1;
    tm.tm_year = 100;
    let mut buf = [0u8; 64];
    let format = CString::new("%B").expect("static string");
    let written = unsafe {
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            format.as_ptr(),
            &tm,
        )
    };
    if written == 0 {
        return String::new();
    }
    CStr::from_bytes_with_nul(&buf[..=written])
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Localized abbreviated weekday names, Sunday first.
pub fn weekday_names() -> [String; 7] {
    let format = CString::new("%a").expect("static string");
    let mut names: [String; 7] = Default::default();
    for (i, name) in names.iter_mut().enumerate() {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_wday = i as i32;
        let mut buf = [0u8; 64];
        let written = unsafe {
            libc::strftime(
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                format.as_ptr(),
                &tm,
            )
        };
        if written > 0 {
            *name = CStr::from_bytes_with_nul(&buf[..=written])
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_math() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(weekday(2026, 10, 22), 4);
        assert_eq!(weekday(2000, 1, 1), 6);
        assert_eq!(weekday(1970, 1, 1), 4);
    }

    #[test]
    fn formats_epoch() {
        unsafe { libc::setenv(c"TZ".as_ptr(), c"UTC".as_ptr(), 1) };
        assert_eq!(format_time(0, "%Y-%m-%d %H:%M"), "1970-01-01 00:00");
        let t = local(86400 * 365);
        assert_eq!((t.year, t.month, t.day), (1971, 1, 1));
    }
}
