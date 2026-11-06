//! Command line surface: clap owns parsing, `Args` stays the internal API.

use std::path::PathBuf;

use clap::{Parser, Subcommand, error::ErrorKind};

const ABOUT: &str = "CLIProxyAPI quotas and tokenmaxx keep-alive";

const LONG_ABOUT: &str = "\
Inspect CLIProxyAPI quotas, send tokenmaxx keep-alive pings, and read the
tokenmaxx journal. Run without a command for the TUI when stdout is a terminal,
otherwise for the quota report.";

const AUTH_NOTES: &str = "\
Auth discovery: --auth-dir > CPA_AUTH_DIR > config > XDG cli-proxy-api/auths > cpa/auths.
Container fallback: CPA_DOCKER_SOCKET (default /var/run/docker.sock), CPA_DOCKER_CONTAINER,
CPA_CONTAINER_AUTH_DIR.";

const QUOTA_NOTES: &str = "\
-p/--provider is repeatable; positional providers are also accepted.
Auth discovery: --auth-dir > CPA_AUTH_DIR > config > XDG cli-proxy-api/auths > cpa/auths.
Container fallback: CPA_DOCKER_SOCKET (default /var/run/docker.sock), CPA_DOCKER_CONTAINER,
CPA_CONTAINER_AUTH_DIR.";

const PING_NOTES: &str = "\
--pings must be positive; default one per account, minimum one.
Ping retries transport failures (DNS/connect/timeout) 3 times; HTTP 4xx/5xx is not retried.
Ping and auto share state after any codex snapshot: earliest reopening (5h, or the weekly reset
of a spent account), or interval_hours without one.";

const AUTO_NOTES: &str = "\
Enabled by default; probe_minutes=15 (0 disables) checks early 5h and weekly Codex resets.
A due instant at most 300s away is waited for and pinged in the same run; a manual ping during
that wait wins.
Journal: $XDG_STATE_HOME/tokenmaxx/auto.log, JSONL mode 0600, rotated at 2 MiB; write failures
only warn. At most one ping request per run, without transport retries.
--dry-run may read quota when a probe is due; it never pings, waits or writes state.";

const LOG_NOTES: &str = "\
Reads $XDG_STATE_HOME/tokenmaxx/auto.log oldest to newest: the last 20 pings by default.
--all adds ticks and reset changes; --limit N keeps the newest N (0 = all).
--json passes the raw journal lines through; --follow polls every 500 ms and reopens after
rotation. A missing journal is not an error, and a closed pipe (| head) ends the output.";

const REPORT_NOTES: &str = "\
Daily table of ticks, probes, pings, wait times and reset changes, then the individual pings and
the unexpected resets. --days N uses local calendar days including today (default 7; 0 = all).";

/// Downstream command dispatch: one mode per invocation.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Mode {
    #[default]
    Default,
    Auto,
    Log,
    Report,
    UnitsApply,
    Quota,
    Ping,
    Help,
    Version,
}

/// Parsed invocation. `main` and the TUI read this, never clap directly.
#[derive(Clone, Debug, Default)]
pub struct Args {
    pub mode: Mode,
    pub json: bool,
    pub raw: bool,
    pub providers: Vec<String>,
    pub auth_dir: Option<PathBuf>,
    pub model: Option<String>,
    pub pings: Option<usize>,
    pub log: Option<PathBuf>,
    pub no_quota: bool,
    pub dry_run: bool,
    pub follow: bool,
    pub all: bool,
    pub limit: Option<usize>,
    pub days: Option<usize>,
    /// Rendered help for `--help` or `help <subcommand>`; empty otherwise.
    pub help_text: String,
}

