//! Pure ratatui rendering for both TUI tabs.
//!
//! [`render`] performs no I/O and is safe for non-interactive `TestBackend`
//! previews.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;

use crate::{
    quota::{self, Window},
    time,
};

use super::{
    app::{self, App, Overlay, Setting, Tab},
    theme,
};

/// Draws the whole app. Long lines are clipped by ratatui, never panicked on.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let header_height = area.height.min(2);
    let footer_height = u16::from(area.height >= 3);
    let body_height = area.height.saturating_sub(header_height + footer_height);
    let header = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: header_height,
    };
    let body = Rect {
        x: area.x,
        y: area.y + header_height,
        width: area.width,
        height: body_height,
    };
    let footer = Rect {
        x: area.x,
        y: area.y + header_height + body_height,
        width: area.width,
        height: footer_height,
    };

    render_header(frame, app, header);
    if body.height > 0 {
        match app.tab {
            Tab::Quotas => render_quotas(frame, app, body),
            Tab::Tokenmaxx => render_tokenmaxx(frame, app, body),
        }
    }
    if footer.height > 0 {
        render_footer(frame, app, footer);
    }

    match app.overlay {
        Overlay::None => {}
        Overlay::Help => render_help(frame, area),
        Overlay::Details => render_details(frame, app, area),
    }
}

/// Human-readable exact reset instant: raw value plus local time when known.
pub(crate) fn exact_reset(window: &Window) -> String {
    match &window.reset_at {
        Some(Value::Number(number)) => {
            if let Some(epoch) = number.as_i64() {
                format!("{} ({})", epoch, time::local(epoch, "%Y-%m-%d %H:%M:%S %Z"))
            } else if let Some(epoch) = number.as_u64().and_then(|value| i64::try_from(value).ok())
            {
                format!("{} ({})", epoch, time::local(epoch, "%Y-%m-%d %H:%M:%S %Z"))
            } else {
                number.to_string()
            }
        }
        Some(Value::String(text)) => time::parse_iso(text).map_or_else(
            || text.clone(),
            |epoch| format!("{text} ({})", time::local(epoch, "%Y-%m-%d %H:%M:%S %Z")),
        ),
        Some(value) => value.to_string(),
        None => window.reset_after.map_or_else(
            || "—".to_string(),
            |seconds| format!("in {}", quota::relative(seconds)),
        ),
    }
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let tabs = Line::from(vec![
        Span::styled(
            "cpaq",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("[Quotas]", theme::tab_style(app.tab == Tab::Quotas)),
        Span::raw("  "),
        Span::styled("[Tokenmaxx]", theme::tab_style(app.tab == Tab::Tokenmaxx)),
    ]);
    let auth = app.quota.auth_dir.as_ref().map_or_else(
        || {
            if app.quota.error.is_some() {
                "auth: not found".to_string()
            } else {
                "auth: resolving…".to_string()
            }
        },
        |dir| format!("auth: {}", app.paths.shorten(dir)),
    );
    let refreshed = app
        .quota
        .last_refresh
        .map_or_else(|| "never".to_string(), |at| time::local(at, "%H:%M:%S"));
    let spinner = if app.quota.refreshing {
        format!("  {}", spinner(app.spinner))
    } else {
        String::new()
    };
    let details = Line::from(vec![Span::styled(
        format!("{auth}  refreshed {refreshed}{spinner}"),
        Style::default().fg(theme::MUTED),
    )]);
    frame.render_widget(Paragraph::new(Text::from(vec![tabs, details])), area);
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let line = if app.status.is_empty() {
        Line::from(Span::styled(
            "q quit · ? help · Tab tabs · j/k move · r refresh · s save/apply · x run now",
            Style::default().fg(theme::MUTED),
        ))
    } else {
        let color = if app.status_error {
            theme::ERROR
        } else {
            theme::OK
        };
        Line::from(Span::styled(
            app::sanitize(&app.status),
            Style::default().fg(color),
        ))
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_quotas(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.quota.accounts.is_empty() {
        let message = if let Some(error) = &app.quota.error {
            format!("{error}\n\nSet --auth-dir or add auth JSON files.")
        } else if app.quota.refreshing {
            "Refreshing quotas…".to_string()
        } else {
            let dir = app.quota.auth_dir.as_ref().map_or_else(
                || "the configured auth directory".to_string(),
                |dir| app.paths.shorten(dir),
            );
            format!("No accounts found in {dir}.\nAdd auth JSON files or set --auth-dir.")
        };
        let paragraph = Paragraph::new(app::sanitize(&message))
            .block(Block::default().borders(Borders::ALL).title("Accounts"))
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, area);
        return;
    }

    let list_area = if let Some(error) = &app.quota.error {
        let chunks = Layout::new(
            Direction::Vertical,
            [Constraint::Length(1), Constraint::Min(1)],
        )
        .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("! {}", app::sanitize(error)),
                Style::default().fg(theme::ERROR),
            ))),
            chunks[0],
        );
        chunks[1]
    } else {
        area
    };

    let (items, selected) = quota_items(app);
    let mut state = ListState::default();
    state.select(selected);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Accounts · grouped by provider"),
        )
        .highlight_style(
            Style::default()
                .bg(theme::SELECTED)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    frame.render_stateful_widget(list, list_area, &mut state);
}

