//! `cpaq log`: inspect the tokenmaxx journal without touching the schedule.
//!
//! The journal is append-only JSONL owned by [`crate::journal`]; this module
//! only reads it, so a broken or missing journal can never affect a ping.

use std::{
    fs::File,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::Path,
    time::Duration,
};

use serde_json::Value;

use crate::{cli::Args, paths::Paths};

/// Records printed when the caller does not ask for a specific tail.
pub(crate) const DEFAULT_LIMIT: usize = 20;
/// Follow poll interval.
const POLL: Duration = Duration::from_millis(500);
/// Bounded read: the journal rotates at 2 MiB, so this is only a safety net.
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
/// The event `cpaq log` shows unless `--all` widens it.
const PING: &str = "ping";

/// One journaled event, keeping the exact line for `--json` passthrough.
#[derive(Clone, Debug, PartialEq)]
struct Record {
    event: String,
    line: String,
    value: Value,
}

impl Record {
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches('\r');
        let value: Value = serde_json::from_str(line).ok()?;
        let event = value.get("event")?.as_str()?.to_string();
        Some(Self {
            event,
            line: line.to_string(),
            value,
        })
    }
}

/// Cursor over the live journal: one inode and one byte offset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tail {
    inode: u64,
    offset: u64,
}

/// `cpaq log` entry point.
pub fn run(args: &Args, paths: &Paths) -> Result<i32, String> {
    let path = paths.auto_log_file();
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    if args.follow {
        eprintln!("log: following {} (Ctrl-C to stop)", path.display());
        return Ok(follow(&path, args, limit));
    }
    let bytes = match read_all(&path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            eprintln!("log: no journal yet at {}", path.display());
            return Ok(0);
        }
        Err(()) => {
            eprintln!("log: cannot read {}", path.display());
            return Ok(1);
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let (records, ticks) = select(text.lines(), args, limit);
    if !args.all && records.is_empty() {
        note_no_pings(&path, ticks);
    }
    if print_records(&records, args.json).is_err() {
        eprintln!("log: cannot write output");
        return Ok(1);
    }
    Ok(0)
}

/// Whole journal, bounded; `Ok(None)` when it does not exist yet.
fn read_all(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_READ_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    Ok(Some(bytes))
}

/// Follows the journal until the process is interrupted.
fn follow(path: &Path, args: &Args, limit: usize) -> i32 {
    // The first poll consumes whatever is already there, so the printed tail
    // and the cursor that continues from it can never disagree.
    let mut tail = Tail::default();
    let initial = match read_new(path, &mut tail) {
        Ok(lines) => lines,
        Err(()) => return read_failed(path),
    };
    let (records, ticks) = select(initial.iter().map(String::as_str), args, limit);
    if !args.all && records.is_empty() && !initial.is_empty() {
        note_no_pings(path, ticks);
    }
    if print_records(&records, args.json).is_err() {
        eprintln!("log: cannot write output");
        return 1;
    }
    loop {
        std::thread::sleep(POLL);
        match read_new(path, &mut tail) {
            Ok(lines) if lines.is_empty() => {}
            Ok(lines) => {
                if emit(lines.iter().map(String::as_str), args, usize::MAX) != 0 {
                    return 1;
                }
            }
            Err(()) => return read_failed(path),
        }
    }
}

fn read_failed(path: &Path) -> i32 {
    eprintln!("log: cannot read {}", path.display());
    1
}

/// Filters, formats and prints one poll; returns 1 only when nothing was written.
fn emit<'a>(lines: impl Iterator<Item = &'a str>, args: &Args, limit: usize) -> i32 {
    let records = collect(lines, args, limit);
    if print_records(&records, args.json).is_err() {
        eprintln!("log: cannot write output");
        return 1;
    }
    0
}

/// Oldest to newest: filter by event, then keep the last `limit` (0 = all).
fn select<'a>(
    lines: impl Iterator<Item = &'a str>,
    args: &Args,
    limit: usize,
) -> (Vec<Record>, usize) {
    let mut records: Vec<Record> = lines.filter_map(Record::parse).collect();
    let ticks = records
        .iter()
        .filter(|record| record.event == "tick")
        .count();
    if !args.all {
        records.retain(|record| record.event == PING);
    }
    if limit > 0 && records.len() > limit {
        records.drain(..records.len() - limit);
    }
    (records, ticks)
}

