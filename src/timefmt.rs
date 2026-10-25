use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parses an RFC 3339 timestamp such as `2026-10-24T07:40:00.137632+00:00` into Unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse().ok() };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);

    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    let offset = match b.get(i) {
        None | Some(b'Z' | b'z') => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let o = num(i + 1, i + 3)? * 3600 + num(i + 4, i + 6)? * 60;
            if sign == b'-' { -o } else { o }
        }
        _ => return None,
    };

    Some(days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second - offset)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Compact duration: `2d 20h`, `4h 37m`, `12m`, `<1m`.
pub fn duration_short(secs: i64) -> String {
    let s = secs.max(0);
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        "<1m".into()
    }
}

pub fn ago(secs: i64) -> String {
    match secs.max(0) {
        0..=4 => "just now".into(),
        s @ 5..=59 => format!("{s}s ago"),
        s @ 60..=3599 => format!("{}m ago", s / 60),
        s => format!("{}h ago", s / 3600),
    }
}

/// Local wall-clock time: `14:40` when within a day, else `Tue 07:00`.
pub fn local_label(unix: i64, now: i64) -> Option<String> {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    let local = to_local(unix)?;
    let hm = format!("{:02}:{:02}", local.hour, local.minute);
    if unix - now < 86400 {
        return Some(hm);
    }
    let days = days_from_civil(local.year, local.month, local.day);
    Some(format!("{} {hm}", DAYS[days.rem_euclid(7) as usize]))
}

struct LocalTime {
    year: i64,
    month: i64,
    day: i64,
    hour: u32,
    minute: u32,
}

#[cfg(windows)]
fn to_local(unix: i64) -> Option<LocalTime> {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

    let ticks = (unix + 11_644_473_600) as u64 * 10_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    Some(LocalTime {
        year: local.wYear as i64,
        month: local.wMonth as i64,
        day: local.wDay as i64,
        hour: local.wHour as u32,
        minute: local.wMinute as u32,
    })
}

#[cfg(unix)]
fn to_local(unix: i64) -> Option<LocalTime> {
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some(LocalTime {
        year: tm.tm_year as i64 + 1900,
        month: tm.tm_mon as i64 + 1,
        day: tm.tm_mday as i64,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_api_timestamps() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-10-24T07:40:00.137632+00:00"), Some(1_792_827_600));
        assert_eq!(parse_rfc3339("2026-10-24T14:40:00+07:00"), Some(1_792_827_600));
        assert_eq!(parse_rfc3339("garbage"), None);
    }

    #[test]
    fn formats_durations() {
        assert_eq!(duration_short(30), "<1m");
        assert_eq!(duration_short(4 * 3600 + 37 * 60), "4h 37m");
        assert_eq!(duration_short(2 * 86400 + 20 * 3600 + 5), "2d 20h");
    }
}
