use std::path::PathBuf;

pub const HELP: &str = "cpaq — CLIProxyAPI quotas and tokenmaxx\n\nUsage:\n  cpaq                         TUI when stdout is a TTY; otherwise quota\n  cpaq quota [--json] [--raw] [--provider P]... [--auth-dir DIR]\n  cpaq --json                  Headless quota JSON\n  cpaq ping [--model M] [--pings N] [--log FILE] [--no-quota]\n  cpaq --help | --version\n\nQuota: -p/--provider is repeatable; positional providers are also accepted.\nAuth discovery: --auth-dir > CPA_AUTH_DIR > config > XDG cli-proxy-api/auths > cpa/auths.\nContainer fallback: CPA_DOCKER_SOCKET (default /var/run/docker.sock), CPA_DOCKER_CONTAINER, CPA_CONTAINER_AUTH_DIR.\nPing: --pings must be positive; default one per account, minimum one.\nPing retries transport failures (DNS/connect/timeout) 3 times; HTTP 4xx/5xx is not retried.\nTUI: q quit, ? help, r refresh, Tab switch tabs, arrows/j/k navigate, Enter details/edit, Esc close.\nTokenmaxx: space toggle/edit, s save & apply, x run now.\n";
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Mode {
    #[default]
    Auto,
    Quota,
    Ping,
    Help,
    Version,
}
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
}
impl Args {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut out = Self::default();
        let mut iter = args.into_iter().peekable();
        if let Some(command) = iter.peek() {
            match command.as_str() {
                "quota" => {
                    out.mode = Mode::Quota;
                    iter.next();
                }
                "ping" => {
                    out.mode = Mode::Ping;
                    iter.next();
                }
                _ => {}
            }
        }
        while let Some(arg) = iter.next() {
            let mut value = || {
                iter.next()
                    .ok_or_else(|| "Missing option value; see --help".to_string())
            };
            match arg.as_str() {
                "--help" | "-h" => out.mode = Mode::Help,
                "--version" | "-V" => out.mode = Mode::Version,
                "--json" if out.mode != Mode::Ping => {
                    out.json = true;
                    out.mode = Mode::Quota;
                }
                "--raw" if out.mode != Mode::Ping => {
                    out.raw = true;
                    out.mode = Mode::Quota;
                }
                "--provider" | "-p" if out.mode != Mode::Ping => {
                    out.providers.push(value()?.to_lowercase())
                }
                "--auth-dir" if out.mode != Mode::Ping => out.auth_dir = Some(value()?.into()),
                "--model" if out.mode == Mode::Ping => out.model = Some(value()?),
                "--pings" if out.mode == Mode::Ping => {
                    let n = value()?
                        .parse::<usize>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("--pings must be a positive integer")?;
                    out.pings = Some(n);
                }
                "--log" if out.mode == Mode::Ping => out.log = Some(value()?.into()),
                "--no-quota" if out.mode == Mode::Ping => out.no_quota = true,
                _ if !arg.starts_with('-') && out.mode != Mode::Ping => {
                    out.providers.push(arg.to_lowercase());
                    out.mode = Mode::Quota;
                }
                _ => return Err("Unknown option or argument; see --help".into()),
            }
        }
        Ok(out)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse(s: &[&str]) -> Result<Args, String> {
        Args::parse(s.iter().map(|s| s.to_string()))
    }
    #[test]
    fn quota_json_repeated_providers_and_auth_override() {
        let a = parse(&[
            "quota",
            "--json",
            "-p",
            "codex",
            "--provider",
            "claude",
            "--auth-dir",
            "auths",
        ])
        .unwrap();
        assert_eq!(a.mode, Mode::Quota);
        assert!(a.json);
        assert_eq!(a.providers, vec!["codex", "claude"]);
        assert_eq!(a.auth_dir, Some("auths".into()));
    }
    #[test]
    fn ping_rejects_quota_flags_and_zero_pings() {
        assert!(parse(&["ping", "--pings", "0"]).is_err());
        assert!(parse(&["ping", "--json"]).is_err());
        assert_eq!(
            parse(&["ping", "--pings", "2", "--no-quota"])
                .unwrap()
                .pings,
            Some(2)
        );
    }
}