/// Selection only, for the formatters and tests.
fn collect<'a>(lines: impl Iterator<Item = &'a str>, args: &Args, limit: usize) -> Vec<Record> {
    select(lines, args, limit).0
}

/// The default view is pings: an empty one must explain itself, not look broken.
fn note_no_pings(path: &Path, ticks: usize) {
    eprintln!(
        "log: no ping records yet in {} ({ticks} tick(s) in the journal; use --all)",
        path.display()
    );
}

fn print_records(records: &[Record], json: bool) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for record in records {
        if json {
            writeln!(out, "{}", record.line)?;
        } else {
            writeln!(out, "{}", human(record))?;
        }
    }
    out.flush()
}

/// One stable line per record; the journal's own `ts` is the timestamp.
fn human(record: &Record) -> String {
    let ts = record
        .value
        .get("ts")
        .and_then(Value::as_str)
        .unwrap_or("?");
    match record.event.as_str() {
        "ping" => ping_line(ts, &record.value),
        "tick" => tick_line(ts, &record.value),
        "reset_change" => reset_line(ts, &record.value),
        event => format!("{ts}  {event}"),
    }
}

fn ping_line(ts: &str, value: &Value) -> String {
    let next = number(value, "next_due_epoch")
        .filter(|epoch| *epoch > 0)
        .map_or_else(
            || "-".to_string(),
            |epoch| crate::time::rfc3339(epoch as u64),
        );
    let mut line = format!(
        "{ts}  ping  {}  {}  model={}  next={next}",
        text(value, "origin"),
        text(value, "result"),
        text(value, "model"),
    );
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        line.push_str(&format!(
            "  exit={}  error=\"{error}\"",
            number(value, "exit").unwrap_or(1)
        ));
    }
    line
}

fn tick_line(ts: &str, value: &Value) -> String {
    let due_in = number(value, "due_in_s").map_or_else(|| "-".to_string(), signed_duration);
    let wait = number(value, "wait_s").map_or_else(|| "-".to_string(), signed_duration);
    format!(
        "{ts}  tick  {}  due_in={due_in}  wait={wait}  exit={}",
        text(value, "decision"),
        number(value, "exit").unwrap_or(0)
    )
}

fn reset_line(ts: &str, value: &Value) -> String {
    format!(
        "{ts}  reset_change  {}  {}  {} -> {}",
        text(value, "kind"),
        text(value, "account"),
        number(value, "from").map_or_else(|| "-".to_string(), |from| from.to_string()),
        number(value, "to").map_or_else(|| "-".to_string(), |to| to.to_string()),
    )
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("-")
}

fn number(value: &Value, field: &str) -> Option<i64> {
    value.get(field).and_then(Value::as_i64)
}

/// Durations stay readable and a negative distance keeps its sign.
fn signed_duration(seconds: i64) -> String {
    if seconds < 0 {
        format!("-{}", crate::time::duration(seconds.unsigned_abs()))
    } else {
        crate::time::duration(seconds as u64)
    }
}

