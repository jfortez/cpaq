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
            .env_remove("INVOCATION_ID")
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
    assert_eq!(
        version.stdout,
        format!("cpaq {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
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
        let state_path = f.0.join("state/tokenmaxx/auto.json");
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let previous = br#"{"last_ping_epoch":10,"next_due_epoch":20}"#;
        fs::write(&state_path, previous).unwrap();
        let before = cpaq::time::now() as u64;
        let output = f.run(&["ping", "--pings", "2", "--no-quota"]);
        let after = cpaq::time::now() as u64;
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
        let state_bytes = fs::read(&state_path).unwrap();
        if statuses == [500, 500] {
            assert_eq!(state_bytes, previous);
        } else {
            let state: serde_json::Value = serde_json::from_slice(&state_bytes).unwrap();
            let last = state["last_ping_epoch"].as_u64().unwrap();
            assert!((before..=after).contains(&last));
            assert_eq!(state["next_due_epoch"], last + 18000);
            let idle = f.run(&["auto", "--json"]);
            assert_eq!(idle.status.code(), Some(0));
            assert!(idle.stderr.is_empty());
            let report: serde_json::Value = serde_json::from_slice(&idle.stdout).unwrap();
            assert_eq!(report["decision"], "not_due");
            assert_eq!(report["state"], state);
            assert_eq!(fs::read(&state_path).unwrap(), state_bytes);
            assert_eq!(
                fs::read(f.0.join("state/tokenmaxx/ping.log")).unwrap(),
                output.stdout
            );
        }
        let journal = fs::read_to_string(f.0.join("state/tokenmaxx/auto.log")).unwrap();
        let ping = journal
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|record| record["event"] == "ping")
            .expect("ping record");
        if statuses == [500, 500] {
            assert_eq!(ping["result"], "error");
            assert_eq!(ping["error"], "http 500");
        } else {
            assert_eq!(ping["result"], "ok");
            assert!(ping.get("error").is_none());
        }
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
        r#"{{"last_ping_epoch":{now},"last_probe_epoch":{now},"next_due_epoch":{}}}"#,
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
fn public_auto_probe_error_advances_only_probe_timestamp_and_dry_run_never_writes() {
    let f = Fixture::new();
    let path = f.0.join("state/tokenmaxx/auto.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let now = cpaq::time::now() as u64;
    let previous = format!(
        r#"{{"last_ping_epoch":10,"next_due_epoch":{},"window_reset_epoch":{}}}"#,
        now + 18000,
        now + 18000
    );
    fs::write(&path, &previous).unwrap();
    for dry_run in [true, false] {
        let args = if dry_run {
            vec!["auto", "--json", "--dry-run"]
        } else {
            vec!["auto", "--json"]
        };
        let out = f.run(&args);
        assert_eq!(out.status.code(), Some(0));
        assert!(out.stderr.is_empty());
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["decision"], "not_due");
        assert_eq!(report["probe_due"], true);
        assert_eq!(report["early_reset_detected"], false);
        assert_eq!(report["state"]["last_ping_epoch"], 10);
        assert_eq!(report["state"]["next_due_epoch"], now + 18000);
        assert!(report["state"]["last_probe_epoch"].as_u64().unwrap() >= now);
        if dry_run {
            assert_eq!(fs::read_to_string(&path).unwrap(), previous);
        } else {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap(),
                report["state"]
            );
        }
        assert!(!f.0.join("state/tokenmaxx/ping.log").exists());
        assert!(!f.0.join("config/systemd").exists());
    }
}

