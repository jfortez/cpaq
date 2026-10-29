//! Short-lived keep-alive scheduling using only local state until a pass is due.

use std::{fs::File, io::Read, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    cli::Args,
    config::{Config, atomic_write},
    paths::Paths,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct State {
    last_ping_epoch: u64,
    next_due_epoch: u64,
}

#[derive(Debug, Serialize)]
struct Report {
    decision: &'static str,
    dry_run: bool,
    state: Option<State>,
}

/// Evaluate the local schedule before resolving any credentials or auth files.
pub fn run(args: &Args, paths: &Paths, config: &Config) -> Result<i32, String> {
    let now = crate::time::now().max(0) as u64;
    let (report, code) = run_with(config, &paths.auto_state_file(), now, args.dry_run, || {
        crate::tokenmaxx::run_output(args, paths, config, !args.json)
    })?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|_| "Cannot serialize auto decision")?
        );
    } else {
        match report.decision {
            "disabled" => println!("auto: tokenmaxx is disabled"),
            "auto_disabled" => println!("auto: auto mode is disabled"),
            "not_due" => {
                if let Some(state) = report.state {
                    println!(
                        "auto: next due {} (in {}m; not due)",
                        rfc3339(state.next_due_epoch),
                        state.next_due_epoch.saturating_sub(now).div_ceil(60)
                    );
                }
            }
            _ => {
                let action = if args.dry_run {
                    "would ping"
                } else {
                    "pass complete"
                };
                println!("auto: due; {action}");
            }
        }
        if args.dry_run {
            println!(
                "auto: would-be state {}",
                serde_json::to_string(&report.state).map_err(|_| "Cannot serialize auto state")?
            );
        }
    }
    Ok(code)
}