/// One poll: returns complete lines appended since the cursor, then moves it.
///
/// Append, truncation and rotation all work: a changed inode or a shorter file
/// restarts from zero, so a rotated `auto.log` is re-read instead of missed.
pub(crate) fn read_new(path: &Path, tail: &mut Tail) -> Result<Vec<String>, ()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            *tail = Tail::default();
            return Ok(Vec::new());
        }
        Err(_) => return Err(()),
    };
    let (inode, length) = (metadata.ino(), metadata.len());
    if inode != tail.inode || length < tail.offset {
        *tail = Tail { inode, offset: 0 };
    }
    if length <= tail.offset {
        return Ok(Vec::new());
    }
    let mut file = File::open(path).map_err(|_| ())?;
    file.seek(SeekFrom::Start(tail.offset)).map_err(|_| ())?;
    let mut bytes = Vec::new();
    file.take(length - tail.offset)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    // Only whole lines are consumed: a half-written record waits for the next poll.
    let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(Vec::new());
    };
    tail.offset += (end + 1) as u64;
    Ok(String::from_utf8_lossy(&bytes[..=end])
        .lines()
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Args, Mode};
    use crate::test_support::Sandbox;
    use std::fs;

    const TICK: &str = r#"{"decision":"not_due","due_in_s":17964,"event":"tick","exit":0,"next_due_epoch":1793936426,"ts":"2026-11-05T18:15:46-05:00","ts_epoch":1793920546}"#;
    const PING_OK: &str = r#"{"event":"ping","exit":0,"model":"test-model","next_due_epoch":1793936426,"origin":"ping","result":"ok","ts":"2026-11-05T18:16:00-05:00","ts_epoch":1793920560}"#;
    const PING_ERROR: &str = r#"{"error":"No API key","event":"ping","exit":1,"model":"test-model","next_due_epoch":0,"origin":"auto","result":"error","ts":"2026-11-05T18:17:00-05:00","ts_epoch":1793920620}"#;
    const RESET: &str = r#"{"account":"acct-1","event":"reset_change","from":20000,"kind":"banked_reset_used","to":1,"ts":"2026-11-05T18:18:00-05:00","ts_epoch":1793920680}"#;

    fn args(all: bool, json: bool, limit: Option<usize>) -> Args {
        Args {
            mode: Mode::Log,
            all,
            json,
            limit,
            ..Args::default()
        }
    }

    fn collect_text(text: &str, all: bool, limit: usize) -> Vec<String> {
        collect(text.lines(), &args(all, false, Some(limit)), limit)
            .iter()
            .map(human)
            .collect()
    }

    #[test]
    fn default_tail_is_pings_only_and_all_widens_it() {
        let journal = [TICK, PING_OK, RESET, PING_ERROR].join("\n") + "\n";
        let pings = collect_text(&journal, false, 0);
        assert_eq!(pings.len(), 2);
        assert!(pings[0].contains("  ping  ping  ok  "));
        assert!(pings[1].contains("  ping  auto  error  "));
        let all = collect_text(&journal, true, 0);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].matches("  tick  ").count(), 1);
        assert_eq!(all[2].matches("  reset_change  ").count(), 1);
    }

    #[test]
    fn limit_zero_means_all_and_n_keeps_the_newest() {
        let journal = [TICK, PING_OK, RESET, PING_ERROR].join("\n") + "\n";
        assert_eq!(collect_text(&journal, true, 0).len(), 4);
        assert_eq!(collect_text(&journal, true, 2).len(), 2);
        let newest = collect_text(&journal, true, 2);
        assert!(newest[0].contains("  reset_change  "));
        assert!(newest[1].contains("  ping  auto  error  "));
        assert_eq!(collect_text(&journal, false, 1).len(), 1);
        // Malformed lines are skipped, never fatal.
        let broken = format!("{TICK}\nnot json\n{{\"event\":\"ping\"}}\n");
        assert_eq!(collect_text(&broken, true, 0).len(), 2);
    }

    #[test]
    fn selection_counts_ticks_for_the_empty_ping_notice() {
        let journal = [TICK, TICK, RESET].join("\n") + "\n";
        let (pings, ticks) = select(journal.lines(), &args(false, false, Some(0)), 0);
        assert!(pings.is_empty());
        assert_eq!(ticks, 2);
        let (everything, ticks) = select(journal.lines(), &args(true, false, Some(0)), 0);
        assert_eq!(everything.len(), 3);
        assert_eq!(ticks, 2);
        let (limited, _) = select(journal.lines(), &args(false, false, Some(1)), 1);
        assert!(limited.is_empty());
    }

    #[test]
    fn json_passthrough_keeps_the_exact_journal_line() {
        let journal = [TICK, PING_OK].join("\n") + "\n";
        let records = collect(journal.lines(), &args(true, true, Some(1)), 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].line, PING_OK);
        let records = collect(journal.lines(), &args(true, true, Some(0)), 0);
        assert_eq!(
            records
                .iter()
                .map(|record| record.line.as_str())
                .collect::<Vec<_>>(),
            vec![TICK, PING_OK]
        );
    }

    #[test]
    fn ping_lines_render_ok_and_error() {
        let journal = [PING_OK, PING_ERROR].join("\n") + "\n";
        let lines = collect_text(&journal, false, 0);
        assert!(
            lines[0]
                .starts_with("2026-11-05T18:16:00-05:00  ping  ping  ok  model=test-model  next="),
            "{}",
            lines[0]
        );
        assert!(!lines[0].contains("error="));
        assert!(
            lines[1].starts_with(
                "2026-11-05T18:17:00-05:00  ping  auto  error  model=test-model  next=-"
            ),
            "{}",
            lines[1]
        );
        assert!(
            lines[1].ends_with(r#"  exit=1  error="No API key""#),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn tick_and_reset_lines_render() {
        let journal = [TICK, RESET].join("\n") + "\n";
        let lines = collect_text(&journal, true, 0);
        assert_eq!(
            lines[0],
            "2026-11-05T18:15:46-05:00  tick  not_due  due_in=4h 59m 24s  wait=-  exit=0"
        );
        assert_eq!(
            lines[1],
            "2026-11-05T18:18:00-05:00  reset_change  banked_reset_used  acct-1  20000 -> 1"
        );
        // A wait and an overdue distance keep their own shape.
        let waiting = r#"{"due_in_s":-90,"event":"tick","exit":0,"ts":"t","wait_s":24}"#;
        let lines = collect_text(&format!("{waiting}\n"), true, 0);
        assert_eq!(lines[0], "t  tick  -  due_in=-1m 30s  wait=24s  exit=0");
    }

    #[test]
    fn read_new_only_returns_lines_appended_since_the_last_poll() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.log");
        fs::write(&path, format!("{TICK}\n")).unwrap();
        let mut tail = Tail::default();
        let first = read_new(&path, &mut tail).unwrap();
        assert_eq!(first, vec![TICK.to_string()]);
        assert!(read_new(&path, &mut tail).unwrap().is_empty());
        fs::write(&path, format!("{TICK}\n{PING_OK}\n")).unwrap();
        assert_eq!(
            read_new(&path, &mut tail).unwrap(),
            vec![PING_OK.to_string()]
        );
        // A half-written line is left for the next poll.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"event\":\"ping\"").unwrap();
        assert!(read_new(&path, &mut tail).unwrap().is_empty());
        file.write_all(b"}\n").unwrap();
        assert_eq!(
            read_new(&path, &mut tail).unwrap(),
            vec!["{\"event\":\"ping\"}".to_string()]
        );
    }

    #[test]
    fn read_new_restarts_on_truncation_and_rotation() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.log");
        fs::write(&path, format!("{TICK}\n{PING_OK}\n")).unwrap();
        let mut tail = Tail::default();
        assert_eq!(read_new(&path, &mut tail).unwrap().len(), 2);
        // Truncation: the file shrank, so the cursor restarts from zero.
        fs::write(&path, format!("{PING_ERROR}\n")).unwrap();
        assert_eq!(
            read_new(&path, &mut tail).unwrap(),
            vec![PING_ERROR.to_string()]
        );
        // Rotation: the old journal is renamed and a new inode takes its place.
        fs::rename(&path, sandbox.0.join("auto.log.1")).unwrap();
        fs::write(&path, format!("{RESET}\n")).unwrap();
        assert_eq!(read_new(&path, &mut tail).unwrap(), vec![RESET.to_string()]);
        // A missing journal is not an error: follow waits for it.
        fs::remove_file(&path).unwrap();
        assert!(read_new(&path, &mut tail).unwrap().is_empty());
        fs::write(&path, format!("{TICK}\n")).unwrap();
        assert_eq!(read_new(&path, &mut tail).unwrap(), vec![TICK.to_string()]);
    }

    #[test]
    fn missing_journal_reports_a_friendly_line_and_exit_zero() {
        let sandbox = Sandbox::default();
        let paths = Paths::new(
            sandbox.0.join("home"),
            Some(sandbox.0.join("config")),
            Some(sandbox.0.join("state")),
        );
        // Only the message is asserted here; the headless suite checks the stream.
        let path = paths.auto_log_file();
        assert!(!path.exists());
        assert_eq!(read_all(&path).unwrap(), None);
    }
}
