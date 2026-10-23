//! Codex usage endpoint (verified against the reference implementation).

use serde_json::Value;

use super::{Transport, USER_AGENT, metadata_text, redact, truthy};
use crate::auth::Auth;
use crate::quota::{Account, Window};

pub(crate) const URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub(crate) fn fill(
    transport: &dyn Transport,
    auth: &Auth,
    raw: bool,
    account: &mut Account,
) -> Result<(), String> {
    let mut headers: Vec<(&str, String)> = vec![
        ("Authorization", bearer(auth)?),
        ("originator", "codex_cli_rs".into()),
        ("User-Agent", USER_AGENT.into()),
    ];
    if let Some(account_id) = auth.account_id.as_deref().filter(|value| !value.is_empty()) {
        headers.push(("chatgpt-account-id", account_id.into()));
    }

    let data = transport
        .get_json(URL, &headers)
        .map_err(|error| error.message())?;

    account.email =
        metadata_text(data.get("email"), &auth.access_token).or_else(|| auth.email.clone());
    account.plan =
        metadata_text(data.get("plan_type"), &auth.access_token).or_else(|| auth.plan.clone());
    let rate = data.get("rate_limit");
    account.allowed = rate
        .and_then(|rate| rate.get("allowed"))
        .and_then(Value::as_bool);
    account.limit_reached = rate
        .and_then(|rate| rate.get("limit_reached"))
        .and_then(Value::as_bool);
    account.windows = ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|key| rate.and_then(|rate| rate.get(*key)))
        .filter(|value| value.as_object().is_some_and(|object| !object.is_empty()))
        .map(window_from)
        .collect();

    if raw {
        account.raw = Some(redact(&data, &auth.access_token));
    }
    Ok(())
}

fn bearer(auth: &Auth) -> Result<String, String> {
    if auth.access_token.trim().is_empty() {
        return Err("no access_token in the auth file".into());
    }
    Ok(format!("Bearer {}", auth.access_token))
}

fn window_from(value: &Value) -> Window {
    Window {
        label: label_for(value.get("limit_window_seconds")),
        used_percent: value.get("used_percent").and_then(Value::as_f64),
        reset_at: value.get("reset_at").filter(|reset| truthy(reset)).cloned(),
        reset_after: value.get("reset_after_seconds").and_then(Value::as_f64),
    }
}

