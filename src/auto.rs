//! Short-lived scheduling with bounded reconciliation of early Codex resets.

use std::{fs::File, io::Read, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    cli::Args,
    config::{Config, atomic_write},
    paths::Paths,
    quota::Account,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct State {
    last_ping_epoch: u64,
    next_due_epoch: u64,
    #[serde(default)]
    last_probe_epoch: u64,
    #[serde(default)]
    window_reset_epoch: u64,
}

#[derive(Debug, Serialize)]
struct Report {
    decision: &'static str,
    dry_run: bool,
    probe_due: bool,
    early_reset_detected: bool,
    stored_window_reset_epoch: Option<u64>,
    provider_reset_epoch: Option<u64>,
    state: Option<State>,
}

/// Resolve auth and fetch only when the local probe interval has elapsed.
fn probe(paths: &Paths, config: &Config) -> Result<Vec<Account>, String> {
    let dir = paths.resolve_auth(None, config.auth_dir())?;
    crate::quota::fetch(&dir, paths, config, &["codex".into()], false)
}

/// Evaluate local scheduling before resolving credentials or auth files.
pub fn run(args: &Args, paths: &Paths, config: &Config) -> Result<i32, String> {
    let now = crate::time::now().max(0) as u64;
    let (report, code) = run_with(
        config,
        &paths.auto_state_file(),
        now,
        args.dry_run,
        || probe(paths, config),
        || crate::tokenmaxx::run_output(args, paths, config, !args.json),
    )?;
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
            _ => println!(
                "auto: {}; {}",
                report.decision,
                if args.dry_run {
                    "would ping"
                } else {
                    "pass complete"
                }
            ),
        }
        if report.probe_due || args.dry_run {
            println!(
                "auto: probe_due={} stored_reset={:?} provider_reset={:?} early_reset={} next_due_epoch={} window_reset_epoch={} last_probe_epoch={}",
                report.probe_due,
                report.stored_window_reset_epoch,
                report.provider_reset_epoch,
                report.early_reset_detected,
                report.state.map_or(0, |s| s.next_due_epoch),
                report.state.map_or(0, |s| s.window_reset_epoch),
                report.state.map_or(0, |s| s.last_probe_epoch)
            );
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

fn probe_due(state: State, now: u64, minutes: u32) -> bool {
    minutes != 0
        && now
            >= state
                .last_probe_epoch
                .saturating_add(u64::from(minutes) * 60)
}

fn stored_reset(state: State) -> u64 {
    if state.window_reset_epoch == 0 {
        state.next_due_epoch
    } else {
        state.window_reset_epoch
    }
}

/// Use the earliest Codex 5h window, as the shared ping scheduler does.
/// Unhealthy accounts veto an early ping; unknown usage is never a fresh window.
fn reconciliation(state: State, now: u64, accounts: &[Account]) -> (Option<u64>, bool) {
    let window = accounts
        .iter()
        .filter(|a| a.provider == "codex")
        .flat_map(|a| &a.windows)
        .filter(|w| w.label == "5h")
        .filter_map(|w| {
            let reset = w.reset_at.as_ref()?;
            let epoch = reset
                .as_u64()
                .or_else(|| u64::try_from(crate::time::parse_iso(reset.as_str()?)?).ok())?;
            (epoch > 0).then_some((epoch, w.used_percent))
        })
        .min_by_key(|(reset, _)| *reset);
    let Some((reset, usage)) = window else {
        return (None, false);
    };
    let healthy = accounts
        .iter()
        .filter(|a| a.provider == "codex")
        .all(|a| a.error.is_none() && a.limit_reached != Some(true) && a.allowed != Some(false));
    (
        Some(reset),
        healthy
            && reset > now
            && reset.saturating_sub(stored_reset(state)) > 60
            && usage.is_some_and(|used| (0.0..=1.0).contains(&used)),
    )
}

fn run_with(
    config: &Config,
    path: &Path,
    now: u64,
    dry_run: bool,
    probe: impl FnOnce() -> Result<Vec<Account>, String>,
    ping: impl FnOnce() -> Result<i32, String>,
) -> Result<(Report, i32), String> {
    let mut report = Report {
        decision: "disabled",
        dry_run,
        probe_due: false,
        early_reset_detected: false,
        stored_window_reset_epoch: None,
        provider_reset_epoch: None,
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
    if let Some(mut state) = report.state.filter(|s| now < s.next_due_epoch) {
        report.decision = "not_due";
        report.stored_window_reset_epoch = Some(stored_reset(state));
        report.probe_due = probe_due(state, now, config.tokenmaxx.probe_minutes);
        if !report.probe_due {
            return Ok((report, 0));
        }
        if let Ok(accounts) = probe() {
            (report.provider_reset_epoch, report.early_reset_detected) =
                reconciliation(state, now, &accounts);
        }
        state.last_probe_epoch = now;
        // Persist the attempt even on provider errors or a failed keep-alive.
        if !dry_run {
            write_state(path, state)?;
        }
        report.state = Some(state);
        if !report.early_reset_detected {
            return Ok((report, 0));
        }
        report.decision = "early_reset";
        if dry_run {
            report.state = Some(scheduled_state(
                now,
                config.tokenmaxx.interval_hours,
                report.provider_reset_epoch,
            )?);
            return Ok((report, 0));
        }
        let code = ping()?;
        if code == 0 {
            // Keep the detected window even if the post-ping snapshot is unavailable.
            report.state = Some(scheduled_state(
                read_state(path)?.map_or(now, |saved| saved.last_ping_epoch),
                config.tokenmaxx.interval_hours,
                report.provider_reset_epoch,
            )?);
            if let Some(updated) = &mut report.state {
                updated.last_probe_epoch = now;
                write_state(path, *updated)?;
            }
        }
        return Ok((report, code));
    }
    report.decision = "due";
    let code = if dry_run {
        report.state = Some(scheduled_state(now, config.tokenmaxx.interval_hours, None)?);
        0
    } else {
        let code = ping()?;
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
        last_probe_epoch: now,
        window_reset_epoch: reset_epoch.unwrap_or(0),
    })
}

fn write_state(path: &Path, state: State) -> Result<(), String> {
    let bytes = serde_json::to_vec(&state).map_err(|_| "Cannot serialize auto state")?;
    atomic_write(path, &bytes).map_err(|_| "Cannot save auto state atomically".into())
}

/// Persist the shared schedule only after the caller has observed a successful ping.
pub(crate) fn record_success(
    path: &Path,
    now: u64,
    interval_hours: u32,
    reset_epoch: Option<u64>,
) -> Result<(), String> {
    write_state(path, scheduled_state(now, interval_hours, reset_epoch)?)
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

    fn run_local(
        config: &Config,
        path: &Path,
        now: u64,
        dry_run: bool,
        ping: impl FnOnce() -> Result<i32, String>,
    ) -> Result<(Report, i32), String> {
        run_with(
            config,
            path,
            now,
            dry_run,
            || panic!("unexpected probe"),
            ping,
        )
    }

    #[test]
    fn missing_state_is_due_and_writes_private_state_after_one_pass() {
        let s = Sandbox::default();
        let path = s.0.join("tokenmaxx/auto.json");
        let calls = Cell::new(0);
        let (report, code) = run_local(&Config::default(), &path, 100, false, || {
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
                next_due_epoch: 18100,
                last_probe_epoch: 100,
                window_reset_epoch: 0
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
        let (report, code) = run_local(&Config::default(), &path, 100, false, || {
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
                next_due_epoch: 500,
                last_probe_epoch: 101,
                window_reset_epoch: 500
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
            let (report, _) = run_local(&Config::default(), &path, 100, false, || {
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
        let (report, _) = run_local(&Config::default(), &path, 100, false, || {
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
        let (report, code) = run_local(&Config::default(), &path, 100, false, || {
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
        let (report, _) = run_local(&Config::default(), &path, 100, true, || {
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
                run_local(&config, &s.0, 100, false, || panic!("disabled called ping")).unwrap();
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
            run_local(&config, &path, 100, true, || panic!("dry-run called ping")).unwrap();
        assert_eq!(report.decision, "due");
        assert!(report.dry_run);
        assert_eq!(
            report.state,
            Some(State {
                last_ping_epoch: 100,
                next_due_epoch: 10900,
                last_probe_epoch: 100,
                window_reset_epoch: 0
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
        let (report, code) = run_local(&Config::default(), &path, 100, false, || Ok(1)).unwrap();
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
        let result = run_local(&Config::default(), &path, 100, false, || {
            Err("setup failed".into())
        });
        assert_eq!(result.unwrap_err(), "setup failed");
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    fn future_state() -> State {
        State {
            last_ping_epoch: 1000,
            next_due_epoch: 20000,
            last_probe_epoch: 1000,
            window_reset_epoch: 20000,
        }
    }

    fn account(reset: serde_json::Value, usage: Option<f64>) -> Account {
        Account {
            provider: "codex".into(),
            allowed: Some(true),
            limit_reached: Some(false),
            windows: vec![crate::quota::Window {
                label: "5h".into(),
                used_percent: usage,
                reset_at: Some(reset),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn probe_interval_bounds_and_zero_disables() {
        let state = future_state();
        assert!(!probe_due(state, 1899, 15));
        assert!(probe_due(state, 1900, 15));
        assert!(probe_due(state, 2000, 15));
        assert!(!probe_due(state, u64::MAX, 0));
    }

    #[test]
    fn early_reset_requires_later_fresh_healthy_five_hour_window() {
        let state = future_state();
        for (reset, usage, expected) in [
            (21000, Some(0.0), true),
            (21000, Some(1.0), true),
            (20060, Some(0.0), false),
            (20061, Some(0.0), true),
            (20000, Some(0.0), false),
            (19000, Some(0.0), false),
            (21000, Some(1.01), false),
            (21000, Some(90.0), false),
            (21000, Some(-1.0), false),
            (21000, Some(f64::NAN), false),
            (21000, None, false),
        ] {
            let a = account(reset.into(), usage);
            assert_eq!(reconciliation(state, 2000, &[a]), (Some(reset), expected));
        }
        let a = account(21000.into(), Some(0.0));
        assert_eq!(
            reconciliation(state, 22000, std::slice::from_ref(&a)),
            (Some(21000), false)
        );
        for kind in ["error", "limit", "blocked", "weekly", "other"] {
            let mut a = a.clone();
            match kind {
                "error" => a.error = Some("HTTP 429".into()),
                "limit" => a.limit_reached = Some(true),
                "blocked" => a.allowed = Some(false),
                "weekly" => a.windows[0].label = "7d".into(),
                _ => a.provider = "claude".into(),
            }
            assert!(!reconciliation(state, 2000, &[a]).1, "{kind}");
        }
        let legacy = State {
            window_reset_epoch: 0,
            ..state
        };
        assert!(reconciliation(legacy, 2000, std::slice::from_ref(&a)).1);
        let mut iso = a.clone();
        iso.windows[0].reset_at = Some("1970-01-01T05:50:00Z".into());
        assert_eq!(reconciliation(state, 2000, &[iso]), (Some(21000), true));
        let mut limited = a.clone();
        limited.limit_reached = Some(true);
        assert!(!reconciliation(state, 2000, &[a.clone(), limited]).1);
        assert_eq!(
            reconciliation(state, 2000, &[a, account(20000.into(), Some(0.0))]),
            (Some(20000), false)
        );
    }

    #[test]
    fn probe_outcomes_preserve_schedule_and_advance_only_probe_on_no_early_reset() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        let state = future_state();
        let outcomes = [
            Err("provider unavailable".into()),
            Ok(vec![account(20000.into(), Some(0.0))]),
            Ok(vec![account(21000.into(), Some(90.0))]),
            Ok(vec![Account {
                limit_reached: Some(true),
                ..account(21000.into(), Some(0.0))
            }]),
            Ok(vec![Account {
                error: Some("HTTP 429".into()),
                ..account(21000.into(), Some(0.0))
            }]),
        ];
        for outcome in outcomes {
            write_state(&path, state).unwrap();
            let (report, code) = run_with(
                &Config::default(),
                &path,
                2000,
                false,
                || outcome,
                || panic!("must not ping"),
            )
            .unwrap();
            let expected = State {
                last_probe_epoch: 2000,
                ..state
            };
            assert_eq!(code, 0);
            assert!(report.probe_due);
            assert!(!report.early_reset_detected);
            assert_eq!(report.decision, "not_due");
            assert_eq!(report.state, Some(expected));
            assert_eq!(read_state(&path).unwrap(), Some(expected));
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(fs::read_dir(&sandbox.0).unwrap().count(), 1);
        }
    }

    #[test]
    fn early_reset_pings_once_and_only_success_changes_schedule() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        let state = future_state();
        for (dry_run, ping_code) in [(true, 0), (false, 0), (false, 1)] {
            write_state(&path, state).unwrap();
            let before = fs::read(&path).unwrap();
            let calls = Cell::new(0);
            let (report, code) = run_with(
                &Config::default(),
                &path,
                2000,
                dry_run,
                || Ok(vec![account(21000.into(), Some(0.0))]),
                || {
                    calls.set(calls.get() + 1);
                    if ping_code == 0 {
                        record_success(&path, 2001, 5, None)?;
                    }
                    Ok(ping_code)
                },
            )
            .unwrap();
            assert_eq!(code, ping_code);
            assert_eq!(calls.get(), usize::from(!dry_run));
            assert_eq!(report.decision, "early_reset");
            assert!(report.early_reset_detected);
            assert_eq!(report.provider_reset_epoch, Some(21000));
            assert_eq!(report.stored_window_reset_epoch, Some(20000));
            let after = report.state.unwrap();
            assert_eq!(after.last_probe_epoch, 2000);
            assert_eq!(
                after.next_due_epoch,
                if ping_code == 0 { 21000 } else { 20000 }
            );
            assert_eq!(
                after.window_reset_epoch,
                if ping_code == 0 { 21000 } else { 20000 }
            );
            if dry_run {
                assert_eq!(fs::read(&path).unwrap(), before);
            } else {
                assert_eq!(read_state(&path).unwrap(), report.state);
            }
        }
    }

    #[test]
    fn disabled_or_not_due_probe_never_fetches_or_writes() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        let state = future_state();
        write_state(&path, state).unwrap();
        let before = fs::read(&path).unwrap();
        for (minutes, now) in [(0, 2000), (15, 1899)] {
            let mut config = Config::default();
            config.tokenmaxx.probe_minutes = minutes;
            let (report, code) = run_with(
                &config,
                &path,
                now,
                false,
                || panic!("must not probe"),
                || panic!("must not ping"),
            )
            .unwrap();
            assert_eq!(code, 0);
            assert_eq!(report.decision, "not_due");
            assert!(!report.probe_due);
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }
}
