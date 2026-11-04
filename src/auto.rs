//! Short-lived scheduling with bounded reconciliation of early Codex resets.

use std::{fs::File, io::Read, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    cli::Args,
    config::{Config, atomic_write},
    paths::Paths,
    quota::{Account, Window},
};

/// Usage at or above this percentage means the window is spent.
const CAPPED_PERCENT: f64 = 99.5;
/// A new window at or below this percentage of usage is still fresh.
const FRESH_PERCENT: f64 = 1.0;
/// A reset that moved by more than this many seconds belongs to a new window.
const RESET_SLACK_SECONDS: u64 = 60;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct State {
    last_ping_epoch: u64,
    next_due_epoch: u64,
    #[serde(default)]
    last_probe_epoch: u64,
    /// Earliest Codex 5h reset seen: the keep-alive anchor.
    #[serde(default)]
    window_reset_epoch: u64,
    /// Earliest Codex weekly/monthly reset seen: the long-window anchor.
    #[serde(default)]
    long_window_reset_epoch: u64,
}

/// Scheduling facts learned from one Codex quota snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Schedule {
    /// Earliest moment a Codex account can serve again (0 when unknown).
    pub(crate) next_due_epoch: u64,
    /// Earliest 5h reset (0 when unknown).
    pub(crate) window_reset_epoch: u64,
    /// Earliest weekly/monthly reset (0 when unknown).
    pub(crate) long_window_reset_epoch: u64,
}