#[derive(Debug, Parser)]
#[command(
    name = "cpaq",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    after_help = AUTH_NOTES,
    propagate_version = true
)]
struct Cli {
    /// Machine-readable quota JSON (implies the quota command).
    #[arg(long)]
    json: bool,
    /// Include the raw provider payload.
    #[arg(long)]
    raw: bool,
    /// Directory with the account auth files.
    #[arg(long, value_name = "DIR")]
    auth_dir: Option<PathBuf>,
    /// Only query these providers (repeatable).
    #[arg(short, long = "provider", value_name = "PROVIDER")]
    provider: Vec<String>,
    /// Providers to query (positional shorthand for `quota`).
    #[arg(value_name = "PROVIDER")]
    providers: Vec<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Quota report for every configured account.
    #[command(after_help = QUOTA_NOTES)]
    Quota {
        /// Machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Include the raw provider payload.
        #[arg(long)]
        raw: bool,
        /// Only query these providers (repeatable).
        #[arg(short, long = "provider", value_name = "PROVIDER")]
        provider: Vec<String>,
        /// Directory with the account auth files.
        #[arg(long, value_name = "DIR")]
        auth_dir: Option<PathBuf>,
        /// Providers to query (positional shorthand).
        #[arg(value_name = "PROVIDER")]
        providers: Vec<String>,
    },
    /// One tokenmaxx keep-alive pass.
    #[command(after_help = PING_NOTES)]
    Ping {
        /// Model used for the keep-alive request.
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// Number of pings; must be positive.
        #[arg(long, value_name = "N", value_parser = positive)]
        pings: Option<usize>,
        /// Write the ping log to this file.
        #[arg(long, value_name = "FILE")]
        log: Option<PathBuf>,
        /// Skip the quota snapshot after the ping.
        #[arg(long)]
        no_quota: bool,
    },
    /// Decide whether a ping is due, probe when due, and ping once.
    #[command(after_help = AUTO_NOTES)]
    Auto {
        /// Machine-readable decision.
        #[arg(long)]
        json: bool,
        /// Report the decision only; never ping, wait or write.
        #[arg(long)]
        dry_run: bool,
    },
    /// Tail the tokenmaxx journal.
    #[command(after_help = LOG_NOTES)]
    Log {
        /// Print the tail, then keep polling for new records.
        #[arg(short = 'f', long)]
        follow: bool,
        /// Pass the raw journal lines through.
        #[arg(long)]
        json: bool,
        /// Include ticks and reset changes, not only pings.
        #[arg(long)]
        all: bool,
        /// Number of records to show (0 = all).
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
    /// Visual report of the tokenmaxx journal.
    #[command(after_help = REPORT_NOTES)]
    Report {
        /// Local calendar days to summarise (0 = all).
        #[arg(long, value_name = "N")]
        days: Option<usize>,
    },
    /// Manage the systemd user units.
    Units {
        #[command(subcommand)]
        action: UnitsAction,
    },
}

#[derive(Debug, Subcommand)]
enum UnitsAction {
    /// Write the units, reload systemd and enable the timer.
    Apply,
}

/// `--pings` accepts only a positive count.
fn positive(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| "--pings must be a positive integer".to_string())
}

/// Repeated flags first, then positional providers, all lower-cased.
fn providers(repeated: Vec<String>, positional: Vec<String>) -> Vec<String> {
    repeated
        .into_iter()
        .chain(positional)
        .map(|provider| provider.to_lowercase())
        .collect()
}

impl Cli {
    /// Root flags and providers only apply without a subcommand.
    fn has_root_quota_input(&self) -> bool {
        self.json
            || self.raw
            || self.auth_dir.is_some()
            || !self.provider.is_empty()
            || !self.providers.is_empty()
    }
}

impl From<Cli> for Args {
    fn from(cli: Cli) -> Self {
        let mut args = Args::default();
        match cli.command {
            Some(Command::Quota {
                json,
                raw,
                provider,
                auth_dir,
                providers: positional,
            }) => {
                args.mode = Mode::Quota;
                args.json = json;
                args.raw = raw;
                args.auth_dir = auth_dir;
                args.providers = providers(provider, positional);
            }
            Some(Command::Ping {
                model,
                pings,
                log,
                no_quota,
            }) => {
                args.mode = Mode::Ping;
                args.model = model;
                args.pings = pings;
                args.log = log;
                args.no_quota = no_quota;
            }
            Some(Command::Auto { json, dry_run }) => {
                args.mode = Mode::Auto;
                args.json = json;
                args.dry_run = dry_run;
            }
            Some(Command::Log {
                follow,
                json,
                all,
                limit,
            }) => {
                args.mode = Mode::Log;
                args.follow = follow;
                args.json = json;
                args.all = all;
                args.limit = limit;
            }
            Some(Command::Report { days }) => {
                args.mode = Mode::Report;
                args.days = days;
            }
            Some(Command::Units {
                action: UnitsAction::Apply,
            }) => args.mode = Mode::UnitsApply,
            None => {
                // No subcommand: the top-level flags decide between the default
                // dispatch and the headless quota report.
                args.json = cli.json;
                args.raw = cli.raw;
                args.auth_dir = cli.auth_dir;
                args.providers = providers(cli.provider, cli.providers);
                args.mode = if args.json || args.raw || !args.providers.is_empty() {
                    Mode::Quota
                } else {
                    Mode::Default
                };
            }
        }
        args
    }
}

