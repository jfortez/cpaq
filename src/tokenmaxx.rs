//! Tokenmaxxing keep-alive ping: sends chat-completion requests through the
//! local CLIProxyAPI so upstream rate-limit windows stay warm.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{
    cli::Args,
    config::{Config, ProxyConfig},
    paths::Paths,
};

/// Attempts per ping when the transport fails (DNS, connect, timeout).
const PING_ATTEMPTS: usize = 3;
/// Backoff before the second and third attempt.
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(3), Duration::from_secs(6)];
/// Whole-run budget: retries stop and attempt timeouts shrink once it is spent,
/// so even a dead network cannot keep the systemd run hanging for long.
const RUN_BUDGET: Duration = Duration::from_secs(120);

/// A single chat-completion request after credentials have been resolved.
#[derive(Clone, Copy)]
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

/// Injectable sleep so retry backoff never slows tests down.
trait Backoff {
    fn wait(&self, delay: Duration);
}

struct ThreadBackoff;

impl Backoff for ThreadBackoff {
    fn wait(&self, delay: Duration) {
        std::thread::sleep(delay);
    }
}

/// Retry policy for one keep-alive pass.
#[derive(Clone, Debug)]
struct Retry {
    attempts: usize,
    delays: Vec<Duration>,
    budget: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: PING_ATTEMPTS,
            delays: RETRY_DELAYS.to_vec(),
            budget: RUN_BUDGET,
        }
    }
}

/// Everything one keep-alive pass talks to, so tests can replace all of it.
struct Seams<'a> {
    transport: &'a dyn Transport,
    backoff: &'a dyn Backoff,
    snapshot: &'a dyn Snapshot,
    retry: Retry,
}

impl Seams<'_> {
    /// Sends one ping, retrying transport failures with backoff.
    ///
    /// An HTTP status is an answer, not a transport failure, so 4xx/5xx are
    /// never retried. The per-attempt timeout shrinks with the remaining budget
    /// to keep the whole pass bounded even when every attempt hangs.
    fn post(&self, request: &HttpRequest<'_>, deadline: Instant) -> Result<Reply, String> {
        let mut last = "request failed".to_string();
        for attempt in 1..=self.retry.attempts.max(1) {
            // Round up: a budget of 7s must not become a 6s timeout mid-run.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let budget_secs = u64::try_from(remaining.as_millis().div_ceil(1000))
                .unwrap_or(u64::MAX)
                .max(1);
            let attempt_request = HttpRequest {
                timeout_secs: request.timeout_secs.min(budget_secs),
                ..*request
            };
            match self.transport.post(&attempt_request) {
                Ok(reply) => return Ok(reply),
                Err(error) => last = error,
            }
            let Some(delay) = self.retry.delays.get(attempt - 1).copied() else {
                break;
            };
            if delay > deadline.saturating_duration_since(Instant::now()) {
                break;
            }
            self.backoff.wait(delay);
        }
        Err(last)
    }
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
    run_output(args, paths, config, true)
}

