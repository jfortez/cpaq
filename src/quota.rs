//! Quota collection, bounded concurrent fetching, and legacy human rendering.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Serialize;
use serde_json::Value;

use crate::auth::Auth;
use crate::config::Config;
use crate::paths::Paths;
use crate::providers::{self, HttpTransport, Transport};
use crate::time::{local, parse_iso};

/// Hard cap on worker threads fetching quotas.
pub const MAX_FETCH_THREADS: usize = 4;

pub(crate) const NO_AUTH_FILES: &str = "no auth files for this provider";

/// One auth file's quota result, shared with the headless JSON output and TUI.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Account {
    pub file: String,
    pub provider: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub account_id: Option<String>,
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub windows: Vec<Window>,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

impl Account {
    pub(crate) fn from_auth(auth: &Auth) -> Self {
        Self {
            file: auth.file.clone(),
            provider: auth.provider.to_ascii_lowercase(),
            email: auth.email.clone(),
            plan: auth.plan.clone(),
            account_id: auth.account_id.clone(),
            ..Self::default()
        }
    }
}

/// One quota window: label, usage and reset information.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Window {
    pub label: String,
    pub used_percent: Option<f64>,
    pub reset_at: Option<Value>,
    pub reset_after: Option<f64>,
}

/// Load auth files and query every selected provider, bounded to four workers.
pub fn fetch(
    dir: &Path,
    paths: &Paths,
    config: &Config,
    providers: &[String],
    raw: bool,
) -> Result<Vec<Account>, String> {
    if !dir.is_dir() {
        return Err(format!(
            "Auth directory not found: {}. Set it with --auth-dir or the CPA_AUTH_DIR environment variable.",
            dir.display()
        ));
    }
    let auths = crate::auth::load(dir, paths, &crate::auth::FileReader::default())?;
    if auths.is_empty() {
        return Err(format!("No auth files found in {}", dir.display()));
    }
    let transport = HttpTransport::new(config.proxy.timeout_secs);
    Ok(collect(&auths, providers, raw, &transport))
}

fn collect(
    auths: &[Auth],
    wanted: &[String],
    raw: bool,
    transport: &dyn Transport,
) -> Vec<Account> {
    let mut filtered: Vec<&Auth> = auths
        .iter()
        .filter(|auth| {
            wanted.is_empty()
                || wanted
                    .iter()
                    .any(|provider| provider.eq_ignore_ascii_case(&auth.provider))
        })
        .collect();
    filtered.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then_with(|| left.provider.cmp(&right.provider))
    });

    let mut accounts = query_all(&filtered, raw, transport);
    let mut seen: Vec<String> = accounts
        .iter()
        .map(|account| account.provider.to_ascii_lowercase())
        .collect();
    for provider in wanted {
        let key = provider.to_ascii_lowercase();
        if seen.iter().any(|seen| seen == &key) {
            continue;
        }
        seen.push(key.clone());
        accounts.push(Account {
            file: "-".into(),
            provider: key,
            error: Some(NO_AUTH_FILES.into()),
            ..Account::default()
        });
    }
    accounts
}

fn query_all(auths: &[&Auth], raw: bool, transport: &dyn Transport) -> Vec<Account> {
    let results: Vec<Mutex<Option<Account>>> = auths.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let workers = auths.len().min(MAX_FETCH_THREADS);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(auth) = auths.get(index) else {
                        break;
                    };
                    let account = query_one(auth, raw, transport);
                    if let Ok(mut slot) = results[index].lock() {
                        *slot = Some(account);
                    }
                }
            });
        }
    });

    results
        .into_iter()
        .filter_map(|slot| slot.into_inner().ok().flatten())
        .collect()
}

fn query_one(auth: &Auth, raw: bool, transport: &dyn Transport) -> Account {
    let mut account = Account::from_auth(auth);
    if let Some(error) = auth.error.clone() {
        account.error = Some(error);
        return account;
    }
    if !providers::is_supported(&auth.provider) {
        account.error = Some(providers::unsupported_message(&account.provider));
        return account;
    }
    match providers::query(transport, auth, raw) {
        Ok(account) => account,
        Err(error) => {
            account.error = Some(error);
            account
        }
    }
}