fn read_state(path: &Path) -> Result<Option<State>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Cannot read auto state".into()),
    };
    // Reject anything >= 1 KiB, without allocating for an unbounded file.
    let mut bytes = Vec::with_capacity(1024);
    file.take(1024)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read auto state")?;
    if bytes.len() >= 1024 {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

fn run_with(
    config: &Config,
    path: &Path,
    now: u64,
    dry_run: bool,
    ping: impl FnOnce() -> Result<i32, String>,
) -> Result<(Report, i32), String> {
    let mut report = Report {
        decision: "disabled",
        dry_run,
        state: None,
    };
    if !config.tokenmaxx.enabled {
        return Ok((report, 0));
    }
    if !config.tokenmaxx.auto {
        report.decision = "auto_disabled";
        return Ok((report, 0));
    }
    report.state = read_state(path)?;
    if report.state.is_some_and(|state| now < state.next_due_epoch) {
        report.decision = "not_due";
        return Ok((report, 0));
    }
    report.decision = "due";
    let code = if dry_run {
        report.state = Some(scheduled_state(now, config.tokenmaxx.interval_hours, None)?);
        0
    } else {
        let code = ping()?;
        // The shared keep-alive pass owns persistence, including manual pings.
        report.state = read_state(path)?;
        code
    };
    Ok((report, code))
}

fn scheduled_state(
    now: u64,
    interval_hours: u32,
    reset_epoch: Option<u64>,
) -> Result<State, String> {
    Ok(State {
        last_ping_epoch: now,
        next_due_epoch: match reset_epoch {
            Some(reset) => reset,
            None => now
                .checked_add(u64::from(interval_hours) * 3600)
                .ok_or("Auto schedule overflow")?,
        },
    })
}

/// Persist the shared schedule only after the caller has observed a successful ping.
pub(crate) fn record_success(
    path: &Path,
    now: u64,
    interval_hours: u32,
    reset_epoch: Option<u64>,
) -> Result<(), String> {
    let state = scheduled_state(now, interval_hours, reset_epoch)?;
    let bytes = serde_json::to_vec(&state).map_err(|_| "Cannot serialize auto state")?;
    atomic_write(path, &bytes).map_err(|_| "Cannot save auto state atomically".into())
}

fn rfc3339(epoch: u64) -> String {
    let Ok(epoch) = i64::try_from(epoch) else {
        return "?".into();
    };
    let mut text = crate::time::local(epoch, "%Y-%m-%dT%H:%M:%S%z");
    if text.len() >= 5 {
        text.insert(text.len() - 2, ':');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Sandbox;
    use std::{cell::Cell, fs, os::unix::fs::PermissionsExt};

    #[test]
    fn missing_state_is_due_and_writes_private_state_after_one_pass() {
        let s = Sandbox::default();
        let path = s.0.join("tokenmaxx/auto.json");
        let calls = Cell::new(0);
        let (report, code) = run_with(&Config::default(), &path, 100, false, || {
            calls.set(calls.get() + 1);
            record_success(&path, 100, 5, None)?;
            Ok(0)
        })
        .unwrap();
        assert_eq!(report.decision, "due");
        assert_eq!(code, 0);
        assert_eq!(calls.get(), 1);
        assert_eq!(
            read_state(&path).unwrap(),
            Some(State {
                last_ping_epoch: 100,
                next_due_epoch: 18100
            })
        );
        assert!(fs::metadata(&path).unwrap().len() < 1024);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn due_auto_reports_the_shared_pass_reset_without_overwriting_it() {
        let s = Sandbox::default();
        let path = s.0.join("auto.json");
        let (report, code) = run_with(&Config::default(), &path, 100, false, || {
            record_success(&path, 101, 5, Some(500))?;
            Ok(0)
        })
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(report.decision, "due");
        assert_eq!(
            report.state,
            Some(State {
                last_ping_epoch: 101,
                next_due_epoch: 500
            })
        );
        assert_eq!(read_state(&path).unwrap(), report.state);
    }

    #[test]
    fn corrupt_state_is_due_and_replaced() {
        let s = Sandbox::default();
        let path = s.0.join("auto.json");
        for bytes in [
            b"broken".as_slice(),
            br#"{"next_due_epoch":1000}"#,
            br#"{"last_ping_epoch":-1,"next_due_epoch":1000}"#,
        ] {
            fs::write(&path, bytes).unwrap();
            let (report, _) = run_with(&Config::default(), &path, 100, false, || {
                record_success(&path, 100, 5, None)?;
                Ok(0)
            })
            .unwrap();
            assert_eq!(report.decision, "due");
            assert_eq!(read_state(&path).unwrap().unwrap().next_due_epoch, 18100);
        }
    }

    #[test]
    fn exact_boundary_is_due() {
        let s = Sandbox::default();
        let path = s.0.join("auto.json");
        fs::write(&path, br#"{"last_ping_epoch":0,"next_due_epoch":100}"#).unwrap();
        let (report, _) = run_with(&Config::default(), &path, 100, false, || {
            record_success(&path, 100, 5, None)?;
            Ok(0)
        })
        .unwrap();
        assert_eq!(report.decision, "due");
        assert_eq!(read_state(&path).unwrap().unwrap().last_ping_epoch, 100);
    }

    #[test]
    fn future_state_is_not_due_and_never_calls_pass_or_writes() {
        let s = Sandbox::default();
        let path = s.0.join("auto.json");
        let bytes = br#"{"last_ping_epoch":0,"next_due_epoch":101}"#;
        fs::write(&path, bytes).unwrap();
        let (report, code) = run_with(&Config::default(), &path, 100, false, || {
            panic!("idle path called ping")
        })
        .unwrap();
        assert_eq!(report.decision, "not_due");
        assert_eq!(code, 0);
        assert_eq!(report.state.unwrap().next_due_epoch, 101);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn oversized_state_is_corrupt_and_read_is_bounded() {
        let s = Sandbox::default();
        let path = s.0.join("auto.json");
        let bytes = format!(
            "{{\"last_ping_epoch\":0,\"next_due_epoch\":1000}}{}",
            " ".repeat(1024)
        );
        fs::write(&path, bytes).unwrap();
        let (report, _) = run_with(&Config::default(), &path, 100, true, || {
            panic!("dry-run called ping")
        })
        .unwrap();
        assert_eq!(report.decision, "due");
        assert_eq!(report.state.unwrap().next_due_epoch, 18100);
    }

    #[test]
    fn disabled_modes_do_not_read_state_or_run_pass() {
        let s = Sandbox::default();
        // A directory would make a state read fail; disabled modes must skip it.
        for auto_disabled in [false, true] {
            let mut config = Config::default();
            config.tokenmaxx.enabled = auto_disabled;
            config.tokenmaxx.auto = !auto_disabled;
            let (report, code) =
                run_with(&config, &s.0, 100, false, || panic!("disabled called ping")).unwrap();
            assert_eq!(
                report.decision,
                if auto_disabled {
                    "auto_disabled"
                } else {
                    "disabled"
                }
            );
            assert_eq!(code, 0);
            assert!(report.state.is_none());
            assert_eq!(fs::read_dir(&s.0).unwrap().count(), 0);
        }
    }

    #[test]
    fn due_dry_run_reports_would_be_state_without_calling_pass_or_writing() {
        let s = Sandbox::default();
        let path = s.0.join("auto.json");
        let mut config = Config::default();
        config.tokenmaxx.interval_hours = 3;
        let (report, code) =
            run_with(&config, &path, 100, true, || panic!("dry-run called ping")).unwrap();
        assert_eq!(report.decision, "due");
        assert!(report.dry_run);
        assert_eq!(
            report.state,
            Some(State {
                last_ping_epoch: 100,
                next_due_epoch: 10900
            })
        );
        assert_eq!(code, 0);
        assert!(!path.exists());
    }

    #[test]
    fn completed_failed_pass_preserves_state_and_exit_one() {
        let s = Sandbox::default();
        let path = s.0.join("auto.json");
        let bytes = br#"{"last_ping_epoch":0,"next_due_epoch":100}"#;
        fs::write(&path, bytes).unwrap();
        let (report, code) = run_with(&Config::default(), &path, 100, false, || Ok(1)).unwrap();
        assert_eq!(code, 1);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(report.state.unwrap().next_due_epoch, 100);
    }

    #[test]
    fn setup_failure_preserves_previous_state() {
        let s = Sandbox::default();
        let path = s.0.join("auto.json");
        let bytes = br#"{"last_ping_epoch":0,"next_due_epoch":100}"#;
        fs::write(&path, bytes).unwrap();
        let result = run_with(&Config::default(), &path, 100, false, || {
            Err("setup failed".into())
        });
        assert_eq!(result.unwrap_err(), "setup failed");
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}
