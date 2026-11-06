//! `cpaq report`: a visual summary of the tokenmaxx journal.
//!
//! Read-only: the journal and the schedule state are only ever inspected, so a
//! report can never change when the next keep-alive runs.

use std::{
    collections::BTreeMap,
    io::{BufWriter, Write},
};

use serde_json::Value;

use crate::{
    cli::Args,
    log::Record,
    paths::Paths,
    time::{duration, local, local_datetime},
};

/// Calendar days shown when the caller does not pass `--days`.
pub(crate) const DEFAULT_DAYS: usize = 7;
/// Detail notes are capped so one long error cannot break the table.
const NOTE_CHARS: usize = 40;
/// A ping and the tick that pinged sit within this many seconds of each other.
const MATCH_WINDOW_SECONDS: i64 = 2;
/// Detail columns stay wide even with no rows: `start origin result model wait
/// next reset note`, matching the reference report instead of a cramped header.
const PING_WIDTHS: [usize; 7] = [19, 6, 6, 12, 5, 19, 40];
/// The same for reset changes: `time kind account from -> to`.
const RESET_WIDTHS: [usize; 4] = [19, 24, 24, 12];

/// `cpaq report` entry point.
pub fn run(args: &Args, paths: &Paths) -> Result<i32, String> {
    let path = paths.auto_log_file();
    let bytes = match crate::log::read_all(&path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            eprintln!("report: no journal yet at {}", path.display());
            return Ok(0);
        }
        Err(()) => {
            eprintln!("report: cannot read {}", path.display());
            return Ok(1);
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let records: Vec<Record> = text.lines().filter_map(Record::parse).collect();
    let now = crate::time::now();
    let next_due =
        crate::auto::state_summary(&paths.auto_state_file()).map(|state| state.next_due_epoch);
    let lines = render(&records, next_due, now, args.days.unwrap_or(DEFAULT_DAYS));
    match crate::log::classify(write_report(&mut std::io::stdout().lock(), &lines)) {
        crate::log::Printed::Failed => {
            eprintln!("report: cannot write output");
            Ok(1)
        }
        // A closed pipe (`| head`) ends the report normally.
        _ => Ok(0),
    }
}

/// Writes the rendered report through one buffered writer.
fn write_report(out: &mut impl Write, lines: &[String]) -> std::io::Result<()> {
    let mut writer = BufWriter::new(out);
    for line in lines {
        writeln!(writer, "{line}")?;
    }
    writer.flush()
}

/// One day of the summary table.
#[derive(Default)]
struct Day {
    ticks: usize,
    probes: usize,
    pings: usize,
    ok: usize,
    errors: usize,
    timer: usize,
    manual: usize,
    resets: usize,
    waits: Vec<i64>,
}

/// Renders the whole report; `now` and `days` are injected so it stays testable.
fn render(records: &[Record], next_due: Option<u64>, now: i64, days: usize) -> Vec<String> {
    let cutoff = if days == 0 {
        None
    } else {
        crate::time::local_midnight_days_ago(now, days as u32)
    };
    let selected: Vec<&Record> = records
        .iter()
        .filter(|record| cutoff.is_none_or(|cutoff| epoch_of(record) >= cutoff))
        .collect();

    let mut table = Vec::new();
    table.push(format!(
        "cpaq tokenmaxx report - {}, through {}",
        if days == 0 {
            "all records".to_string()
        } else {
            format!("last {days} day(s)")
        },
        local(now, "%Y-%m-%d")
    ));
    table.push(String::new());

    // Summary: one row per local day that has records, ascending.
    let mut days_map: BTreeMap<String, Day> = BTreeMap::new();
    for record in &selected {
        let day = days_map
            .entry(local(epoch_of(record), "%Y-%m-%d"))
            .or_default();
        match record.event.as_str() {
            "tick" => {
                day.ticks += 1;
                if flag(record, "probe_due") {
                    day.probes += 1;
                }
                if let Some(wait) = record.value.get("wait_s").and_then(Value::as_i64) {
                    day.waits.push(wait);
                }
            }
            "ping" => {
                day.pings += 1;
                if string(record, "result") == "ok" {
                    day.ok += 1;
                } else {
                    day.errors += 1;
                }
                if string(record, "trigger") == "systemd" {
                    day.timer += 1;
                } else {
                    day.manual += 1;
                }
            }
            "reset_change" => day.resets += 1,
            _ => {}
        }
    }
    let rows: Vec<Vec<String>> = days_map
        .iter()
        .map(|(date, day)| {
            let total: i64 = day.waits.iter().sum();
            let average = if day.waits.is_empty() {
                "-".to_string()
            } else {
                // Round to the nearest second so a short wait cannot look long.
                minute_seconds((total + day.waits.len() as i64 / 2) / day.waits.len() as i64)
            };
            vec![
                date.clone(),
                day.ticks.to_string(),
                day.probes.to_string(),
                day.pings.to_string(),
                day.ok.to_string(),
                day.errors.to_string(),
                day.timer.to_string(),
                day.manual.to_string(),
                average,
                day.waits
                    .iter()
                    .copied()
                    .max()
                    .map_or_else(|| "-".to_string(), minute_seconds),
                day.resets.to_string(),
            ]
        })
        .collect();
    table.extend(table_lines(
        &[
            "Date", "Ticks", "Probes", "Pings", "OK", "Err", "Timer", "Manual", "Avg wait",
            "Max wait", "Resets",
        ],
        &rows,
        0,
        &[],
    ));

    if let Some(next_due) = next_due.filter(|epoch| *epoch > 0) {
        let in_text = if next_due as i64 > now {
            format!("in {}", span_text(next_due as i64 - now))
        } else {
            "overdue".to_string()
        };
        table.push(String::new());
        table.push(format!(
            "Next due: {} ({in_text})",
            local_datetime(next_due)
        ));
    }

    // Ping detail: the tick that performed each ping supplies the wait.
    let ticks: Vec<&Record> = selected
        .iter()
        .copied()
        .filter(|record| record.event == "tick")
        .collect();
    let raw_pings: Vec<&Record> = selected
        .iter()
        .copied()
        .filter(|record| record.event == "ping")
        .collect();
    let mut pings = raw_pings;
    pings.sort_by_key(|record| epoch_of(record));
    let ping_rows: Vec<Vec<String>> = pings
        .iter()
        .map(|ping| {
            let start = epoch_of(ping);
            vec![
                local_datetime(start as u64),
                match string(ping, "trigger") {
                    "systemd" => "timer".to_string(),
                    _ => "manual".to_string(),
                },
                string(ping, "result").to_string(),
                string(ping, "model").to_string(),
                matched_wait(&ticks, start).map_or_else(|| "-".to_string(), minute_seconds),
                match ping.value.get("next_due_epoch").and_then(Value::as_u64) {
                    Some(epoch) if epoch > 0 => local_datetime(epoch),
                    _ => "-".to_string(),
                },
                note(ping),
            ]
        })
        .collect();
    table.push(String::new());
    table.push("Pings".to_string());
    table.extend(table_lines(
        &[
            "start",
            "via",
            "result",
            "model",
            "wait",
            "next reset",
            "note",
        ],
        &ping_rows,
        2,
        &PING_WIDTHS,
    ));
    if ping_rows.is_empty() {
        table.push("  (no pings yet)".to_string());
    }

    // Reset changes: provider movements found by the probe.
    let raw_resets: Vec<&Record> = selected
        .iter()
        .copied()
        .filter(|record| record.event == "reset_change")
        .collect();
    let mut resets = raw_resets;
    resets.sort_by_key(|record| epoch_of(record));
    let reset_rows: Vec<Vec<String>> = resets
        .iter()
        .map(|reset| {
            let kind = string(reset, "kind");
            vec![
                local_datetime(epoch_of(reset) as u64),
                kind.to_string(),
                string(reset, "account").to_string(),
                format!(
                    "{} -> {}",
                    reset_value(kind, reset.value.get("from")),
                    reset_value(kind, reset.value.get("to")),
                ),
            ]
        })
        .collect();
    table.push(String::new());
    table.push("Reset changes".to_string());
    table.extend(table_lines(
        &["time", "kind", "account", "from -> to"],
        &reset_rows,
        2,
        &RESET_WIDTHS,
    ));
    if reset_rows.is_empty() {
        table.push("  (none)".to_string());
    }
    table
}

/// One aligned table: left-aligned, two-space gutters, no trailing spaces.
///
/// `minimums` keeps a detail table readable when it has no rows yet.
fn table_lines(
    headers: &[&str],
    rows: &[Vec<String>],
    indent: usize,
    minimums: &[usize],
) -> Vec<String> {
    let mut widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| width(header).max(minimums.get(index).copied().unwrap_or(0)))
        .collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(width(cell));
        }
    }
    let mut lines = Vec::with_capacity(rows.len() + 1);
    for cells in std::iter::once(headers.iter().map(|h| (*h).to_string()).collect())
        .chain(rows.iter().cloned())
    {
        let mut line = " ".repeat(indent);
        for (index, cell) in cells.iter().enumerate() {
            line.push_str(cell);
            // The last column is never padded: no trailing spaces.
            if index + 1 < cells.len() {
                line.push_str(&" ".repeat(widths[index].saturating_sub(width(cell)) + 2));
            }
        }
        lines.push(line);
    }
    lines
}