fn quota_items(app: &App) -> (Vec<ListItem<'static>>, Option<usize>) {
    let mut items = Vec::new();
    let mut selected = None;
    let mut current_provider: Option<&str> = None;

    for (index, account) in app.quota.accounts.iter().enumerate() {
        if current_provider != Some(account.provider.as_str()) {
            current_provider = Some(account.provider.as_str());
            let provider = if account.provider.is_empty() {
                "unknown"
            } else {
                account.provider.as_str()
            };
            items.push(ListItem::new(Line::from(Span::styled(
                format!("▌ {provider}"),
                theme::provider_style(provider),
            ))));
        }

        let marker = theme::marker(account);
        let status = theme::status_label(account);
        let email = account.email.as_deref().unwrap_or("(no email)");
        let plan = account
            .plan
            .as_deref()
            .map_or_else(String::new, |plan| format!(" ({plan})"));
        let mut lines = vec![Line::from(vec![
            Span::styled(format!("{marker} "), theme::status_style(account)),
            Span::styled(
                email.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(plan),
            Span::raw(" — "),
            Span::styled(status.to_string(), theme::status_style(account)),
        ])];

        if account.windows.is_empty() {
            lines.push(Line::from(Span::styled(
                "   no quota windows",
                Style::default().fg(theme::MUTED),
            )));
        }
        for window in &account.windows {
            lines.push(window_line(window, app.now));
        }
        if let Some(error) = &account.error {
            lines.push(Line::from(vec![
                Span::styled("   ! ", Style::default().fg(theme::ERROR)),
                Span::styled(app::sanitize(error), Style::default().fg(theme::ERROR)),
            ]));
        }

        if index == app.quota.selected {
            selected = Some(items.len());
        }
        items.push(ListItem::new(Text::from(lines)));
    }
    (items, selected)
}

fn window_line(window: &Window, now: i64) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("   {:<4}", window.label),
            Style::default().fg(theme::MUTED),
        ),
        Span::raw(" "),
        Span::styled(
            quota::bar(window.used_percent),
            Style::default().fg(theme::ACCENT),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:>6}", quota::percent(window.used_percent)),
            Style::default(),
        ),
        Span::raw("  "),
        Span::styled(
            quota::reset_text(window, now),
            Style::default().fg(theme::MUTED),
        ),
    ])
}

