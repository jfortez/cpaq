//! Short-lived scheduling with bounded reconciliation of early Codex resets.

use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    cli::Args,
    config::{Config, atomic_write},
    journal,
    paths::Paths,
    quota::{Account, Credits, Window},
};

/// Usage at or above this percentage means the window is spent.
const CAPPED_PERCENT: f64 = 99.5;
/// A new window at or below this percentage of usage is still fresh.
const FRESH_PERCENT: f64 = 1.0;
/// A reset that moved by more than this many seconds belongs to a new window.
const RESET_SLACK_SECONDS: u64 = 60;
/// A due instant this close is waited for inside the same run.
const WAIT_WINDOW_SECONDS: u64 = 300;
/// Largest state file read: rejects garbage while leaving room for the
/// per-account credits snapshot on a large auth directory.
const STATE_MAX_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
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
    /// Last banked-reset counts per account, keyed by account id or auth file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    credits: BTreeMap<String, Credits>,
}

/// State facts a ping record needs, without exposing the schedule type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StateSummary {
    pub(crate) last_ping_epoch: u64,
    pub(crate) next_due_epoch: u64,
    pub(crate) window_reset_epoch: u64,
    pub(crate) long_window_reset_epoch: u64,
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
    /// Decision instant, after any precision wait.
    #[serde(skip)]
    now: u64,
    /// Seconds spent waiting for the exact due instant.
    #[serde(skip)]
    wait_s: u64,
    /// Probe snapshot of this run, when one was fetched.
    #[serde(skip)]
    accounts: Option<Vec<Account>>,
    /// Provider movements this run has to journal.
    #[serde(skip)]
    reset_changes: Vec<ResetChange>,
}

/// Injectable wall clock and sleeper, so the precision wait stays testable.
pub(crate) trait Clock {
    fn now(&self) -> u64;
    /// Sleeps up to the exact second `epoch`, sub-second precision included.
    fn sleep_until(&self, epoch: u64);
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        crate::time::now().max(0) as u64
    }

    fn sleep_until(&self, epoch: u64) {
        loop {
            let target = UNIX_EPOCH + Duration::from_secs(epoch);
            let Ok(remaining) = target.duration_since(SystemTime::now()) else {
                return;
            };
            if remaining.is_zero() {
                return;
            }
            std::thread::sleep(remaining);
        }
    }
}

/// One provider window that moved since the stored plan.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Movement {
    account: String,
    from: u64,
    to: u64,
}

/// One journaled reset event: a window movement or a banked-credit change.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ResetChange {
    kind: &'static str,
    account: String,
    from: i64,
    to: i64,
}

/// What one probe snapshot taught about the stored plan.
#[derive(Clone, Debug, Default)]
struct Observed {
    provider_short: Option<u64>,
    provider_long: Option<u64>,
    early_short: Option<Movement>,
    early_long: Option<Movement>,
    changes: Vec<ResetChange>,
}

impl Observed {
    fn early_reset(&self) -> bool {
        self.early_short.is_some() || self.early_long.is_some()
    }
}

/// Resolve auth and fetch only when the local probe interval has elapsed.
fn probe(paths: &Paths, config: &Config) -> Result<Vec<Account>, String> {
    let dir = paths.resolve_auth(None, config.auth_dir())?;
    crate::quota::fetch(&dir, paths, config, &["codex".into()], false)
}

