use crate::paths::Paths;
use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use serde_json::{Map, Value};
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Default)]
pub struct Auth {
    pub file: String,
    pub provider: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub account_id: Option<String>,
    pub access_token: String,
    pub error: Option<String>,
}

pub trait AuthReader {
    fn read(&self, path: &Path) -> io::Result<String>;
    fn docker_read(&self, container: &str, path: &str) -> Result<String, String>;
}
pub struct FileReader {
    socket: PathBuf,
}
impl FileReader {
    /// Reader bound to the Docker socket from `CPA_DOCKER_SOCKET` or the default.
    pub fn new() -> Self {
        Self {
            socket: crate::docker::socket_path(),
        }
    }
    /// Reader bound to an explicit socket path, so tests never touch the real one.
    pub fn with_socket(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }
}
impl Default for FileReader {
    fn default() -> Self {
        Self::new()
    }
}
impl AuthReader for FileReader {
    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
    /// Reads a root-owned file through the Docker Engine API rather than
    /// spawning `docker exec`, whose CLI child dominated the ping's memory peak.
    fn docker_read(&self, container: &str, path: &str) -> Result<String, String> {
        crate::docker::read_file(&self.socket, container, path)
    }
}

/// Parse one auth file. Malformed input yields an `Auth` carrying a safe error
/// message; raw JSON errors are never echoed because they may quote the file.
pub fn parse(file: &str, text: &str) -> Auth {
    let mut auth = Auth {
        file: file.to_string(),
        provider: provider_from_file(file),
        ..Auth::default()
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        auth.error = Some("Auth file is not valid JSON".into());
        return auth;
    };
    let Value::Object(root) = value else {
        auth.error = Some("Auth file root must be a JSON object".into());
        return auth;
    };
    let tokens = root.get("tokens").and_then(Value::as_object);
    if let Some(kind) = root
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
    {
        auth.provider = kind.to_ascii_lowercase();
    }
    // Top-level fields win over the nested `tokens` shape.
    auth.access_token = string_field(&root, tokens, "access_token")
        .unwrap_or_default()
        .to_string();
    auth.account_id = string_field(&root, tokens, "account_id").map(str::to_string);
    auth.email = string_field(&root, tokens, "email").map(str::to_string);
    auth.plan = string_field(&root, tokens, "plan_type").map(str::to_string);
    let claims = string_field(&root, tokens, "id_token").and_then(jwt_claims);
    if auth.email.is_none() {
        auth.email = claims
            .as_ref()
            .and_then(|claims| claims.get("email"))
            .and_then(Value::as_str)
            .filter(|email| !email.is_empty())
            .map(str::to_string);
    }
    if auth.plan.is_none() {
        auth.plan = claims.as_ref().and_then(plan_from_claims);
    }
    auth
}

/// Load every `.json` account in `dir`, sorted by filename. File-level failures
/// become `Auth::error` entries so one bad account never hides the rest.
pub fn load(dir: &Path, paths: &Paths, reader: &impl AuthReader) -> Result<Vec<Auth>, String> {
    let entries = std::fs::read_dir(dir).map_err(|error| {
        format!(
            "Cannot read auth directory {}: {}",
            dir.display(),
            error.kind()
        )
    })?;
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    let mut auths = Vec::with_capacity(files.len());
    for path in files {
        let file = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let provider = provider_from_file(&file);
        let auth = match read_auth(&path, &file, paths, reader) {
            Ok(text) => parse(&file, &text),
            Err(error) => Auth {
                file,
                provider,
                error: Some(error),
                ..Auth::default()
            },
        };
        auths.push(auth);
    }
    Ok(auths)
}

fn string_field<'a>(
    root: &'a Map<String, Value>,
    tokens: Option<&'a Map<String, Value>>,
    key: &str,
) -> Option<&'a str> {
    root.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            tokens
                .and_then(|nested| nested.get(key))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
}

fn provider_from_file(file: &str) -> String {
    let name = Path::new(file)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(file);
    let stem = name.strip_suffix(".json").unwrap_or(name);
    stem.split('-')
        .next()
        .unwrap_or(stem)
        .trim()
        .to_ascii_lowercase()
}

