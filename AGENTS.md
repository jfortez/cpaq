# Agent instructions

- Keep the scope to quota inspection and tokenmaxx management. Do not expand it.
- Work only inside this repository/worktree. Do not install binaries system-wide
  and do not touch `~/.config/systemd/user` from tests — systemd interaction must
  be behind a fake-able trait or function.
- Rust stable, edition 2024. Prefer small, readable modules over clever code.
- No async runtime, no reqwest, no unnecessary crates.
- Every behavioral change needs a unit test. TUI must have a `TestBackend` render test.
- Before committing: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`.
- Keep commits few and title-only (no body). Do not commit `target/`.
- Do not print, log or embed any token, API key or credential.
