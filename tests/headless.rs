use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/cli-test-state")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(path.join("home")).unwrap();
        fs::create_dir_all(path.join("config")).unwrap();
        fs::create_dir_all(path.join("auths")).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cpaq"))
            .args(args)
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env_remove("CPA_AUTH_DIR")
            .env_remove("CPA_DOCKER_CONTAINER")
            .env_remove("CPA_CONTAINER_AUTH_DIR")
            .output()
            .unwrap()
    }
    fn auth_path(&self) -> String {
        self.0.join("auths").display().to_string()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn public_help_and_version_are_successful_without_config() {
    let f = Fixture::new();
    let help = f.run(&["--help"]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    let text = String::from_utf8(help.stdout).unwrap();
    for option in [
        "--auth-dir",
        "--json",
        "--raw",
        "--provider",
        "--no-quota",
        "--pings",
        "--log",
        "--model",
    ] {
        assert!(text.contains(option));
    }
    let version = f.run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(version.stdout, b"cpaq 0.1.0\n");
    assert!(version.stderr.is_empty());
}
#[test]
fn missing_or_empty_auth_directory_exits_two_with_actionable_error() {
    let f = Fixture::new();
    let absent = f.run(&["quota", "--json"]);
    assert_eq!(absent.status.code(), Some(2));
    assert!(absent.stdout.is_empty());
    let error = String::from_utf8(absent.stderr).unwrap();
    assert!(error.contains("--auth-dir"));
    assert!(error.contains("cli-proxy-api/auths"));
    assert!(error.contains("cpa/auths"));
    let empty = f.run(&["quota", "--auth-dir", &f.auth_path()]);
    assert_eq!(empty.status.code(), Some(2));
    assert!(empty.stdout.is_empty());
    assert!(!empty.stderr.is_empty());
}
#[test]
fn unsupported_accounts_are_listed_without_aborting_or_exposing_tokens() {
    let f = Fixture::new();
    fs::write(
        f.0.join("auths/other-a.json"),
        r#"{"type":"other","email":"test@example.org","access_token":"private-fixture-value"}"#,
    )
    .unwrap();
    let json = f.run(&["quota", "--json", "--raw", "--auth-dir", &f.auth_path()]);
    assert_eq!(json.status.code(), Some(0));
    assert!(json.stderr.is_empty());
    let text = String::from_utf8(json.stdout).unwrap();
    assert!(!text.contains("private-fixture-value"));
    let data: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(data[0]["file"], "other-a.json");
    assert_eq!(data[0]["provider"], "other");
    assert!(data[0]["error"].as_str().is_some());
    let human = f.run(&["quota", "--auth-dir", &f.auth_path()]);
    assert!(human.status.success());
    assert!(human.stderr.is_empty());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("── OTHER ──"));
    assert!(text.contains("CLIProxyAPI — provider quotas"));
}
#[test]
fn malformed_account_isolated_from_unsupported_account_with_exit_one() {
    let f = Fixture::new();
    fs::write(
        f.0.join("auths/codex-bad.json"),
        r#"{"access_token":"private-fixture-value" broken}"#,
    )
    .unwrap();
    fs::write(f.0.join("auths/other-good.json"), r#"{"type":"other"}"#).unwrap();
    let out = f.run(&["quota", "--json", "--auth-dir", &f.auth_path()]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stderr, b"(1 query error(s))\n");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("private-fixture-value"));
    let data: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(data.as_array().unwrap().len(), 2);
}
#[test]
fn invalid_options_and_config_never_write_state() {
    let f = Fixture::new();
    let out = f.run(&["ping", "--pings", "0"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(out.stderr, b"--pings must be a positive integer\n");
    fs::create_dir_all(f.0.join("config/cpaq")).unwrap();
    fs::write(
        f.0.join("config/cpaq/config.toml"),
        "[tokenmaxx]\nanchor='25:00'\n",
    )
    .unwrap();
    let out = f.run(&["quota"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        out.stderr,
        "anchor must be HH:MM (00:00–23:59)\n".as_bytes()
    );
    assert!(!f.0.join("state").exists());
    assert!(!f.0.join("config/systemd").exists());
}

#[test]
fn public_ping_posts_to_fake_proxy_and_tees_safe_logs() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };
    for statuses in [[500, 200], [500, 500]] {
        let f = Fixture::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = Vec::new();
            while requests.len() < statuses.len() && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&chunk[..count]);
                    assert!(request.len() < 16384);
                    if let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..split]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if request.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                let body = r#"{"usage":{"prompt_tokens":1,"completion_tokens":2},"untrusted":"private-fixture-value"}"#;
                let status = statuses[requests.len()];
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                requests.push(String::from_utf8(request).unwrap());
            }
            requests
        });
        fs::create_dir_all(f.0.join("config/cpaq")).unwrap();
        fs::write(f.0.join("config/cpaq/config.toml"), format!(
            "[proxy]\nbase_url='http://{address}'\napi_key='private-fixture-value'\ntimeout_secs=1\n[tokenmaxx]\nmodel='test-model'\n"
        )).unwrap();
        let output = f.run(&["ping", "--pings", "2", "--no-quota"]);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            output.status.code(),
            Some(i32::from(statuses == [500, 500]))
        );
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout.clone()).unwrap();
        assert!(!text.contains("private-fixture-value"));
        assert!(!text.contains("quota"));
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("ping 1/2"));
        assert!(text.contains("ping 2/2"));
        assert_eq!(
            fs::read(f.0.join("state/tokenmaxx/ping.log")).unwrap(),
            output.stdout
        );
        for request in requests {
            assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
            let (headers, body) = request.split_once("\r\n\r\n").unwrap();
            assert!(
                headers
                    .to_lowercase()
                    .contains("authorization: bearer private-fixture-value")
            );
            let value: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(value["model"], "test-model");
            assert_eq!(value["max_tokens"], 16);
            assert_eq!(
                value["messages"],
                serde_json::json!([{"role":"user","content":"hi"}])
            );
        }
    }
}
