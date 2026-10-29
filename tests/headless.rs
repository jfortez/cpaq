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

#[test]
fn auto_idle_and_dry_run_short_circuit_without_credentials_or_auth() {
    let f = Fixture::new();
    let state = f.0.join("state/tokenmaxx/auto.json");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    let now = cpaq::time::now() as u64;
    let previous = format!(
        r#"{{"last_ping_epoch":{now},"next_due_epoch":{}}}"#,
        now + 18000
    );
    fs::write(&state, &previous).unwrap();
    for args in [
        vec!["auto"],
        vec!["auto", "--dry-run"],
        vec!["auto", "--json"],
    ] {
        let out = f.run(&args);
        assert_eq!(out.status.code(), Some(0));
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if args.contains(&"--json") {
            let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(value["decision"], "not_due");
            assert_eq!(value["state"]["next_due_epoch"], now + 18000);
        } else {
            assert!(
                String::from_utf8(out.stdout)
                    .unwrap()
                    .starts_with("auto: next due ")
            );
        }
        assert_eq!(fs::read_to_string(&state).unwrap(), previous);
        assert!(!f.0.join("state/tokenmaxx/ping.log").exists());
    }
    fs::remove_file(&state).unwrap();
    let out = f.run(&["auto", "--dry-run", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["decision"], "due");
    assert_eq!(value["dry_run"], true);
    assert_eq!(
        value["state"]["next_due_epoch"].as_u64().unwrap()
            - value["state"]["last_ping_epoch"].as_u64().unwrap(),
        18000
    );
    assert!(!state.exists());
    assert!(!f.0.join("state/tokenmaxx/ping.log").exists());
}

#[test]
fn auto_disabled_reasons_exit_zero_without_writing_state() {
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("config/cpaq")).unwrap();
    for (config, reason) in [
        ("enabled=false", "auto: tokenmaxx is disabled\n"),
        ("auto=false", "auto: auto mode is disabled\n"),
    ] {
        fs::write(
            f.0.join("config/cpaq/config.toml"),
            format!("[tokenmaxx]\n{config}\n"),
        )
        .unwrap();
        let out = f.run(&["auto"]);
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(out.stdout, reason.as_bytes());
        assert!(out.stderr.is_empty());
        assert!(!f.0.join("state").exists());
    }
}

#[test]
fn public_units_apply_uses_fake_systemctl_and_writes_auto_units() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let bin = f.0.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let script = bin.join("systemctl");
    fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CPAQ_TEST_CALLS\"\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let calls = f.0.join("calls");
    let out = Command::new(env!("CARGO_BIN_EXE_cpaq"))
        .args(["units", "apply"])
        .env("HOME", f.0.join("home"))
        .env("XDG_CONFIG_HOME", f.0.join("config"))
        .env("XDG_STATE_HOME", f.0.join("state"))
        .env("PATH", &bin)
        .env("CPAQ_TEST_CALLS", &calls)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"tokenmaxx: units applied\n");
    assert!(out.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(&calls).unwrap(),
        "--user daemon-reload\n--user enable --now tokenmaxx.timer\n"
    );
    let service = fs::read_to_string(f.0.join("config/systemd/user/tokenmaxx.service")).unwrap();
    assert!(service.contains(&format!("ExecStart={} auto\n", env!("CARGO_BIN_EXE_cpaq"))));
    assert!(service.contains("ExecStopPost=/usr/bin/systemctl --user try-restart tokenmaxx.timer"));
    let timer = fs::read_to_string(f.0.join("config/systemd/user/tokenmaxx.timer")).unwrap();
    for directive in [
        "OnBootSec=90s",
        "OnCalendar=*:0/5",
        "Persistent=true",
        "AccuracySec=30s",
    ] {
        assert!(timer.contains(directive));
    }
    assert!(!timer.contains("OnUnitInactiveSec="));
    assert!(
        cpaq::config::Config::load(&f.0.join("config/cpaq/config.toml"))
            .unwrap()
            .tokenmaxx
            .auto
    );
    assert!(!f.0.join("state").exists());
}

#[test]
fn public_auto_due_json_pings_once_logs_and_then_stays_idle() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = Vec::new();
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    loop {
                        let mut chunk = [0; 4096];
                        let count = stream.read(&mut chunk).unwrap();
                        assert!(count > 0);
                        received.extend_from_slice(&chunk[..count]);
                        assert!(received.len() < 16384);
                        if let Some(split) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&received[..split]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap();
                            if received.len() >= split + 4 + length {
                                break;
                            }
                        }
                    }
                    let body = r#"{"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    return (listener, received);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("fake proxy accept: {error}"),
            }
        }
        panic!("auto never contacted fake proxy");
    });
    fs::create_dir_all(f.0.join("config/cpaq")).unwrap();
    fs::write(f.0.join("config/cpaq/config.toml"), format!("[proxy]\nbase_url='http://{address}'\napi_key='fixture'\ntimeout_secs=1\n[tokenmaxx]\ninterval_hours=3\n")).unwrap();
    let before = cpaq::time::now() as u64;
    let out = f.run(&["auto", "--json"]);
    let after = cpaq::time::now() as u64;
    let (listener, request) = server.join().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["decision"], "due");
    let state_path = f.0.join("state/tokenmaxx/auto.json");
    let state_bytes = fs::read(&state_path).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&state_bytes).unwrap();
    assert_eq!(report["state"], state);
    let last = state["last_ping_epoch"].as_u64().unwrap();
    assert!((before..=after).contains(&last));
    assert_eq!(state["next_due_epoch"].as_u64().unwrap(), last + 10800);
    let request = String::from_utf8(request).unwrap();
    assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
    let body: serde_json::Value =
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["max_tokens"], 16);
    let log = fs::read(f.0.join("state/tokenmaxx/ping.log")).unwrap();
    assert!(String::from_utf8_lossy(&log).contains("ping 1/1"));
    assert!(String::from_utf8_lossy(&log).contains("quota snapshot unavailable"));
    for args in [vec!["auto"], vec!["auto", "--dry-run"]] {
        let idle = f.run(&args);
        assert_eq!(idle.status.code(), Some(0));
        assert!(idle.stderr.is_empty());
        let text = String::from_utf8(idle.stdout).unwrap();
        assert!(text.starts_with("auto: next due "));
        let stamp = text.split_whitespace().nth(3).unwrap();
        assert_eq!(cpaq::time::parse_iso(stamp), Some((last + 10800) as i64));
        assert_eq!(fs::read(&state_path).unwrap(), state_bytes);
        assert_eq!(fs::read(f.0.join("state/tokenmaxx/ping.log")).unwrap(), log);
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}