/// Evaluate local scheduling before resolving credentials or auth files.
pub fn run(args: &Args, paths: &Paths, config: &Config) -> Result<i32, String> {
    let outcome = run_clocked(
        config,
        &paths.auto_state_file(),
        &SystemClock,
        args.dry_run,
        || probe(paths, config),
        || crate::tokenmaxx::run_output(args, paths, config, !args.json),
    );
    let (report, code) = match outcome {
        Ok(decided) => decided,
        Err(error) => {
            // A failed run is still one tick; the error itself passes through.
            if !args.dry_run {
                let now = crate::time::now().max(0) as u64;
                let state = read_state(&paths.auto_state_file()).ok().flatten();
                journal::append_or_warn(
                    &journal::path(paths),
                    &[error_tick_record(now, &error, state.as_ref())],
                );
            }
            return Err(error);
        }
    };
    let now = report.now;
    if !args.dry_run {
        // Journal problems are warned about and never change the outcome.
        let mut records: Vec<Value> = report
            .reset_changes
            .iter()
            .map(|change| reset_change_record(now, change))
            .collect();
        records.push(tick_record(&report, code));
        journal::append_or_warn(&journal::path(paths), &records);
    }
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
                if let Some(state) = report.state.as_ref() {
                    println!(
                        "auto: next due {} (in {}; not due)",
                        crate::time::rfc3339(state.next_due_epoch),
                        crate::time::duration(state.next_due_epoch.saturating_sub(now))
                    );
                }
            }
            "deferred" => {
                if let Some(state) = report.state.as_ref() {
                    println!(
                        "auto: deferred until {} (in {}; every Codex window is spent)",
                        crate::time::rfc3339(state.next_due_epoch),
                        crate::time::duration(state.next_due_epoch.saturating_sub(now))
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
                report.state.as_ref().map_or(0, |s| s.next_due_epoch),
                report.state.as_ref().map_or(0, |s| s.window_reset_epoch),
                report
                    .state
                    .as_ref()
                    .map_or(0, |s| s.long_window_reset_epoch),
                report.state.as_ref().map_or(0, |s| s.last_probe_epoch)
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
    // Reject anything >= the bound, without allocating for an unbounded file.
    let mut bytes = Vec::with_capacity(1024);
    file.take(STATE_MAX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read auto state")?;
    if bytes.len() >= STATE_MAX_BYTES as usize {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Reads the schedule facts a ping record needs from the state file.
pub(crate) fn state_summary(path: &Path) -> Option<StateSummary> {
    read_state(path).ok().flatten().map(|state| StateSummary {
        last_ping_epoch: state.last_ping_epoch,
        next_due_epoch: state.next_due_epoch,
        window_reset_epoch: state.window_reset_epoch,
        long_window_reset_epoch: state.long_window_reset_epoch,
    })
}

/// Seconds this run should wait for an exact due instant (0 when none).
fn wait_window(state: Option<&State>, now: u64) -> u64 {
    state
        .map(|state| state.next_due_epoch.saturating_sub(now))
        .filter(|wait| (1..=WAIT_WINDOW_SECONDS).contains(wait))
        .unwrap_or(0)
}

fn probe_due(state: &State, now: u64, minutes: u32) -> bool {
    minutes != 0
        && now
            >= state
                .last_probe_epoch
                .saturating_add(u64::from(minutes) * 60)
}

fn stored_reset(state: &State) -> u64 {
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
fn reconciliation(state: &State, now: u64, accounts: &[Account]) -> Observed {
    let mut observed = Observed::default();
    let mut provider_short: Option<(u64, String)> = None;
    let mut provider_long: Option<(u64, String)> = None;
    let mut candidate_short: Option<(u64, bool, String)> = None;
    let mut candidate_long: Option<(u64, bool, String)> = None;
    for account in accounts
        .iter()
        .filter(|account| account.provider == "codex" && account.error.is_none())
    {
        let key = account_key(account);
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
            let (provider, candidate) = if is_five_hour(window) {
                (&mut provider_short, &mut candidate_short)
            } else {
                (&mut provider_long, &mut candidate_long)
            };
            if provider.as_ref().is_none_or(|(seen, _)| reset < *seen) {
                *provider = Some((reset, key.clone()));
            }
            if blocked {
                continue;
            }
            if candidate.as_ref().is_none_or(|(seen, _, _)| reset < *seen) {
                *candidate = Some((reset, fresh(window), key.clone()));
            }
        }
    }
    let movement = |candidate: Option<(u64, bool, String)>, anchor: u64| {
        candidate
            .filter(|(reset, fresh, _)| {
                *fresh
                    && anchor > 0
                    && *reset > anchor.saturating_add(RESET_SLACK_SECONDS)
                    && *reset > now
            })
            .map(|(reset, _, account)| Movement {
                account,
                from: anchor,
                to: reset,
            })
    };
    let shortened = |provider: &Option<(u64, String)>, anchor: u64| {
        provider
            .as_ref()
            .filter(|(reset, _)| {
                anchor > RESET_SLACK_SECONDS && *reset < anchor.saturating_sub(RESET_SLACK_SECONDS)
            })
            .map(|(reset, account)| Movement {
                account: account.clone(),
                from: anchor,
                to: *reset,
            })
    };
    observed.provider_short = provider_short.as_ref().map(|(reset, _)| *reset);
    observed.provider_long = provider_long.as_ref().map(|(reset, _)| *reset);
    observed.early_short = movement(candidate_short, stored_reset(state));
    observed.early_long = movement(candidate_long, state.long_window_reset_epoch);
    observed.changes = [
        observed
            .early_short
            .as_ref()
            .map(|movement| movement.reset_change("window_reopened_early")),
        observed
            .early_long
            .as_ref()
            .map(|movement| movement.reset_change("long_window_reopened_early")),
        shortened(&provider_short, stored_reset(state))
            .map(|movement| movement.reset_change("window_shortened")),
        shortened(&provider_long, state.long_window_reset_epoch)
            .map(|movement| movement.reset_change("window_shortened")),
    ]
    .into_iter()
    .flatten()
    .collect();
    observed
}

impl Movement {
    fn reset_change(&self, kind: &'static str) -> ResetChange {
        ResetChange {
            kind,
            account: self.account.clone(),
            from: self.from as i64,
            to: self.to as i64,
        }
    }
}

/// Everything one probe snapshot teaches about the stored plan.
fn observe(state: &mut State, now: u64, accounts: &[Account]) -> Observed {
    let mut observed = reconciliation(state, now, accounts);
    observed.changes.extend(credits_diff(state, accounts));
    observed
}

/// Diffs banked-reset counts against the stored snapshot and keeps the new one.
fn credits_diff(state: &mut State, accounts: &[Account]) -> Vec<ResetChange> {
    let mut changes = Vec::new();
    for account in accounts
        .iter()
        .filter(|account| account.provider == "codex")
    {
        let Some(credits) = account.credits else {
            continue;
        };
        let key = account_key(account);
        let Some(previous) = state.credits.insert(key.clone(), credits) else {
            continue;
        };
        let change = |kind, from: u32, to: u32| ResetChange {
            kind,
            account: key.clone(),
            from: from.into(),
            to: to.into(),
        };
        if credits.available < previous.available {
            changes.push(change(
                "banked_reset_used",
                previous.available,
                credits.available,
            ));
        } else if credits.available > previous.available {
            changes.push(change(
                "banked_reset_earned",
                previous.available,
                credits.available,
            ));
        }
        if credits.applicable != previous.applicable {
            changes.push(change(
                "banked_reset_applicable",
                previous.applicable,
                credits.applicable,
            ));
        }
    }
    changes
}

/// Stable per-account journal key: the provider account id, else the auth file.
fn account_key(account: &Account) -> String {
    account
        .account_id
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| account.file.clone())
}

fn run_clocked(
    config: &Config,
    path: &Path,
    clock: &dyn Clock,
    dry_run: bool,
    probe: impl FnOnce() -> Result<Vec<Account>, String>,
    ping: impl FnOnce() -> Result<i32, String>,
) -> Result<(Report, i32), String> {
    let mut now = clock.now();
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
        now,
        wait_s: 0,
        accounts: None,
        reset_changes: Vec::new(),
    };
    if !config.tokenmaxx.enabled {
        return Ok((report, 0));
    }
    if !config.tokenmaxx.auto {
        report.decision = "auto_disabled";
        return Ok((report, 0));
    }
    let mut state = read_state(path)?;
    // A due instant inside the wait window is pinged at its exact second.
    if !dry_run {
        report.wait_s = wait_window(state.as_ref(), now);
        if report.wait_s > 0 {
            if let Some(state) = state.as_ref() {
                clock.sleep_until(state.next_due_epoch);
            }
            now = clock.now();
            report.now = now;
            // Re-read both sides: a manual ping may have opened a new window.
            state = read_state(path)?;
        }
    }
    report.state = state;
    if let Some(mut state) = report
        .state
        .clone()
        .filter(|state| now < state.next_due_epoch)
    {
        report.decision = "not_due";
        report.stored_window_reset_epoch = Some(stored_reset(&state)).filter(|reset| *reset > 0);
        report.stored_long_window_reset_epoch =
            Some(state.long_window_reset_epoch).filter(|reset| *reset > 0);
        report.probe_due = probe_due(&state, now, config.tokenmaxx.probe_minutes);
        if !report.probe_due {
            return Ok((report, 0));
        }
        if let Ok(accounts) = probe() {
            let observed = observe(&mut state, now, &accounts);
            report.provider_reset_epoch = observed.provider_short;
            report.provider_long_reset_epoch = observed.provider_long;
            report.early_reset_detected = observed.early_reset();
            report.reset_changes = observed.changes;
            // The provider can also reopen earlier than the stored plan.
            if !report.early_reset_detected
                && let Some(schedule) = schedule_from(&accounts, now)
                && schedule.next_due_epoch > now
                && schedule.next_due_epoch < state.next_due_epoch
            {
                state.next_due_epoch = schedule.next_due_epoch;
            }
            report.accounts = Some(accounts);
        }
        state.last_probe_epoch = now;
        let credits = state.credits.clone();
        // Persist the attempt even on provider errors or a failed keep-alive.
        if !dry_run {
            write_state(path, &state)?;
        }
        report.state = Some(state);
        if !report.early_reset_detected {
            return Ok((report, 0));
        }
        report.decision = "early_reset";
        if dry_run {
            let state = scheduled_state(
                now,
                config.tokenmaxx.interval_hours,
                schedule_for_reset(
                    report.provider_reset_epoch,
                    report.provider_long_reset_epoch,
                ),
            )?
            .keep_credits(&credits);
            report.state = Some(state);
            return Ok((report, 0));
        }
        let code = ping()?;
        if code == 0 {
            // The pass re-anchors both windows from its own post-ping snapshot;
            // fall back to the detected window when that snapshot was unavailable.
            let saved = read_state(path)?;
            let last_ping = saved.as_ref().map_or(now, |state| state.last_ping_epoch);
            let anchored = saved.and_then(|state| (state.window_reset_epoch > 0).then_some(state));
            report.state = Some(match anchored {
                Some(mut state) => {
                    state.last_probe_epoch = now;
                    write_state(path, &state)?;
                    state
                }
                None => {
                    let mut state = scheduled_state(
                        last_ping,
                        config.tokenmaxx.interval_hours,
                        schedule_for_reset(
                            report.provider_reset_epoch,
                            report.provider_long_reset_epoch,
                        ),
                    )?
                    .keep_credits(&credits);
                    state.last_probe_epoch = now;
                    write_state(path, &state)?;
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
            && let Some((deferred, changes)) =
                defer_while_capped(path, now, report.state.as_ref(), probe())?
        {
            report.state = Some(deferred);
            report.reset_changes = changes;
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
    previous: Option<&State>,
    accounts: Result<Vec<Account>, String>,
) -> Result<Option<(State, Vec<ResetChange>)>, String> {
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
    let mut plan = previous.cloned().unwrap_or_default();
    let observed = observe(&mut plan, now, &accounts);
    let state = State {
        last_ping_epoch: previous.map_or(0, |state| state.last_ping_epoch),
        next_due_epoch: schedule.next_due_epoch,
        last_probe_epoch: now,
        window_reset_epoch: schedule.window_reset_epoch,
        long_window_reset_epoch: schedule.long_window_reset_epoch,
        credits: plan.credits,
    };
    write_state(path, &state)?;
    Ok(Some((state, observed.changes)))
}

impl State {
    /// Keeps the last banked-credit snapshot across a fresh schedule.
    fn keep_credits(mut self, credits: &BTreeMap<String, Credits>) -> Self {
        self.credits.clone_from(credits);
        self
    }
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
        credits: BTreeMap::new(),
    })
}

fn write_state(path: &Path, state: &State) -> Result<(), String> {
    let bytes = serde_json::to_vec(state).map_err(|_| "Cannot serialize auto state")?;
    atomic_write(path, &bytes).map_err(|_| "Cannot save auto state atomically".into())
}

/// Persist the shared schedule only after the caller has observed a successful ping.
pub(crate) fn record_success(
    path: &Path,
    now: u64,
    interval_hours: u32,
    schedule: Option<Schedule>,
) -> Result<(), String> {
    let credits = read_state(path)?.map_or_else(BTreeMap::new, |state| state.credits);
    let state = scheduled_state(now, interval_hours, schedule)?.keep_credits(&credits);
    write_state(path, &state)
}

/// The single `tick` record every non-dry-run run appends.
fn tick_record(report: &Report, code: i32) -> Value {
    let now = report.now;
    let state = report.state.as_ref();
    let next_due = state.map_or(0, |state| state.next_due_epoch);
    let mut record = journal::base("tick", now);
    record.insert("decision".into(), report.decision.into());
    record.insert("trigger".into(), journal::trigger().into());
    record.insert(
        "last_ping_epoch".into(),
        state.map_or(0, |state| state.last_ping_epoch).into(),
    );
    record.insert("next_due_epoch".into(), next_due.into());
    record.insert("due_in_s".into(), (next_due as i64 - now as i64).into());
    record.insert("probe_due".into(), report.probe_due.into());
    if report.wait_s > 0 {
        record.insert("wait_s".into(), report.wait_s.into());
    }
    record.insert("exit".into(), code.into());
    if let Some(accounts) = report.accounts.as_ref() {
        record.insert(
            "stored_5h".into(),
            optional(report.stored_window_reset_epoch),
        );
        record.insert("provider_5h".into(), optional(report.provider_reset_epoch));
        record.insert(
            "stored_long".into(),
            optional(report.stored_long_window_reset_epoch),
        );
        record.insert(
            "provider_long".into(),
            optional(report.provider_long_reset_epoch),
        );
        record.insert("early_reset".into(), report.early_reset_detected.into());
        record.insert(
            "accounts".into(),
            accounts
                .iter()
                .map(account_record)
                .collect::<Vec<_>>()
                .into(),
        );
    }
    Value::Object(record)
}

/// The tick of a run that could not decide: same record shape, plus the error.
fn error_tick_record(now: u64, error: &str, state: Option<&State>) -> Value {
    let mut record = journal::base("tick", now);
    record.insert("decision".into(), "error".into());
    record.insert("trigger".into(), journal::trigger().into());
    record.insert("exit".into(), 1.into());
    record.insert("error".into(), journal::sanitize_error(error).into());
    if let Some(state) = state {
        record.insert("last_ping_epoch".into(), state.last_ping_epoch.into());
        record.insert("next_due_epoch".into(), state.next_due_epoch.into());
        record.insert(
            "due_in_s".into(),
            (state.next_due_epoch as i64 - now as i64).into(),
        );
    }
    Value::Object(record)
}

fn reset_change_record(now: u64, change: &ResetChange) -> Value {
    let mut record = journal::base("reset_change", now);
    record.insert("kind".into(), change.kind.into());
    record.insert("account".into(), change.account.clone().into());
    record.insert("from".into(), change.from.into());
    record.insert("to".into(), change.to.into());
    Value::Object(record)
}

/// Journal view of one probe account; never carries credentials.
fn account_record(account: &Account) -> Value {
    let mut record = Map::new();
    record.insert("file".into(), account.file.clone().into());
    record.insert("email".into(), optional(account.email.clone()));
    record.insert("plan".into(), optional(account.plan.clone()));
    record.insert("allowed".into(), optional(account.allowed));
    record.insert("limit_reached".into(), optional(account.limit_reached));
    record.insert("error".into(), optional(account.error.clone()));
    record.insert(
        "windows".into(),
        account
            .windows
            .iter()
            .map(|window| {
                let mut value = Map::new();
                value.insert("label".into(), window.label.clone().into());
                value.insert("used_percent".into(), optional(window.used_percent));
                value.insert("reset_at".into(), optional(window.reset_at.clone()));
                value.insert("reset_after".into(), optional(window.reset_after));
                Value::Object(value)
            })
            .collect::<Vec<_>>()
            .into(),
    );
    record.insert(
        "credits".into(),
        account.credits.map_or(Value::Null, |credits| {
            let mut value = Map::new();
            value.insert("available".into(), credits.available.into());
            value.insert("applicable".into(), credits.applicable.into());
            Value::Object(value)
        }),
    );
    Value::Object(record)
}

fn optional<T: Into<Value>>(value: Option<T>) -> Value {
    value.map_or(Value::Null, Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Sandbox;
    use std::{
        cell::{Cell, RefCell},
        fs,
        os::unix::fs::PermissionsExt,
    };

    /// Fixed clock: existing tests never enter the precision wait.
    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now(&self) -> u64 {
            self.0
        }

        fn sleep_until(&self, _epoch: u64) {
            panic!("a fixed clock must never sleep");
        }
    }

    /// Injects a fixed decision instant; the wait window stays for run_clocked.
    fn run_with(
        config: &Config,
        path: &Path,
        now: u64,
        dry_run: bool,
        probe: impl FnOnce() -> Result<Vec<Account>, String>,
        ping: impl FnOnce() -> Result<i32, String>,
    ) -> Result<(Report, i32), String> {
        run_clocked(config, path, &FixedClock(now), dry_run, probe, ping)
    }

    /// Tuple view used by the reconciliation tests.
    fn check(state: &State, now: u64, accounts: &[Account]) -> (Option<u64>, Option<u64>, bool) {
        let observed = reconciliation(state, now, accounts);
        (
            observed.provider_short,
            observed.provider_long,
            observed.early_reset(),
        )
    }

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
                long_window_reset_epoch: 0,
                credits: BTreeMap::new()
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
                long_window_reset_epoch: 0,
                credits: BTreeMap::new()
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
        // Beyond the precision wait window, so no clock sleep is attempted.
        let bytes = br#"{"last_ping_epoch":0,"next_due_epoch":1001}"#;
        fs::write(&path, bytes).unwrap();
        let (report, code) = run_local(&Config::default(), &path, 100, false, || {
            panic!("idle path called ping")
        })
        .unwrap();
        assert_eq!(report.decision, "not_due");
        assert_eq!(code, 0);
        assert_eq!(report.state.unwrap().next_due_epoch, 1001);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn oversized_state_is_corrupt_and_read_is_bounded() {
        let s = Sandbox::default();
        let path = s.0.join("auto.json");
        // A state that outgrew the bound must never be parsed as a plan.
        fs::write(&path, " ".repeat(STATE_MAX_BYTES as usize + 1)).unwrap();
        let (report, _) = run_local(&Config::default(), &path, 100, true, || {
            panic!("dry-run called ping")
        })
        .unwrap();
        assert_eq!(report.decision, "due");
        assert_eq!(report.state.unwrap().next_due_epoch, 18100);
    }

    #[test]
    fn state_with_many_credit_snapshots_still_parses() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        let mut state = plan(20000, 1000);
        for index in 0..60 {
            state.credits.insert(
                format!("acct-{index:02}"),
                Credits {
                    available: 2,
                    applicable: 0,
                },
            );
        }
        write_state(&path, &state).unwrap();
        let size = fs::metadata(&path).unwrap().len();
        assert!(size > 1024, "the fixture must exceed the old 1 KiB bound");
        assert!(size < STATE_MAX_BYTES, "the fixture must fit the new bound");
        assert_eq!(read_state(&path).unwrap(), Some(state));
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
                long_window_reset_epoch: 0,
                credits: BTreeMap::new()
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
            credits: BTreeMap::new(),
        }
    }

    fn state_with_due(next_due_epoch: u64) -> State {
        State {
            last_ping_epoch: 500,
            next_due_epoch,
            last_probe_epoch: 100,
            window_reset_epoch: next_due_epoch,
            long_window_reset_epoch: 0,
            credits: BTreeMap::new(),
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
        assert!(!probe_due(&state, 1899, 15));
        assert!(probe_due(&state, 1900, 15));
        assert!(probe_due(&state, 2000, 15));
        assert!(!probe_due(&state, u64::MAX, 0));
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
            assert_eq!(check(&state, 2000, &[a]), (Some(reset), None, expected));
        }
        let a = account(21000.into(), Some(0.0));
        assert_eq!(
            check(&state, 22000, std::slice::from_ref(&a)),
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
            assert!(!check(&state, 2000, &[a]).2, "{kind}");
        }
        // A weekly window is compared with the long anchor, never the 5h one.
        let weekly = codex(vec![window("7d", 21000, 0.0)]);
        assert_eq!(
            check(&state, 2000, std::slice::from_ref(&weekly)),
            (None, Some(21000), false)
        );
        let mut anchored = state.clone();
        anchored.long_window_reset_epoch = 20000;
        assert!(check(&anchored, 2000, std::slice::from_ref(&weekly)).2);
        let legacy = State {
            window_reset_epoch: 0,
            ..state.clone()
        };
        assert!(check(&legacy, 2000, std::slice::from_ref(&a)).2);
        let mut iso = a.clone();
        iso.windows[0].reset_at = Some("1970-01-01T05:50:00Z".into());
        assert_eq!(check(&state, 2000, &[iso]), (Some(21000), None, true));
        // A spent account is skipped, but does not veto the healthy one.
        let mut limited = a.clone();
        limited.limit_reached = Some(true);
        assert!(check(&state, 2000, &[a.clone(), limited]).2);
        assert_eq!(
            check(&state, 2000, &[a, account(20000.into(), Some(0.0))]),
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
            credits: BTreeMap::new(),
        };
        assert_eq!(report.state.as_ref(), Some(&expected));
        assert_eq!(read_state(&path).unwrap(), Some(expected));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Live shape: the weekly reset is minutes away while the fresh 5h window
        // would reopen hours later, so the deferral must land on the weekly reset.
        write_state(&path, &state_with_due(1500)).unwrap();
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
        write_state(&path, &state).unwrap();
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
            credits: BTreeMap::new(),
        };
        assert_eq!(report.state.as_ref(), Some(&expected));
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
        write_state(&path, &state).unwrap();
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
        write_state(&path, &future_state()).unwrap();
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
            write_state(&path, &state).unwrap();
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
                ..state.clone()
            };
            assert_eq!(code, 0);
            assert!(report.probe_due);
            assert!(!report.early_reset_detected);
            assert_eq!(report.decision, "not_due");
            assert_eq!(report.state.as_ref(), Some(&expected));
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
            write_state(&path, &state).unwrap();
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
            let after = report.state.clone().unwrap();
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
        write_state(&path, &future_state()).unwrap();
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
            credits: BTreeMap::new(),
        };
        assert_eq!(report.state.as_ref(), Some(&expected));
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
                long_window_reset_epoch: 7000,
                credits: BTreeMap::new()
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
        write_state(&path, &state).unwrap();
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

    /// Clock that advances instantly, and can simulate a manual ping mid-wait.
    struct TestClock {
        now: Cell<u64>,
        slept: RefCell<Vec<u64>>,
        after_sleep: RefCell<Option<Box<dyn FnOnce()>>>,
    }

    impl TestClock {
        fn at(now: u64) -> Self {
            Self {
                now: Cell::new(now),
                slept: RefCell::new(Vec::new()),
                after_sleep: RefCell::new(None),
            }
        }

        fn slept(&self) -> Vec<u64> {
            self.slept.borrow().clone()
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> u64 {
            self.now.get()
        }

        fn sleep_until(&self, epoch: u64) {
            self.slept.borrow_mut().push(epoch);
            self.now.set(epoch);
            if let Some(callback) = self.after_sleep.borrow_mut().take() {
                callback();
            }
        }
    }

    /// A stored plan whose probe is not due, so the first decision is the wait.
    fn plan(next_due_epoch: u64, now: u64) -> State {
        State {
            last_ping_epoch: 0,
            next_due_epoch,
            last_probe_epoch: now,
            window_reset_epoch: next_due_epoch,
            long_window_reset_epoch: 0,
            credits: BTreeMap::new(),
        }
    }

    fn codex_account(windows: Vec<Window>, credits: Option<Credits>) -> Account {
        Account {
            file: "codex.json".into(),
            provider: "codex".into(),
            allowed: Some(true),
            limit_reached: Some(false),
            windows,
            credits,
            ..Default::default()
        }
    }

    fn credited(account_id: &str, available: u32, applicable: u32) -> Account {
        Account {
            account_id: Some(account_id.into()),
            credits: Some(Credits {
                available,
                applicable,
            }),
            ..codex_account(vec![window("5h", 20000, 40.0)], None)
        }
    }

    /// One probe run; returns what the run would journal.
    fn probe_run(path: &Path, now: u64, accounts: Vec<Account>) -> Vec<ResetChange> {
        let (report, _) = run_with(
            &Config::default(),
            path,
            now,
            false,
            move || Ok(accounts),
            || Ok(0),
        )
        .unwrap();
        report.reset_changes
    }

    #[test]
    fn precision_wait_pings_at_the_exact_due_second() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        write_state(&path, &plan(1700, 1000)).unwrap();
        // A five-minute tick fires 24s before the due second.
        let clock = TestClock::at(1676);
        let calls = Cell::new(0);
        let (report, code) = run_clocked(
            &Config::default(),
            &path,
            &clock,
            false,
            || panic!("the precision wait is not a probe"),
            || {
                calls.set(calls.get() + 1);
                assert_eq!(clock.now(), 1700, "the ping must land on the due second");
                record_success(&path, clock.now(), 5, None)?;
                Ok(0)
            },
        )
        .unwrap();
        assert_eq!(clock.slept(), vec![1700]);
        assert_eq!(calls.get(), 1);
        assert_eq!(code, 0);
        assert_eq!(report.decision, "due");
        assert_eq!(report.wait_s, 24);
        assert_eq!(read_state(&path).unwrap().unwrap().last_ping_epoch, 1700);
    }

    #[test]
    fn manual_ping_during_the_wait_wins() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        write_state(&path, &plan(1700, 1000)).unwrap();
        let clock = TestClock::at(1676);
        let next = path.clone();
        *clock.after_sleep.borrow_mut() = Some(Box::new(move || {
            // A manual `cpaq ping` opened a new window while auto waited.
            let opened = State {
                last_ping_epoch: 1700,
                next_due_epoch: 20000,
                last_probe_epoch: 1700,
                window_reset_epoch: 20000,
                ..plan(20000, 1700)
            };
            write_state(&next, &opened).unwrap();
        }));
        let (report, code) = run_clocked(
            &Config::default(),
            &path,
            &clock,
            false,
            || panic!("a fresh window is not a probe"),
            || panic!("a manual ping must suppress the auto ping"),
        )
        .unwrap();
        assert_eq!(clock.slept(), vec![1700]);
        assert_eq!(code, 0);
        assert_eq!(report.decision, "not_due");
        assert_eq!(read_state(&path).unwrap().unwrap().next_due_epoch, 20000);
    }

    #[test]
    fn dry_run_never_waits_or_writes() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        write_state(&path, &plan(1705, 1000)).unwrap();
        let before = fs::read(&path).unwrap();
        let clock = TestClock::at(1700);
        let (report, code) = run_clocked(
            &Config::default(),
            &path,
            &clock,
            true,
            || panic!("dry-run must not probe"),
            || panic!("dry-run must not ping"),
        )
        .unwrap();
        assert!(clock.slept().is_empty(), "dry-run must not sleep");
        // Without the wait the run can only report the idle decision.
        assert_eq!(report.decision, "not_due");
        assert_eq!(report.wait_s, 0);
        assert_eq!(code, 0);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn far_due_is_not_waited_for() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        write_state(&path, &plan(2001, 1700)).unwrap();
        let clock = TestClock::at(1700);
        let (report, code) = run_clocked(
            &Config::default(),
            &path,
            &clock,
            false,
            || panic!("an idle tick must not probe"),
            || panic!("an idle tick must not ping"),
        )
        .unwrap();
        assert!(clock.slept().is_empty());
        assert_eq!(report.decision, "not_due");
        assert_eq!(code, 0);
    }

    #[test]
    fn duration_text_shows_seconds() {
        assert_eq!(crate::time::duration(17964), "4h 59m 24s");
        assert_eq!(crate::time::duration(64), "1m 4s");
        assert_eq!(crate::time::duration(4), "4s");
        assert_eq!(crate::time::duration(0), "0s");
    }

    #[test]
    fn banked_reset_counts_are_diffed_and_persisted() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        write_state(&path, &plan(20000, 1000)).unwrap();
        // The first snapshot only stores the counts.
        assert!(probe_run(&path, 2000, vec![credited("acct-1", 2, 0)]).is_empty());
        assert_eq!(
            read_state(&path).unwrap().unwrap().credits.get("acct-1"),
            Some(&Credits {
                available: 2,
                applicable: 0
            })
        );
        // 2 -> 1 spends one credit; 0 -> 1 makes one applicable.
        assert_eq!(
            probe_run(&path, 4000, vec![credited("acct-1", 1, 1)]),
            vec![
                ResetChange {
                    kind: "banked_reset_used",
                    account: "acct-1".into(),
                    from: 2,
                    to: 1
                },
                ResetChange {
                    kind: "banked_reset_applicable",
                    account: "acct-1".into(),
                    from: 0,
                    to: 1
                }
            ]
        );
        // Unchanged counts stay quiet.
        assert!(probe_run(&path, 6000, vec![credited("acct-1", 1, 1)]).is_empty());
        // A credit earned is reported too.
        assert_eq!(
            probe_run(&path, 8000, vec![credited("acct-1", 2, 1)]),
            vec![ResetChange {
                kind: "banked_reset_earned",
                account: "acct-1".into(),
                from: 1,
                to: 2
            }]
        );
    }

    #[test]
    fn unexpected_reset_movements_are_journaled() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        // A fresh 5h window reopened 1000s later: staff/OpenAI reset.
        write_state(&path, &plan(20000, 1000)).unwrap();
        assert_eq!(
            probe_run(
                &path,
                3000,
                vec![codex_account(vec![window("5h", 21000, 0.0)], None)]
            ),
            vec![ResetChange {
                kind: "window_reopened_early",
                account: "codex.json".into(),
                from: 20000,
                to: 21000
            }]
        );
        // Once a ping re-anchored the window, the same snapshot is quiet.
        write_state(&path, &plan(21000, 1000)).unwrap();
        assert!(
            probe_run(
                &path,
                5000,
                vec![codex_account(vec![window("5h", 21000, 0.0)], None)]
            )
            .is_empty()
        );
        // A provider reset ahead of the stored one shortens the window.
        write_state(&path, &plan(20000, 1000)).unwrap();
        assert_eq!(
            probe_run(
                &path,
                3000,
                vec![codex_account(vec![window("5h", 19000, 40.0)], None)]
            ),
            vec![ResetChange {
                kind: "window_shortened",
                account: "codex.json".into(),
                from: 20000,
                to: 19000
            }]
        );
        // The weekly class has its own earlier reopening.
        write_state(
            &path,
            &State {
                long_window_reset_epoch: 20000,
                ..plan(15000, 1000)
            },
        )
        .unwrap();
        assert_eq!(
            probe_run(
                &path,
                3000,
                vec![codex_account(vec![window("7d", 40000, 0.0)], None)]
            ),
            vec![ResetChange {
                kind: "long_window_reopened_early",
                account: "codex.json".into(),
                from: 20000,
                to: 40000
            }]
        );
    }

    #[test]
    fn journal_key_falls_back_to_the_auth_file() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        write_state(&path, &plan(20000, 1000)).unwrap();
        let changes = probe_run(
            &path,
            3000,
            vec![codex_account(vec![window("5h", 21000, 0.0)], None)],
        );
        assert_eq!(changes[0].account, "codex.json");
    }

    #[test]
    fn tick_record_carries_the_run_facts() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        write_state(&path, &plan(20000, 1000)).unwrap();
        let (report, code) = run_with(
            &Config::default(),
            &path,
            2000,
            false,
            || Ok(vec![credited("acct-1", 2, 0)]),
            || panic!("must not ping"),
        )
        .unwrap();
        let tick = tick_record(&report, code);
        assert_eq!(tick["event"], "tick");
        assert_eq!(tick["decision"], "not_due");
        // The trigger mirrors the ambient environment: never assert "manual".
        assert_eq!(tick["trigger"], journal::trigger());
        assert!(["systemd", "manual"].contains(&tick["trigger"].as_str().unwrap()));
        assert_eq!(tick["probe_due"], true);
        assert_eq!(tick["exit"], 0);
        assert_eq!(tick["due_in_s"], 18000);
        assert!(tick.get("wait_s").is_none());
        assert!(tick["ts"].as_str().unwrap().contains('T'));
        assert!(tick["ts_epoch"].is_number());
        assert_eq!(tick["stored_5h"], 20000);
        assert_eq!(tick["provider_5h"], 20000);
        assert_eq!(tick["early_reset"], false);
        let account = &tick["accounts"][0];
        for field in [
            "file",
            "email",
            "plan",
            "allowed",
            "limit_reached",
            "error",
            "windows",
            "credits",
        ] {
            assert!(account.get(field).is_some(), "account lost {field}");
        }
        assert_eq!(account["credits"]["available"], 2);
        assert_eq!(account["credits"]["applicable"], 0);
        assert_eq!(account["windows"][0]["label"], "5h");
        assert_eq!(account["windows"][0]["used_percent"], 40.0);
        assert!(account["windows"][0].get("reset_at").is_some());
        assert!(account["windows"][0].get("reset_after").is_some());
        // A plan without banked credits reports null, not an empty object.
        let plain = account_record(&codex_account(vec![window("5h", 20000, 40.0)], None));
        assert_eq!(plain["credits"], Value::Null);
        assert_eq!(plain["email"], Value::Null);
        let change = ResetChange {
            kind: "banked_reset_used",
            account: "acct-1".into(),
            from: 2,
            to: 1,
        };
        let record = reset_change_record(2000, &change);
        assert_eq!(record["event"], "reset_change");
        assert_eq!(record["kind"], "banked_reset_used");
        assert_eq!(record["account"], "acct-1");
        assert_eq!(record["from"], 2);
        assert_eq!(record["to"], 1);
    }

    #[test]
    fn json_report_stays_backward_compatible() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("auto.json");
        write_state(&path, &plan(20000, 1000)).unwrap();
        let (report, _) = run_with(
            &Config::default(),
            &path,
            2000,
            false,
            || Ok(vec![credited("acct-1", 2, 0)]),
            || panic!("must not ping"),
        )
        .unwrap();
        let value = serde_json::to_value(&report).unwrap();
        for field in ["wait_s", "accounts", "reset_changes", "now"] {
            assert!(value.get(field).is_none(), "json leaked {field}");
        }
        assert_eq!(value["decision"], "not_due");
        assert_eq!(value["probe_due"], true);
    }
}