fn read_auth(
    path: &Path,
    file: &str,
    paths: &Paths,
    reader: &impl AuthReader,
) -> Result<String, String> {
    match reader.read(path) {
        Ok(text) => Ok(text),
        // Only a permissions problem justifies the docker escape hatch.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            let directory = paths.container_auth_dir.trim_end_matches('/');
            reader
                .docker_read(&paths.docker_container, &format!("{directory}/{file}"))
                .map_err(|detail| format!("Auth file unreadable ({detail})"))
        }
        Err(error) => Err(format!(
            "Auth file unreadable: {}",
            describe_read_error(error.kind())
        )),
    }
}

fn describe_read_error(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "file not found",
        io::ErrorKind::InvalidData => "file is not valid UTF-8",
        _ => "I/O error",
    }
}

/// Decode a JWT payload without validating the signature; invalid tokens are
/// simply absent claims rather than errors.
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok().filter(Value::is_object)
}

fn plan_from_claims(claims: &Value) -> Option<String> {
    plan_in(claims)
        .or_else(|| claims.get("https://api.openai.com/auth").and_then(plan_in))
        .map(str::to_string)
}

fn plan_in(value: &Value) -> Option<&str> {
    value
        .get("chatgpt_plan_type")
        .or_else(|| value.get("plan_type"))
        .and_then(Value::as_str)
        .filter(|plan| !plan.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_auth_shapes_bom_and_jwt_fallback() {
        let flat = parse(
            "codex-a.json",
            "\u{feff}{\"type\":\"codex\",\"email\":\"a@example.org\",\"access_token\":\"fixture\",\"plan_type\":\"plus\"}",
        );
        assert_eq!(flat.provider, "codex");
        assert_eq!(flat.plan.as_deref(), Some("plus"));
        let payload = URL_SAFE_NO_PAD.encode(br#"{"email":"b@example.org","https://api.openai.com/auth":{"chatgpt_plan_type":"pro"}}"#);
        let nested = parse(
            "codex-b.json",
            &format!(
                r#"{{"tokens":{{"access_token":"fixture","account_id":"acct","id_token":"x.{payload}.x"}}}}"#
            ),
        );
        assert_eq!(nested.email.as_deref(), Some("b@example.org"));
        assert_eq!(nested.plan.as_deref(), Some("pro"));
        assert_eq!(nested.account_id.as_deref(), Some("acct"));
    }
    #[test]
    fn malformed_auth_never_echoes_credential_material() {
        let a = parse("codex-bad.json", r#"{"access_token":"fixture" bad}"#);
        assert!(a.error.is_some());
        assert!(!a.error.unwrap().contains("fixture"));
    }
    #[test]
    fn permission_error_uses_injected_docker_reader() {
        use std::cell::RefCell;
        struct Fake(RefCell<Vec<String>>);
        impl AuthReader for Fake {
            fn read(&self, _: &Path) -> io::Result<String> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
            fn docker_read(&self, c: &str, p: &str) -> Result<String, String> {
                self.0.borrow_mut().push(format!("{c}:{p}"));
                Ok(r#"{"type":"codex","access_token":"fixture"}"#.into())
            }
        }
        let s = crate::test_support::Sandbox::default();
        std::fs::write(s.0.join("codex-a.json"), "{}").unwrap();
        let paths = Paths::new(s.0.clone(), None, None);
        let fake = Fake(RefCell::new(vec![]));
        let auths = load(&s.0, &paths, &fake).unwrap();
        assert_eq!(auths.len(), 1);
        assert_eq!(
            *fake.0.borrow(),
            vec!["cli-proxy-api:/root/.cli-proxy-api/codex-a.json"]
        );
    }
    #[test]
    fn sorted_files_isolated_errors_and_top_level_override() {
        let s = crate::test_support::Sandbox::default();
        std::fs::write(s.0.join("codex-b.json"), r#"{"type":"codex","email":"top@example.org","access_token":"top","account_id":"top-acct","tokens":{"email":"nested@example.org","access_token":"nested","account_id":"nested-acct"}}"#).unwrap();
        std::fs::write(
            s.0.join("codex-a.json"),
            r#"{"access_token":"secret-fixture" bad}"#,
        )
        .unwrap();
        std::fs::write(s.0.join("notes.txt"), "{}").unwrap();
        std::fs::create_dir(s.0.join("codex-c.json")).unwrap();
        let paths = Paths::new(s.0.clone(), None, None);
        let auths = load(&s.0, &paths, &FileReader::default()).unwrap();
        assert_eq!(
            auths.iter().map(|a| a.file.as_str()).collect::<Vec<_>>(),
            ["codex-a.json", "codex-b.json"]
        );
        assert_eq!(auths[0].provider, "codex");
        let err = auths[0].error.as_deref().unwrap_or_default();
        assert!(!err.is_empty());
        assert!(!err.contains("secret-fixture"));
        assert_eq!(auths[1].email.as_deref(), Some("top@example.org"));
        assert_eq!(auths[1].access_token, "top");
        assert_eq!(auths[1].account_id.as_deref(), Some("top-acct"));
    }
    #[test]
    fn filename_prefix_provider_fallback_and_type_override() {
        let nested = format!("\u{feff}{}", r#"{"tokens":{"access_token":"fixture"}}"#);
        let a = parse("claude-abc.json", &nested);
        assert_eq!(a.provider, "claude");
        assert_eq!(a.access_token, "fixture");
        assert_eq!(parse("gemini-pro-1.json", "{}").provider, "gemini");
        assert_eq!(parse("codex.json", "{}").provider, "codex");
        assert_eq!(
            parse("claude-x.json", r#"{"type":"codex"}"#).provider,
            "codex"
        );
    }
    #[test]
    fn jwt_plan_claim_variants_and_invalid_payloads_are_safe() {
        let token = |claims: &str| format!("x.{}.x", URL_SAFE_NO_PAD.encode(claims.as_bytes()));
        for (claims, plan) in [
            (r#"{"chatgpt_plan_type":"plus"}"#, "plus"),
            (r#"{"plan_type":"team"}"#, "team"),
            (
                r#"{"https://api.openai.com/auth":{"chatgpt_plan_type":"pro"}}"#,
                "pro",
            ),
            (
                r#"{"https://api.openai.com/auth":{"plan_type":"enterprise"}}"#,
                "enterprise",
            ),
        ] {
            let a = parse(
                "codex-j.json",
                &format!(
                    r#"{{"type":"codex","access_token":"fixture","id_token":"{}"}}"#,
                    token(claims)
                ),
            );
            assert_eq!(a.plan.as_deref(), Some(plan));
            assert!(a.error.is_none());
        }
        let broken = parse(
            "codex-j.json",
            r#"{"type":"codex","access_token":"fixture","id_token":"x.@@@.x"}"#,
        );
        assert_eq!(broken.email, None);
        assert_eq!(broken.plan, None);
        assert!(broken.error.is_none());
    }
    #[test]
    fn docker_fallback_only_on_permission_denied() {
        use std::cell::RefCell;
        struct Fake {
            kind: io::ErrorKind,
            calls: RefCell<Vec<String>>,
            docker: Result<&'static str, &'static str>,
        }
        impl AuthReader for Fake {
            fn read(&self, _: &Path) -> io::Result<String> {
                Err(io::Error::from(self.kind))
            }
            fn docker_read(&self, c: &str, p: &str) -> Result<String, String> {
                self.calls.borrow_mut().push(format!("{c}:{p}"));
                self.docker.map(str::to_string).map_err(str::to_string)
            }
        }
        let s = crate::test_support::Sandbox::default();
        std::fs::write(s.0.join("codex-a.json"), "{}").unwrap();
        let mut paths = Paths::new(s.0.clone(), None, None);
        paths.container_auth_dir = "/srv/auths/".into();
        paths.docker_container = "box".into();
        let denied = Fake {
            kind: io::ErrorKind::PermissionDenied,
            calls: RefCell::new(vec![]),
            docker: Ok(r#"{"type":"codex","access_token":"fixture"}"#),
        };
        let auths = load(&s.0, &paths, &denied).unwrap();
        assert_eq!(*denied.calls.borrow(), ["box:/srv/auths/codex-a.json"]);
        assert_eq!(auths[0].access_token, "fixture");
        assert!(auths[0].error.is_none());
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::InvalidData] {
            let fake = Fake {
                kind,
                calls: RefCell::new(vec![]),
                docker: Ok("unused"),
            };
            let auths = load(&s.0, &paths, &fake).unwrap();
            assert!(auths[0].error.is_some());
            assert!(fake.calls.borrow().is_empty());
        }
        let failing = Fake {
            kind: io::ErrorKind::PermissionDenied,
            calls: RefCell::new(vec![]),
            docker: Err("Docker API request failed; retry later"),
        };
        let auths = load(&s.0, &paths, &failing).unwrap();
        let err = auths[0].error.as_deref().unwrap();
        assert!(err.contains("Docker API request failed"));
        assert!(!err.contains("fixture"));
        assert_eq!(auths[0].provider, "codex");
    }
    #[test]
    fn missing_auth_dir_errors_while_empty_dir_is_ok() {
        let s = crate::test_support::Sandbox::default();
        let paths = Paths::new(s.0.clone(), None, None);
        assert!(load(&s.0.join("absent"), &paths, &FileReader::default()).is_err());
        assert!(
            load(&s.0, &paths, &FileReader::default())
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn malformed_roots_and_field_types_do_not_panic() {
        for text in ["[]", "null", "42", "\"codex\""] {
            let a = parse("codex-x.json", text);
            assert!(a.error.is_some());
            assert_eq!(a.provider, "codex");
        }
        let a = parse(
            "claude-x.json",
            r#"{"type":7,"tokens":[],"email":null,"access_token":"fixture"}"#,
        );
        assert_eq!(a.provider, "claude");
        assert_eq!(a.email, None);
        assert_eq!(a.access_token, "fixture");
        assert!(a.error.is_none());
    }
    #[test]
    fn permission_denied_reads_the_file_through_the_container_socket() {
        struct SocketReader(FileReader);
        impl AuthReader for SocketReader {
            fn read(&self, _: &Path) -> io::Result<String> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
            fn docker_read(&self, container: &str, path: &str) -> Result<String, String> {
                self.0.docker_read(container, path)
            }
        }
        let s = crate::test_support::Sandbox::default();
        std::fs::write(s.0.join("codex-a.json"), "{}").unwrap();
        let archive = crate::docker::testing::tar_regular(
            "codex-a.json",
            br#"{"type":"codex","access_token":"fixture"}"#,
        );
        let (socket, server) = crate::docker::testing::serve(
            &s.0,
            vec![crate::docker::testing::with_length(&archive)],
        );
        let paths = Paths::new(s.0.clone(), None, None);

        let auths = load(&s.0, &paths, &SocketReader(FileReader::with_socket(socket))).unwrap();

        let requests = server.join().unwrap();
        let request = String::from_utf8(requests[0].clone()).unwrap();
        assert!(
            request.starts_with(
                "GET /v1.41/containers/cli-proxy-api/archive?path=%2Froot%2F.cli-proxy-api%2Fcodex-a.json "
            ),
            "{request}"
        );
        assert_eq!(auths[0].access_token, "fixture");
        assert!(auths[0].error.is_none());
    }
    #[test]
    fn socket_failures_are_reported_per_file_without_the_docker_cli() {
        let s = crate::test_support::Sandbox::default();
        std::fs::write(s.0.join("codex-a.json"), "{}").unwrap();
        let reader = FileReader::with_socket(s.0.join("absent.sock"));

        let error = reader
            .docker_read("cli-proxy-api", "/root/.cli-proxy-api/codex-a.json")
            .unwrap_err();
        assert_eq!(error, "Cannot reach the Docker socket");
        assert!(!error.contains("docker exec"));
    }
}