impl Args {
    /// Parses argv (without the program name).
    ///
    /// Help and version are successful short circuits: the rendered help comes
    /// back in [`Args::help_text`] so `main` can print it without clap.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut argv: Vec<String> = vec!["cpaq".to_string()];
        argv.extend(args);
        match Cli::try_parse_from(argv) {
            Ok(cli) => {
                // clap accepts root flags and providers before a subcommand;
                // refuse them instead of silently dropping them.
                if cli.command.is_some() && cli.has_root_quota_input() {
                    return Err("Top-level quota options apply only without a subcommand; \
                         put them after the subcommand instead (see cpaq <command> --help)"
                        .into());
                }
                Ok(Self::from(cli))
            }
            Err(error) => match error.kind() {
                ErrorKind::DisplayHelp => Ok(Self {
                    mode: Mode::Help,
                    help_text: error.to_string(),
                    ..Self::default()
                }),
                ErrorKind::DisplayVersion => Ok(Self {
                    mode: Mode::Version,
                    ..Self::default()
                }),
                _ => Err(error.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &[&str]) -> Result<Args, String> {
        Args::parse(s.iter().map(|s| s.to_string()))
    }

    #[test]
    fn top_level_flags_and_positional_providers() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.mode, Mode::Default);
        assert!(args.help_text.is_empty());

        let args = parse(&["--json"]).unwrap();
        assert_eq!(args.mode, Mode::Quota);
        assert!(args.json);

        let args = parse(&["--raw"]).unwrap();
        assert_eq!(args.mode, Mode::Quota);
        assert!(args.raw);

        // An auth directory alone does not force the quota branch.
        let args = parse(&["--auth-dir", "auths"]).unwrap();
        assert_eq!(args.mode, Mode::Default);
        assert_eq!(args.auth_dir, Some(PathBuf::from("auths")));

        let args = parse(&["codex", "Claude"]).unwrap();
        assert_eq!(args.mode, Mode::Quota);
        assert_eq!(args.providers, vec!["codex", "claude"]);

        let args = parse(&["-p", "CODEX", "--provider", "claude", "codex"]).unwrap();
        assert_eq!(args.mode, Mode::Quota);
        assert_eq!(args.providers, vec!["codex", "claude", "codex"]);
    }

    #[test]
    fn quota_subcommand_accepts_its_own_flags() {
        let args = parse(&[
            "quota",
            "--json",
            "-p",
            "codex",
            "--provider",
            "claude",
            "--auth-dir",
            "auths",
            "codex",
        ])
        .unwrap();
        assert_eq!(args.mode, Mode::Quota);
        assert!(args.json && !args.raw);
        assert_eq!(args.providers, vec!["codex", "claude", "codex"]);
        assert_eq!(args.auth_dir, Some(PathBuf::from("auths")));
        assert!(parse(&["quota", "--raw"]).unwrap().raw);
    }

    #[test]
    fn ping_flags_and_positive_pings() {
        let args = parse(&[
            "ping",
            "--model",
            "m",
            "--pings",
            "2",
            "--log",
            "x.log",
            "--no-quota",
        ])
        .unwrap();
        assert_eq!(args.mode, Mode::Ping);
        assert_eq!(args.model.as_deref(), Some("m"));
        assert_eq!(args.pings, Some(2));
        assert_eq!(args.log, Some(PathBuf::from("x.log")));
        assert!(args.no_quota);
        assert!(parse(&["ping"]).unwrap().pings.is_none());
        assert!(parse(&["ping", "--pings", "0"]).is_err());
        assert!(parse(&["ping", "--pings"]).is_err());
        assert!(parse(&["ping", "--json"]).is_err());
    }

