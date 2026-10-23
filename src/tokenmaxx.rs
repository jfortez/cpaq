//! Tokenmaxxing keep-alive ping: sends chat-completion requests through the
//! local CLIProxyAPI so upstream rate-limit windows stay warm.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    cli::Args,
    config::{Config, ProxyConfig},
    paths::Paths,
};

/// A single chat-completion request after credentials have been resolved.
struct HttpRequest<'a> {
    url: &'a str,
    authorization: &'a str,
    content_type: &'static str,
    body: &'a str,
    timeout_secs: u64,
}

struct Reply {
    status: u16,
    body: String,
}

/// Injectable HTTP seam so unit tests never touch a real proxy.
trait Transport {
    fn post(&self, request: &HttpRequest<'_>) -> Result<Reply, String>;
}

struct UreqTransport;

impl Transport for UreqTransport {
    fn post(&self, request: &HttpRequest<'_>) -> Result<Reply, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(request.timeout_secs.max(1)))
            .build();
        let response = agent
            .post(request.url)
            .set("authorization", request.authorization)
            .set("content-type", request.content_type)
            .send_string(request.body);
        match response {
            Ok(response) => Ok(Reply {
                status: response.status(),
                body: read_body(response),
            }),
            Err(ureq::Error::Status(status, response)) => Ok(Reply {
                status,
                body: read_body(response),
            }),
            // Never surface the transport error text: it can contain the URL.
            Err(ureq::Error::Transport(_)) => Err("request failed".into()),
        }
    }
}

fn read_body(response: ureq::Response) -> String {
    response.into_string().unwrap_or_default()
}

/// Injectable quota snapshot seam around the frozen `quota` module API.
trait Snapshot {
    fn render(&self, dir: &Path, paths: &Paths, config: &Config) -> Result<String, String>;
}

struct LiveSnapshot;

impl Snapshot for LiveSnapshot {
    fn render(&self, dir: &Path, paths: &Paths, config: &Config) -> Result<String, String> {
        let accounts = crate::quota::fetch(dir, paths, config, &[], false)?;
        Ok(crate::quota::render_human(
            &accounts,
            dir,
            crate::time::now(),
        ))
    }
}

/// Destination for timestamped lines; production tees a file and stdout.
trait LogSink {
    fn write_line(&mut self, line: &str) -> Result<(), String>;
}

struct FileSink {
    file: File,
}

impl FileSink {
    fn open(path: &Path) -> Result<Self, String> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|_| "Cannot create ping log directory".to_string())?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|_| "Cannot open ping log for append".to_string())?;
        Ok(Self { file })
    }
}

impl LogSink for FileSink {
    fn write_line(&mut self, line: &str) -> Result<(), String> {
        writeln!(self.file, "{line}").map_err(|_| "Cannot append to ping log".to_string())
    }
}

struct StdoutSink;

impl LogSink for StdoutSink {
    fn write_line(&mut self, line: &str) -> Result<(), String> {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{line}")
            .and_then(|()| stdout.flush())
            .map_err(|_| "Cannot write ping output".to_string())
    }
}

struct Tee<A, B>(A, B);

impl<A: LogSink, B: LogSink> LogSink for Tee<A, B> {
    fn write_line(&mut self, line: &str) -> Result<(), String> {
        self.0.write_line(line)?;
        self.1.write_line(line)
    }
}

/// Run one keep-alive pass.
///
/// Returns `0` when at least one ping succeeded and `1` when every ping
/// failed. Configuration, credential and log-setup failures are errors.
pub fn run(args: &Args, paths: &Paths, config: &Config) -> Result<i32, String> {
    let proxy = config.proxy_credentials(paths)?;
    let path = log_path(args, paths, config);
    let file = FileSink::open(&path)?;
    let mut sink = Tee(file, StdoutSink);
    run_with(
        args,
        paths,
        config,
        &proxy,
        &UreqTransport,
        &mut sink,
        &LiveSnapshot,
    )
}

fn log_path(args: &Args, paths: &Paths, config: &Config) -> PathBuf {
    args.log
        .as_deref()
        .map_or_else(|| config.log_path(paths), |path| paths.expand(path))
}

