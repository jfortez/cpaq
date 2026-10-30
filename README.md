# cpaq

A fast terminal UI for CLIProxyAPI quotas and the tokenmaxx keep-alive.
Single Rust binary, no async runtime.

## Install

```sh
cargo install cpaq
```

Or from a local checkout:

```sh
cargo build --release
install -m755 target/release/cpaq ~/.local/bin/cpaq
```

## Usage

```sh
cpaq                # TUI: Quotas | Tokenmaxx
cpaq quota          # headless quota report
cpaq quota --json   # machine-readable output
cpaq ping           # unconditional keep-alive pass
cpaq auto           # local due check and bounded early-reset probe (systemd timer)
cpaq auto --dry-run # decision and would-be state; quota reads only, no ping/writes
cpaq units apply    # save config and regenerate/enable the user units
```

Keys: `q` quit · `Tab` switch tab · `j`/`k` move · `Enter` details/edit ·
`r` refresh · `a` toggle auto (saved immediately) · `s` save and apply timer · `x` run now · `?` help.

## Configuration

Config lives in `$XDG_CONFIG_HOME/cpaq/config.toml` (`~/.config/cpaq/config.toml`):

```toml
[proxy]
base_url = "http://127.0.0.1:8317"
api_key = ""        # empty: read ~/.pi/agent/cliproxyapi.json

[tokenmaxx]
enabled = true
auto = true       # default, including older configs
anchor = "05:00"
interval_hours = 5
probe_minutes = 15 # early-reset check interval; 0 disables
model = "gpt-6-luna"
```

Auth files are read from `$XDG_CONFIG_HOME/cli-proxy-api/auths`, falling back to
`$XDG_CONFIG_HOME/cpa/auths`. Override with `--auth-dir` or `CPA_AUTH_DIR`.

Root-owned auth files are read through the Docker Engine API over
`/var/run/docker.sock` (override with `CPA_DOCKER_SOCKET`), so no `docker` CLI
child process is spawned; `CPA_DOCKER_CONTAINER` and `CPA_CONTAINER_AUTH_DIR`
name the container and the directory inside it.

Saving the Tokenmaxx tab writes `tokenmaxx.service` and `tokenmaxx.timer` under
`$XDG_CONFIG_HOME/systemd/user`, reloads systemd and enables the timer. The
service waits for `network-online.target`, runs `cpaq auto`, and re-arms the
timer after every run, so a boot catch-up or a failed run does not break the
chain. With `auto = true`, the timer checks every five minutes; local state
controls when to ping or probe. With `auto = false`, the timer retains the configured
anchor and hourly interval. A boot check runs after 90 seconds. `auto` reads only
`$XDG_STATE_HOME/tokenmaxx/auto.json` (`~/.local/state/tokenmaxx/auto.json`)
until a ping or probe is due; other idle checks do not read auth or contact
Docker/providers. State is shared by `ping` and `auto`, written atomically with
mode 0600 after a successful ping or a probe attempt. Failed pings preserve the
schedule; probe attempts advance only `last_probe_epoch` unless a ping succeeds. The next due time is the
earliest Codex 5h reset from the existing quota snapshot, falling back to the
last ping plus `interval_hours` when unavailable (including `--no-quota`).
`--json` returns the decision and state; ping details still go to the normal log.
Early resets: every `probe_minutes` (default 15; `0` disables), auto reads the
existing Codex quota snapshot. A 5h reset more than 60 seconds later than the
stored window, with usage at most 1% and no blocked/limited/error Codex account,
triggers one immediate ping. Unknown/high usage or unavailable quota only advances
the probe timestamp, not the schedule. Legacy state compares against `next_due_epoch`.
`--dry-run` reports the probe, comparison and would-be state without pinging or
writing. Auto sends at most one ping request, without transport retries.

`cpaq ping` retries transport failures (DNS, connect, timeout) up to three attempts
with short backoff, keeps a whole run inside a two-minute budget, and
retries the quota snapshot once; HTTP 4xx/5xx responses are not retried.

## Providers

- Codex quotas are supported.
- Claude's OAuth usage endpoint is preliminary.
- Other account types are listed as unsupported.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

## Releasing

Releases are patch-only: the minor version stays pinned at 1, so versions are
always `0.1.N`. From `main`, run the `release` workflow (Actions → release →
Run workflow). It runs the checks, bumps the patch with `scripts/release.sh`,
commits `chore(release): vX.Y.Z`, tags it, publishes to crates.io and creates
the GitHub release with generated notes.

`scripts/release.sh --dry-run` prints the next version and writes nothing.

One-time maintainer setup: add a crates.io API token with publish scope as the
`CARGO_REGISTRY_TOKEN` repository secret (Settings → Secrets and variables →
Actions). No other secret is needed.