fn width(text: &str) -> usize {
    text.chars().count()
}

/// `m:ss`, e.g. `0:26` or `1:39`.
fn minute_seconds(seconds: i64) -> String {
    let seconds = seconds.max(0);
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Compact next-due span: `3h 28m`, `28m`, `42s`.
fn span_text(seconds: i64) -> String {
    let seconds = seconds.max(0) as u64;
    let (hours, minutes) = (seconds / 3600, seconds % 3600 / 60);
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        duration(seconds)
    }
}

/// The tick that pinged: same second (±2 s) and a pinging decision.
fn matched_wait(ticks: &[&Record], ping_epoch: i64) -> Option<i64> {
    ticks
        .iter()
        .filter(|tick| matches!(string(tick, "decision"), "due" | "early_reset"))
        .filter(|tick| (epoch_of(tick) - ping_epoch).abs() <= MATCH_WINDOW_SECONDS)
        .find_map(|tick| tick.value.get("wait_s").and_then(Value::as_i64))
}

/// Window movements carry epochs, banked credits carry counts.
fn reset_value(kind: &str, value: Option<&Value>) -> String {
    let Some(number) = value.and_then(Value::as_i64) else {
        return "-".to_string();
    };
    let window_movement = matches!(
        kind,
        "window_reopened_early" | "long_window_reopened_early" | "window_shortened"
    );
    if !window_movement {
        return number.to_string();
    }
    match u64::try_from(number) {
        Ok(epoch) if epoch > 0 => local_datetime(epoch),
        _ => "-".to_string(),
    }
}

