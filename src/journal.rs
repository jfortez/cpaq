//! Append-only JSONL journal for auto runs and keep-alive pings.
//!
//! One record per line, one previous file kept. Every write is best effort: a
//! journal problem must never block a ping or change an exit code.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use serde_json::{Map, Value};

use crate::paths::Paths;

/// Rotate once the journal grew past this size.
pub(crate) const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Suffix of the single kept previous journal.
const ROTATED_SUFFIX: &str = ".1";
/// Longest error text kept in one record.
const MAX_ERROR_CHARS: usize = 200;

pub(crate) fn path(paths: &Paths) -> PathBuf {
    paths.auto_log_file()
}

/// Whether systemd or a human started this run.
pub(crate) fn trigger() -> &'static str {
    trigger_from(std::env::var_os("INVOCATION_ID").as_deref())
}

/// Classifies one `INVOCATION_ID` lookup: systemd sets it for a unit-triggered
/// run, and a manual run leaves it empty. Pure, so tests never read ambient env.
pub(crate) fn trigger_from(invocation: Option<&std::ffi::OsStr>) -> &'static str {
    match invocation {
        Some(value) if !value.is_empty() => "systemd",
        _ => "manual",
    }
}

/// Every record carries the local timestamp and its epoch second.
pub(crate) fn base(event: &str, now: u64) -> Map<String, Value> {
    let mut record = Map::new();
    record.insert("ts".into(), Value::String(crate::time::rfc3339(now)));
    record.insert("ts_epoch".into(), Value::from(now));
    record.insert("event".into(), Value::String(event.into()));
    record
}

/// One-line, length-capped error text; control characters never reach the log.
pub(crate) fn sanitize_error(message: &str) -> String {
    let mut text = String::new();
    let mut kept = 0;
    for character in message.chars() {
        let character = match character {
            '\n' | '\r' | '\t' => ' ',
            character if character.is_control() => continue,
            character => character,
        };
        if kept == MAX_ERROR_CHARS {
            break;
        }
        text.push(character);
        kept += 1;
    }
    text.trim().to_string()
}

/// Appends records as JSONL lines, rotating a journal that outgrew the cap.
pub(crate) fn append(path: &Path, records: &[Value]) -> io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    rotate(path)?;
    let mut file = open_private(path)?;
    let mut bytes = Vec::new();
    for record in records {
        bytes.extend(serde_json::to_vec(record).map_err(io::Error::other)?);
        bytes.push(b'\n');
    }
    file.write_all(&bytes)
}

/// Journal failures are warned once and never change the caller's outcome.
pub(crate) fn append_or_warn(path: &Path, records: &[Value]) {
    if records.is_empty() {
        return;
    }
    if append(path, records).is_err() {
        eprintln!("warning: cannot write the auto journal");
    }
}

fn open_private(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn rotate(path: &Path) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_BYTES => {
            let mut rotated = path.as_os_str().to_owned();
            rotated.push(ROTATED_SUFFIX);
            fs::rename(path, Path::new(&rotated))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Sandbox;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn trigger_distinguishes_systemd_from_a_manual_run() {
        use std::ffi::OsStr;
        assert_eq!(trigger_from(None), "manual");
        assert_eq!(trigger_from(Some(OsStr::new(""))), "manual");
        assert_eq!(
            trigger_from(Some(OsStr::new("c0ffee-invocation"))),
            "systemd"
        );
        // The ambient lookup is that same classifier.
        assert_eq!(
            trigger(),
            trigger_from(std::env::var_os("INVOCATION_ID").as_deref())
        );
    }

    #[test]
    fn error_text_stays_on_one_bounded_line() {
        let message = "Auth directory not found.\nTried: /a,/b.\tSet --auth-dir.";
        assert_eq!(
            sanitize_error(message),
            "Auth directory not found. Tried: /a,/b. Set --auth-dir."
        );
        assert_eq!(
            sanitize_error("Cannot read \u{7}auto\u{1b} state"),
            "Cannot read auto state"
        );
        let long = "x".repeat(MAX_ERROR_CHARS + 50);
        assert_eq!(sanitize_error(&long).chars().count(), MAX_ERROR_CHARS);
        assert_eq!(sanitize_error("  padded\n "), "padded");
    }

    fn record(event: &str, now: u64) -> Value {
        Value::Object(base(event, now))
    }

    #[test]
    fn appends_one_private_line_per_record() {
        let sandbox = Sandbox::default();
        let log = sandbox.0.join("tokenmaxx/auto.log");
        append(&log, &[record("tick", 100), record("ping", 101)]).unwrap();
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let lines: Vec<String> = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 2);
        for (line, event) in lines.iter().zip(["tick", "ping"]) {
            let value: Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["event"], event);
            assert_eq!(value["ts_epoch"], if event == "tick" { 100 } else { 101 });
            assert!(value["ts"].as_str().unwrap().contains('T'));
        }
    }

    #[test]
    fn rotation_keeps_exactly_one_previous_journal() {
        let sandbox = Sandbox::default();
        let log = sandbox.0.join("auto.log");
        fs::write(&log, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        append(&log, &[record("tick", 100)]).unwrap();
        let rotated = sandbox.0.join("auto.log.1");
        assert_eq!(
            fs::metadata(&rotated).unwrap().len(),
            MAX_BYTES + 1,
            "the full journal is kept once"
        );
        assert_eq!(
            fs::read_to_string(&log).unwrap().lines().count(),
            1,
            "the live journal restarts empty"
        );
        // A second rotation replaces the kept file instead of adding another.
        fs::write(&log, vec![b'y'; (MAX_BYTES + 1) as usize]).unwrap();
        append(&log, &[record("ping", 101)]).unwrap();
        assert_eq!(fs::metadata(&rotated).unwrap().len(), MAX_BYTES + 1);
        assert_eq!(fs::read_dir(&sandbox.0).unwrap().count(), 2);
    }

    #[test]
    fn an_unwritable_journal_stays_quietly_non_fatal() {
        let sandbox = Sandbox::default();
        let log = sandbox.0.join("auto.log");
        fs::create_dir(&log).unwrap();
        assert!(append(&log, &[record("tick", 100)]).is_err());
        append_or_warn(&log, &[record("tick", 100)]);
    }
}