fn render_tokenmaxx(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let log_height = area.height.saturating_sub(3).min(4);
    let list_height = area.height.saturating_sub(log_height);
    let list_area = Rect {
        height: list_height,
        ..area
    };
    let log_area = Rect {
        y: area.y + list_height,
        height: log_height,
        ..area
    };

    if list_height > 0 {
        let selected = app
            .tokenmaxx
            .selected
            .min(Setting::ALL.len().saturating_sub(1));
        let mut items = Vec::new();
        for setting in Setting::ALL {
            let editing = app.edit_setting == Some(setting);
            let value = if editing {
                app.edit.as_ref().map_or_else(
                    || app.setting_value(setting),
                    |edit| format!("{}▏", edit.value),
                )
            } else {
                app.setting_value(setting)
            };
            let value_style = if setting.editable() {
                Style::default()
            } else {
                Style::default().fg(theme::MUTED)
            };
            let mut spans = vec![
                Span::styled(
                    format!("{:<11}", setting.label()),
                    Style::default().fg(theme::ACCENT),
                ),
                Span::raw(" "),
                Span::styled(value, value_style),
            ];
            if !setting.editable() {
                spans.push(Span::styled(
                    "  (read-only)",
                    Style::default().fg(theme::MUTED),
                ));
            }
            if editing {
                spans.push(Span::styled(
                    "   editing · Enter save · Esc cancel",
                    Style::default().fg(theme::WARN),
                ));
            }
            items.push(ListItem::new(Line::from(spans)));
        }
        if let Some(error) = &app.tokenmaxx.timer_error {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("timer error  {}", app::sanitize(error)),
                Style::default().fg(theme::ERROR),
            ))));
        }

        let mut state = ListState::default();
        state.select(Some(selected));
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Tokenmaxx settings"),
            )
            .highlight_style(
                Style::default()
                    .bg(theme::SELECTED)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▸ ");
        frame.render_stateful_widget(list, list_area, &mut state);
    }

    if log_area.height > 0 {
        let title = format!("Log tail — {}", app.paths.shorten(&app.log_path()));
        let lines = if let Some(error) = &app.tokenmaxx.log_error {
            vec![Line::from(Span::styled(
                app::sanitize(error),
                Style::default().fg(theme::ERROR),
            ))]
        } else if app.tokenmaxx.log_lines.is_empty() {
            vec![Line::from(Span::styled(
                "No log lines yet.",
                Style::default().fg(theme::MUTED),
            ))]
        } else {
            app.tokenmaxx
                .log_lines
                .iter()
                .map(|line| Line::from(app::sanitize(line)))
                .collect()
        };
        let paragraph = Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, log_area);
    }
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(80, 80, area);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::styled(
            "Help",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("q quit · Ctrl-C quit anywhere"),
        Line::raw("Tab switch tabs"),
        Line::raw("j/k or arrows navigate"),
        Line::raw("Enter or space toggle/edit; Esc cancels"),
        Line::raw("r refresh quotas and timer status"),
        Line::raw("s save & apply tokenmaxx units"),
        Line::raw("x run now (systemctl --user start)"),
        Line::raw("? close this help"),
    ];
    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title("Help"))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, popup);
}

fn render_details(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(account) = app.quota.accounts.get(app.quota.selected) else {
        return;
    };
    let popup = centered_rect(80, 80, area);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    frame.render_widget(Clear, popup);

    let mut lines = vec![
        detail_line("file", &account.file),
        detail_line("provider", &account.provider),
        detail_line("email", account.email.as_deref().unwrap_or("—")),
        detail_line("plan", account.plan.as_deref().unwrap_or("—")),
        detail_line("account id", account.account_id.as_deref().unwrap_or("—")),
        detail_line("allowed", &format!("{:?}", account.allowed)),
        detail_line("limit reached", &format!("{:?}", account.limit_reached)),
        detail_line("status", theme::status_label(account)),
        Line::raw(""),
    ];
    for window in &account.windows {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}  ", window.label),
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                quota::bar(window.used_percent),
                Style::default().fg(theme::ACCENT),
            ),
            Span::raw(format!("  {}  ", quota::percent(window.used_percent))),
            Span::styled(
                format!("exact reset: {}", exact_reset(window)),
                Style::default().fg(theme::MUTED),
            ),
        ]));
    }
    if let Some(error) = &account.error {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("error: ", Style::default().fg(theme::ERROR)),
            Span::styled(app::sanitize(error), Style::default().fg(theme::ERROR)),
        ]));
    }

    let paragraph = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Account details"),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, popup);
}

fn detail_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<14}"), Style::default().fg(theme::MUTED)),
        Span::raw(value.to_string()),
    ])
}

