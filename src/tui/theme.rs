//! Colors and status styling shared by the TUI widgets.

use ratatui::style::{Color, Modifier, Style};

use crate::quota::Account;

pub const ACCENT: Color = Color::Cyan;
pub const OK: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const ERROR: Color = Color::Red;
pub const MUTED: Color = Color::DarkGray;
pub const SELECTED: Color = Color::Indexed(24);

/// Style for a tab label in the header.
pub fn tab_style(selected: bool) -> Style {
    if selected {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED)
    }
}

/// Provider accents keep grouped sections visually distinct.
pub fn provider_style(provider: &str) -> Style {
    let color = match provider.to_ascii_lowercase().as_str() {
        "codex" => Color::Magenta,
        "claude" => Color::Blue,
        _ => MUTED,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// The short status word shown next to an account.
pub fn status_label(account: &Account) -> &'static str {
    if account.error.is_some() {
        "error"
    } else if account.limit_reached == Some(true) {
        "LIMIT"
    } else if account.allowed == Some(false) {
        "blocked"
    } else if account.allowed == Some(true) {
        "OK"
    } else {
        "unknown"
    }
}

/// Status marker: filled for known states, hollow for unknown.
pub fn marker(account: &Account) -> &'static str {
    if account.allowed.is_some() || account.limit_reached.is_some() || account.error.is_some() {
        "●"
    } else {
        "○"
    }
}

/// Style matching [`status_label`].
pub fn status_style(account: &Account) -> Style {
    if account.error.is_some() {
        Style::default().fg(ERROR)
    } else if account.limit_reached == Some(true) || account.allowed == Some(false) {
        Style::default().fg(WARN).add_modifier(Modifier::BOLD)
    } else if account.allowed == Some(true) {
        Style::default().fg(OK)
    } else {
        Style::default().fg(MUTED)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::quota::{Account, Window};

    fn account(error: Option<&str>, allowed: Option<bool>, limit: Option<bool>) -> Account {
        Account {
            file: "codex-a.json".into(),
            provider: "codex".into(),
            email: Some("a@example.org".into()),
            plan: Some("plus".into()),
            account_id: Some("acct".into()),
            allowed,
            limit_reached: limit,
            windows: vec![Window {
                label: "5h".into(),
                used_percent: Some(50.0),
                reset_at: Some(json!(1_700_000_000i64)),
                reset_after: Some(3600.0),
            }],
            error: error.map(str::to_string),
            raw: None,
        }
    }

    #[test]
    fn account_status_words_mark_each_state() {
        assert_eq!(status_label(&account(None, Some(true), Some(false))), "OK");
        assert_eq!(
            status_label(&account(None, Some(false), Some(true))),
            "LIMIT"
        );
        assert_eq!(
            status_label(&account(None, Some(false), Some(false))),
            "blocked"
        );
        assert_eq!(
            status_label(&account(Some("HTTP 401"), None, None)),
            "error"
        );
        assert_eq!(status_label(&account(None, None, None)), "unknown");
    }

    #[test]
    fn markers_distinguish_known_and_unknown_accounts() {
        assert_eq!(marker(&account(None, Some(true), Some(false))), "●");
        assert_eq!(marker(&account(None, None, None)), "○");
    }
}