/// Legacy window label from `limit_window_seconds`.
pub(crate) fn label_for(seconds: Option<&Value>) -> String {
    let hours = seconds
        .and_then(|value| match value {
            Value::Number(number) => number.as_f64(),
            Value::String(text) => text.trim().parse::<f64>().ok(),
            _ => None,
        })
        .map(|seconds| seconds / 3600.0);
    let Some(hours) = hours else {
        return "?".into();
    };
    for (size, label) in [(5.0, "5h"), (24.0, "1d"), (168.0, "7d"), (720.0, "30d")] {
        if (hours - size).abs() <= 1.5 {
            return label.into();
        }
    }
    format!("{hours:.0}h")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{FetchError, USER_AGENT};
    use crate::quota::Window;
    use serde_json::json;
    use std::sync::Mutex;

    struct Request {
        url: String,
        headers: Vec<(String, String)>,
    }

    struct Fake {
        response: Value,
        requests: Mutex<Vec<Request>>,
    }

    impl Fake {
        fn new(response: Value) -> Self {
            Self {
                response,
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> std::sync::MutexGuard<'_, Vec<Request>> {
            self.requests.lock().unwrap()
        }
    }

    impl Transport for Fake {
        fn get_json(&self, url: &str, headers: &[(&str, String)]) -> Result<Value, FetchError> {
            self.requests.lock().unwrap().push(Request {
                url: url.into(),
                headers: headers
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), value.clone()))
                    .collect(),
            });
            Ok(self.response.clone())
        }
    }

    fn header(request: &Request, name: &str) -> Option<String> {
        request
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }

    fn auth() -> Auth {
        Auth {
            file: "codex-a.json".into(),
            provider: "codex".into(),
            email: Some("fallback@example.org".into()),
            plan: Some("fallback-plan".into()),
            account_id: Some("acct-1".into()),
            access_token: "fixture-access-token".into(),
            ..Default::default()
        }
    }

    #[test]
    fn sends_required_headers_and_maps_usage_response() {
        let fake = Fake::new(json!({
            "email": "acct@example.org",
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 42.5,
                    "reset_at": 1_700_000_000,
                    "reset_after_seconds": 3600,
                    "limit_window_seconds": 18000
                },
                "secondary_window": {
                    "used_percent": 10,
                    "reset_at": null,
                    "reset_after_seconds": null,
                    "limit_window_seconds": 604800
                }
            }
        }));
        let auth = auth();
        let mut account = Account::from_auth(&auth);

        fill(&fake, &auth, false, &mut account).unwrap();

        {
            let requests = fake.requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].url, URL);
            assert_eq!(
                header(&requests[0], "Authorization").as_deref(),
                Some("Bearer fixture-access-token")
            );
            assert_eq!(
                header(&requests[0], "chatgpt-account-id").as_deref(),
                Some("acct-1")
            );
            assert_eq!(
                header(&requests[0], "originator").as_deref(),
                Some("codex_cli_rs")
            );
            assert_eq!(
                header(&requests[0], "User-Agent").as_deref(),
                Some(USER_AGENT)
            );
        }

        assert_eq!(account.email.as_deref(), Some("acct@example.org"));
        assert_eq!(account.plan.as_deref(), Some("pro"));
        assert_eq!(account.allowed, Some(true));
        assert_eq!(account.limit_reached, Some(false));
        assert!(account.raw.is_none());
        assert_eq!(account.windows.len(), 2);
        assert_eq!(
            account.windows[0],
            Window {
                label: "5h".into(),
                used_percent: Some(42.5),
                reset_at: Some(json!(1_700_000_000)),
                reset_after: Some(3600.0),
            }
        );
        assert_eq!(account.windows[1].label, "7d");
        assert_eq!(account.windows[1].used_percent, Some(10.0));
        assert!(account.windows[1].reset_at.is_none());
        assert!(account.windows[1].reset_after.is_none());
    }

    #[test]
    fn missing_response_identity_falls_back_to_auth_fields() {
        let fake = Fake::new(json!({"rate_limit": {"allowed": false}}));
        let auth = auth();
        let mut account = Account::from_auth(&auth);

        fill(&fake, &auth, false, &mut account).unwrap();

        assert_eq!(account.email.as_deref(), Some("fallback@example.org"));
        assert_eq!(account.plan.as_deref(), Some("fallback-plan"));
        assert_eq!(account.allowed, Some(false));
        assert!(account.windows.is_empty());
    }

    #[test]
    fn credential_looking_response_metadata_falls_back_to_auth() {
        let token = "known-access-token-fixture";
        let fake = Fake::new(json!({
            "email": format!("Bearer {token}"),
            "plan_type": token,
            "rate_limit": {"allowed": true}
        }));
        let mut auth = auth();
        auth.access_token = token.into();
        let mut account = Account::from_auth(&auth);

        fill(&fake, &auth, false, &mut account).unwrap();

        assert_eq!(account.email.as_deref(), Some("fallback@example.org"));
        assert_eq!(account.plan.as_deref(), Some("fallback-plan"));
    }

    #[test]
    fn labels_match_legacy_window_sizes() {
        for (seconds, expected) in [
            (json!(18000), "5h"),
            (json!(86400), "1d"),
            (json!(604800), "7d"),
            (json!(2592000), "30d"),
            (json!(21600), "5h"),
            (json!(28800), "8h"),
            (json!("18000"), "5h"),
            (json!("nope"), "?"),
        ] {
            assert_eq!(label_for(Some(&seconds)), expected, "{seconds}");
        }
        assert_eq!(label_for(None), "?");
    }

    #[test]
    fn raw_response_is_redacted_recursively() {
        let token = "known-access-token-fixture";
        let fake = Fake::new(json!({
            "rate_limit": {"allowed": true},
            "access_token": token,
            "nested": {"refresh_token": "refresh-fixture", "safe": "kept"},
            "note": format!("uses {token} inline")
        }));
        let mut auth = auth();
        auth.access_token = token.into();
        let mut account = Account::from_auth(&auth);

        fill(&fake, &auth, true, &mut account).unwrap();

        let raw = serde_json::to_string(account.raw.as_ref().unwrap()).unwrap();
        assert!(!raw.contains(token), "{raw}");
        assert!(!raw.contains("refresh-fixture"), "{raw}");
        assert!(raw.contains("<redacted>"), "{raw}");
        assert!(raw.contains("kept"), "{raw}");
    }

    #[test]
    fn missing_access_token_is_an_error_without_a_request() {
        let fake = Fake::new(json!({}));
        let mut auth = auth();
        auth.access_token.clear();
        let mut account = Account::from_auth(&auth);

        let error = fill(&fake, &auth, false, &mut account).unwrap_err();

        assert_eq!(error, "no access_token in the auth file");
        assert!(fake.requests().is_empty());
    }
}