#[derive(Debug, Serialize)]
struct Report {
    decision: &'static str,
    dry_run: bool,
    probe_due: bool,
    early_reset_detected: bool,
    stored_window_reset_epoch: Option<u64>,
    provider_reset_epoch: Option<u64>,
    stored_long_window_reset_epoch: Option<u64>,
    provider_long_reset_epoch: Option<u64>,
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
            "deferred" => {
                if let Some(state) = report.state {
                    println!(
                        "auto: deferred until {} (in {}m; every Codex window is spent)",
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
                "auto: probe_due={} stored_5h={:?} provider_5h={:?} stored_long={:?} provider_long={:?} early_reset={} next_due_epoch={} window_reset_epoch={} long_window_reset_epoch={} last_probe_epoch={}",
                report.probe_due,
                report.stored_window_reset_epoch,
                report.provider_reset_epoch,
                report.stored_long_window_reset_epoch,
                report.provider_long_reset_epoch,
                report.early_reset_detected,
                report.state.map_or(0, |s| s.next_due_epoch),
                report.state.map_or(0, |s| s.window_reset_epoch),
                report.state.map_or(0, |s| s.long_window_reset_epoch),
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

fn epoch_of(window: &Window) -> Option<u64> {
    let reset = window.reset_at.as_ref()?;
    let epoch = reset
        .as_u64()
        .or_else(|| u64::try_from(crate::time::parse_iso(reset.as_str()?)?).ok())?;
    (epoch > 0).then_some(epoch)
}

fn is_five_hour(window: &Window) -> bool {
    window.label == "5h"
}

/// A spent window keeps the account down until its reset, when that is ahead.
fn capped_at(window: &Window, now: u64) -> Option<u64> {
    if !window
        .used_percent
        .is_some_and(|usage| usage >= CAPPED_PERCENT)
    {
        return None;
    }
    epoch_of(window).filter(|reset| *reset > now)
}

/// A window that just opened: almost no usage.
fn fresh(window: &Window) -> bool {
    window
        .used_percent
        .is_some_and(|usage| (0.0..=FRESH_PERCENT).contains(&usage))
}

/// Earliest moment this account can serve a request again.
/// An account capped by two windows needs both to reset, so the latest capped
/// reset wins; otherwise the next 5h opening is the moment to ping.
fn available_at(account: &Account, now: u64) -> Option<u64> {
    if account.error.is_some() {
        return None;
    }
    let capped = account
        .windows
        .iter()
        .filter_map(|window| capped_at(window, now))
        .max();
    if capped.is_some() {
        return capped;
    }
    if account.limit_reached == Some(true) || account.allowed == Some(false) {
        return account
            .windows
            .iter()
            .filter_map(epoch_of)
            .filter(|reset| *reset > now)
            .min();
    }
    account
        .windows
        .iter()
        .filter(|window| is_five_hour(window))
        .filter_map(epoch_of)
        .filter(|reset| *reset > now)
        .min()
}

/// Scheduling facts for every Codex account in one snapshot.
pub(crate) fn schedule_from(accounts: &[Account], now: u64) -> Option<Schedule> {
    let codex = || {
        accounts
            .iter()
            .filter(|account| account.provider == "codex")
    };
    let earliest = |five_hour: bool| {
        codex()
            .flat_map(|account| account.windows.iter())
            .filter(|window| is_five_hour(window) == five_hour)
            .filter_map(epoch_of)
            .min()
    };
    let schedule = Schedule {
        next_due_epoch: codex()
            .filter_map(|account| available_at(account, now))
            .min()
            .unwrap_or(0),
        window_reset_epoch: earliest(true).unwrap_or(0),
        long_window_reset_epoch: earliest(false).unwrap_or(0),
    };
    (schedule != Schedule::default()).then_some(schedule)
}

/// Compare the snapshot with both stored anchors.
///
/// Each class uses its earliest reset, the same window the shared schedule
/// follows. A fresh window whose reset moved later belongs to an account that
/// can serve a ping now; blocked accounts are skipped, so a weekly cap cannot
/// trigger an early ping while its 5h window keeps rolling.
fn reconciliation(
    state: State,
    now: u64,
    accounts: &[Account],
) -> (Option<u64>, Option<u64>, bool) {
    let mut provider_short: Option<u64> = None;
    let mut provider_long: Option<u64> = None;
    let mut candidate_short: Option<(u64, bool)> = None;
    let mut candidate_long: Option<(u64, bool)> = None;
    for account in accounts
        .iter()
        .filter(|account| account.provider == "codex" && account.error.is_none())
    {
        let blocked = account.limit_reached == Some(true)
            || account.allowed == Some(false)
            || account
                .windows
                .iter()
                .any(|window| capped_at(window, now).is_some());
        for window in &account.windows {
            let Some(reset) = epoch_of(window) else {
                continue;
            };
            let five_hour = is_five_hour(window);
            let (provider, candidate) = if five_hour {
                (&mut provider_short, &mut candidate_short)
            } else {
                (&mut provider_long, &mut candidate_long)
            };
            *provider = Some((*provider).map_or(reset, |seen| seen.min(reset)));
            if blocked {
                continue;
            }
            if candidate.is_none_or(|(seen, _)| reset < seen) {
                *candidate = Some((reset, fresh(window)));
            }
        }
    }
    let early = |candidate: Option<(u64, bool)>, anchor: u64| {
        candidate.is_some_and(|(reset, fresh)| {
            fresh && anchor > 0 && reset > anchor.saturating_add(RESET_SLACK_SECONDS) && reset > now
        })
    };
    (
        provider_short,
        provider_long,
        early(candidate_short, stored_reset(state))
            || early(candidate_long, state.long_window_reset_epoch),
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
        stored_long_window_reset_epoch: None,
        provider_long_reset_epoch: None,
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
        report.stored_window_reset_epoch = Some(stored_reset(state)).filter(|reset| *reset > 0);
        report.stored_long_window_reset_epoch =
            Some(state.long_window_reset_epoch).filter(|reset| *reset > 0);
        report.probe_due = probe_due(state, now, config.tokenmaxx.probe_minutes);
        if !report.probe_due {
            return Ok((report, 0));
        }
        if let Ok(accounts) = probe() {
            (
                report.provider_reset_epoch,
                report.provider_long_reset_epoch,
                report.early_reset_detected,
            ) = reconciliation(state, now, &accounts);
            // The provider can also reopen earlier than the stored plan.
            if !report.early_reset_detected
                && let Some(schedule) = schedule_from(&accounts, now)
                && schedule.next_due_epoch > now
                && schedule.next_due_epoch < state.next_due_epoch
            {
                state.next_due_epoch = schedule.next_due_epoch;
            }
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
                schedule_for_reset(
                    report.provider_reset_epoch,
                    report.provider_long_reset_epoch,
                ),
            )?);
            return Ok((report, 0));
        }
        let code = ping()?;
        if code == 0 {
            // The pass re-anchors both windows from its own post-ping snapshot;
            // fall back to the detected window when that snapshot was unavailable.
            let saved = read_state(path)?;
            report.state = Some(match saved.filter(|state| state.window_reset_epoch > 0) {
                Some(state) => {
                    let mut state = state;
                    state.last_probe_epoch = now;
                    write_state(path, state)?;
                    state
                }
                None => {
                    let mut state = scheduled_state(
                        saved.map_or(now, |state| state.last_ping_epoch),
                        config.tokenmaxx.interval_hours,
                        schedule_for_reset(
                            report.provider_reset_epoch,
                            report.provider_long_reset_epoch,
                        ),
                    )?;
                    state.last_probe_epoch = now;
                    write_state(path, state)?;
                    state
                }
            });
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
        if code != 0
            && let Some(deferred) = defer_while_capped(path, now, report.state, probe())?
        {
            report.state = Some(deferred);
            report.decision = "deferred";
        }
        code
    };
    Ok((report, code))
}

/// Anchor a ping on the 5h chain: a long-window reset must never park the
/// keep-alive for a whole week.
fn schedule_for_reset(short: Option<u64>, long: Option<u64>) -> Option<Schedule> {
    let schedule = Schedule {
        next_due_epoch: short.unwrap_or(0),
        window_reset_epoch: short.unwrap_or(0),
        long_window_reset_epoch: long.unwrap_or(0),
    };
    (schedule != Schedule::default()).then_some(schedule)
}

/// A failed pass with every Codex account spent is not worth a five-minute
/// retry: wait for the earliest reopening instead.
fn defer_while_capped(
    path: &Path,
    now: u64,
    previous: Option<State>,
    accounts: Result<Vec<Account>, String>,
) -> Result<Option<State>, String> {
    let Ok(accounts) = accounts else {
        return Ok(None);
    };
    let codex: Vec<&Account> = accounts
        .iter()
        .filter(|account| account.provider == "codex")
        .collect();
    if codex.is_empty() {
        return Ok(None);
    }
    let blocked = codex.iter().all(|account| {
        account.error.is_some()
            || account
                .windows
                .iter()
                .any(|window| capped_at(window, now).is_some())
    });
    if !blocked {
        return Ok(None);
    }
    let Some(schedule) = schedule_from(&accounts, now)
        .filter(|schedule| schedule.next_due_epoch > now.saturating_add(RESET_SLACK_SECONDS))
    else {
        return Ok(None);
    };
    let state = State {
        last_ping_epoch: previous.map_or(0, |state| state.last_ping_epoch),
        next_due_epoch: schedule.next_due_epoch,
        last_probe_epoch: now,
        window_reset_epoch: schedule.window_reset_epoch,
        long_window_reset_epoch: schedule.long_window_reset_epoch,
    };
    write_state(path, state)?;
    Ok(Some(state))
}

fn scheduled_state(
    now: u64,
    interval_hours: u32,
    schedule: Option<Schedule>,
) -> Result<State, String> {
    let schedule = schedule.filter(|schedule| schedule.next_due_epoch > 0);
    Ok(State {
        last_ping_epoch: now,
        next_due_epoch: match schedule {
            Some(schedule) => schedule.next_due_epoch,
            None => now
                .checked_add(u64::from(interval_hours) * 3600)
                .ok_or("Auto schedule overflow")?,
        },
        last_probe_epoch: now,
        window_reset_epoch: schedule.map_or(0, |schedule| schedule.window_reset_epoch),
        long_window_reset_epoch: schedule.map_or(0, |schedule| schedule.long_window_reset_epoch),
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
    schedule: Option<Schedule>,
) -> Result<(), String> {
    write_state(path, scheduled_state(now, interval_hours, schedule)?)
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
            || Err("no provider".into()),
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
                window_reset_epoch: 0,
                long_window_reset_epoch: 0
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
            record_success(
                &path,
                101,
                5,
                Some(Schedule {
                    next_due_epoch: 500,
                    window_reset_epoch: 500,
                    long_window_reset_epoch: 0,
                }),
            )?;
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
                window_reset_epoch: 500,
                long_window_reset_epoch: 0
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
                window_reset_epoch: 0,
                long_window_reset_epoch: 0
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
            long_window_reset_epoch: 0,
        }
    }

    fn state_with_due(next_due_epoch: u64) -> State {
        State {
            last_ping_epoch: 500,
            next_due_epoch,
            last_probe_epoch: 100,
            window_reset_epoch: next_due_epoch,
            long_window_reset_epoch: 0,
        }
    }

    fn window(label: &str, reset: u64, usage: f64) -> Window {
        Window {
            label: label.into(),
            used_percent: Some(usage),
            reset_at: Some(serde_json::json!(reset)),
            reset_after: None,
        }
    }

    fn codex(windows: Vec<Window>) -> Account {
        Account {
            provider: "codex".into(),
            allowed: Some(true),
            limit_reached: Some(false),
            windows,
            ..Default::default()
        }
    }

    fn account(reset: serde_json::Value, usage: Option<f64>) -> Account {
        Account {
            provider: "codex".into(),
            allowed: Some(true),
            limit_reached: Some(false),
            windows: vec![Window {
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
    fn early_reset_requires_later_fresh_five_hour_window() {
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
            (21000, Some(100.0), false),
            (21000, Some(-1.0), false),
            (21000, Some(f64::NAN), false),
            (21000, None, false),
        ] {
            let a = account(reset.into(), usage);
            assert_eq!(
                reconciliation(state, 2000, &[a]),
                (Some(reset), None, expected)
            );
        }
        let a = account(21000.into(), Some(0.0));
        assert_eq!(
            reconciliation(state, 22000, std::slice::from_ref(&a)),
            (Some(21000), None, false)
        );
        for kind in ["error", "limit", "blocked", "other"] {
            let mut a = a.clone();
            match kind {
                "error" => a.error = Some("HTTP 429".into()),
                "limit" => a.limit_reached = Some(true),
                "blocked" => a.allowed = Some(false),
                _ => a.provider = "claude".into(),
            }
            assert!(!reconciliation(state, 2000, &[a]).2, "{kind}");
        }
        // A weekly window is compared with the long anchor, never the 5h one.
        let weekly = codex(vec![window("7d", 21000, 0.0)]);
        assert_eq!(
            reconciliation(state, 2000, std::slice::from_ref(&weekly)),
            (None, Some(21000), false)
        );
        let mut anchored = state;
        anchored.long_window_reset_epoch = 20000;
        assert!(reconciliation(anchored, 2000, std::slice::from_ref(&weekly)).2);
        let legacy = State {
            window_reset_epoch: 0,
            ..state
        };
        assert!(reconciliation(legacy, 2000, std::slice::from_ref(&a)).2);
        let mut iso = a.clone();
        iso.windows[0].reset_at = Some("1970-01-01T05:50:00Z".into());
        assert_eq!(
            reconciliation(state, 2000, &[iso]),
            (Some(21000), None, true)
        );
        // A spent account is skipped, but does not veto the healthy one.
        let mut limited = a.clone();
        limited.limit_reached = Some(true);
        assert!(reconciliation(state, 2000, &[a.clone(), limited]).2);
        assert_eq!(
            reconciliation(state, 2000, &[a, account(20000.into(), Some(0.0))]),
            (Some(20000), None, false)
        );
    }

    #[test]
    fn schedule_uses_the_earliest_five_hour_reset_from_codex_accounts_only() {
        let mut accounts = vec![
            Account {
                provider: "claude".into(),
                windows: vec![window("5h", 1, 0.0)],
                ..Account::default()
            },
            codex(vec![window("7d", 2, 0.0), window("5h", 500, 40.0)]),
            Account {
                provider: "codex".into(),
                windows: vec![Window {
                    label: "5h".into(),
                    used_percent: Some(0.0),
                    reset_at: Some(serde_json::json!("1970-01-01T00:05:00Z")),
                    ..Window::default()
                }],
                ..Account::default()
            },
        ];
        assert_eq!(
            schedule_from(&accounts, 0),
            Some(Schedule {
                next_due_epoch: 300,
                window_reset_epoch: 300,
                long_window_reset_epoch: 2
            })
        );
        accounts[2].windows[0].reset_at = Some(serde_json::json!(-1));
        assert_eq!(schedule_from(&accounts, 0).unwrap().next_due_epoch, 500);
        accounts[1].windows[1].reset_at = None;
        assert_eq!(
            schedule_from(&accounts, 0),
            Some(Schedule {
                next_due_epoch: 0,
                window_reset_epoch: 0,
                long_window_reset_epoch: 2
            })
        );
        assert_eq!(
            schedule_from(
                &[Account {
                    provider: "claude".into(),
                    windows: vec![window("5h", 10, 0.0)],
                    ..Account::default()
                }],
                0
            ),
            None
        );
    }

    #[test]
    fn weekly_cap_waits_for_the_long_window_instead_of_the_five_hour_reset() {
        let now = 2000;
        let team = codex(vec![window("5h", 5000, 0.0), window("7d", 9000, 100.0)]);
        let free = codex(vec![window("30d", 50000, 100.0)]);
        assert_eq!(
            schedule_from(&[team.clone(), free.clone()], now),
            Some(Schedule {
                next_due_epoch: 9000,
                window_reset_epoch: 5000,
                long_window_reset_epoch: 9000
            })
        );
        // A single account needs both of its spent windows to reset.
        let both = codex(vec![window("5h", 5000, 100.0), window("7d", 9000, 100.0)]);
        assert_eq!(available_at(&both, now), Some(9000));
        // Once the weekly window reopens the 5h chain resumes as usual.
        let reopened = codex(vec![window("5h", 5000, 0.0), window("7d", 9000, 0.0)]);
        assert_eq!(available_at(&reopened, now), Some(5000));
        // The weekly reset can land before the 5h one: the account waits for the
        // weekly reset even though its fresh 5h window would reopen later.
        let weekly_first = codex(vec![window("5h", 20000, 0.0), window("7d", 2360, 100.0)]);
        assert_eq!(available_at(&weekly_first, now), Some(2360));
        assert_eq!(
            schedule_from(&[weekly_first], now),
            Some(Schedule {
                next_due_epoch: 2360,
                window_reset_epoch: 20000,
                long_window_reset_epoch: 2360
            })
        );
        // A 5h cap still holding after the weekly reset keeps the account down.
        let both_soon = codex(vec![window("5h", 9200, 100.0), window("7d", 2360, 100.0)]);
        assert_eq!(available_at(&both_soon, now), Some(9200));
        // A spent window whose reset already passed no longer blocks.
        let stale = codex(vec![window("7d", 1500, 100.0), window("5h", 5000, 0.0)]);
        assert_eq!(available_at(&stale, now), Some(5000));
        // Accounts without provider data are not availability signals.
        let mut broken = team.clone();
        broken.error = Some("HTTP 401".into());
        assert_eq!(available_at(&broken, now), None);
        assert_eq!(
            schedule_from(&[broken], now).map(|s| s.next_due_epoch),
            Some(0)
        );
    }

    #[test]
    fn failed_pass_defers_to_the_weekly_reset_when_every_account_is_spent() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        fs::write(
            &path,
            br#"{"last_ping_epoch":500,"next_due_epoch":1500,"last_probe_epoch":100,"window_reset_epoch":1500}"#,
        )
        .unwrap();
        let team = codex(vec![window("5h", 5000, 0.0), window("7d", 9000, 100.0)]);
        let free = codex(vec![window("30d", 50000, 100.0)]);
        let (report, code) = run_with(
            &Config::default(),
            &path,
            2000,
            false,
            || Ok(vec![team.clone(), free.clone()]),
            || Ok(1),
        )
        .unwrap();
        assert_eq!(code, 1);
        assert_eq!(report.decision, "deferred");
        let expected = State {
            last_ping_epoch: 500,
            next_due_epoch: 9000,
            last_probe_epoch: 2000,
            window_reset_epoch: 5000,
            long_window_reset_epoch: 9000,
        };
        assert_eq!(report.state, Some(expected));
        assert_eq!(read_state(&path).unwrap(), Some(expected));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Live shape: the weekly reset is minutes away while the fresh 5h window
        // would reopen hours later, so the deferral must land on the weekly reset.
        write_state(&path, state_with_due(1500)).unwrap();
        let team = codex(vec![window("5h", 20000, 0.0), window("7d", 2360, 100.0)]);
        let (report, code) = run_with(
            &Config::default(),
            &path,
            2000,
            false,
            || Ok(vec![team]),
            || Ok(1),
        )
        .unwrap();
        assert_eq!(code, 1);
        assert_eq!(report.decision, "deferred");
        assert_eq!(read_state(&path).unwrap().unwrap().next_due_epoch, 2360);
    }

    #[test]
    fn failed_pass_retries_next_tick_when_an_account_can_still_serve() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        let bytes =
            br#"{"last_ping_epoch":500,"next_due_epoch":1500,"last_probe_epoch":100,"window_reset_epoch":1500}"#;
        fs::write(&path, bytes).unwrap();
        let healthy = codex(vec![window("5h", 5000, 40.0)]);
        for probe in [
            Ok(vec![healthy.clone()]),
            Ok(vec![
                healthy.clone(),
                codex(vec![window("7d", 9000, 100.0)]),
            ]),
            Err("provider unavailable".to_string()),
            Ok(vec![Account {
                provider: "codex".into(),
                error: Some("HTTP 401".into()),
                ..Account::default()
            }]),
        ] {
            let (report, code) = run_with(
                &Config::default(),
                &path,
                2000,
                false,
                || probe.clone(),
                || Ok(1),
            )
            .unwrap();
            assert_eq!(code, 1);
            assert_eq!(report.decision, "due");
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn early_weekly_reset_pings_and_reanchors_both_windows() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        let state = State {
            long_window_reset_epoch: 20000,
            ..future_state()
        };
        write_state(&path, state).unwrap();
        let calls = Cell::new(0);
        let (report, code) = run_with(
            &Config::default(),
            &path,
            2000,
            false,
            || {
                Ok(vec![
                    codex(vec![window("5h", 21000, 0.0), window("7d", 40000, 0.0)]),
                    codex(vec![window("30d", 50000, 100.0)]),
                ])
            },
            || {
                calls.set(calls.get() + 1);
                record_success(
                    &path,
                    2001,
                    5,
                    Some(Schedule {
                        next_due_epoch: 21000,
                        window_reset_epoch: 21000,
                        long_window_reset_epoch: 40000,
                    }),
                )?;
                Ok(0)
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(code, 0);
        assert_eq!(report.decision, "early_reset");
        assert!(report.early_reset_detected);
        assert_eq!(report.provider_reset_epoch, Some(21000));
        assert_eq!(report.provider_long_reset_epoch, Some(40000));
        assert_eq!(report.stored_window_reset_epoch, Some(20000));
        assert_eq!(report.stored_long_window_reset_epoch, Some(20000));
        let expected = State {
            last_ping_epoch: 2001,
            next_due_epoch: 21000,
            last_probe_epoch: 2000,
            window_reset_epoch: 21000,
            long_window_reset_epoch: 40000,
        };
        assert_eq!(report.state, Some(expected));
        assert_eq!(read_state(&path).unwrap(), Some(expected));
    }

    #[test]
    fn spent_weekly_account_never_triggers_an_early_ping_while_its_five_hour_window_rolls() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        let state = State {
            long_window_reset_epoch: 20000,
            ..future_state()
        };
        write_state(&path, state).unwrap();
        let before = fs::read(&path).unwrap();
        let (report, code) = run_with(
            &Config::default(),
            &path,
            2000,
            false,
            || {
                Ok(vec![codex(vec![
                    window("5h", 21000, 0.0),
                    window("7d", 40000, 100.0),
                ])])
            },
            || panic!("spent account must not be pinged"),
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(!report.early_reset_detected);
        assert_eq!(report.decision, "not_due");
        assert_eq!(report.provider_long_reset_epoch, Some(40000));
        assert_eq!(read_state(&path).unwrap().unwrap().last_probe_epoch, 2000);
        assert_ne!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn probe_tightens_a_later_due_time_when_the_provider_reopens_sooner() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        write_state(&path, future_state()).unwrap();
        let (report, code) = run_with(
            &Config::default(),
            &path,
            2000,
            false,
            || Ok(vec![codex(vec![window("5h", 19000, 40.0)])]),
            || panic!("must not ping"),
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(report.decision, "not_due");
        assert!(!report.early_reset_detected);
        assert_eq!(read_state(&path).unwrap().unwrap().next_due_epoch, 19000);
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
                        record_success(
                            &path,
                            2001,
                            5,
                            Some(Schedule {
                                next_due_epoch: 21000,
                                window_reset_epoch: 21000,
                                long_window_reset_epoch: 0,
                            }),
                        )?;
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
    fn early_reset_keeps_the_post_ping_schedule_from_the_pass() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        write_state(&path, future_state()).unwrap();
        let (report, code) = run_with(
            &Config::default(),
            &path,
            2000,
            false,
            || Ok(vec![account(21000.into(), Some(0.0))]),
            || {
                // The pass re-anchors on its own snapshot after the ping.
                record_success(
                    &path,
                    2001,
                    5,
                    Some(Schedule {
                        next_due_epoch: 25000,
                        window_reset_epoch: 25000,
                        long_window_reset_epoch: 90000,
                    }),
                )?;
                Ok(0)
            },
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(report.decision, "early_reset");
        let expected = State {
            last_ping_epoch: 2001,
            next_due_epoch: 25000,
            last_probe_epoch: 2000,
            window_reset_epoch: 25000,
            long_window_reset_epoch: 90000,
        };
        assert_eq!(report.state, Some(expected));
        assert_eq!(read_state(&path).unwrap(), Some(expected));
    }

    #[test]
    fn record_success_uses_the_schedule_and_falls_back_to_the_interval() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        record_success(
            &path,
            100,
            5,
            Some(Schedule {
                next_due_epoch: 7000,
                window_reset_epoch: 6000,
                long_window_reset_epoch: 7000,
            }),
        )
        .unwrap();
        assert_eq!(
            read_state(&path).unwrap(),
            Some(State {
                last_ping_epoch: 100,
                next_due_epoch: 7000,
                last_probe_epoch: 100,
                window_reset_epoch: 6000,
                long_window_reset_epoch: 7000
            })
        );
        record_success(&path, 100, 5, Some(Schedule::default())).unwrap();
        assert_eq!(read_state(&path).unwrap().unwrap().next_due_epoch, 18100);
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
