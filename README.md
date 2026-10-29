# cpaq

A fast terminal UI for CLIProxyAPI quotas and the tokenmaxx keep-alive.
Single Rust binary, no async runtime.

## Install

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
cpaq auto           # local due check; ping only when due (systemd timer)
cpaq auto --dry-run # decision and would-be state, without HTTP or writes
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
chain. With `auto = true`, the timer checks every five minutes; local state alone
controls when to ping. With `auto = false`, the timer retains the configured
anchor and hourly interval. A boot check runs after 90 seconds. `auto` reads only
`$XDG_STATE_HOME/tokenmaxx/auto.json` (`~/.local/state/tokenmaxx/auto.json`)
until due; idle checks do not read auth or contact Docker/providers. State is
shared by `ping` and `auto`, written atomically with mode 0600 after at least one
successful ping; failed passes leave it unchanged. The next due time is the
earliest Codex 5h reset from the existing quota snapshot, falling back to the
last ping plus `interval_hours` when unavailable (including `--no-quota`).
`--json` returns the decision and state; ping details still go to the normal log.
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