/// Render the provider-grouped human output used by logs and scripts.
pub fn render_human(accounts: &[Account], dir: &Path, now: i64) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "CLIProxyAPI — provider quotas · {}\n",
        local(now, "%Y-%m-%d %H:%M")
    ));
    out.push_str(&format!("(auth-dir: {})\n\n", dir.display()));

    let mut by_provider: BTreeMap<&str, Vec<&Account>> = BTreeMap::new();
    for account in accounts {
        by_provider
            .entry(account.provider.as_str())
            .or_default()
            .push(account);
    }

    for (provider, group) in by_provider {
        out.push_str(&format!("── {} ──\n", provider.to_ascii_uppercase()));
        for account in group {
            if let Some(error) = &account.error {
                out.push_str(&format!("  ✗ {}: {}\n", account.file, error));
                continue;
            }
            let status = if account.limit_reached == Some(true) {
                "⛔ LIMIT REACHED"
            } else if account.allowed == Some(false) {
                "blocked (allowed=false)"
            } else {
                "OK"
            };
            let email = account
                .email
                .as_deref()
                .filter(|email| !email.is_empty())
                .unwrap_or(&account.file);
            let plan = account
                .plan
                .as_deref()
                .filter(|plan| !plan.is_empty())
                .unwrap_or("?");
            out.push_str(&format!("  ● {email}  ({plan}) — {status}\n"));
            if account.windows.is_empty() {
                out.push_str("      (no quota windows in the response)\n");
            }
            for window in &account.windows {
                out.push_str(&format!(
                    "      {:>7}  {:>5}{}",
                    window.label,
                    percent(window.used_percent),
                    bar(window.used_percent)
                ));
                if let Some(reset) = window
                    .reset_at
                    .as_ref()
                    .filter(|reset| providers::truthy(reset))
                    .and_then(reset_label)
                {
                    out.push_str(&format!("  reset {reset}"));
                }
                if let Some(seconds) = window.reset_after {
                    out.push_str(&format!(" (in {})", relative(seconds)));
                }
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

/// Number of account-level query failures (unsupported providers are nonfatal).
pub fn query_error_count(accounts: &[Account]) -> usize {
    accounts
        .iter()
        .filter(|account| {
            account.error.is_some()
                && account.file != "-"
                && providers::is_supported(&account.provider)
        })
        .count()
}

/// Ten-cell usage bar, or an empty string when usage is unknown.
pub fn bar(used: Option<f64>) -> String {
    let Some(used) = used else {
        return String::new();
    };
    let filled = if used.is_finite() {
        (used / 10.0).round_ties_even().clamp(0.0, 10.0) as usize
    } else {
        0
    };
    format!(" [{}{}]", "█".repeat(filled), "░".repeat(10 - filled))
}

/// Legacy percentage text (`?` when unknown).
pub fn percent(used: Option<f64>) -> String {
    used.map_or_else(|| "?".into(), |value| format!("{}%", general(value)))
}

/// Six-significant-digit general formatting, matching Python's `%g` for
/// the percentage range the providers return.
fn general(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0.0 {
        return "0".into();
    }
    let exponent = value.abs().log10().floor() as i32;
    let decimals = (5 - exponent).max(0) as usize;
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

/// Legacy relative duration such as `2h 5m` or `3d 1h`.
pub fn relative(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "0m".into();
    }
    let total = seconds as u64;
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// Human reset text (`reset <local datetime> (in <relative>)`).
pub fn reset_text(window: &Window, now: i64) -> String {
    let reset = window
        .reset_at
        .as_ref()
        .filter(|reset| providers::truthy(reset));
    let rendered = reset.and_then(reset_label);
    let epoch = reset.and_then(reset_epoch);
    let remaining = epoch
        .and_then(|epoch| epoch.checked_sub(now))
        .filter(|remaining| *remaining > 0)
        .map(|remaining| relative(remaining as f64));
    let relative = window
        .reset_after
        .filter(|seconds| seconds.is_finite())
        .map(relative)
        .or(remaining);

    match (rendered, relative) {
        (Some(rendered), Some(relative)) => format!("reset {rendered} (in {relative})"),
        (Some(rendered), None) => format!("reset {rendered}"),
        (None, Some(relative)) => format!("in {relative}"),
        (None, None) => String::new(),
    }
}

fn reset_label(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|epoch| epoch as i64))
            .map(|epoch| local(epoch, "%a %-d %b %H:%M")),
        Value::String(text) => Some(parse_iso(text).map_or_else(
            || text.chars().take(16).collect(),
            |epoch| local(epoch, "%a %-d %b %H:%M"),
        )),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn reset_epoch(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|epoch| epoch as i64)),
        Value::String(text) => parse_iso(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Sandbox;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    struct Fake {
        response: Value,
        delay: Duration,
        active: AtomicUsize,
        peak: AtomicUsize,
        calls: AtomicUsize,
    }

    impl Fake {
        fn new(response: Value) -> Self {
            Self {
                response,
                delay: Duration::ZERO,
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
            }
        }

        fn with_delay(response: Value, delay: Duration) -> Self {
            Self {
                delay,
                ..Self::new(response)
            }
        }
    }

    impl Transport for Fake {
        fn get_json(
            &self,
            _url: &str,
            _headers: &[(&str, String)],
        ) -> Result<Value, providers::FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(self.response.clone())
        }
    }

    fn codex_response() -> Value {
        json!({
            "email": "acct@example.org",
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "primary_window": {
                    "used_percent": 10,
                    "limit_window_seconds": 18000
                }
            }
        })
    }

    fn auth(file: &str, provider: &str) -> Auth {
        Auth {
            file: file.into(),
            provider: provider.into(),
            email: Some("acct@example.org".into()),
            plan: Some("pro".into()),
            access_token: format!("fixture-token-{file}"),
            ..Default::default()
        }
    }

    #[test]
    fn formatting_helpers_match_legacy_output() {
        assert_eq!(bar(None), "");
        assert_eq!(bar(Some(0.0)), " [░░░░░░░░░░]");
        assert_eq!(bar(Some(3.0)), " [░░░░░░░░░░]");
        assert_eq!(bar(Some(5.0)), " [░░░░░░░░░░]");
        assert_eq!(bar(Some(15.0)), " [██░░░░░░░░]");
        assert_eq!(bar(Some(25.0)), " [██░░░░░░░░]");
        assert_eq!(bar(Some(100.0)), " [██████████]");
        assert_eq!(bar(Some(150.0)), " [██████████]");
        assert_eq!(bar(Some(-4.0)), " [░░░░░░░░░░]");

        assert_eq!(percent(None), "?");
        assert_eq!(percent(Some(3.0)), "3%");
        assert_eq!(percent(Some(12.5)), "12.5%");
        assert_eq!(percent(Some(100.0)), "100%");
        assert_eq!(percent(Some(1.25)), "1.25%");
        assert_eq!(percent(Some(12.3456789)), "12.3457%");

        assert_eq!(relative(0.0), "0m");
        assert_eq!(relative(90.0), "1m");
        assert_eq!(relative(3600.0), "1h 0m");
        assert_eq!(relative(3660.0), "1h 1m");
        assert_eq!(relative(86400.0), "1d 0h");
        assert_eq!(relative(90000.0), "1d 1h");
    }

    #[test]
    fn reset_text_combines_local_time_and_relative() {
        let with_relative = Window {
            label: "5h".into(),
            used_percent: Some(10.0),
            reset_at: Some(json!(1_700_003_600)),
            reset_after: Some(3600.0),
        };
        assert_eq!(
            reset_text(&with_relative, 1_700_000_000),
            format!(
                "reset {} (in 1h 0m)",
                local(1_700_003_600, "%a %-d %b %H:%M")
            )
        );

        let only_relative = Window {
            reset_after: Some(90.0),
            ..Window::default()
        };
        assert_eq!(reset_text(&only_relative, 0), "in 1m");

        let unparseable = Window {
            reset_at: Some(json!("2023-11-14T22:13")),
            ..Window::default()
        };
        assert_eq!(reset_text(&unparseable, 0), "reset 2023-11-14T22:13");

        let iso = Window {
            reset_at: Some(json!("1970-01-01T01:00:00+01:00")),
            ..Window::default()
        };
        assert_eq!(
            reset_text(&iso, 10),
            format!("reset {}", local(0, "%a %-d %b %H:%M"))
        );
    }

    #[test]
    fn human_output_matches_legacy_reference() {
        let accounts = vec![
            Account {
                file: "claude-a.json".into(),
                provider: "claude".into(),
                email: Some("c@example.org".into()),
                plan: Some("pro".into()),
                windows: vec![Window {
                    label: "5h".into(),
                    used_percent: Some(12.5),
                    ..Window::default()
                }],
                ..Account::default()
            },
            Account {
                file: "codex-a.json".into(),
                provider: "codex".into(),
                email: Some("a@example.org".into()),
                plan: Some("plus".into()),
                allowed: Some(true),
                limit_reached: Some(false),
                windows: vec![
                    Window {
                        label: "5h".into(),
                        used_percent: Some(3.0),
                        reset_at: Some(json!(1_700_003_600)),
                        reset_after: Some(3600.0),
                    },
                    Window {
                        label: "7d".into(),
                        ..Window::default()
                    },
                ],
                ..Account::default()
            },
            Account {
                file: "codex-b.json".into(),
                provider: "codex".into(),
                error: Some(
                    "HTTP 401 Unauthorized — token expired? The proxy refreshes it automatically; retry later"
                        .into(),
                ),
                ..Account::default()
            },
            Account {
                file: "gemini-a.json".into(),
                provider: "gemini".into(),
                error: Some(
                    "provider 'gemini' not supported yet (add fetch_* and register it in PROVIDERS)"
                        .into(),
                ),
                ..Account::default()
            },
        ];

        let golden = "\
CLIProxyAPI — provider quotas · <STAMP>
(auth-dir: /tmp/auths)

── CLAUDE ──
  ● c@example.org  (pro) — OK
           5h  12.5% [█░░░░░░░░░]

── CODEX ──
  ● a@example.org  (plus) — OK
           5h     3% [░░░░░░░░░░]  reset <RESET> (in 1h 0m)
           7d      ?
  ✗ codex-b.json: HTTP 401 Unauthorized — token expired? The proxy refreshes it automatically; retry later

── GEMINI ──
  ✗ gemini-a.json: provider 'gemini' not supported yet (add fetch_* and register it in PROVIDERS)

";
        let expected = golden
            .replace("<STAMP>", &local(1_700_000_000, "%Y-%m-%d %H:%M"))
            .replace("<RESET>", &local(1_700_003_600, "%a %-d %b %H:%M"));

        assert_eq!(
            render_human(&accounts, Path::new("/tmp/auths"), 1_700_000_000),
            expected
        );
    }

    #[test]
    fn fetch_rejects_missing_or_empty_auth_dir() {
        let sandbox = Sandbox::default();
        let paths = Paths::new(sandbox.0.clone(), None, None);
        let config = Config::default();

        let missing = sandbox.0.join("missing");
        let error = fetch(&missing, &paths, &config, &[], false).unwrap_err();
        assert!(error.contains("Auth directory not found"), "{error}");

        let empty = sandbox.0.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let error = fetch(&empty, &paths, &config, &[], false).unwrap_err();
        assert!(error.contains("No auth files found"), "{error}");
    }

    #[test]
    fn bounded_concurrency_keeps_deterministic_order() {
        let auths: Vec<Auth> = (0..12)
            .map(|index| auth(&format!("codex-{index:02}.json"), "codex"))
            .collect();
        let fake = Fake::with_delay(codex_response(), Duration::from_millis(25));

        let accounts = collect(&auths, &[], false, &fake);

        let files: Vec<&str> = accounts
            .iter()
            .map(|account| account.file.as_str())
            .collect();
        let expected: Vec<String> = (0..12)
            .map(|index| format!("codex-{index:02}.json"))
            .collect();
        assert_eq!(files, expected);
        assert!(accounts.iter().all(|account| account.error.is_none()));
        assert_eq!(MAX_FETCH_THREADS, 4);
        assert_eq!(fake.peak.load(Ordering::SeqCst), MAX_FETCH_THREADS);
    }

    #[test]
    fn filters_providers_and_adds_legacy_placeholders() {
        let auths = vec![
            auth("codex-a.json", "codex"),
            auth("claude-a.json", "claude"),
        ];
        let fake = Fake::new(codex_response());
        let wanted = vec!["claude".to_string(), "gemini".to_string()];

        let accounts = collect(&auths, &wanted, false, &fake);

        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].provider, "claude");
        assert_eq!(accounts[1].file, "-");
        assert_eq!(accounts[1].provider, "gemini");
        assert_eq!(accounts[1].error.as_deref(), Some(NO_AUTH_FILES));
        assert_eq!(query_error_count(&accounts), 0);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn per_account_errors_are_isolated_and_counted() {
        let mut broken = auth("codex-broken.json", "codex");
        broken.error = Some("unreadable JSON or missing permissions".into());
        let auths = vec![
            broken,
            auth("codex-ok.json", "codex"),
            auth("gemini-a.json", "gemini"),
        ];
        let fake = Fake::new(codex_response());

        let accounts = collect(&auths, &[], false, &fake);

        assert_eq!(accounts.len(), 3);
        assert_eq!(
            accounts[0].error.as_deref(),
            Some("unreadable JSON or missing permissions")
        );
        assert!(accounts[1].error.is_none());
        assert!(accounts[2].error.is_some());
        assert_eq!(query_error_count(&accounts), 1);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn serialized_account_omits_absent_raw() {
        let text = serde_json::to_string(&Account::default()).unwrap();
        assert!(!text.contains("\"raw\""), "{text}");
    }
}
