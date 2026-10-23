//! Provider-specific quota endpoint requests and response mapping.
//!
//! HTTP is behind [`Transport`] so mappings and concurrency are testable
//! without network access or credentials.

use std::time::Duration;

use serde_json::Value;

use crate::auth::Auth;
use crate::quota::Account;

pub mod claude;
pub mod codex;

/// Browser-like user agent used by the reference codex client.
pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36";

/// A sanitized transport failure. It never carries a response body.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FetchError {
    Status { code: u16, reason: String },
    Other(String),
}

impl FetchError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Status { code, reason } => {
                let base = if reason.is_empty() {
                    format!("HTTP {code}")
                } else {
                    format!("HTTP {code} {reason}")
                };
                if *code == 401 {
                    format!(
                        "{base} — token expired? The proxy refreshes it automatically; retry later"
                    )
                } else {
                    base
                }
            }
            Self::Other(message) => message.clone(),
        }
    }
}

/// Request transport abstraction; tests inject a fake.
pub(crate) trait Transport: Sync {
    fn get_json(&self, url: &str, headers: &[(&str, String)]) -> Result<Value, FetchError>;
}

/// Blocking HTTP transport using ureq with the configured proxy timeout.
pub(crate) struct HttpTransport {
    timeout: Duration,
}

impl HttpTransport {
    pub(crate) fn new(timeout_secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(timeout_secs.max(1)),
        }
    }
}

impl Transport for HttpTransport {
    fn get_json(&self, url: &str, headers: &[(&str, String)]) -> Result<Value, FetchError> {
        let mut request = ureq::get(url).timeout(self.timeout);
        for (name, value) in headers {
            request = request.set(name, value);
        }
        match request.call() {
            Ok(response) => response
                .into_json::<Value>()
                .map_err(|_| FetchError::Other("invalid JSON response".into())),
            // Status text is server-controlled, so only our static label is used.
            Err(ureq::Error::Status(code, _response)) => Err(status_error(code)),
            Err(ureq::Error::Transport(error)) => Err(FetchError::Other(format!(
                "network error: {}",
                error.kind()
            ))),
        }
    }
}

/// Static reason labels for common HTTP failures; never echoes remote text.
fn status_label(code: u16) -> &'static str {
    match code {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

fn status_error(code: u16) -> FetchError {
    FetchError::Status {
        code,
        reason: status_label(code).into(),
    }
}

pub(crate) fn is_supported(provider: &str) -> bool {
    matches!(provider.to_ascii_lowercase().as_str(), "codex" | "claude")
}

pub(crate) fn unsupported_message(provider: &str) -> String {
    format!("provider '{provider}' not supported yet (add fetch_* and register it in PROVIDERS)")
}

pub(crate) fn query(transport: &dyn Transport, auth: &Auth, raw: bool) -> Result<Account, String> {
    let mut account = Account::from_auth(auth);
    match auth.provider.to_ascii_lowercase().as_str() {
        "codex" => codex::fill(transport, auth, raw, &mut account)?,
        "claude" => claude::fill(transport, auth, raw, &mut account)?,
        other => return Err(unsupported_message(other)),
    }
    Ok(account)
}

/// Response metadata must never surface credential material.
pub(crate) fn metadata_text(value: Option<&Value>, access_token: &str) -> Option<String> {
    let text = value?.as_str()?;
    if text.is_empty() || contains_credential(text, access_token) {
        return None;
    }
    Some(text.to_owned())
}

fn contains_credential(text: &str, access_token: &str) -> bool {
    if !access_token.is_empty() && text.contains(access_token) {
        return true;
    }
    text.to_ascii_lowercase().contains("bearer ") || text.starts_with("sk-") || jwt_shaped(text)
}

fn jwt_shaped(text: &str) -> bool {
    let mut parts = text.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(header), Some(_), Some(_), None) if header.starts_with("eyJ")
    )
}

/// Recursively remove credential-looking fields and known token material.
pub(crate) fn redact(value: &Value, access_token: &str) -> Value {
    let mut redacted = value.clone();
    redact_in_place(&mut redacted, access_token);
    redacted
}

fn redact_in_place(value: &mut Value, access_token: &str) {
    match value {
        Value::Object(fields) => {
            for (key, child) in fields.iter_mut() {
                if credential_key(key) {
                    *child = Value::String("<redacted>".into());
                } else {
                    redact_in_place(child, access_token);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_in_place(item, access_token);
            }
        }
        Value::String(text) if !access_token.is_empty() && text.contains(access_token) => {
            *text = text.replace(access_token, "<redacted>");
        }
        _ => {}
    }
}

fn credential_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "authorization",
        "apikey",
        "credential",
        "cookie",
        "privatekey",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redaction_removes_nested_credentials_and_known_tokens() {
        let token = "known-access-token-fixture";
        let value = json!({
            "email": "a@example.org",
            "access_token": token,
            "nested": {
                "refresh_token": "refresh-fixture",
                "client_secret": "secret-fixture",
                "password": "password-fixture",
                "authorization": "Bearer something-fixture",
                "safe": "kept",
                "note": format!("prefix {token} suffix")
            },
            "list": [
                {"cookie": "cookie-fixture"},
                token
            ]
        });

        let text = serde_json::to_string(&redact(&value, token)).unwrap();

        for secret in [
            token,
            "refresh-fixture",
            "secret-fixture",
            "password-fixture",
            "something-fixture",
            "cookie-fixture",
        ] {
            assert!(!text.contains(secret), "leaked {secret}: {text}");
        }
        assert!(text.contains("<redacted>"), "{text}");
        assert!(text.contains("a@example.org"), "{text}");
        assert!(text.contains("kept"), "{text}");
    }

    #[test]
    fn status_errors_include_401_hint_without_remote_body() {
        let unauthorized = status_error(401).message();
        assert_eq!(
            unauthorized,
            "HTTP 401 Unauthorized — token expired? The proxy refreshes it automatically; retry later"
        );
        assert!(!unauthorized.contains("fixture-body"));

        assert_eq!(status_error(403).message(), "HTTP 403 Forbidden");
        assert_eq!(status_error(429).message(), "HTTP 429 Too Many Requests");
        assert_eq!(status_error(599).message(), "HTTP 599");
        assert_eq!(
            FetchError::Other("network error: Connection refused".into()).message(),
            "network error: Connection refused"
        );
    }

    #[test]
    fn remote_reason_text_is_never_used() {
        // The ureq boundary maps only the numeric code to a static label, so a
        // hostile reason phrase cannot reach output.
        let error = status_error(401);
        assert_eq!(
            error,
            FetchError::Status {
                code: 401,
                reason: "Unauthorized".into()
            }
        );
        assert!(!format!("{error:?}").contains("Bearer"));
        assert!(!format!("{error:?}").contains("fixture"));
    }

    #[test]
    fn credential_looking_metadata_is_rejected() {
        let token = "known-access-token-fixture";
        assert_eq!(
            metadata_text(Some(&json!("a@example.org")), token).as_deref(),
            Some("a@example.org")
        );
        assert_eq!(
            metadata_text(Some(&json!(format!("Bearer {token}"))), token),
            None
        );
        assert_eq!(metadata_text(Some(&json!(token)), token), None);
        assert_eq!(metadata_text(Some(&json!("sk-live-fixture")), token), None);
        assert_eq!(metadata_text(Some(&json!("")), token), None);
        assert_eq!(metadata_text(Some(&json!(42)), token), None);
    }
}