fn run_with(
    args: &Args,
    paths: &Paths,
    config: &Config,
    proxy: &ProxyConfig,
    transport: &dyn Transport,
    sink: &mut dyn LogSink,
    snapshot: &dyn Snapshot,
) -> Result<i32, String> {
    // A missing auth directory is not fatal: the keep-alive still needs its
    // minimum of one ping.
    let auth_dir = paths.resolve_auth(None, config.auth_dir()).ok();
    let accounts = auth_dir.as_deref().map_or(0, count_accounts);
    let per_account = config.tokenmaxx.pings_per_account.max(1);
    let total = args
        .pings
        .unwrap_or_else(|| accounts.saturating_mul(per_account).max(1));
    let model = args
        .model
        .clone()
        .unwrap_or_else(|| config.tokenmaxx.model.clone());
    let endpoint = format!(
        "{}/v1/chat/completions",
        proxy.base_url.trim_end_matches('/')
    );
    let authorization = format!("Bearer {}", proxy.api_key);
    let body = serde_json::json!({
        "model": &model,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 16,
        "stream": false,
    })
    .to_string();
    let model_label = log_value(&model);

    let mut succeeded = 0_usize;
    for index in 1..=total {
        let stamp = timestamp();
        let request = HttpRequest {
            url: &endpoint,
            authorization: &authorization,
            content_type: "application/json",
            body: &body,
            timeout_secs: proxy.timeout_secs,
        };
        let line = match transport.post(&request) {
            Ok(reply) if (200..300).contains(&reply.status) => {
                succeeded += 1;
                usage_summary(&reply.body).map_or_else(
                    || format!("{stamp} ping {index}/{total} model={model_label} status=ok"),
                    |usage| {
                        format!(
                            "{stamp} ping {index}/{total} model={model_label} status=ok {usage}"
                        )
                    },
                )
            }
            Ok(reply) => format!(
                "{stamp} ping {index}/{total} model={model_label} status=error http={}",
                reply.status
            ),
            Err(_) => format!(
                "{stamp} ping {index}/{total} model={model_label} status=error reason=request-failed"
            ),
        };
        sink.write_line(&line)?;
    }

    if !args.no_quota {
        record_snapshot(auth_dir.as_deref(), paths, config, snapshot, sink)?;
    }

    Ok(i32::from(succeeded == 0))
}

fn count_accounts(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|entry| {
                let path = entry.path();
                path.is_file()
                    && path.extension().and_then(|extension| extension.to_str()) == Some("json")
            })
            .count()
    })
}

fn record_snapshot(
    dir: Option<&Path>,
    paths: &Paths,
    config: &Config,
    snapshot: &dyn Snapshot,
    sink: &mut dyn LogSink,
) -> Result<(), String> {
    match dir.map(|dir| snapshot.render(dir, paths, config)) {
        Some(Ok(text)) => {
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                sink.write_line(&format!("{} quota| {}", timestamp(), line.trim()))?;
            }
        }
        // The raw snapshot error can contain paths or provider details; keep a
        // fixed line and preserve the ping outcome.
        Some(Err(_)) | None => {
            sink.write_line(&format!("{} quota snapshot unavailable", timestamp()))?;
        }
    }
    Ok(())
}

