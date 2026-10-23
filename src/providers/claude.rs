//! Claude OAuth usage endpoint (preliminary, matching the reference mapping).

use super::{Transport, redact, truthy};
use crate::auth::Auth;
use crate::quota::{Account, Window};

pub(crate) const URL: &str = "https://api.anthropic.com/api/oauth/usage";

const WINDOWS: [(&str, &str); 4] = [
    ("five_hour", "5h"),
    ("seven_day", "7d"),
    ("seven_day_opus", "7d-opus"),
    ("seven_day_sonnet", "7d-sonnet"),
];

pub(crate) fn fill(
    transport: &dyn Transport,
    auth: &Auth,
    raw: bool,
    account: &mut Account,
) -> Result<(), String> {
    let headers: Vec<(&str, String)> = vec![
        ("Authorization", bearer(auth)?),
        ("anthropic-beta", "oauth-2025-04-20".into()),
        ("User-Agent", "claude-cli/2.1.0 (external, cli)".into()),
        ("Accept", "application/json".into()),
    ];

    let data = transport
        .get_json(URL, &headers)
        .map_err(|error| error.message())?;

    account.windows = WINDOWS
        .iter()
        .filter_map(|(key, label)| {
            let usage = data.get(*key)?.as_object()?;
            let utilization = usage.get("utilization").filter(|value| !value.is_null())?;
            let reset = usage
                .get("resets_at")
                .filter(|value| truthy(value))
                .or_else(|| usage.get("reset_at").filter(|value| truthy(value)));
            Some(Window {
                label: (*label).into(),
                used_percent: utilization.as_f64(),
                reset_at: reset.cloned(),
                reset_after: None,
            })
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::FetchError;
    use serde_json::{Value, json};
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
            file: "claude-a.json".into(),
            provider: "claude".into(),
            email: Some("claude@example.org".into()),
            plan: Some("max".into()),
            access_token: "fixture-access-token".into(),
            ..Default::default()
        }
    }

    #[test]
    fn sends_required_headers_and_maps_usage_windows() {
        let fake = Fake::new(json!({
            "five_hour": {
                "utilization": 25.0,
                "resets_at": "2023-11-14T22:13:20Z"
            },
            "seven_day": {
                "utilization": 5.5,
                "reset_at": 1_700_000_000
            },
            "seven_day_opus": {"utilization": 0},
            "seven_day_sonnet": null
        }));
        let auth = auth();
        let mut account = Account::from_auth(&auth);

        fill(&fake, &auth, false, &mut account).unwrap();

        {
            let requests = fake.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].url, URL);
            assert_eq!(
                header(&requests[0], "Authorization").as_deref(),
                Some("Bearer fixture-access-token")
            );
            assert_eq!(
                header(&requests[0], "anthropic-beta").as_deref(),
                Some("oauth-2025-04-20")
            );
            assert_eq!(
                header(&requests[0], "User-Agent").as_deref(),
                Some("claude-cli/2.1.0 (external, cli)")
            );
            assert_eq!(
                header(&requests[0], "Accept").as_deref(),
                Some("application/json")
            );
        }

        assert_eq!(account.email.as_deref(), Some("claude@example.org"));
        assert_eq!(account.plan.as_deref(), Some("max"));
        assert_eq!(account.allowed, None);
        assert_eq!(account.limit_reached, None);
        assert_eq!(account.windows.len(), 3);
        assert_eq!(account.windows[0].label, "5h");
        assert_eq!(account.windows[0].used_percent, Some(25.0));
        assert_eq!(
            account.windows[0].reset_at.as_ref().and_then(Value::as_str),
            Some("2023-11-14T22:13:20Z")
        );
        assert!(account.windows[0].reset_after.is_none());
        assert_eq!(account.windows[1].label, "7d");
        assert_eq!(account.windows[1].reset_at, Some(json!(1_700_000_000)));
        assert_eq!(account.windows[2].label, "7d-opus");
        assert_eq!(account.windows[2].used_percent, Some(0.0));
        assert!(account.windows[2].reset_at.is_none());
    }

    #[test]
    fn skips_windows_without_utilization() {
        let fake = Fake::new(json!({
            "five_hour": {"resets_at": "2023-11-14T22:13:20Z"},
            "seven_day": {"utilization": null},
            "seven_day_opus": {"utilization": 80},
            "seven_day_sonnet": "unexpected"
        }));
        let auth = auth();
        let mut account = Account::from_auth(&auth);

        fill(&fake, &auth, false, &mut account).unwrap();

        assert_eq!(account.windows.len(), 1);
        assert_eq!(account.windows[0].label, "7d-opus");
        assert_eq!(account.windows[0].used_percent, Some(80.0));
    }

    #[test]
    fn raw_response_is_redacted() {
        let token = "known-access-token-fixture";
        let fake = Fake::new(json!({
            "five_hour": {"utilization": 1, "access_token": token},
            "nested": {"password": "password-fixture"}
        }));
        let mut auth = auth();
        auth.access_token = token.into();
        let mut account = Account::from_auth(&auth);

        fill(&fake, &auth, true, &mut account).unwrap();

        let raw = serde_json::to_string(account.raw.as_ref().unwrap()).unwrap();
        assert!(!raw.contains(token), "{raw}");
        assert!(!raw.contains("password-fixture"), "{raw}");
        assert!(raw.contains("<redacted>"), "{raw}");
    }

    #[test]
    fn missing_access_token_is_an_error_without_a_request() {
        let fake = Fake::new(json!({}));
        let mut auth = auth();
        auth.access_token.clear();
        let mut account = Account::from_auth(&auth);

        let error = fill(&fake, &auth, false, &mut account).unwrap_err();

        assert_eq!(error, "no access_token in the auth file");
        assert!(fake.requests.lock().unwrap().is_empty());
    }
}