/// Sanitized error text that fits the note column, or `-` when there is none.
fn note(ping: &Record) -> String {
    let Some(error) = ping.value.get("error").and_then(Value::as_str) else {
        return "-".to_string();
    };
    let error = crate::journal::sanitize_error(error);
    if error.chars().count() <= NOTE_CHARS {
        return error;
    }
    // Keep the cell inside the column while showing that text was cut.
    let mut note: String = error.chars().take(NOTE_CHARS - 3).collect();
    note.push_str("...");
    note
}

fn string<'a>(record: &'a Record, field: &str) -> &'a str {
    record
        .value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or("-")
}

fn flag(record: &Record, field: &str) -> bool {
    record
        .value
        .get(field)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn epoch_of(record: &Record) -> i64 {
    record
        .value
        .get("ts_epoch")
        .and_then(Value::as_i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Args, Mode};
    use crate::test_support::Sandbox;
    use serde_json::json;

    fn record(value: Value) -> Record {
        Record::parse(&value.to_string()).unwrap()
    }

    fn tick(epoch: i64, decision: &str, probe: bool, wait: Option<i64>) -> Record {
        let mut value = json!({
            "event": "tick",
            "ts": local(epoch, "%Y-%m-%dT%H:%M:%S%z"),
            "ts_epoch": epoch,
            "decision": decision,
            "probe_due": probe,
            "exit": 0,
        });
        if let Some(wait) = wait {
            value["wait_s"] = json!(wait);
        }
        record(value)
    }

    fn ping(epoch: i64, origin: &str, result: &str, error: Option<&str>) -> Record {
        let mut value = json!({
            "event": "ping",
            "ts": local(epoch, "%Y-%m-%dT%H:%M:%S%z"),
            "ts_epoch": epoch,
            "origin": origin,
            "trigger": if origin == "auto" { "systemd" } else { "manual" },
            "result": result,
            "model": "gpt-6-luna",
            "next_due_epoch": epoch + 18_000,
            "exit": if result == "ok" { 0 } else { 1 },
        });
        if let Some(error) = error {
            value["error"] = json!(error);
        }
        record(value)
    }

    fn reset(epoch: i64) -> Record {
        record(json!({
            "event": "reset_change",
            "ts": local(epoch, "%Y-%m-%dT%H:%M:%S%z"),
            "ts_epoch": epoch,
            "kind": "banked_reset_used",
            "account": "acct-1",
            "from": 2,
            "to": 1,
        }))
    }

    /// Fixed instant plus two local days of records around its midnight.
    fn fixture() -> (Vec<Record>, i64, i64, i64) {
        let now = 1_793_930_000_i64;
        let midnight = crate::time::local_midnight_days_ago(now, 1).unwrap();
        let yesterday = midnight - 3600;
        let today = midnight + 60;
        let records = vec![
            // Yesterday: two probes, one wait per pinging tick, one ping, one reset.
            tick(yesterday, "due", true, Some(20)),
            tick(yesterday + 300, "not_due", false, None),
            tick(yesterday + 600, "not_due", true, Some(40)),
            tick(yesterday + 61, "not_due", false, Some(99)),
            ping(yesterday + 60, "auto", "ok", None),
            reset(yesterday + 700),
            // Today: one due tick that waited, then a manual ping and an error.
            tick(today, "due", false, Some(26)),
            ping(today, "ping", "ok", None),
            ping(
                today + 120,
                "ping",
                "error",
                Some("Auth directory not found. Tried: /home/x/a,/home/x/b. Set --auth-dir."),
            ),
        ];
        (records, now, yesterday, today)
    }

    fn report(records: &[Record], next_due: Option<u64>, now: i64, days: usize) -> Vec<String> {
        render(records, next_due, now, days)
    }

    #[test]
    fn summary_groups_records_by_local_day() {
        let (records, now, yesterday, today) = fixture();
        let lines = report(&records, None, now, 2);
        let date_yesterday = local(yesterday, "%Y-%m-%d");
        let date_today = local(today, "%Y-%m-%d");
        assert_eq!(
            lines[0],
            format!(
                "cpaq tokenmaxx report - last 2 day(s), through {}",
                local(now, "%Y-%m-%d")
            )
        );
        assert_eq!(lines[1], "");
        // The header comes from the spec reference, so alignment is pinned.
        assert_eq!(
            lines[2],
            "Date        Ticks  Probes  Pings  OK  Err  Timer  Manual  Avg wait  Max wait  Resets"
        );
        assert_eq!(
            lines[3],
            format!(
                "{date_yesterday}  4      2       1      1   0    1      0       0:53      1:39      1"
            )
        );
        assert_eq!(
            lines[4],
            format!(
                "{date_today}  1      0       2      1   1    0      2       0:26      0:26      0"
            )
        );
        assert!(
            lines.iter().all(|line| line == line.trim_end()),
            "trailing space"
        );
    }

    #[test]
    fn manual_auto_run_counts_as_manual_not_timer() {
        let now = 1_793_930_000_i64;
        let epoch = crate::time::local_midnight_days_ago(now, 0).unwrap() + 3600;
        let record = record(json!({
            "event": "ping",
            "ts": local(epoch, "%Y-%m-%dT%H:%M:%S%z"),
            "ts_epoch": epoch,
            "origin": "auto",
            "trigger": "manual",
            "result": "ok",
            "model": "gpt-6-luna",
            "next_due_epoch": epoch + 18_000,
            "exit": 0,
        }));
        let lines = report(&[record], None, now, 7);
        let row = lines
            .iter()
            .find(|line| line.starts_with(&local(now, "%Y-%m-%d")))
            .unwrap();
        let cells: Vec<&str> = row.split_whitespace().collect();
        // date ticks probes pings ok err timer manual avg max resets
        assert_eq!(cells[6], "0", "Timer: {row}");
        assert_eq!(cells[7], "1", "Manual: {row}");
        assert!(
            lines.iter().any(|line| line.contains("  manual  ok")),
            "{lines:#?}"
        );
    }

    #[test]
    fn days_zero_shows_everything_and_days_one_starts_today() {
        let (records, now, yesterday, today) = fixture();
        let (date_yesterday, date_today) = (local(yesterday, "%Y-%m-%d"), local(today, "%Y-%m-%d"));
        let dated = |lines: &[String]| {
            lines
                .iter()
                .filter(|line| line.starts_with(&date_today) || line.starts_with(&date_yesterday))
                .cloned()
                .collect::<Vec<String>>()
        };
        let all = report(&records, None, now, 0);
        assert!(all[0].starts_with("cpaq tokenmaxx report - all records, through "));
        assert_eq!(dated(&all).len(), 2, "{all:#?}");
        // days = 1 keeps today only.
        let today_only = report(&records, None, now, 1);
        assert!(today_only[0].contains("- last 1 day(s), through "));
        let rows = dated(&today_only);
        assert_eq!(rows.len(), 1, "{today_only:#?}");
        assert!(rows[0].starts_with(&date_today));
    }

    #[test]
    fn pings_carry_their_matching_tick_wait_and_next_reset() {
        let (records, now, yesterday, today) = fixture();
        let lines = report(&records, None, now, 2);
        let manual: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains("  manual  "))
            .collect();
        assert_eq!(manual.len(), 2, "{lines:#?}");
        let start = local_datetime(today as u64);
        assert!(
            manual[0].starts_with(&format!("  {start}  manual  ok      gpt-6-luna")),
            "{}",
            manual[0]
        );
        assert!(manual[0].contains("0:26"), "{}", manual[0]);
        assert!(manual[0].ends_with("  -"), "{}", manual[0]);
        // The second manual ping matches no pinging tick: no wait, error note.
        assert!(manual[1].contains("  error   gpt-6-luna"), "{}", manual[1]);
        let auto: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains("  timer   "))
            .collect();
        assert_eq!(auto.len(), 1, "{lines:#?}");
        assert!(
            auto[0].starts_with(&format!(
                "  {}  timer   ok",
                local_datetime((yesterday + 60) as u64)
            )),
            "{}",
            auto[0]
        );
        // Its only nearby tick is `not_due`, so the wait stays empty.
        let wait_cell = auto[0]
            .rsplit_once("gpt-6-luna")
            .map(|(_, tail)| tail.trim_start().split("  ").next().unwrap())
            .unwrap();
        assert_eq!(wait_cell, "-", "{}", auto[0]);
    }

    #[test]
    fn error_notes_are_truncated_and_sanitized() {
        let (records, now, _, _) = fixture();
        let midnight = crate::time::local_midnight_days_ago(now, 1).unwrap();
        let lines = report(&records, None, now, 2);
        let error: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains("  error  "))
            .collect();
        assert_eq!(error.len(), 1, "{lines:#?}");
        let note = error[0].split("gpt-6-luna").nth(1).unwrap();
        let note = note.trim_start().split("  ").next().unwrap();
        let _ = note;
        let source = "Auth directory not found. Tried: /home/x/a,/home/x/b. Set --auth-dir.";
        let expected: String = source
            .chars()
            .take(NOTE_CHARS - 3)
            .chain("...".chars())
            .collect();
        assert_eq!(expected.chars().count(), NOTE_CHARS);
        assert!(error[0].ends_with(&expected), "{}", error[0]);
        assert!(error[0].ends_with("..."), "{}", error[0]);
        assert!(!error[0].contains("--auth-dir."));
        // A short error is shown whole, without a marker.
        let short = ping(midnight + 180, "auto", "error", Some("boom"));
        let lines = report(&[short], None, now, 7);
        let row = lines
            .iter()
            .find(|line| line.contains("  error  "))
            .unwrap();
        assert!(row.ends_with("  boom"), "{row}");
        assert!(!row.contains("..."), "{row}");
    }

    #[test]
    fn empty_sections_keep_their_headers() {
        let now = 1_793_930_000_i64;
        let midnight = crate::time::local_midnight_days_ago(now, 1).unwrap();
        let records = vec![tick(midnight + 60, "not_due", false, None)];
        let lines = report(&records, None, now, 7);
        assert!(lines.iter().any(|line| line == "Pings"), "{lines:#?}");
        assert!(lines.iter().any(|line| line == "  (no pings yet)"));
        assert!(lines.iter().any(|line| line == "Reset changes"));
        assert!(lines.iter().any(|line| line == "  (none)"));
        // The column headers keep the wide reference layout, row-free.
        let pings = lines
            .iter()
            .find(|line| line.trim_start().starts_with("start "))
            .unwrap();
        for (column, start) in [
            ("start", 2),
            ("via", 23),
            ("result", 31),
            ("model", 39),
            ("wait", 53),
            ("next reset", 60),
            ("note", 81),
        ] {
            assert_eq!(pings.find(column), Some(start), "{column} in {pings}");
        }
        let resets = lines
            .iter()
            .find(|line| line.trim_start().starts_with("time "))
            .unwrap();
        for (column, start) in [
            ("time", 2),
            ("kind", 23),
            ("account", 49),
            ("from -> to", 75),
        ] {
            assert_eq!(resets.find(column), Some(start), "{column} in {resets}");
        }
    }

    #[test]
    fn next_due_line_formats_the_gap_or_says_overdue() {
        let (records, now, _, _) = fixture();
        let soon = report(&records, Some((now + 3 * 3600 + 28 * 60) as u64), now, 2);
        let line = soon
            .iter()
            .find(|line| line.starts_with("Next due: "))
            .unwrap();
        assert_eq!(
            line,
            &format!(
                "Next due: {} (in 3h 28m)",
                local_datetime((now + 3 * 3600 + 28 * 60) as u64)
            )
        );
        let overdue = report(&records, Some((now - 60) as u64), now, 2);
        assert!(overdue.iter().any(|line| line.ends_with("(overdue)")));
        // No state: no line at all.
        let none = report(&records, None, now, 2);
        assert!(!none.iter().any(|line| line.starts_with("Next due:")));
    }

    #[test]
    fn closed_pipe_ends_the_report_without_an_error() {
        use std::io::ErrorKind;

        struct ClosedPipe;

        impl Write for ClosedPipe {
            fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
                Err(ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Err(ErrorKind::BrokenPipe.into())
            }
        }

        let lines = render(&[], None, 1_793_930_000, 7);
        let printed = crate::log::classify(write_report(&mut ClosedPipe, &lines));
        assert_eq!(printed, crate::log::Printed::ClosedPipe);
        assert_ne!(printed, crate::log::Printed::Failed);
        // The same lines still render through a healthy writer.
        let mut buffer = Vec::new();
        write_report(&mut buffer, &lines).unwrap();
        assert_eq!(
            String::from_utf8(buffer).unwrap(),
            format!("{}\n", lines.join("\n"))
        );
    }

    #[test]
    fn reset_change_values_follow_the_kind() {
        let now = 1_793_930_000_i64;
        let midnight = crate::time::local_midnight_days_ago(now, 1).unwrap();
        let change = |epoch: i64, kind: &str, from: Option<i64>, to: Option<i64>| {
            let mut value = json!({
                "event": "reset_change",
                "ts": local(epoch, "%Y-%m-%dT%H:%M:%S%z"),
                "ts_epoch": epoch,
                "kind": kind,
                "account": "acct-1",
            });
            if let Some(from) = from {
                value["from"] = json!(from);
            }
            if let Some(to) = to {
                value["to"] = json!(to);
            }
            record(value)
        };
        let records = vec![
            change(
                midnight + 60,
                "window_reopened_early",
                Some(1793936426),
                Some(1793940000),
            ),
            change(midnight + 120, "banked_reset_used", Some(2), Some(1)),
            change(midnight + 180, "banked_reset_earned", None, Some(3)),
        ];
        let lines = report(&records, None, now, 7);
        let window = lines
            .iter()
            .find(|line| line.contains("window_reopened_early"))
            .unwrap();
        assert!(
            window.ends_with(&format!(
                "{} -> {}",
                local_datetime(1793936426),
                local_datetime(1793940000)
            )),
            "{window}"
        );
        let banked = lines
            .iter()
            .find(|line| line.contains("banked_reset_used"))
            .unwrap();
        assert!(banked.ends_with("2 -> 1"), "{banked}");
        let earned = lines
            .iter()
            .find(|line| line.contains("banked_reset_earned"))
            .unwrap();
        assert!(earned.ends_with("- -> 3"), "{earned}");
    }

    #[test]
    fn missing_journal_message_and_exit_code() {
        let sandbox = Sandbox::default();
        let paths = Paths::new(
            sandbox.0.join("home"),
            Some(sandbox.0.join("config")),
            Some(sandbox.0.join("state")),
        );
        let args = Args {
            mode: Mode::Report,
            ..Args::default()
        };
        assert!(!paths.auto_log_file().exists());
        assert_eq!(run(&args, &paths).unwrap(), 0);
    }
}