fn spinner(index: usize) -> char {
    ['⠋', '⠙', '⠹', '⠸'][index % 4]
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let width = area.width.saturating_mul(percent_x.min(100)) / 100;
    let height = area.height.saturating_mul(percent_y.min(100)) / 100;
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;

    use super::*;
    use crate::{
        cli::Args,
        config::Config,
        paths::Paths,
        quota::Account,
        quota::Window,
        systemd::TimerStatus,
        tui::app::{EditState, WorkerMessage, sample_account},
    };

    fn test_paths() -> Paths {
        Paths::new(
            PathBuf::from("/home/cpaq-test"),
            Some(PathBuf::from("/home/cpaq-test/.config")),
            Some(PathBuf::from("/home/cpaq-test/.local/state")),
        )
    }

    fn app_with(accounts: Vec<Account>) -> App {
        let mut app = App::new(test_paths(), Config::default(), Args::default());
        app.now = 1_700_000_000;
        app.apply_message(WorkerMessage::Quota {
            auth_dir: PathBuf::from("/home/cpaq-test/.config/cli-proxy-api/auths"),
            accounts,
            at: 1_700_000_000,
        });
        app
    }

    fn render_text(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..height {
            for x in 0..width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn fixture_window() -> Window {
        Window {
            label: "5h".into(),
            used_percent: Some(50.0),
            reset_at: Some(json!(1_700_003_600i64)),
            reset_after: Some(3600.0),
        }
    }

    #[test]
    fn both_tabs_draw_their_own_content() {
        let mut app = app_with(vec![sample_account(
            "codex-a.json",
            "codex",
            "alice@example.org",
            Some(50.0),
            None,
        )]);
        app.tokenmaxx.timer = Some(TimerStatus {
            enabled: "enabled".into(),
            active: "active".into(),
            next_run: "Tue 2025-10-21 05:00:00 UTC".into(),
            last_run: "Mon 2025-10-20 05:00:00 UTC".into(),
        });
        app.tokenmaxx.log_lines = vec!["log-tail-line".into()];
        let expected_reset = quota::reset_text(&app.quota.accounts[0].windows[0], app.now);

        let quotas = render_text(&app, 120, 30);
        assert!(quotas.contains("Quotas"));
        assert!(quotas.contains("codex"));
        assert!(quotas.contains("alice@example.org"));
        assert!(quotas.contains("OK"));
        assert!(quotas.contains(&quota::bar(Some(50.0))));
        assert!(quotas.contains(&quota::percent(Some(50.0))));
        assert!(quotas.contains(&expected_reset));

        app.tab = Tab::Tokenmaxx;
        let tokenmaxx = render_text(&app, 120, 30);
        assert!(tokenmaxx.contains("Tokenmaxx"));
        assert!(tokenmaxx.contains("anchor"));
        assert!(tokenmaxx.contains("05:00"));
        assert!(tokenmaxx.contains("proxy base"));
        assert!(tokenmaxx.contains("http://127.0.0.1:8317"));
        assert!(tokenmaxx.contains("next run"));
        assert!(tokenmaxx.contains("Tue 2025-10-21 05:00:00 UTC"));
        assert!(tokenmaxx.contains("log-tail-line"));
    }

    #[test]
    fn empty_and_tiny_terminals_render_both_tabs_without_panicking() {
        let mut app = app_with(Vec::new());
        app.quota.error = Some("Auth directory not found. Tried: /a, /b".into());

        let quotas = render_text(&app, 40, 10);
        assert!(quotas.contains("Quotas"));
        assert!(quotas.contains("Auth directory"));

        app.quota.error = None;
        let empty = render_text(&app, 40, 10);
        assert!(empty.contains("No accounts"));

        app.tab = Tab::Tokenmaxx;
        let tokenmaxx = render_text(&app, 40, 10);
        assert!(tokenmaxx.contains("Tokenmaxx"));
        assert!(tokenmaxx.contains("enabled"));

        app.overlay = Overlay::Help;
        let help = render_text(&app, 40, 10);
        assert!(help.contains("Help"));

        app.overlay = Overlay::Details;
        let details = render_text(&app, 40, 10);
        assert!(!details.is_empty());
    }

    #[test]
    fn provider_groups_bars_and_per_account_errors_all_render() {
        let mut app = app_with(vec![
            sample_account("c.json", "claude", "c@example.org", Some(100.0), None),
            sample_account(
                "a.json",
                "codex",
                "a@example.org",
                Some(10.0),
                Some("HTTP 401 Bearer topsecret retry later"),
            ),
            sample_account("b.json", "codex", "b@example.org", Some(50.0), None),
        ]);
        let claude = app
            .quota
            .accounts
            .iter_mut()
            .find(|account| account.provider == "claude")
            .unwrap();
        claude.limit_reached = Some(true);
        claude.allowed = Some(false);

        let text = render_text(&app, 120, 30);
        assert!(text.contains("a@example.org"));
        assert!(text.contains("b@example.org"));
        assert!(text.contains("c@example.org"));
        assert!(text.contains("error"));
        assert!(text.contains("LIMIT"));
        assert!(text.contains(&quota::bar(Some(50.0))));

        let claude_header = text.find("▌ claude").unwrap();
        let claude_email = text.find("c@example.org").unwrap();
        let codex_header = text.find("▌ codex").unwrap();
        let codex_email = text.find("a@example.org").unwrap();
        assert!(claude_header < claude_email);
        assert!(claude_email < codex_header);
        assert!(codex_header < codex_email);
        assert!(!text.contains("topsecret"));
    }

    #[test]
    fn details_show_file_account_id_exact_reset_and_sanitized_error() {
        let mut account = sample_account(
            "codex-a.json",
            "codex",
            "alice@example.org",
            Some(50.0),
            Some("HTTP 401 Bearer topsecret retry later"),
        );
        account.account_id = Some("acct-visible".into());
        let mut app = app_with(vec![account]);
        app.overlay = Overlay::Details;

        let text = render_text(&app, 120, 35);
        assert!(text.contains("Account details"));
        assert!(text.contains("codex-a.json"));
        assert!(text.contains("acct-visible"));
        assert!(text.contains(&time::local(1_700_000_000, "%Y-%m-%d %H:%M:%S")));
        assert!(text.contains("HTTP 401"));
        assert!(!text.contains("topsecret"));
    }

    #[test]
    fn exact_reset_formats_numeric_iso_and_relative_values() {
        let mut window = fixture_window();
        assert_eq!(
            exact_reset(&window),
            format!(
                "{} ({})",
                1_700_003_600,
                time::local(1_700_003_600, "%Y-%m-%d %H:%M:%S %Z")
            )
        );

        window.reset_at = Some(json!("1970-01-01T00:00:00Z"));
        let iso = exact_reset(&window);
        assert!(iso.contains("1970-01-01T00:00:00Z"));
        assert!(iso.contains(&time::local(0, "%Y-%m-%d %H:%M:%S %Z")));

        window.reset_at = None;
        window.reset_after = Some(3600.0);
        assert!(exact_reset(&window).contains(&quota::relative(3600.0)));
    }

    #[test]
    fn help_overlay_lists_the_spec_keys() {
        let mut app = app_with(Vec::new());
        app.overlay = Overlay::Help;
        let text = render_text(&app, 100, 30);
        assert!(text.contains("Help"));
        assert!(text.contains("q quit"));
        assert!(text.contains("s save & apply"));
        assert!(text.contains("x run now"));
    }

    #[test]
    fn selection_scrolls_so_the_last_account_stays_visible() {
        let accounts = (0..15)
            .map(|i| {
                sample_account(
                    &format!("codex-{i}.json"),
                    "codex",
                    &format!("user{i:02}@example.org"),
                    Some(10.0),
                    None,
                )
            })
            .collect();
        let mut app = app_with(accounts);
        app.quota.selected = 14;

        let text = render_text(&app, 40, 10);
        assert!(text.contains("user14@example.org"));
        assert!(!text.contains("user00@example.org"));
    }

    #[test]
    fn edit_buffer_and_status_are_visible() {
        let mut app = app_with(Vec::new());
        app.tab = Tab::Tokenmaxx;
        app.tokenmaxx.selected = 1;
        app.edit_setting = Some(Setting::Anchor);
        app.edit = Some(EditState {
            value: "09:99".into(),
            cursor: 5,
        });
        app.status = "Change pending — press s to save".into();

        let text = render_text(&app, 120, 30);
        assert!(text.contains("09:99"));
        assert!(text.contains("press s to save"));
    }
}
