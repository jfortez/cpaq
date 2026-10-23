//! Terminal lifecycle, background workers and the event loop.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Stdout},
    path::{Path, PathBuf},
    sync::{
        Once,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use crossterm::{
    cursor::Show,
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    cli::Args,
    config::Config,
    paths::Paths,
    quota,
    systemd::{self, SystemRunner},
    time,
};

pub mod app;
pub mod theme;
pub mod ui;

pub use app::{App, Command, Overlay, Setting, Tab, WorkerMessage};

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Runs the interactive TUI.
///
/// The first frame is drawn before auth discovery, provider requests, timer
/// status or log reads are started; all of those jobs run on worker threads.
pub fn run(paths: Paths, config: Config, args: Args) -> Result<(), String> {
    install_panic_hook();
    let mut terminal = init_terminal()?;
    let _restore = RestoreGuard;
    let mut app = App::new(paths, config, args);

    terminal
        .draw(|frame| ui::render(frame, &app))
        .map_err(|_| "Cannot draw the first TUI frame".to_string())?;

    let (quota_jobs, quota_requests) = mpsc::channel();
    let (quota_messages, quota_results) = mpsc::channel();
    let (system_jobs, system_requests) = mpsc::channel();
    let (system_messages, system_results) = mpsc::channel();

    let _quota_worker = spawn_quota_worker(quota_requests, quota_messages)?;
    let _system_worker = spawn_system_worker(system_requests, system_messages)?;

    for command in app.startup_commands() {
        dispatch(command, &app, &quota_jobs, &system_jobs);
    }

    event_loop(
        &mut terminal,
        &mut app,
        &quota_jobs,
        &system_jobs,
        &quota_results,
        &system_results,
    )
}

/// Reads only the last `max_bytes` of a log and returns at most `max_lines`
/// sanitized lines. Never reads a whole log file just to show its tail.
pub(crate) fn read_log_tail(
    path: &Path,
    max_bytes: u64,
    max_lines: usize,
) -> Result<Vec<String>, String> {
    if max_lines == 0 {
        return Ok(Vec::new());
    }
    let mut file = File::open(path).map_err(|_| {
        format!(
            "Cannot read log tail: {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        )
    })?;
    let length = file
        .metadata()
        .map_err(|_| "Cannot inspect log file".to_string())?
        .len();
    let start = length.saturating_sub(max_bytes.max(1));
    file.seek(SeekFrom::Start(start))
        .map_err(|_| "Cannot seek log file".to_string())?;
    let mut buffer = Vec::new();
    file.take(max_bytes.max(1))
        .read_to_end(&mut buffer)
        .map_err(|_| "Cannot read log tail".to_string())?;

    let text = String::from_utf8_lossy(&buffer);
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            let bounded: String = line.chars().take(400).collect();
            app::sanitize(&bounded)
        })
        .collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let keep = lines.len().saturating_sub(max_lines);
    Ok(lines.split_off(keep))
}

fn event_loop(
    terminal: &mut TuiTerminal,
    app: &mut App,
    quota_jobs: &Sender<app::QuotaRequest>,
    system_jobs: &Sender<SystemJob>,
    quota_results: &Receiver<WorkerMessage>,
    system_results: &Receiver<WorkerMessage>,
) -> Result<(), String> {
    loop {
        while let Ok(message) = quota_results.try_recv() {
            for command in app.apply_message(message) {
                dispatch(command, app, quota_jobs, system_jobs);
            }
        }
        while let Ok(message) = system_results.try_recv() {
            for command in app.apply_message(message) {
                dispatch(command, app, quota_jobs, system_jobs);
            }
        }
        if app.quit {
            return Ok(());
        }

        app.tick();
        terminal
            .draw(|frame| ui::render(frame, app))
            .map_err(|_| "Cannot draw the TUI frame".to_string())?;

        if event::poll(Duration::from_millis(150))
            .map_err(|_| "Cannot poll terminal events".to_string())?
        {
            match event::read().map_err(|_| "Cannot read terminal events".to_string())? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    for command in app.handle_key(key) {
                        dispatch(command, app, quota_jobs, system_jobs);
                    }
                }
                _ => {}
            }
        }
    }
}

fn dispatch(
    command: Command,
    app: &App,
    quota_jobs: &Sender<app::QuotaRequest>,
    system_jobs: &Sender<SystemJob>,
) {
    match command {
        Command::RefreshQuota => {
            let _ = quota_jobs.send(app.quota_request());
        }
        Command::RefreshTimer => {
            let _ = system_jobs.send(SystemJob::Timer);
        }
        Command::RefreshLog => {
            let _ = system_jobs.send(SystemJob::Log {
                path: app.log_path(),
            });
        }
        Command::Apply(config) => {
            let _ = system_jobs.send(SystemJob::Apply {
                config,
                paths: app.paths.clone(),
            });
        }
        Command::RunNow => {
            let _ = system_jobs.send(SystemJob::RunNow);
        }
    }
}

enum SystemJob {
    Timer,
    Log { path: PathBuf },
    Apply { config: Box<Config>, paths: Paths },
    RunNow,
}

