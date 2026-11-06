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
cpaq                # print the command list (same as --help)
cpaq tui            # TUI: Quotas | Tokenmaxx
cpaq quota          # headless quota report
cpaq quota --json   # machine-readable output
cpaq ping           # unconditional keep-alive pass
cpaq auto           # local due check and bounded early-reset probe (systemd timer)
cpaq auto --dry-run # decision and would-be state; quota reads only, no ping/writes
cpaq log            # journal tail: the last 20 ping records
cpaq report         # visual report: daily summary plus ping and reset details
cpaq units apply    # save config and regenerate/enable the user units
```

Keys: `q` quit · `Tab` switch tab · `j`/`k` move · `Enter` details/edit ·
`r` refresh · `a` toggle auto (saved immediately) · `s` save and apply timer ·
`p` ping now (one keep-alive pass) · `x` run now · `?` help.

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
mode 0600 after a successful ping or a probe attempt. Probe attempts advance
`last_probe_epoch`; a failed ping preserves the schedule unless every Codex
account is spent, in which case auto waits for the earliest reopening instead of
retrying every five minutes.

The next due time is the earliest moment a Codex account can serve again: its 5h
reset when nothing is capped, or the latest reset among its spent windows, so a
weekly-capped account waits for the weekly reset even while its 5h window rolls.
It falls back to the last ping plus `interval_hours` when the snapshot is
unavailable (including `--no-quota`).

Early resets: every `probe_minutes` (default 15; `0` disables), auto reads the
existing Codex quota snapshot. The earliest 5h or weekly reset that moved more
than 60 seconds later than its stored anchor, with usage at most 1% and only
unblocked accounts considered, triggers one immediate ping; an earlier reopening
shortens `next_due` without pinging. Unknown/high usage or unavailable quota only
advances the probe timestamp, not the schedule. Legacy state compares the 5h
anchor against `next_due_epoch`.
`--dry-run` reports the probe, comparison and would-be state without pinging or
writing. Auto sends at most one ping request, without transport retries.

`cpaq ping` retries transport failures (DNS, connect, timeout) up to three attempts
with short backoff, keeps a whole run inside a two-minute budget, and
retries the quota snapshot once; HTTP 4xx/5xx responses are not retried.

## Exact scheduling and the auto journal

The five-minute timer only wakes `cpaq auto`; the exact instant is decided
inside the run. When the stored due time is at most five minutes away, the run
sleeps at sub-second precision up to that second, re-reads the state and the
clock, and pings there, so a reset at `17:35:24` is not lost to a tick at
`17:40:24`. A manual ping during that wait wins: the re-read state is no longer
due and the waiting run exits without a second ping. The generated timer uses
`AccuracySec=1s` and the service allows 600 seconds, enough for the wait plus
one ping.

Every non-dry-run run appends one JSONL `tick` record to
`$XDG_STATE_HOME/tokenmaxx/auto.log` (mode 0600, rotated to `auto.log.1` past
2 MiB), and every ping appends a `ping` record with `origin` `ping` or `auto`.
When a probe runs, the tick also carries the stored and provider anchors and the
per-account snapshot; unexpected provider movements (`window_reopened_early`,
`long_window_reopened_early`, `window_shortened`) and banked-reset changes
(`banked_reset_used`, `banked_reset_earned`, `banked_reset_applicable`) are
written as separate `reset_change` records. A journal write failure prints one
warning and never changes the exit code or blocks a ping. The journal holds no
credentials, and `cpaq auto --json` keeps its existing shape.

`cpaq log` reads that journal: by default the last 20 `ping` records, oldest to
newest, with `--all` widening the selection to ticks and reset changes and
`--limit N` (`0` = all) choosing the tail. `--json` passes the raw JSONL lines
through, and `--follow`/`-f` prints that tail once and then polls every 500 ms,
reopening from zero after a truncation or a rotation, so a running tail survives
`auto.log` becoming `auto.log.1`.

`cpaq report --days N` turns the journal into a readable report (`0` = all
records, default seven local calendar days): a per-day table with ticks,
probes, pings, ok/err, auto/manual, average and maximum wait and reset counts,
then the next due instant, the pings with the wait of the tick that sent them,
and the unexpected reset changes from the probe.

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