/// Runs the existing pass, optionally keeping stdout free for auto JSON.
pub(crate) fn run_output(
    args: &Args,
    paths: &Paths,
    config: &Config,
    stdout: bool,
) -> Result<i32, String> {
    let proxy = config.proxy_credentials(paths)?;
    let path = log_path(args, paths, config);
    let mut file = FileSink::open(&path)?;
    let seams = Seams {
        transport: &UreqTransport,
        backoff: &ThreadBackoff,
        snapshot: &LiveSnapshot,
        retry: Retry::default(),
    };
    if stdout {
        run_with(
            args,
            paths,
            config,
            &proxy,
            &seams,
            &mut Tee(file, StdoutSink),
        )
    } else {
        run_with(args, paths, config, &proxy, &seams, &mut file)
    }
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
    seams: &Seams<'_>,
    sink: &mut dyn LogSink,
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
    let deadline = Instant::now() + seams.retry.budget;

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
        let line = match seams.post(&request, deadline) {
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
        record_snapshot(auth_dir.as_deref(), paths, config, seams.snapshot, sink)?;
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
    match dir.map(|dir| snapshot_with_retry(dir, paths, config, snapshot)) {
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

/// Renders the quota snapshot, retrying once: a transient DNS/connect failure
/// right after boot should not lose the whole snapshot.
fn snapshot_with_retry(
    dir: &Path,
    paths: &Paths,
    config: &Config,
    snapshot: &dyn Snapshot,
) -> Result<String, String> {
    snapshot
        .render(dir, paths, config)
        .or_else(|_| snapshot.render(dir, paths, config))
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

    #[derive(Default)]
    struct RecordingBackoff {
        waits: RefCell<Vec<Duration>>,
    }

    impl Backoff for RecordingBackoff {
        fn wait(&self, delay: Duration) {
            self.waits.borrow_mut().push(delay);
        }
    }

    struct FixedSnapshot {
        results: RefCell<VecDeque<Result<String, String>>>,
        calls: Cell<usize>,
    }

    impl FixedSnapshot {
        fn ok(text: &str) -> Self {
            Self::sequence([Ok(text.to_string())])
        }

        fn sequence(results: impl IntoIterator<Item = Result<String, String>>) -> Self {
            Self {
                results: RefCell::new(results.into_iter().collect()),
                calls: Cell::new(0),
            }
        }
    }

    impl Snapshot for FixedSnapshot {
        fn render(&self, _: &Path, _: &Paths, _: &Config) -> Result<String, String> {
            self.calls.set(self.calls.get() + 1);
            self.results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(String::new()))
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
        execute_with(
            args,
            paths,
            config,
            transport,
            sink,
            snapshot,
            &RecordingBackoff::default(),
            Retry::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_with(
        args: &Args,
        paths: &Paths,
        config: &Config,
        transport: &FakeTransport,
        sink: &mut Capture,
        snapshot: &FixedSnapshot,
        backoff: &RecordingBackoff,
        retry: Retry,
    ) -> Result<i32, String> {
        let proxy = config.proxy_credentials(paths)?;
        let seams = Seams {
            transport,
            backoff,
            snapshot,
            retry,
        };
        run_with(args, paths, config, &proxy, &seams, sink)
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

        let snapshot = FixedSnapshot::sequence([
            Err("raw /secret/path failure".into()),
            Err("raw /secret/path failure".into()),
        ]);
        let mut sink = Capture::default();
        assert_eq!(
            execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap(),
            0
        );
        assert_eq!(snapshot.calls.get(), 2, "the snapshot is retried once");
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
    fn transport_failures_retry_with_backoff_then_succeed() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        auth_dir(&paths, &["codex-a.json"]);
        let config = fixture_config();
        let mut args = args();
        args.pings = Some(1);
        args.no_quota = true;
        let transport = FakeTransport::with([
            Err("request failed".into()),
            Err("request failed".into()),
            ok_reply(200, "{}"),
        ]);
        let snapshot = FixedSnapshot::ok("");
        let backoff = RecordingBackoff::default();
        let mut sink = Capture::default();

        let code = execute_with(
            &args,
            &paths,
            &config,
            &transport,
            &mut sink,
            &snapshot,
            &backoff,
            Retry::default(),
        )
        .unwrap();

        assert_eq!(code, 0);
        assert_eq!(transport.requests(), 3);
        assert_eq!(
            *backoff.waits.borrow(),
            [Duration::from_secs(3), Duration::from_secs(6)]
        );
        assert_eq!(sink.lines.len(), 1);
        assert!(sink.lines[0].contains("ping 1/1"));
        assert!(sink.lines[0].contains("status=ok"));
    }

    #[test]
    fn http_status_errors_are_answers_and_never_retried() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        auth_dir(&paths, &["codex-a.json"]);
        let config = fixture_config();
        let mut args = args();
        args.pings = Some(1);
        args.no_quota = true;
        let snapshot = FixedSnapshot::ok("");

        for status in [429, 500, 503] {
            let transport = FakeTransport::with([ok_reply(status, "secret")]);
            let backoff = RecordingBackoff::default();
            let mut sink = Capture::default();
            let code = execute_with(
                &args,
                &paths,
                &config,
                &transport,
                &mut sink,
                &snapshot,
                &backoff,
                Retry::default(),
            )
            .unwrap();
            assert_eq!(code, 1, "http {status} is a failed ping");
            assert_eq!(transport.requests(), 1, "http {status} must not retry");
            assert!(backoff.waits.borrow().is_empty());
            assert!(sink.lines[0].contains(&format!("http={status}")));
        }
    }

    #[test]
    fn exhausted_attempts_report_one_fixed_line() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        auth_dir(&paths, &["codex-a.json"]);
        let config = fixture_config();
        let mut args = args();
        args.pings = Some(1);
        args.no_quota = true;
        let transport = FakeTransport::with([
            Err("secret-cause".into()),
            Err("secret-cause".into()),
            Err("secret-cause".into()),
        ]);
        let snapshot = FixedSnapshot::ok("");
        let backoff = RecordingBackoff::default();
        let mut sink = Capture::default();

        let code = execute_with(
            &args,
            &paths,
            &config,
            &transport,
            &mut sink,
            &snapshot,
            &backoff,
            Retry::default(),
        )
        .unwrap();

        assert_eq!(code, 1);
        assert_eq!(transport.requests(), PING_ATTEMPTS);
        assert_eq!(sink.lines.len(), 1);
        assert!(sink.lines[0].contains("status=error reason=request-failed"));
        assert!(!sink.lines[0].contains("secret-cause"));
    }

    #[test]
    fn a_spent_budget_stops_retrying_and_shrinks_timeouts() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        auth_dir(&paths, &["codex-a.json"]);
        let mut config = fixture_config();
        config.proxy.timeout_secs = 30;
        let mut args = args();
        args.pings = Some(1);
        args.no_quota = true;
        let snapshot = FixedSnapshot::ok("");

        // No budget left: the first attempt still runs, the retry does not.
        let transport = FakeTransport::with([Err("request failed".into()), ok_reply(200, "{}")]);
        let backoff = RecordingBackoff::default();
        let mut sink = Capture::default();
        let code = execute_with(
            &args,
            &paths,
            &config,
            &transport,
            &mut sink,
            &snapshot,
            &backoff,
            Retry {
                budget: Duration::ZERO,
                ..Retry::default()
            },
        )
        .unwrap();
        assert_eq!(code, 1);
        assert_eq!(transport.requests(), 1);
        assert!(backoff.waits.borrow().is_empty());
        assert_eq!(transport.seen.borrow()[0].timeout_secs, 1);

        // A short budget caps every attempt, keeping the pass bounded.
        let transport = FakeTransport::default();
        let backoff = RecordingBackoff::default();
        let mut sink = Capture::default();
        execute_with(
            &args,
            &paths,
            &config,
            &transport,
            &mut sink,
            &snapshot,
            &backoff,
            Retry {
                budget: Duration::from_secs(7),
                ..Retry::default()
            },
        )
        .unwrap();
        assert_eq!(transport.seen.borrow()[0].timeout_secs, 7);
    }

    #[test]
    fn quota_snapshot_retries_once_then_recovers() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        auth_dir(&paths, &["codex-a.json"]);
        let config = fixture_config();
        let args = args();
        let transport = FakeTransport::default();
        let snapshot =
            FixedSnapshot::sequence([Err("transient".into()), Ok("● a@example.org — OK".into())]);
        let mut sink = Capture::default();

        assert_eq!(
            execute(&args, &paths, &config, &transport, &mut sink, &snapshot).unwrap(),
            0
        );
        assert_eq!(snapshot.calls.get(), 2);
        assert!(
            sink.lines
                .iter()
                .any(|line| line.contains("quota| ● a@example.org — OK"))
        );
        assert!(
            sink.lines
                .iter()
                .all(|line| !line.contains("quota snapshot unavailable"))
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