fn usage_summary(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let usage = value.get("usage")?;
    let number = |name: &str| usage.get(name).and_then(serde_json::Value::as_u64);
    let mut parts = Vec::new();
    if let Some(prompt) = number("prompt_tokens") {
        parts.push(format!("tokens_in={prompt}"));
    }
    if let Some(completion) = number("completion_tokens") {
        parts.push(format!("tokens_out={completion}"));
    }
    if let Some(total) = number("total_tokens") {
        parts.push(format!("tokens_total={total}"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn timestamp() -> String {
    crate::time::local(crate::time::now(), "%Y-%m-%d %H:%M:%S")
}

fn log_value(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() || character.is_whitespace() {
                '_'
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Mode;
    use crate::test_support::Sandbox;
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        fs,
        path::{Path, PathBuf},
    };

    #[derive(Clone, Debug, PartialEq)]
    struct Recorded {
        url: String,
        authorization: String,
        content_type: String,
        body: String,
        timeout_secs: u64,
    }

    #[derive(Default)]
    struct FakeTransport {
        seen: RefCell<Vec<Recorded>>,
        replies: RefCell<VecDeque<Result<Reply, String>>>,
    }

    impl FakeTransport {
        fn with(replies: impl IntoIterator<Item = Result<Reply, String>>) -> Self {
            Self {
                seen: RefCell::new(Vec::new()),
                replies: RefCell::new(replies.into_iter().collect()),
            }
        }

        fn requests(&self) -> usize {
            self.seen.borrow().len()
        }
    }

    impl Transport for FakeTransport {
        fn post(&self, request: &HttpRequest<'_>) -> Result<Reply, String> {
            self.seen.borrow_mut().push(Recorded {
                url: request.url.into(),
                authorization: request.authorization.into(),
                content_type: request.content_type.into(),
                body: request.body.into(),
                timeout_secs: request.timeout_secs,
            });
            self.replies.borrow_mut().pop_front().unwrap_or(Ok(Reply {
                status: 200,
                body: "{}".into(),
            }))
        }
    }

    #[derive(Default)]
    struct Capture {
        lines: Vec<String>,
    }

    impl LogSink for Capture {
        fn write_line(&mut self, line: &str) -> Result<(), String> {
            self.lines.push(line.into());
            Ok(())
        }
    }

    struct FixedSnapshot {
        text: Result<String, String>,
        calls: Cell<usize>,
    }

    impl FixedSnapshot {
        fn ok(text: &str) -> Self {
            Self {
                text: Ok(text.into()),
                calls: Cell::new(0),
            }
        }

        fn err(text: &str) -> Self {
            Self {
                text: Err(text.into()),
                calls: Cell::new(0),
            }
        }
    }

    impl Snapshot for FixedSnapshot {
        fn render(&self, _: &Path, _: &Paths, _: &Config) -> Result<String, String> {
            self.calls.set(self.calls.get() + 1);
            self.text.clone()
        }
    }

    fn args() -> Args {
        Args {
            mode: Mode::Ping,
            ..Args::default()
        }
    }

    fn sandbox_paths(sandbox: &Sandbox) -> Paths {
        Paths::new(
            sandbox.0.join("home"),
            Some(sandbox.0.join("config")),
            Some(sandbox.0.join("state")),
        )
    }

    fn fixture_config() -> Config {
        let mut config = Config::default();
        config.proxy.base_url = "http://proxy.test:8317".into();
        config.proxy.api_key = "fixture-key".into();
        config.proxy.timeout_secs = 5;
        config.tokenmaxx.model = "cfg-model".into();
        config.tokenmaxx.pings_per_account = 1;
        config
    }

    fn auth_dir(paths: &Paths, files: &[&str]) -> PathBuf {
        let dir = paths.config_home.join("cli-proxy-api/auths");
        fs::create_dir_all(&dir).unwrap();
        for file in files {
            fs::write(dir.join(file), "{}").unwrap();
        }
        dir
    }

    fn ok_reply(status: u16, body: &str) -> Result<Reply, String> {
        Ok(Reply {
            status,
            body: body.into(),
        })
    }

    fn execute(
        args: &Args,
        paths: &Paths,
        config: &Config,
        transport: &FakeTransport,
        sink: &mut Capture,
        snapshot: &FixedSnapshot,
    ) -> Result<i32, String> {
        let proxy = config.proxy_credentials(paths)?;
        run_with(args, paths, config, &proxy, transport, sink, snapshot)
    }

    #[test]
    fn posts_chat_completion_payload_and_bearer_header() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        auth_dir(&paths, &["codex-a.json"]);
        let mut config = fixture_config();
        config.proxy.base_url = "http://proxy.test:8317/".into();
        let mut args = args();
        args.model = Some("cli-model".into());
        args.pings = Some(1);
        args.no_quota = true;
        let transport = FakeTransport::default();
        let mut sink = Capture::default();
        let snapshot = FixedSnapshot::ok("ignored");

        let code = execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap();

        assert_eq!(code, 0);
        let seen = transport.seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].url, "http://proxy.test:8317/v1/chat/completions");
        assert_eq!(seen[0].authorization, "Bearer fixture-key");
        assert_eq!(seen[0].content_type, "application/json");
        assert_eq!(seen[0].timeout_secs, 5);
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body["model"], "cli-model");
        assert_eq!(body["max_tokens"], 16);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert_eq!(body["stream"], false);
        assert!(sink.lines.iter().all(|line| !line.contains("fixture-key")));
    }

    #[test]
    fn pings_scale_per_account_and_cli_pings_override() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        let dir = auth_dir(
            &paths,
            &[
                "codex-a.json",
                "codex-b.json",
                "claude-c.json",
                "ignored.JSON",
            ],
        );
        fs::create_dir(dir.join("not-an-account.json")).unwrap();
        let mut config = fixture_config();
        config.tokenmaxx.pings_per_account = 2;
        let mut args = args();
        args.no_quota = true;
        let snapshot = FixedSnapshot::ok("");

        let transport = FakeTransport::default();
        let mut sink = Capture::default();
        assert_eq!(
            execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap(),
            0
        );
        assert_eq!(transport.requests(), 6);
        assert_eq!(sink.lines.len(), 6);
        assert!(
            sink.lines
                .iter()
                .all(|line| line.len() > 19 && line.contains(" ping "))
        );

        let transport = FakeTransport::default();
        let mut sink = Capture::default();
        args.pings = Some(4);
        assert_eq!(
            execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap(),
            0
        );
        assert_eq!(transport.requests(), 4);
    }

    #[test]
    fn missing_auth_still_pings_once_and_config_model_log_path_apply() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        let mut config = fixture_config();
        config.tokenmaxx.log_path = Some("~/custom/ping.log".into());
        let mut args = args();
        args.no_quota = true;
        let transport = FakeTransport::default();
        let mut sink = Capture::default();
        let snapshot = FixedSnapshot::ok("");

        assert_eq!(
            execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap(),
            0
        );
        assert_eq!(transport.requests(), 1);
        let body: serde_json::Value =
            serde_json::from_str(&transport.seen.borrow()[0].body).unwrap();
        assert_eq!(body["model"], "cfg-model");
        assert_eq!(
            log_path(&args, &paths, &config),
            paths.home.join("custom/ping.log")
        );

        args.log = Some("~/cli.log".into());
        assert_eq!(log_path(&args, &paths, &config), paths.home.join("cli.log"));
    }

    #[test]
    fn success_and_http_failure_update_code_and_safe_log_lines() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        auth_dir(&paths, &["codex-a.json", "codex-b.json"]);
        let mut config = fixture_config();
        config.proxy.base_url = "http://user:secret@proxy.test:8317".into();
        let args = args();
        let snapshot = FixedSnapshot::ok("");
        let transport = FakeTransport::with([
            ok_reply(
                200,
                r#"{"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
            ),
            ok_reply(500, r#"{"error":"leaked-body-token"}"#),
        ]);
        let mut sink = Capture::default();

        assert_eq!(
            execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap(),
            0
        );
        assert_eq!(sink.lines.len(), 2);
        assert!(sink.lines[0].contains("status=ok"));
        assert!(sink.lines[0].contains("tokens_total=7"));
        assert!(sink.lines[1].contains("status=error"));
        assert!(sink.lines[1].contains("http=500"));
        for line in &sink.lines {
            assert!(!line.contains("leaked-body-token"));
            assert!(!line.contains("fixture-key"));
            assert!(!line.contains("user:secret"));
            assert!(!line.contains("http://"));
        }

        let transport = FakeTransport::with([ok_reply(500, "secret"), ok_reply(503, "secret")]);
        let mut sink = Capture::default();
        assert_eq!(
            execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap(),
            1
        );
    }

    #[test]
    fn quota_snapshot_toggle_and_failure_preserve_ping_outcome() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        auth_dir(&paths, &["codex-a.json"]);
        let config = fixture_config();
        let transport = FakeTransport::default();
        let mut args = args();
        args.no_quota = true;
        let snapshot = FixedSnapshot::ok("snapshot-must-not-run");
        let mut sink = Capture::default();

        assert_eq!(
            execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap(),
            0
        );
        assert_eq!(snapshot.calls.get(), 0);
        assert_eq!(sink.lines.len(), 1);

        args.no_quota = false;
        let snapshot = FixedSnapshot::ok("● a@example.org (plus) — OK\ntokens used 10%");
        let mut sink = Capture::default();
        assert_eq!(
            execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap(),
            0
        );
        assert_eq!(snapshot.calls.get(), 1);
        assert_eq!(sink.lines.len(), 3);
        assert_eq!(
            sink.lines[1].get(19..),
            Some(" quota| ● a@example.org (plus) — OK")
        );
        assert_eq!(sink.lines[2].get(19..), Some(" quota| tokens used 10%"));
        assert!(sink.lines.iter().any(|line| line.contains("a@example.org")));
        assert!(
            sink.lines
                .iter()
                .any(|line| line.contains("tokens used 10%"))
        );

        let snapshot = FixedSnapshot::err("raw /secret/path failure");
        let mut sink = Capture::default();
        assert_eq!(
            execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap(),
            0
        );
        assert!(
            sink.lines
                .iter()
                .any(|line| line.contains("quota snapshot unavailable"))
        );
        assert!(
            sink.lines
                .iter()
                .all(|line| !line.contains("raw /secret/path"))
        );
    }

    #[test]
    fn file_sink_appends_and_creates_parent_directories() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("state/tokenmaxx/ping.log");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "older\n").unwrap();

        let mut sink = FileSink::open(&path).unwrap();
        sink.write_line("newer").unwrap();
        drop(sink);

        assert_eq!(fs::read_to_string(&path).unwrap(), "older\nnewer\n");
    }

    #[test]
    fn run_reports_missing_credentials_and_unwritable_log() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        let mut config = fixture_config();
        config.proxy.api_key.clear();
        assert!(run(&args(), &paths, &config).is_err());

        config.proxy.api_key = "fixture-key".into();
        let blocked = sandbox.0.join("blocked-log");
        fs::create_dir_all(&blocked).unwrap();
        config.tokenmaxx.log_path = Some(blocked);
        assert!(run(&args(), &paths, &config).is_err());
    }
}