#[test]
fn auto_disabled_reasons_exit_zero_without_writing_state() {
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("config/cpaq")).unwrap();
    for (config, reason, decision) in [
        ("enabled=false", "auto: tokenmaxx is disabled\n", "disabled"),
        (
            "auto=false",
            "auto: auto mode is disabled\n",
            "auto_disabled",
        ),
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
        // The schedule stays untouched, while the run itself is still journaled.
        assert!(!f.0.join("state/tokenmaxx/auto.json").exists());
        let journal = fs::read_to_string(f.0.join("state/tokenmaxx/auto.log")).unwrap();
        let tick: serde_json::Value =
            serde_json::from_str(journal.lines().last().unwrap()).unwrap();
        assert_eq!(tick["event"], "tick");
        assert_eq!(tick["decision"], decision);
        assert_eq!(tick["exit"], 0);
        assert_eq!(tick["probe_due"], false);
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
    // The exact-second wait and its ping must fit the start timeout.
    assert!(service.contains("TimeoutStartSec=600\n"));
    let timer = fs::read_to_string(f.0.join("config/systemd/user/tokenmaxx.timer")).unwrap();
    for directive in [
        "OnBootSec=90s",
        "OnCalendar=*:0/5",
        "Persistent=true",
        "AccuracySec=1s",
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
    // Exactly one ping record (origin auto) and one tick record, JSONL, 0600.
    use std::os::unix::fs::PermissionsExt;
    let journal_path = f.0.join("state/tokenmaxx/auto.log");
    assert_eq!(
        fs::metadata(&journal_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let journal = fs::read_to_string(&journal_path).unwrap();
    let records: Vec<serde_json::Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2, "{journal}");
    let ping = records
        .iter()
        .find(|record| record["event"] == "ping")
        .unwrap();
    assert_eq!(ping["origin"], "auto");
    assert_eq!(ping["trigger"], "manual");
    assert_eq!(ping["result"], "ok");
    assert_eq!(ping["exit"], 0);
    assert!(
        ping["model"]
            .as_str()
            .is_some_and(|model| !model.is_empty())
    );
    for field in [
        "last_ping_epoch",
        "next_due_epoch",
        "window_reset_epoch",
        "long_window_reset_epoch",
    ] {
        assert!(ping[field].is_u64(), "ping lost {field}");
    }
    let tick = records
        .iter()
        .find(|record| record["event"] == "tick")
        .unwrap();
    assert_eq!(tick["decision"], "due");
    assert_eq!(tick["trigger"], "manual");
    assert_eq!(tick["exit"], 0);
    assert!(tick["due_in_s"].is_i64());
    assert!(tick.get("wait_s").is_none());
    assert!(tick["ts_epoch"].is_u64());
    assert!(cpaq::time::parse_iso(tick["ts"].as_str().unwrap()).is_some());
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

#[test]
fn public_auto_caps_multi_account_pass_and_never_retries_failed_request() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    for transport_error in [false, true] {
        let f = Fixture::new();
        for file in ["other-a.json", "other-b.json"] {
            fs::write(f.0.join("auths").join(file), r#"{"type":"other"}"#).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
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
                assert!(request.len() < 16384);
            }
            if !transport_error {
                write!(
                    stream,
                    "HTTP/1.1 503 Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
            drop(stream);
            listener
        });
        fs::create_dir_all(f.0.join("config/cpaq")).unwrap();
        fs::write(f.0.join("config/cpaq/config.toml"), format!(
            "[auth]\ndir='{}'\n[proxy]\nbase_url='http://{address}'\napi_key='fixture'\ntimeout_secs=1\n[tokenmaxx]\npings_per_account=3\n", f.auth_path()
        )).unwrap();
        let out = f.run(&["auto", "--json"]);
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stderr.is_empty());
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["decision"], "due");
        assert_eq!(report["state"], serde_json::Value::Null);
        assert!(!f.0.join("state/tokenmaxx/auto.json").exists());
        let log = fs::read_to_string(f.0.join("state/tokenmaxx/ping.log")).unwrap();
        assert_eq!(
            log.lines().filter(|line| line.contains(" ping ")).count(),
            1
        );
        assert!(log.contains("ping 1/1"));
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }
}

fn idle_state(f: &Fixture, seconds_ahead: i64) -> i64 {
    fs::create_dir_all(f.0.join("config/cpaq")).unwrap();
    fs::write(f.0.join("config/cpaq/config.toml"), "[tokenmaxx]\n").unwrap();
    let state_path = f.0.join("state/tokenmaxx/auto.json");
    fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    let now = cpaq::time::now();
    let next_due = now + seconds_ahead;
    fs::write(
        &state_path,
        format!(
            r#"{{"last_ping_epoch":0,"next_due_epoch":{next_due},"last_probe_epoch":{now},"window_reset_epoch":{next_due}}}"#
        ),
    )
    .unwrap();
    next_due
}

fn journal_records(f: &Fixture) -> Vec<serde_json::Value> {
    fs::read_to_string(f.0.join("state/tokenmaxx/auto.log"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn public_auto_idle_tick_has_no_wait_and_records_seconds() {
    // Beyond the 300s precision wait: the run must decide and return at once.
    let f = Fixture::new();
    let next_due = idle_state(&f, 3600 + 24);
    let started = std::time::Instant::now();
    let out = f.run(&["auto"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let text = String::from_utf8(out.stdout).unwrap();
    let line = text.lines().next().unwrap();
    let records = journal_records(&f);
    assert_eq!(records.len(), 1, "{text}");
    let tick = &records[0];
    assert_eq!(tick["event"], "tick");
    assert_eq!(tick["decision"], "not_due");
    assert_eq!(tick["probe_due"], false);
    assert_eq!(tick["exit"], 0);
    assert!(tick.get("wait_s").is_none());
    let remaining = (next_due - tick["ts_epoch"].as_i64().unwrap()) as u64;
    assert_eq!(tick["due_in_s"].as_i64().unwrap(), remaining as i64);
    let expected = format!(
        "(in {}h {}m {}s; not due)",
        remaining / 3600,
        remaining % 3600 / 60,
        remaining % 60
    );
    assert!(line.contains(&expected), "{line} lacks {expected}");
    assert!(line.starts_with("auto: next due "));
}

#[test]
fn public_ping_journals_origin_ping() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let (mut request, mut chunk) = (Vec::new(), [0; 4096]);
        loop {
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
            if let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..split]).to_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if request.len() >= split + 4 + length {
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
    });
    fs::create_dir_all(f.0.join("config/cpaq")).unwrap();
    fs::write(
        f.0.join("config/cpaq/config.toml"),
        format!("[proxy]\nbase_url='http://{address}'\napi_key='fixture'\ntimeout_secs=1\n"),
    )
    .unwrap();
    let out = f.run(&["ping", "--pings", "1", "--no-quota"]);
    server.join().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let records = journal_records(&f);
    assert_eq!(records.len(), 1, "{records:?}");
    let ping = &records[0];
    assert_eq!(ping["event"], "ping");
    assert_eq!(ping["origin"], "ping");
    assert_eq!(ping["trigger"], "manual");
    assert_eq!(ping["result"], "ok");
    assert_eq!(ping["exit"], 0);
    assert!(ping["next_due_epoch"].is_u64());
    assert!(records.iter().all(|record| record["event"] != "tick"));
}

#[test]
fn public_auto_journal_failure_is_a_single_warning() {
    let f = Fixture::new();
    idle_state(&f, 3600);
    // An unusable journal path must not change the decision or the exit code.
    fs::create_dir_all(f.0.join("state/tokenmaxx/auto.log")).unwrap();
    let out = f.run(&["auto"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("; not due)"));
    assert_eq!(
        out.stderr,
        b"warning: cannot write the auto journal\n".to_vec()
    );
}

#[test]
fn public_auto_error_still_journals_one_tick() {
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("config/cpaq")).unwrap();
    // No proxy credentials and no schedule: the run is due and fails at the ping.
    fs::write(f.0.join("config/cpaq/config.toml"), "[tokenmaxx]\n").unwrap();
    let out = f.run(&["auto"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.to_lowercase().contains("no api key"), "{stderr}");
    let records = journal_records(&f);
    let tick = records
        .iter()
        .find(|record| record["event"] == "tick")
        .unwrap_or_else(|| panic!("no tick recorded: {records:?}"));
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(tick["decision"], "error");
    assert_eq!(tick["exit"], 1);
    assert_eq!(tick["trigger"], "manual");
    assert!(
        tick["error"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("no api key")
    );
    // No schedule was readable, so there is no due distance to report.
    assert!(tick.get("due_in_s").is_none());
    let ping = records
        .iter()
        .find(|record| record["event"] == "ping")
        .unwrap();
    assert_eq!(ping["origin"], "auto");
    assert_eq!(ping["result"], "error");
    assert_eq!(ping["exit"], 1);
}

#[test]
fn public_auto_dry_run_error_writes_no_journal() {
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("config/cpaq")).unwrap();
    fs::write(f.0.join("config/cpaq/config.toml"), "[tokenmaxx]\n").unwrap();
    // A directory where the state file should be makes the read fail.
    fs::create_dir_all(f.0.join("state/tokenmaxx/auto.json")).unwrap();
    let out = f.run(&["auto", "--dry-run"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Cannot read auto state"));
    assert!(!f.0.join("state/tokenmaxx/auto.log").exists());
}

#[test]
fn public_ping_error_still_journals_one_record() {
    let f = Fixture::new();
    // No config and no auth dir: the pass cannot resolve proxy credentials.
    let out = f.run(&["ping", "--pings", "1", "--no-quota"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.to_lowercase().contains("no api key"), "{stderr}");
    let records = journal_records(&f);
    assert_eq!(records.len(), 1, "{records:?}");
    let ping = &records[0];
    assert_eq!(ping["event"], "ping");
    assert_eq!(ping["origin"], "ping");
    assert_eq!(ping["trigger"], "manual");
    assert_eq!(ping["result"], "error");
    assert_eq!(ping["exit"], 1);
    assert!(
        ping["error"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("no api key")
    );
    assert!(
        ping["model"]
            .as_str()
            .is_some_and(|model| !model.is_empty())
    );
}

const LOG_TICK: &str = r#"{"decision":"not_due","due_in_s":17964,"event":"tick","exit":0,"ts":"2026-11-05T18:15:46-05:00","ts_epoch":1793920546}"#;
const LOG_PING_OK: &str = r#"{"event":"ping","exit":0,"model":"test-model","next_due_epoch":1793936426,"origin":"ping","result":"ok","ts":"2026-11-05T18:16:00-05:00","ts_epoch":1793920560}"#;
const LOG_RESET: &str = r#"{"account":"acct-1","event":"reset_change","from":2,"kind":"banked_reset_used","to":1,"ts":"2026-11-05T18:17:00-05:00","ts_epoch":1793920620}"#;
const LOG_PING_ERROR: &str = r#"{"error":"No API key","event":"ping","exit":1,"model":"test-model","next_due_epoch":0,"origin":"auto","result":"error","ts":"2026-11-05T18:18:00-05:00","ts_epoch":1793920680}"#;

fn journal_fixture(f: &Fixture, lines: &[&str]) {
    let path = f.0.join("state/tokenmaxx/auto.log");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
}

#[test]
fn public_log_shows_pings_by_default_and_widens_with_all() {
    let f = Fixture::new();
    journal_fixture(&f, &[LOG_TICK, LOG_PING_OK, LOG_RESET, LOG_PING_ERROR]);

    let out = f.run(&["log"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(
        lines[0].starts_with("2026-11-05T18:16:00-05:00  ping  ping  ok  model=test-model  next="),
        "{}",
        lines[0]
    );
    assert!(!lines[0].contains("error="));
    assert!(
        lines[1]
            .starts_with("2026-11-05T18:18:00-05:00  ping  auto  error  model=test-model  next=-"),
        "{}",
        lines[1]
    );
    assert!(lines[1].ends_with(r#"  exit=1  error="No API key""#));

    // --json passes the journal lines through untouched.
    let out = f.run(&["log", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{LOG_PING_OK}\n{LOG_PING_ERROR}\n")
    );

    // --all widens the selector and keeps the journal order.
    let out = f.run(&["log", "--all"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert_eq!(
        lines[0],
        "2026-11-05T18:15:46-05:00  tick  not_due  due_in=4h 59m 24s  wait=-  exit=0"
    );
    assert_eq!(
        lines[2],
        "2026-11-05T18:17:00-05:00  reset_change  banked_reset_used  acct-1  2 -> 1"
    );

    // --limit keeps the newest N after filtering; 0 means everything.
    let out = f.run(&["log", "--all", "--limit", "2"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.lines().next().unwrap().contains("  reset_change  "));
    let out = f.run(&["log", "--limit", "0"]);
    assert_eq!(out.status.code(), Some(0));
    let unlimited = String::from_utf8(out.stdout).unwrap();
    assert_eq!(unlimited.lines().count(), 2, "{unlimited}");
    let out = f.run(&["log", "--all", "--limit", "0", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{LOG_TICK}\n{LOG_PING_OK}\n{LOG_RESET}\n{LOG_PING_ERROR}\n")
    );
}

#[test]
fn public_log_without_pings_says_so_and_keeps_stdout_clean() {
    let f = Fixture::new();
    journal_fixture(&f, &[LOG_TICK, LOG_TICK]);
    let path = f.0.join("state/tokenmaxx/auto.log");
    let notice = format!(
        "log: no ping records yet in {} (2 tick(s) in the journal; use --all)\n",
        path.display()
    );
    for args in [vec!["log"], vec!["log", "--json"]] {
        let out = f.run(&args);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert_eq!(String::from_utf8(out.stderr).unwrap(), notice, "{args:?}");
    }
    // --all still shows the ticks and stays quiet.
    let out = f.run(&["log", "--all"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.lines().all(|line| line.contains("  tick  ")));
}

const REPORT_TICK: &str = r#"{"decision":"due","due_in_s":-3,"event":"tick","exit":0,"probe_due":true,"ts":"2026-11-05T20:00:00-05:00","ts_epoch":1793930000,"wait_s":26}"#;
const REPORT_TICK_IDLE: &str = r#"{"decision":"not_due","due_in_s":1000,"event":"tick","exit":0,"probe_due":false,"ts":"2026-11-05T20:10:00-05:00","ts_epoch":1793930600}"#;
const REPORT_PING: &str = r#"{"event":"ping","exit":0,"finished_epoch":1793930028,"model":"gpt-6-luna","next_due_epoch":1793948000,"origin":"ping","result":"ok","ts":"2026-11-05T20:00:00-05:00","ts_epoch":1793930000}"#;
const REPORT_RESET: &str = r#"{"account":"acct-1","event":"reset_change","from":2,"kind":"banked_reset_used","to":1,"ts":"2026-11-05T20:12:00-05:00","ts_epoch":1793930720}"#;

#[test]
fn public_report_renders_summary_details_and_next_due() {
    let f = Fixture::new();
    journal_fixture(
        &f,
        &[REPORT_TICK, REPORT_PING, REPORT_TICK_IDLE, REPORT_RESET],
    );
    let soon = cpaq::time::now() as u64 + 3600;
    fs::write(
        f.0.join("state/tokenmaxx/auto.json"),
        format!(
            r#"{{"last_ping_epoch":1793930000,"next_due_epoch":{soon},"last_probe_epoch":1793930000,"window_reset_epoch":{soon}}}"#
        ),
    )
    .unwrap();

    let out = f.run(&["report", "--days", "0"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with("cpaq tokenmaxx report - all records, through 20"),
        "{text}"
    );
    assert!(
        text.contains("Date        Ticks  Probes  Pings  OK  Err  Timer  Manual"),
        "{text}"
    );
    // Two ticks (one probe), one ok manual ping and one reset change.
    let row = text
        .lines()
        .find(|line| line.contains("  2      1       1      1   0    0      1       "))
        .unwrap_or_else(|| panic!("no summary row: {text}"));
    assert!(
        row.starts_with(&cpaq::time::local(1793930000, "%Y-%m-%d")),
        "{row}"
    );
    assert!(row.ends_with("0:26      0:26      1"), "{row}");
    assert!(
        text.contains(&format!("Next due: {} ", cpaq::time::local_datetime(soon))),
        "{text}"
    );
    assert!(text.contains("(in "), "{text}");
    // Details: the manual ping picks up the wait of the tick that pinged.
    assert!(text.contains("Pings"), "{text}");
    let ping = text
        .lines()
        .find(|line| line.contains("  manual  ok      gpt-6-luna"))
        .unwrap_or_else(|| panic!("no ping row: {text}"));
    assert!(ping.contains("0:26"), "{ping}");
    assert!(
        ping.starts_with(&format!(
            "  {}  manual  ok",
            cpaq::time::local_datetime(1793930000)
        )),
        "{ping}"
    );
    assert!(
        ping.contains(&cpaq::time::local_datetime(1793948000)),
        "{ping}"
    );
    // Detail columns are padded, so assert the cells rather than the gaps.
    let reset = text
        .lines()
        .find(|line| line.contains("banked_reset_used"))
        .unwrap_or_else(|| panic!("no reset row: {text}"));
    assert!(reset.starts_with("  "), "{reset}");
    assert!(reset.contains("acct-1"), "{reset}");
    assert!(reset.ends_with("2 -> 1"), "{reset}");
    assert!(
        text.lines().all(|line| line == line.trim_end()),
        "trailing space"
    );
    // --days 1 drops records dated before today.
    let recent = f.run(&["report", "--days", "1"]);
    assert_eq!(recent.status.code(), Some(0));
    let recent = String::from_utf8(recent.stdout).unwrap();
    assert!(recent.contains("last 1 day(s)"), "{recent}");
}

#[test]
fn public_report_missing_journal_is_a_friendly_exit_zero() {
    let f = Fixture::new();
    let out = f.run(&["report"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    let path = f.0.join("state/tokenmaxx/auto.log");
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        format!("report: no journal yet at {}\n", path.display())
    );
    // Without a state file the next-due line is omitted but the report renders.
    let f = Fixture::new();
    journal_fixture(&f, &[REPORT_TICK_IDLE]);
    let out = f.run(&["report", "--days", "0"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("Next due:"), "{text}");
    assert!(text.contains("  (no pings yet)"), "{text}");
    assert!(text.contains("  (none)"), "{text}");
}

#[test]
fn public_log_missing_journal_is_a_friendly_exit_zero() {
    let f = Fixture::new();
    let out = f.run(&["log"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8(out.stderr).unwrap();
    let path = f.0.join("state/tokenmaxx/auto.log");
    assert_eq!(
        stderr,
        format!("log: no journal yet at {}\n", path.display())
    );
    // A follow request on a missing journal is not an error either.
    let out = f.run(&["log", "--all", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
}

#[test]
fn public_log_rejects_invalid_options() {
    let f = Fixture::new();
    for args in [
        vec!["log", "--limit", "many"],
        vec!["log", "--limit"],
        vec!["log", "--dry-run"],
        vec!["auto", "--follow"],
    ] {
        let out = f.run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
    }
}
