use std::time::{SystemTime, UNIX_EPOCH};
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
/// Compact duration: `4h 59m 24s`, `1m 4s`, `4s`.
pub fn duration(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}
/// Local wall clock: `2026-11-05 22:40:26`.
pub fn local_datetime(epoch: u64) -> String {
    local(epoch as i64, "%Y-%m-%d %H:%M:%S")
}

/// Epoch of local midnight `days - 1` calendar days before `now`.
///
/// Calendar arithmetic runs through `mktime`, so month, year and DST edges
/// normalize correctly instead of assuming fixed-length days.
pub fn local_midnight_days_ago(now: i64, days: u32) -> Option<i64> {
    let stamp = now as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r initializes our owned tm from a valid time_t.
    unsafe {
        if libc::localtime_r(&stamp, tm.as_mut_ptr()).is_null() {
            return None;
        }
        let mut tm = tm.assume_init();
        tm.tm_mday -= days.saturating_sub(1) as libc::c_int;
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        tm.tm_isdst = -1;
        let midnight = libc::mktime(&mut tm);
        (midnight != -1).then_some(midnight as i64)
    }
}

/// Local RFC3339 timestamp with a numeric offset.
pub fn rfc3339(epoch: u64) -> String {
    let Ok(epoch) = i64::try_from(epoch) else {
        return "?".into();
    };
    let mut text = local(epoch, "%Y-%m-%dT%H:%M:%S%z");
    if text.len() >= 5 {
        text.insert(text.len() - 2, ':');
    }
    text
}
pub fn local(epoch: i64, format: &str) -> String {
    let Ok(format) = std::ffi::CString::new(format) else {
        return "?".into();
    };
    let stamp = epoch as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    let mut bytes = [0u8; 128];
    // SAFETY: localtime_r initializes our owned tm; strftime is bounded by bytes.len().
    unsafe {
        if libc::localtime_r(&stamp, tm.as_mut_ptr()).is_null() {
            return "?".into();
        }
        let len = libc::strftime(
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            format.as_ptr(),
            tm.as_ptr(),
        );
        String::from_utf8_lossy(&bytes[..len]).into_owned()
    }
}
pub fn parse_iso(text: &str) -> Option<i64> {
    let date = text.get(..19)?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let c = std::ffi::CString::new(date).ok()?;
    // SAFETY: both C strings are terminated; tm points to initialized owned storage.
    unsafe {
        let end = libc::strptime(c.as_ptr(), c"%Y-%m-%dT%H:%M:%S".as_ptr(), &mut tm);
        if end.is_null() || *end != 0 {
            return None;
        }
        let utc = libc::timegm(&mut tm) as i64;
        let suffix = text.get(19..)?;
        let zone = suffix
            .find(['Z', '+', '-'])
            .map(|i| &suffix[i..])
            .unwrap_or("");
        if zone == "Z" {
            return Some(utc);
        }
        if zone.len() != 6 || zone.as_bytes()[3] != b':' {
            return None;
        }
        let h = zone.get(1..3)?.parse::<i64>().ok()?;
        let m = zone.get(4..6)?.parse::<i64>().ok()?;
        if h > 23 || m > 59 {
            return None;
        }
        Some(utc - if zone.starts_with('+') { 1 } else { -1 } * (h * 3600 + m * 60))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_datetime_and_midnight_cutoffs_follow_the_calendar() {
        assert_eq!(local_datetime(0), local(0, "%Y-%m-%d %H:%M:%S"));
        assert_eq!(local_datetime(0).len(), 19);
        let now = 1_793_930_000_i64;
        let midnight = local_midnight_days_ago(now, 1).unwrap();
        assert_eq!(local(midnight, "%H:%M:%S"), "00:00:00");
        assert_eq!(local(midnight, "%Y-%m-%d"), local(now, "%Y-%m-%d"));
        assert!(midnight <= now);
        let earlier = local_midnight_days_ago(now, 2).unwrap();
        assert_eq!(local(earlier, "%H:%M:%S"), "00:00:00");
        // One calendar day apart, DST included.
        assert!((22 * 3600..=26 * 3600).contains(&(midnight - earlier)));
        // The first of a month walks back into the previous month.
        let first = local_midnight_days_ago(now, 1).unwrap();
        let previous_month = local_midnight_days_ago(first, 2).unwrap();
        assert_eq!(local(previous_month, "%H:%M:%S"), "00:00:00");
        assert!(previous_month < midnight);
    }

    #[test]
    fn iso_offsets_and_invalid_input() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_iso("1970-01-01T00:00:00.123Z"), Some(0));
        assert_eq!(parse_iso("bad"), None);
    }
}
