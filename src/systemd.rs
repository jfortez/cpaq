//! Tokenmaxx systemd user units: rendering, apply, status and run-now.
//!
//! All systemd interaction goes through [`Runner`], so tests and the TUI can
//! inject a fake without ever touching the real user manager.

use std::path::Path;
use std::process::{Command, Output};

use crate::{
    config::{Config, atomic_write},
    paths::Paths,
};

const TIMER_UNIT: &str = "tokenmaxx.timer";
const SERVICE_UNIT: &str = "tokenmaxx.service";
const UNKNOWN: &str = "unknown";

/// Runs `systemctl --user` commands; injectable so tests never touch systemd.
///
/// The `Send` bound lets the TUI move a runner into a background refresh thread.
pub trait Runner: Send {
    /// Runs `systemctl --user <args>` and returns stdout on success.
    fn run(&self, args: &[&str]) -> Result<String, String>;
}

/// Real runner: spawns `systemctl --user` directly (no shell, no sudo) and
/// deliberately drops raw stderr from error messages.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, args: &[&str]) -> Result<String, String> {
        let output = systemctl_command(args)
            .output()
            .map_err(|_| "cannot run systemctl --user".to_string())?;
        interpret(args, output)
    }
}

/// Builds the direct `systemctl --user` invocation: no shell, no sudo.
fn systemctl_command(args: &[&str]) -> Command {
    let mut command = Command::new("systemctl");
    command.arg("--user").args(args);
    command
}

/// Maps a finished `systemctl` invocation to a user-facing result without
/// echoing raw stderr, which could contain account or credential material.
fn interpret(args: &[&str], output: Output) -> Result<String, String> {
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let code = output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string());
    Err(format!(
        "systemctl --user {} failed (exit {code})",
        args.join(" ")
    ))
}

/// Read-only timer/service state shown in the Tokenmaxx tab.
#[derive(Clone, Debug, Default)]
pub struct TimerStatus {
    pub enabled: String,
    pub active: String,
    pub next_run: String,
    pub last_run: String,
}

/// Writes the tokenmaxx user units and config under the injected [`Paths`],
/// then reloads systemd and enables or disables the timer.
///
/// Validation and unit rendering happen before any filesystem or systemd side
/// effect, so an invalid config or executable path leaves the user's setup
/// untouched.
pub fn apply(
    config: &Config,
    paths: &Paths,
    executable: &Path,
    runner: &impl Runner,
) -> Result<(), String> {
    config.validate()?;
    let exec = escape_exec_path(executable)?;
    let service = render_service(&exec);
    let timer = render_timer(config);

    config.save(&paths.config_file())?;
    let unit_dir = paths.unit_dir();
    atomic_write(&unit_dir.join(SERVICE_UNIT), service.as_bytes())
        .map_err(|_| "cannot write tokenmaxx.service".to_string())?;
    atomic_write(&unit_dir.join(TIMER_UNIT), timer.as_bytes())
        .map_err(|_| "cannot write tokenmaxx.timer".to_string())?;

    runner.run(&["daemon-reload"])?;
    let action = if config.tokenmaxx.enabled {
        "enable"
    } else {
        "disable"
    };
    runner.run(&[action, "--now", TIMER_UNIT])?;
    Ok(())
}

/// Runs the tokenmaxx service once, for the Tokenmaxx tab's `x` key.
pub fn run_now(runner: &impl Runner) -> Result<(), String> {
    runner.run(&["start", SERVICE_UNIT]).map(|_| ())
}

/// Queries the timer's enabled/active/next run and the service's last run.
pub fn status(runner: &impl Runner) -> Result<TimerStatus, String> {
    let timer = runner.run(&[
        "show",
        "--property=UnitFileState,ActiveState,NextElapseUSecRealtime",
        TIMER_UNIT,
    ])?;
    let service = runner.run(&["show", "--property=ExecMainStartTimestamp", SERVICE_UNIT])?;
    Ok(TimerStatus {
        enabled: property(&timer, "UnitFileState"),
        active: property(&timer, "ActiveState"),
        next_run: property(&timer, "NextElapseUSecRealtime"),
        last_run: property(&service, "ExecMainStartTimestamp"),
    })
}