    #[test]
    fn auto_log_report_and_units_flags() {
        let auto = parse(&["auto", "--json", "--dry-run"]).unwrap();
        assert_eq!(auto.mode, Mode::Auto);
        assert!(auto.json && auto.dry_run);
        assert_eq!(parse(&["auto"]).unwrap().mode, Mode::Auto);

        let log = parse(&["log", "--follow", "--all", "--json", "--limit", "0"]).unwrap();
        assert_eq!(log.mode, Mode::Log);
        assert!(log.follow && log.all && log.json);
        assert_eq!(log.limit, Some(0));
        assert!(parse(&["log", "-f"]).unwrap().follow);
        assert_eq!(parse(&["log", "--limit", "5"]).unwrap().limit, Some(5));
        assert_eq!(parse(&["log"]).unwrap().limit, None);

        let report = parse(&["report", "--days", "0"]).unwrap();
        assert_eq!(report.mode, Mode::Report);
        assert_eq!(report.days, Some(0));
        assert_eq!(parse(&["report"]).unwrap().days, None);

        assert_eq!(parse(&["units", "apply"]).unwrap().mode, Mode::UnitsApply);
        for args in [vec!["units"], vec!["units", "remove"]] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn cross_command_flags_and_bad_values_are_rejected() {
        // The rejection must explain the fix, not just fail.
        let error = parse(&["--json", "quota"]).unwrap_err();
        assert!(error.contains("subcommand"), "{error}");
        assert!(error.contains("--help"), "{error}");
        for args in [
            vec!["auto", "--follow"],
            vec!["auto", "--all"],
            vec!["auto", "--limit", "3"],
            vec!["log", "--days", "2"],
            vec!["log", "--limit", "many"],
            vec!["report", "--json"],
            vec!["report", "--days", "-1"],
            vec!["report", "--days", "many"],
            vec!["quota", "--pings", "2"],
            vec!["ping", "--dry-run"],
            vec!["ping", "--days", "2"],
            vec!["--follow"],
            vec!["--unknown"],
            // Root quota options only apply without a subcommand.
            vec!["--json", "ping"],
            vec!["--json", "quota"],
            vec!["--auth-dir", "auths", "log"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn help_and_version_short_circuit_with_their_own_text() {
        for args in [
            vec!["--help"],
            vec!["-h"],
            vec!["help"],
            vec!["help", "quota"],
            vec!["quota", "--help"],
            vec!["ping", "--help"],
            vec!["log", "-h"],
            vec!["report", "--help"],
        ] {
            let parsed = parse(&args).unwrap();
            assert_eq!(parsed.mode, Mode::Help, "{args:?}");
            assert!(!parsed.help_text.trim().is_empty(), "{args:?}");
        }
        let top = parse(&["--help"]).unwrap().help_text;
        for command in ["quota", "ping", "auto", "log", "report", "units"] {
            assert!(top.contains(command), "top-level help lost {command}");
        }
        assert!(top.contains("--auth-dir") && top.contains("--provider"));
        let quota = parse(&["quota", "--help"]).unwrap().help_text;
        assert!(quota.contains("--raw") && quota.contains("--auth-dir"));
        assert!(!quota.contains("--dry-run"), "quota help leaked auto flags");
        // `help quota` renders the same page as `quota --help`.
        assert_eq!(parse(&["help", "quota"]).unwrap().help_text, quota);
        let ping = parse(&["ping", "--help"]).unwrap().help_text;
        for flag in ["--model", "--pings", "--log", "--no-quota"] {
            assert!(ping.contains(flag), "ping help lost {flag}");
        }
        for args in [vec!["--version"], vec!["-V"], vec!["quota", "--version"]] {
            assert_eq!(parse(&args).unwrap().mode, Mode::Version);
        }
    }
}
