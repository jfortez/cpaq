use std::time::{SystemTime, UNIX_EPOCH};
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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
    fn iso_offsets_and_invalid_input() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_iso("1970-01-01T00:00:00.123Z"), Some(0));
        assert_eq!(parse_iso("bad"), None);
    }
}