/// Escapes an absolute executable path for an `ExecStart=` line.
///
/// systemd splits arguments on whitespace and expands `%` specifiers, so paths
/// with spaces, quotes or `%` need quoting/escaping. Control characters would
/// allow injecting extra unit directives, so they are rejected outright.
/// Dollar signs are rejected to avoid systemd environment expansion ambiguity.
fn escape_exec_path(path: &Path) -> Result<String, String> {
    if !path.is_absolute() {
        return Err("tokenmaxx executable path must be absolute".into());
    }
    let Some(text) = path.to_str() else {
        return Err("tokenmaxx executable path must be valid UTF-8".into());
    };
    if text.chars().any(char::is_control) {
        return Err("tokenmaxx executable path must not contain control characters".into());
    }
    if text.contains('$') {
        return Err("tokenmaxx executable path must not contain dollar signs".into());
    }

    let quoted = text
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\'));
    let mut escaped = String::with_capacity(text.len() + 2);
    if quoted {
        escaped.push('"');
    }
    for c in text.chars() {
        match c {
            '%' => escaped.push_str("%%"),
            '"' | '\\' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    if quoted {
        escaped.push('"');
    }
    Ok(escaped)
}

fn render_service(exec: &str) -> String {
    format!(
        "[Unit]\n\
         Description=tokenmaxx — CLIProxyAPI rate-limit keep-alive (cpaq)\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exec} ping\n"
    )
}

fn render_timer(config: &Config) -> String {
    let tokenmaxx = &config.tokenmaxx;
    format!(
        "[Unit]\n\
         Description=tokenmaxx keep-alive — anchor {anchor}, every {interval}h (cpaq)\n\
         \n\
         [Timer]\n\
         OnCalendar=*-*-* {anchor}:00\n\
         OnUnitInactiveSec={interval}h\n\
         AccuracySec=30s\n\
         Persistent=true\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        anchor = tokenmaxx.anchor,
        interval = tokenmaxx.interval_hours,
    )
}

/// Reads one `Key=Value` property from `systemctl show` output.
fn property(output: &str, key: &str) -> String {
    output
        .lines()
        .filter_map(|line| line.split_once('='))
        .find(|(name, _)| name.trim() == key)
        .map_or(UNKNOWN, |(_, value)| {
            let value = value.trim();
            if value.is_empty() { UNKNOWN } else { value }
        })
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::test_support::Sandbox;

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<Vec<String>>>,
        fail_on: Option<String>,
        timer_show: String,
        service_show: String,
    }

    impl FakeRunner {
        fn with_show(timer_show: &str, service_show: &str) -> Self {
            Self {
                timer_show: timer_show.into(),
                service_show: service_show.into(),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, args: &[&str]) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            if self
                .fail_on
                .as_deref()
                .is_some_and(|needle| args.join(" ").contains(needle))
            {
                return Err(format!("injected failure for {}", args.join(" ")));
            }
            if args.first() == Some(&"show") {
                return Ok(if args.contains(&TIMER_UNIT) {
                    self.timer_show.clone()
                } else {
                    self.service_show.clone()
                });
            }
            Ok(String::new())
        }
    }

    fn sandbox_paths(sandbox: &Sandbox) -> Paths {
        Paths::new(
            sandbox.0.clone(),
            Some(sandbox.0.join("config")),
            Some(sandbox.0.join("state")),
        )
    }

    fn executable(sandbox: &Sandbox) -> PathBuf {
        sandbox.0.join("bin/cpaq")
    }

    fn read_units(paths: &Paths) -> (String, String) {
        (
            fs::read_to_string(paths.unit_dir().join(SERVICE_UNIT)).unwrap(),
            fs::read_to_string(paths.unit_dir().join(TIMER_UNIT)).unwrap(),
        )
    }

    fn call_names(calls: &[Vec<String>]) -> Vec<String> {
        calls.iter().map(|call| call.join(" ")).collect()
    }

    #[test]
    fn apply_writes_exact_units_config_and_enables_timer() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        let runner = FakeRunner::default();

        apply(
            &Config::default(),
            &paths,
            Path::new("/usr/local/bin/cpaq"),
            &runner,
        )
        .unwrap();

        let (service, timer) = read_units(&paths);
        assert_eq!(
            service,
            "[Unit]\n\
             Description=tokenmaxx — CLIProxyAPI rate-limit keep-alive (cpaq)\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart=/usr/local/bin/cpaq ping\n"
        );
        assert_eq!(
            timer,
            "[Unit]\n\
             Description=tokenmaxx keep-alive — anchor 05:00, every 5h (cpaq)\n\
             \n\
             [Timer]\n\
             OnCalendar=*-*-* 05:00:00\n\
             OnUnitInactiveSec=5h\n\
             AccuracySec=30s\n\
             Persistent=true\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n"
        );
        assert_eq!(
            Config::load(&paths.config_file()).unwrap().tokenmaxx.anchor,
            "05:00"
        );
        assert_eq!(
            call_names(&runner.calls()),
            ["daemon-reload", "enable --now tokenmaxx.timer"]
        );
    }

    #[test]
    fn apply_disables_timer_after_writing_units() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        let runner = FakeRunner::default();
        let mut config = Config::default();
        config.tokenmaxx.enabled = false;

        apply(&config, &paths, &executable(&sandbox), &runner).unwrap();

        assert_eq!(
            call_names(&runner.calls()),
            ["daemon-reload", "disable --now tokenmaxx.timer"]
        );
        assert!(paths.unit_dir().join(SERVICE_UNIT).exists());
        assert!(paths.unit_dir().join(TIMER_UNIT).exists());
    }

    #[test]
    fn timer_uses_configured_anchor_and_interval() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        let runner = FakeRunner::default();
        let mut config = Config::default();
        config.tokenmaxx.anchor = "23:45".into();
        config.tokenmaxx.interval_hours = 3;

        apply(&config, &paths, &executable(&sandbox), &runner).unwrap();

        let (_, timer) = read_units(&paths);
        assert!(timer.contains("Description=tokenmaxx keep-alive — anchor 23:45, every 3h (cpaq)"));
        assert!(timer.contains("OnCalendar=*-*-* 23:45:00"));
        assert!(timer.contains("OnUnitInactiveSec=3h"));
    }

    #[test]
    fn apply_stops_before_enable_when_daemon_reload_fails() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        let runner = FakeRunner {
            fail_on: Some("daemon-reload".into()),
            ..FakeRunner::default()
        };

        assert!(apply(&Config::default(), &paths, &executable(&sandbox), &runner).is_err());
        assert_eq!(call_names(&runner.calls()), ["daemon-reload"]);
    }

    #[test]
    fn apply_propagates_enable_failure() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        let runner = FakeRunner {
            fail_on: Some("enable".into()),
            ..FakeRunner::default()
        };

        let error = apply(&Config::default(), &paths, &executable(&sandbox), &runner).unwrap_err();
        assert!(error.contains("enable"));
        assert_eq!(
            call_names(&runner.calls()),
            ["daemon-reload", "enable --now tokenmaxx.timer"]
        );
    }

    #[test]
    fn run_now_starts_the_service_and_reports_failures() {
        let runner = FakeRunner::default();
        run_now(&runner).unwrap();
        assert_eq!(call_names(&runner.calls()), ["start tokenmaxx.service"]);

        let failing = FakeRunner {
            fail_on: Some("start".into()),
            ..FakeRunner::default()
        };
        assert!(run_now(&failing).is_err());
    }

    #[test]
    fn status_reads_timer_and_service_properties() {
        let runner = FakeRunner::with_show(
            "UnitFileState=enabled\n\
             ActiveState=active\n\
             NextElapseUSecRealtime=Tue 2025-10-21 05:00:00 UTC\n",
            "ExecMainStartTimestamp=Mon 2025-10-20 05:00:00 UTC\n",
        );

        let status = status(&runner).unwrap();
        assert_eq!(status.enabled, "enabled");
        assert_eq!(status.active, "active");
        assert_eq!(status.next_run, "Tue 2025-10-21 05:00:00 UTC");
        assert_eq!(status.last_run, "Mon 2025-10-20 05:00:00 UTC");
        assert_eq!(
            runner.calls(),
            [
                vec![
                    "show".to_string(),
                    "--property=UnitFileState,ActiveState,NextElapseUSecRealtime".to_string(),
                    TIMER_UNIT.to_string()
                ],
                vec![
                    "show".to_string(),
                    "--property=ExecMainStartTimestamp".to_string(),
                    SERVICE_UNIT.to_string()
                ],
            ]
        );
    }

    #[test]
    fn status_marks_missing_properties_unknown() {
        let runner = FakeRunner::with_show("ActiveState=inactive\n", "");
        let status = status(&runner).unwrap();
        assert_eq!(status.enabled, UNKNOWN);
        assert_eq!(status.active, "inactive");
        assert_eq!(status.next_run, UNKNOWN);
        assert_eq!(status.last_run, UNKNOWN);
    }

    #[test]
    fn status_propagates_show_failure() {
        let mut runner = FakeRunner::with_show("ActiveState=active\n", "");
        runner.fail_on = Some(SERVICE_UNIT.into());
        assert!(status(&runner).is_err());
    }

    #[test]
    fn invalid_config_writes_nothing_and_calls_nothing() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        let runner = FakeRunner::default();
        let mut config = Config::default();
        config.tokenmaxx.anchor = "24:00".into();

        assert!(apply(&config, &paths, &executable(&sandbox), &runner).is_err());
        assert!(!paths.config_file().exists());
        assert!(!paths.unit_dir().exists());
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn invalid_executable_paths_are_rejected_before_side_effects() {
        for executable in [
            Path::new("cpaq"),
            Path::new("/opt/cpaq\nExecStart=/bin/sh"),
            Path::new("/opt/cpaq\rExecStart=/bin/sh"),
            Path::new("/opt/$HOME/cpaq"),
        ] {
            let sandbox = Sandbox::default();
            let paths = sandbox_paths(&sandbox);
            let runner = FakeRunner::default();

            assert!(
                apply(&Config::default(), &paths, executable, &runner).is_err(),
                "expected rejection for {executable:?}"
            );
            assert!(!paths.config_file().exists());
            assert!(!paths.unit_dir().exists());
            assert!(runner.calls().is_empty());
        }
    }

    #[test]
    fn exec_start_escapes_spaces_percent_and_quotes() {
        let sandbox = Sandbox::default();
        let paths = sandbox_paths(&sandbox);
        let runner = FakeRunner::default();

        apply(
            &Config::default(),
            &paths,
            Path::new("/opt/My Tools/100%/qa\"te/cpaq"),
            &runner,
        )
        .unwrap();

        let (service, _) = read_units(&paths);
        assert!(service.contains("ExecStart=\"/opt/My Tools/100%%/qa\\\"te/cpaq\" ping\n"));
    }

    #[test]
    fn system_runner_errors_hide_raw_stderr() {
        use std::os::unix::process::ExitStatusExt;

        let output = Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: b"partial".to_vec(),
            stderr: b"token=super-secret".to_vec(),
        };
        let error = interpret(&["daemon-reload"], output).unwrap_err();
        assert!(error.contains("daemon-reload"));
        assert!(!error.contains("super-secret"));
        assert!(!error.contains("partial"));
    }

    #[test]
    fn system_runner_calls_systemctl_user_directly() {
        let command = systemctl_command(&["daemon-reload"]);
        assert_eq!(command.get_program(), "systemctl");
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["--user", "daemon-reload"]
        );
    }

    #[test]
    fn runner_is_send_for_background_refresh() {
        fn assert_send<T: Send>() {}
        assert_send::<SystemRunner>();
        assert_send::<FakeRunner>();
    }
}