fn spawn_quota_worker(
    requests: Receiver<app::QuotaRequest>,
    messages: Sender<WorkerMessage>,
) -> Result<thread::JoinHandle<()>, String> {
    thread::Builder::new()
        .name("cpaq-quota".to_string())
        .spawn(move || {
            while let Ok(request) = requests.recv() {
                if messages.send(run_quota_job(request)).is_err() {
                    break;
                }
            }
        })
        .map_err(|_| "Cannot start quota worker".to_string())
}

fn run_quota_job(request: app::QuotaRequest) -> WorkerMessage {
    match request
        .paths
        .resolve_auth(request.auth_dir.as_deref(), request.config.auth_dir())
    {
        Ok(dir) => match quota::fetch(
            &dir,
            &request.paths,
            &request.config,
            &request.providers,
            false,
        ) {
            Ok(accounts) => WorkerMessage::Quota {
                auth_dir: dir,
                accounts,
                at: time::now(),
            },
            Err(error) => WorkerMessage::QuotaError {
                auth_dir: Some(dir),
                error: app::sanitize(&error),
                at: time::now(),
            },
        },
        Err(error) => WorkerMessage::QuotaError {
            auth_dir: None,
            error: app::sanitize(&error),
            at: time::now(),
        },
    }
}

fn spawn_system_worker(
    requests: Receiver<SystemJob>,
    messages: Sender<WorkerMessage>,
) -> Result<thread::JoinHandle<()>, String> {
    thread::Builder::new()
        .name("cpaq-system".to_string())
        .spawn(move || {
            let runner = SystemRunner;
            while let Ok(job) = requests.recv() {
                let message = match job {
                    SystemJob::Timer => WorkerMessage::Timer(systemd::status(&runner)),
                    SystemJob::Log { path } => {
                        match read_log_tail(&path, app::LOG_TAIL_BYTES, app::LOG_TAIL_LINES) {
                            Ok(lines) => WorkerMessage::Log { lines, error: None },
                            Err(error) => WorkerMessage::Log {
                                lines: Vec::new(),
                                error: Some(error),
                            },
                        }
                    }
                    SystemJob::Apply { config, paths } => {
                        let result = std::env::current_exe()
                            .map_err(|_| "Cannot resolve the cpaq executable path".to_string())
                            .and_then(|executable| {
                                app::apply_validated(config.as_ref(), &paths, &executable, &runner)
                            });
                        WorkerMessage::Applied { config, result }
                    }
                    SystemJob::RunNow => WorkerMessage::RunNow(systemd::run_now(&runner)),
                };
                if messages.send(message).is_err() {
                    break;
                }
            }
        })
        .map_err(|_| "Cannot start system worker".to_string())
}

fn init_terminal() -> Result<TuiTerminal, String> {
    enable_raw_mode().map_err(|_| "Cannot enable raw terminal mode".to_string())?;
    let mut stdout = std::io::stdout();
    if execute!(stdout, EnterAlternateScreen).is_err() {
        let _ = disable_raw_mode();
        return Err("Cannot enter the alternate screen".to_string());
    }
    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => Ok(terminal),
        Err(_) => {
            restore_terminal();
            Err("Cannot initialize the terminal".to_string())
        }
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen, Show);
}

struct RestoreGuard;

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Installs a panic hook that restores the terminal before unwinding or abort.
fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{test_support::Sandbox, tui::app::App};

    fn test_app() -> App {
        App::new(
            Paths::new(
                PathBuf::from("/home/cpaq-test"),
                Some(PathBuf::from("/home/cpaq-test/.config")),
                Some(PathBuf::from("/home/cpaq-test/.local/state")),
            ),
            Config::default(),
            Args::default(),
        )
    }

    #[test]
    fn log_tail_is_bounded_to_the_file_end() {
        let sandbox = Sandbox::default();
        let log = sandbox.0.join("ping.log");
        let mut body = String::from("oldest-line\n");
        for _ in 0..200 {
            body.push_str("filler line that should be skipped\n");
        }
        body.push_str("newest-visible-line\n");
        std::fs::write(&log, body).unwrap();

        let lines = read_log_tail(&log, 128, 2).unwrap();
        assert!(lines.len() <= 2);
        assert_eq!(lines.last().unwrap(), "newest-visible-line");
        assert!(!lines.iter().any(|line| line.contains("oldest-line")));
    }

    #[test]
    fn log_tail_redacts_credentials() {
        let sandbox = Sandbox::default();
        let log = sandbox.0.join("ping.log");
        std::fs::write(&log, "2025 ping ok\nAuthorization: Bearer supersecret\n").unwrap();

        let lines = read_log_tail(&log, 4096, 10).unwrap();
        let text = lines.join("\n");
        assert!(text.contains("[redacted]"));
        assert!(!text.contains("supersecret"));
    }

    #[test]
    fn missing_log_is_reported_without_panicking() {
        let sandbox = Sandbox::default();
        let error = read_log_tail(&sandbox.0.join("absent.log"), 1024, 5).unwrap_err();
        assert!(error.contains("log"));
    }

    #[test]
    fn tick_never_reads_or_replaces_the_log_tail() {
        let mut app = test_app();
        app.tokenmaxx.log_lines = vec!["unchanged".into()];
        for _ in 0..50 {
            app.tick();
        }
        assert_eq!(app.tokenmaxx.log_lines, ["unchanged"]);
    }
}
