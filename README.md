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
cpaq ping           # keep-alive ping (run by the systemd timer)
```

Keys: `q` quit · `Tab` switch tab · `j`/`k` move · `Enter` details/edit ·
`r` refresh · `s` save and apply timer · `x` run now · `?` help.

## Configuration

Config lives in `$XDG_CONFIG_HOME/cpaq/config.toml` (`~/.config/cpaq/config.toml`):

```toml
[proxy]
base_url = "http://127.0.0.1:8317"
api_key = ""        # empty: read ~/.pi/agent/cliproxyapi.json

[tokenmaxx]
enabled = true
anchor = "05:00"
interval_hours = 5
model = "gpt-6-luna"
```

Auth files are read from `$XDG_CONFIG_HOME/cli-proxy-api/auths`, falling back to
`$XDG_CONFIG_HOME/cpa/auths`. Override with `--auth-dir` or `CPA_AUTH_DIR`.

Saving the Tokenmaxx tab writes `tokenmaxx.service` and `tokenmaxx.timer` under
`$XDG_CONFIG_HOME/systemd/user`, reloads systemd and enables the timer. The
service waits for `network-online.target`, runs `cpaq ping`, and re-arms the
timer after every run, so a boot catch-up or a failed run does not break the
chain. `cpaq ping` retries transport failures (DNS, connect, timeout) up to three
attempts with short backoff, keeps a whole run inside a two-minute budget, and
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
