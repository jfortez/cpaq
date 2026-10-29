//! TUI application state, key handling and background-job requests.
//!
//! [`App::new`] performs no filesystem, network or systemd work. The caller
//! draws the first frame with [`crate::tui::ui::render`] and only then starts
//! the workers and applies [`App::startup_commands`]. This keeps the first
//! frame independent of auth discovery and provider requests.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    cli::Args,
    config::{Config, TokenmaxxConfig},
    paths::Paths,
    quota::Account,
    systemd::{self, Runner, TimerStatus},
    time,
};

/// Maximum bytes read from the tail of the tokenmaxx log.
pub const LOG_TAIL_BYTES: u64 = 8 * 1024;
/// Maximum log lines kept in memory for the Tokenmaxx tab.
pub const LOG_TAIL_LINES: usize = 6;

/// Active top-level tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Quotas,
    Tokenmaxx,
}

/// Modal overlay, if any.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overlay {
    #[default]
    None,
    Help,
    Details,
}

/// Tokenmaxx fields in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    Enabled,
    Anchor,
    Interval,
    Model,
    LogPath,
    Proxy,
    Timer,
    NextRun,
    LastRun,
    Auto,
}

impl Setting {
    pub const ALL: [Self; 10] = [
        Self::Enabled,
        Self::Anchor,
        Self::Interval,
        Self::Model,
        Self::LogPath,
        Self::Proxy,
        Self::Timer,
        Self::NextRun,
        Self::LastRun,
        Self::Auto,
    ];

    /// Short label rendered in the settings list.
    pub fn label(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Auto => "auto",
            Self::Anchor => "anchor",
            Self::Interval => "interval",
            Self::Model => "model",
            Self::LogPath => "log path",
            Self::Proxy => "proxy base",
            Self::Timer => "timer",
            Self::NextRun => "next run",
            Self::LastRun => "last run",
        }
    }

    /// Whether the user can toggle or edit this field.
    pub fn editable(self) -> bool {
        matches!(
            self,
            Self::Enabled
                | Self::Auto
                | Self::Anchor
                | Self::Interval
                | Self::Model
                | Self::LogPath
        )
    }
}

/// Inline edit buffer, cursor measured in characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditState {
    pub value: String,
    pub cursor: usize,
}

/// Editable tokenmaxx settings while the user is in the TUI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub enabled: bool,
    pub auto: bool,
    pub anchor: String,
    pub interval_hours: String,
    pub model: String,
    pub log_path: String,
    pub log_path_dirty: bool,
}

/// Quotas-tab data. Filled only by background worker messages.
#[derive(Default)]
pub struct QuotaState {
    pub accounts: Vec<Account>,
    pub selected: usize,
    pub auth_dir: Option<PathBuf>,
    pub error: Option<String>,
    pub last_refresh: Option<i64>,
    pub refreshing: bool,
}

/// Tokenmaxx-tab data and pending action flags.
pub struct TokenmaxxState {
    pub selected: usize,
    pub draft: Draft,
    pub proxy_base_url: String,
    pub timer: Option<TimerStatus>,
    pub timer_error: Option<String>,
    pub log_lines: Vec<String>,
    pub log_error: Option<String>,
    pub status_pending: bool,
    pub log_pending: bool,
    pub save_pending: bool,
    pub run_pending: bool,
}

/// Work the UI asks the background workers to perform.
///
/// The UI never blocks on these jobs and marks each category pending so the
/// same job cannot overlap itself.
pub enum Command {
    RefreshQuota,
    RefreshTimer,
    RefreshLog,
    Apply(Box<Config>),
    RunNow,
}

/// Results delivered from background workers through `mpsc`.
pub enum WorkerMessage {
    Quota {
        auth_dir: PathBuf,
        accounts: Vec<Account>,
        at: i64,
    },
    QuotaError {
        auth_dir: Option<PathBuf>,
        error: String,
        at: i64,
    },
    Timer(Result<TimerStatus, String>),
    Log {
        lines: Vec<String>,
        error: Option<String>,
    },
    Applied {
        config: Box<Config>,
        result: Result<(), String>,
    },
    RunNow(Result<(), String>),
}

/// Request sent to the single quota worker thread.
pub struct QuotaRequest {
    pub paths: Paths,
    pub config: Config,
    pub providers: Vec<String>,
    pub auth_dir: Option<PathBuf>,
}

/// Whole TUI state.
pub struct App {
    pub paths: Paths,
    pub base_config: Config,
    pub providers: Vec<String>,
    pub auth_override: Option<PathBuf>,
    pub tab: Tab,
    pub overlay: Overlay,
    pub edit: Option<EditState>,
    pub edit_setting: Option<Setting>,
    pub quit: bool,
    pub now: i64,
    pub spinner: usize,
    pub status: String,
    pub status_error: bool,
    pub status_expires: Option<i64>,
    pub quota: QuotaState,
    pub tokenmaxx: TokenmaxxState,
}

impl App {
    /// Builds state without touching auth files, the network or systemd.
    pub fn new(paths: Paths, config: Config, args: Args) -> Self {
        let tokenmaxx = TokenmaxxState {
            selected: 0,
            draft: draft_from_config(&config, &paths),
            proxy_base_url: safe_proxy_url(&config.proxy.base_url),
            timer: None,
            timer_error: None,
            log_lines: Vec::new(),
            log_error: None,
            status_pending: false,
            log_pending: false,
            save_pending: false,
            run_pending: false,
        };
        Self {
            paths,
            base_config: config,
            providers: args.providers,
            auth_override: args.auth_dir,
            tab: Tab::Quotas,
            overlay: Overlay::None,
            edit: None,
            edit_setting: None,
            quit: false,
            now: time::now(),
            spinner: 0,
            status: String::new(),
            status_error: false,
            status_expires: None,
            quota: QuotaState::default(),
            tokenmaxx,
        }
    }

    /// Jobs to dispatch only after the first frame has been drawn.
    pub fn startup_commands(&mut self) -> Vec<Command> {
        self.quota.refreshing = true;
        self.tokenmaxx.status_pending = true;
        self.tokenmaxx.log_pending = true;
        vec![
            Command::RefreshQuota,
            Command::RefreshTimer,
            Command::RefreshLog,
        ]
    }

    /// Builds the quota-worker request from the current base config.
    pub fn quota_request(&self) -> QuotaRequest {
        QuotaRequest {
            paths: self.paths.clone(),
            config: self.base_config.clone(),
            providers: self.providers.clone(),
            auth_dir: self.auth_override.clone(),
        }
    }

    /// Path used for the bounded log tail.
    pub fn log_path(&self) -> PathBuf {
        self.base_config.log_path(&self.paths)
    }

    /// Processes a key press and returns background jobs to dispatch.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Command> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            self.quit = true;
            return Vec::new();
        }

        if self.edit.is_some() {
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                self.commit_edit();
                self.switch_tab();
                return Vec::new();
            }
            self.handle_edit_key(key);
            return Vec::new();
        }

        if key.code == KeyCode::Char('q') {
            self.quit = true;
            return Vec::new();
        }

        if self.overlay == Overlay::Help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.overlay = Overlay::None;
            }
            return Vec::new();
        }

        if self.overlay == Overlay::Details && matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
            self.overlay = Overlay::None;
            return Vec::new();
        }

        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                self.overlay = Overlay::None;
                self.switch_tab();
                Vec::new()
            }
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                Vec::new()
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('a') if self.tab == Tab::Tokenmaxx => {
                self.toggle_auto();
                Vec::new()
            }
            KeyCode::Char('s') if self.tab == Tab::Tokenmaxx => self.request_save(),
            KeyCode::Char('x') if self.tab == Tab::Tokenmaxx => self.request_run_now(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Vec::new()
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(),
            KeyCode::Esc => {
                self.overlay = Overlay::None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Folds a background worker result into UI state.
    pub fn apply_message(&mut self, message: WorkerMessage) -> Vec<Command> {
        match message {
            WorkerMessage::Quota {
                auth_dir,
                mut accounts,
                at,
            } => {
                accounts.sort_by(|a, b| {
                    a.provider
                        .cmp(&b.provider)
                        .then_with(|| a.email.cmp(&b.email))
                        .then_with(|| a.file.cmp(&b.file))
                });
                self.quota.accounts = accounts;
                self.quota.auth_dir = Some(auth_dir);
                self.quota.error = None;
                self.quota.last_refresh = Some(at);
                self.quota.refreshing = false;
                self.clamp_quota_selection();
                Vec::new()
            }
            WorkerMessage::QuotaError {
                auth_dir,
                error,
                at,
            } => {
                if let Some(dir) = auth_dir {
                    self.quota.auth_dir = Some(dir);
                }
                self.quota.last_refresh = Some(at);
                self.quota.error = Some(sanitize(&error));
                self.quota.refreshing = false;
                self.set_status(error, true);
                Vec::new()
            }
            WorkerMessage::Timer(Ok(timer)) => {
                self.tokenmaxx.timer = Some(timer);
                self.tokenmaxx.timer_error = None;
                self.tokenmaxx.status_pending = false;
                Vec::new()
            }
            WorkerMessage::Timer(Err(error)) => {
                self.tokenmaxx.timer_error = Some(sanitize(&error));
                self.tokenmaxx.status_pending = false;
                Vec::new()
            }
            WorkerMessage::Log { lines, error } => {
                self.tokenmaxx.log_lines = lines
                    .into_iter()
                    .take(LOG_TAIL_LINES)
                    .map(|line| sanitize(&line))
                    .collect();
                self.tokenmaxx.log_error = error.map(|error| sanitize(&error));
                self.tokenmaxx.log_pending = false;
                Vec::new()
            }
            WorkerMessage::Applied { config, result } => {
                self.tokenmaxx.save_pending = false;
                match result {
                    Ok(()) => {
                        self.base_config = *config;
                        self.set_status("Saved and applied tokenmaxx", false);
                        self.refresh_timer_and_log()
                    }
                    Err(error) => {
                        self.set_status(format!("Apply failed: {error}"), true);
                        Vec::new()
                    }
                }
            }
            WorkerMessage::RunNow(result) => {
                self.tokenmaxx.run_pending = false;
                match result {
                    Ok(()) => {
                        self.set_status("Started tokenmaxx.service", false);
                        self.refresh_timer_and_log()
                    }
                    Err(error) => {
                        self.set_status(format!("Run now failed: {error}"), true);
                        Vec::new()
                    }
                }
            }
        }
    }

    /// Advances timers and expires transient status text. Never performs I/O.
    pub fn tick(&mut self) {
        self.now = time::now();
        self.spinner = (self.spinner + 1) % 4;
        if self
            .status_expires
            .is_some_and(|expires| self.now >= expires)
        {
            self.status.clear();
            self.status_error = false;
            self.status_expires = None;
        }
    }

    /// Builds and validates a config snapshot before any save/apply job.
    pub fn build_config(&self) -> Result<Config, String> {
        let interval_hours = self
            .tokenmaxx
            .draft
            .interval_hours
            .trim()
            .parse::<u32>()
            .map_err(|_| "interval_hours must be a whole number".to_string())?;
        let log_path = if self.tokenmaxx.draft.log_path_dirty {
            let path = self.tokenmaxx.draft.log_path.trim();
            (!path.is_empty()).then(|| PathBuf::from(path))
        } else {
            self.base_config.tokenmaxx.log_path.clone()
        };
        let mut config = self.base_config.clone();
        config.tokenmaxx = TokenmaxxConfig {
            enabled: self.tokenmaxx.draft.enabled,
            auto: self.tokenmaxx.draft.auto,
            anchor: self.tokenmaxx.draft.anchor.trim().to_string(),
            interval_hours,
            probe_minutes: self.base_config.tokenmaxx.probe_minutes,
            model: self.tokenmaxx.draft.model.trim().to_string(),
            pings_per_account: self.base_config.tokenmaxx.pings_per_account,
            log_path,
        };
        config.validate()?;
        Ok(config)
    }

    /// Current display/edit value for a tokenmaxx field.
    pub fn setting_value(&self, setting: Setting) -> String {
        match setting {
            Setting::Enabled => yes_no(self.tokenmaxx.draft.enabled),
            Setting::Auto => if self.tokenmaxx.draft.auto {
                "on"
            } else {
                "off"
            }
            .into(),
            Setting::Anchor => self.tokenmaxx.draft.anchor.clone(),
            Setting::Interval => format!("{} h", self.tokenmaxx.draft.interval_hours),
            Setting::Model => self.tokenmaxx.draft.model.clone(),
            Setting::LogPath => self.tokenmaxx.draft.log_path.clone(),
            Setting::Proxy => self.tokenmaxx.proxy_base_url.clone(),
            Setting::Timer => self.tokenmaxx.timer.as_ref().map_or_else(
                || pending_or_unavailable(self.tokenmaxx.status_pending),
                |timer| {
                    format!(
                        "enabled={} active={}",
                        non_empty(&timer.enabled),
                        non_empty(&timer.active)
                    )
                },
            ),
            Setting::NextRun => self.timer_field(|timer| &timer.next_run),
            Setting::LastRun => self.timer_field(|timer| &timer.last_run),
        }
    }

    /// Whether `setting` is currently being edited.
    pub fn is_editing(&self, setting: Setting) -> bool {
        self.edit_setting == Some(setting)
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = sanitize(&text.into());
        self.status_error = error;
        self.status_expires = Some(self.now + 8);
    }

    fn timer_field(&self, field: impl Fn(&TimerStatus) -> &String) -> String {
        self.tokenmaxx.timer.as_ref().map_or_else(
            || pending_or_unavailable(self.tokenmaxx.status_pending),
            |timer| non_empty(field(timer)),
        )
    }

    fn switch_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Quotas => Tab::Tokenmaxx,
            Tab::Tokenmaxx => Tab::Quotas,
        };
    }

    fn move_selection(&mut self, delta: i32) {
        match self.tab {
            Tab::Quotas => {
                let max = self.quota.accounts.len().saturating_sub(1);
                self.quota.selected = clamp_step(self.quota.selected, delta, max);
            }
            Tab::Tokenmaxx => {
                self.tokenmaxx.selected = clamp_step(
                    self.tokenmaxx.selected,
                    delta,
                    Setting::ALL.len().saturating_sub(1),
                );
            }
        }
    }

    fn activate(&mut self) -> Vec<Command> {
        match self.tab {
            Tab::Quotas => {
                if self.quota.accounts.get(self.quota.selected).is_some() {
                    self.overlay = Overlay::Details;
                }
                Vec::new()
            }
            Tab::Tokenmaxx => {
                let setting = Setting::ALL[self
                    .tokenmaxx
                    .selected
                    .min(Setting::ALL.len().saturating_sub(1))];
                if setting == Setting::Auto {
                    self.toggle_auto();
                } else if setting == Setting::Enabled {
                    self.tokenmaxx.draft.enabled = !self.tokenmaxx.draft.enabled;
                    self.set_status("Change pending — press s to save", false);
                } else if setting.editable() {
                    self.begin_edit(setting);
                } else {
                    self.set_status(format!("{} is read-only", setting.label()), false);
                }
                Vec::new()
            }
        }
    }

    fn toggle_auto(&mut self) {
        if self.tokenmaxx.save_pending {
            self.set_status("Save/apply already running", true);
            return;
        }
        // Persist only this toggle, leaving unrelated draft edits unsaved.
        let mut config = self.base_config.clone();
        config.tokenmaxx.auto = !self.tokenmaxx.draft.auto;
        match config.save(&self.paths.config_file()) {
            Ok(()) => {
                self.tokenmaxx.draft.auto = config.tokenmaxx.auto;
                self.base_config.tokenmaxx.auto = config.tokenmaxx.auto;
                self.set_status("Auto saved — press s to re-apply units", false);
            }
            Err(error) => self.set_status(error, true),
        }
    }

    fn begin_edit(&mut self, setting: Setting) {
        let value = match setting {
            Setting::Anchor => self.tokenmaxx.draft.anchor.clone(),
            Setting::Interval => self.tokenmaxx.draft.interval_hours.clone(),
            Setting::Model => self.tokenmaxx.draft.model.clone(),
            Setting::LogPath => self.tokenmaxx.draft.log_path.clone(),
            _ => return,
        };
        self.edit_setting = Some(setting);
        self.edit = Some(EditState {
            cursor: value.chars().count(),
            value,
        });
    }

    fn commit_edit(&mut self) {
        let (Some(setting), Some(edit)) = (self.edit_setting.take(), self.edit.take()) else {
            return;
        };
        let value = edit.value.trim().to_string();
        match setting {
            Setting::Anchor => self.tokenmaxx.draft.anchor = value,
            Setting::Interval => self.tokenmaxx.draft.interval_hours = value,
            Setting::Model => self.tokenmaxx.draft.model = value,
            Setting::LogPath => {
                let saved = draft_from_config(&self.base_config, &self.paths).log_path;
                if value.is_empty() || value == saved {
                    self.tokenmaxx.draft.log_path = saved;
                    self.tokenmaxx.draft.log_path_dirty = false;
                } else {
                    self.tokenmaxx.draft.log_path = value;
                    self.tokenmaxx.draft.log_path_dirty = true;
                }
            }
            _ => {}
        }
        self.set_status("Change pending — press s to save", false);
    }

    fn cancel_edit(&mut self) {
        self.edit = None;
        self.edit_setting = None;
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.cancel_edit(),
            KeyCode::Enter => self.commit_edit(),
            KeyCode::Backspace => {
                if edit.cursor > 0 {
                    let start = byte_index(&edit.value, edit.cursor - 1);
                    let end = byte_index(&edit.value, edit.cursor);
                    edit.value.replace_range(start..end, "");
                    edit.cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if edit.cursor < edit.value.chars().count() {
                    let start = byte_index(&edit.value, edit.cursor);
                    let end = byte_index(&edit.value, edit.cursor + 1);
                    edit.value.replace_range(start..end, "");
                }
            }
            KeyCode::Left => edit.cursor = edit.cursor.saturating_sub(1),
            KeyCode::Right => {
                edit.cursor = (edit.cursor + 1).min(edit.value.chars().count());
            }
            KeyCode::Home => edit.cursor = 0,
            KeyCode::End => edit.cursor = edit.value.chars().count(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let index = byte_index(&edit.value, edit.cursor);
                edit.value.insert(index, character);
                edit.cursor += 1;
            }
            _ => {}
        }
    }

    fn refresh(&mut self) -> Vec<Command> {
        let mut commands = Vec::new();
        if self.quota.refreshing {
            self.set_status("Quota refresh already running", false);
        } else {
            self.quota.refreshing = true;
            commands.push(Command::RefreshQuota);
        }
        if self.tab == Tab::Tokenmaxx {
            commands.extend(self.request_timer());
            commands.extend(self.request_log());
        }
        commands
    }

    fn refresh_timer_and_log(&mut self) -> Vec<Command> {
        let mut commands = self.request_timer();
        commands.extend(self.request_log());
        commands
    }

    fn request_timer(&mut self) -> Vec<Command> {
        if self.tokenmaxx.status_pending {
            return Vec::new();
        }
        self.tokenmaxx.status_pending = true;
        vec![Command::RefreshTimer]
    }

    fn request_log(&mut self) -> Vec<Command> {
        if self.tokenmaxx.log_pending {
            return Vec::new();
        }
        self.tokenmaxx.log_pending = true;
        vec![Command::RefreshLog]
    }

    fn request_save(&mut self) -> Vec<Command> {
        if self.tokenmaxx.save_pending {
            self.set_status("Save and apply already in progress", false);
            return Vec::new();
        }
        match self.build_config() {
            Ok(config) => {
                self.tokenmaxx.save_pending = true;
                self.set_status("Saving and applying tokenmaxx…", false);
                vec![Command::Apply(Box::new(config))]
            }
            Err(error) => {
                self.set_status(error, true);
                Vec::new()
            }
        }
    }

    fn request_run_now(&mut self) -> Vec<Command> {
        if self.tokenmaxx.run_pending {
            self.set_status("Run now already in progress", false);
            return Vec::new();
        }
        self.tokenmaxx.run_pending = true;
        self.set_status("Starting tokenmaxx.service…", false);
        vec![Command::RunNow]
    }

    fn clamp_quota_selection(&mut self) {
        self.quota.selected = self
            .quota
            .selected
            .min(self.quota.accounts.len().saturating_sub(1));
    }
}

fn draft_from_config(config: &Config, paths: &Paths) -> Draft {
    Draft {
        enabled: config.tokenmaxx.enabled,
        auto: config.tokenmaxx.auto,
        anchor: config.tokenmaxx.anchor.clone(),
        interval_hours: config.tokenmaxx.interval_hours.to_string(),
        model: config.tokenmaxx.model.clone(),
        log_path: paths.shorten(&config.log_path(paths)),
        log_path_dirty: false,
    }
}

fn yes_no(value: bool) -> String {
    if value { "yes" } else { "no" }.to_string()
}

fn non_empty(value: &str) -> String {
    if value.trim().is_empty() {
        "—".to_string()
    } else {
        value.to_string()
    }
}

fn pending_or_unavailable(pending: bool) -> String {
    if pending {
        "querying…".to_string()
    } else {
        "unavailable".to_string()
    }
}

fn clamp_step(current: usize, delta: i32, max: usize) -> usize {
    let next = i64::try_from(current).unwrap_or(i64::MAX) + i64::from(delta);
    next.clamp(0, i64::try_from(max).unwrap_or(i64::MAX)) as usize
}

fn byte_index(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map_or(value.len(), |(index, _)| index)
}

/// Removes credentials from URLs before they are displayed.
pub fn safe_proxy_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "—".to_string();
    }
    let end = trimmed.find(['?', '#']).unwrap_or(trimmed.len());
    let mut text = trimmed[..end].to_string();
    if let Some(scheme) = text.find("://") {
        let start = scheme + 3;
        let authority_end = text[start..]
            .find('/')
            .map_or(text.len(), |offset| start + offset);
        if let Some(at) = text[start..authority_end].rfind('@').map(|at| start + at) {
            text.replace_range(start..=at, "");
        }
    }
    text
}

/// Redacts bearer tokens and API-key-shaped words from user-visible text.
pub fn sanitize(text: &str) -> String {
    let mut words = Vec::new();
    let mut redact_next = false;
    for word in text.split_whitespace() {
        if redact_next {
            redact_next = false;
            words.push("[redacted]".to_string());
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if lower == "bearer" {
            words.push(word.to_string());
            redact_next = true;
        } else if lower.starts_with("bearer=")
            || lower.starts_with("api_key=")
            || lower.starts_with("apikey=")
            || lower.starts_with("token=")
            || lower.starts_with("access_token=")
            || (lower.starts_with("sk-") && word.len() > 8)
        {
            words.push("[redacted]".to_string());
        } else {
            words.push(word.to_string());
        }
    }
    words.join(" ")
}

/// Validates a config snapshot and then delegates to the systemd worker.
pub(crate) fn apply_validated(
    config: &Config,
    paths: &Paths,
    executable: &Path,
    runner: &impl Runner,
) -> Result<(), String> {
    config.validate()?;
    systemd::apply(config, paths, executable, runner)
}

#[cfg(test)]
pub(crate) fn sample_account(
    file: &str,
    provider: &str,
    email: &str,
    used: Option<f64>,
    error: Option<&str>,
) -> Account {
    use serde_json::json;

    use crate::quota::Window;

    Account {
        file: file.into(),
        provider: provider.into(),
        email: Some(email.into()),
        plan: Some("plus".into()),
        account_id: Some(format!("acct-{provider}")),
        allowed: error.is_none().then_some(true),
        limit_reached: Some(false),
        windows: vec![Window {
            label: "5h".into(),
            used_percent: used,
            reset_at: Some(json!(1_700_000_000i64)),
            reset_after: Some(3600.0),
        }],
        error: error.map(str::to_string),
        raw: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crossterm::event::KeyEvent;

    use super::*;
    use crate::{quota::Account, test_support::Sandbox};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn test_paths() -> Paths {
        Paths::new(
            PathBuf::from("/home/cpaq-test"),
            Some(PathBuf::from("/home/cpaq-test/.config")),
            Some(PathBuf::from("/home/cpaq-test/.local/state")),
        )
    }

    fn test_app() -> App {
        App::new(test_paths(), Config::default(), Args::default())
    }

    fn quota_message(accounts: Vec<Account>) -> WorkerMessage {
        WorkerMessage::Quota {
            auth_dir: PathBuf::from("/home/cpaq-test/.config/cli-proxy-api/auths"),
            accounts,
            at: 1_700_000_000,
        }
    }

    #[test]
    fn auto_key_saves_only_toggle_and_does_not_apply_units() {
        let sandbox = Sandbox::default();
        let paths = Paths::new(sandbox.0.clone(), None, None);
        let mut app = App::new(paths.clone(), Config::default(), Args::default());
        app.tab = Tab::Tokenmaxx;
        app.tokenmaxx.draft.model = "unsaved-model".into();
        assert!(app.handle_key(key(KeyCode::Char('a'))).is_empty());
        assert!(!Config::load(&paths.config_file()).unwrap().tokenmaxx.auto);
        assert_eq!(
            Config::load(&paths.config_file()).unwrap().tokenmaxx.model,
            Config::default().tokenmaxx.model
        );
        assert_eq!(app.tokenmaxx.draft.model, "unsaved-model");
        assert!(!app.build_config().unwrap().tokenmaxx.auto);
        assert!(!paths.unit_dir().exists());
        app.handle_key(key(KeyCode::Char('a')));
        assert!(Config::load(&paths.config_file()).unwrap().tokenmaxx.auto);
        assert!(app.build_config().unwrap().tokenmaxx.auto);
    }

    #[test]
    fn auto_toggle_save_failure_keeps_previous_value() {
        let sandbox = Sandbox::default();
        let paths = Paths::new(sandbox.0.clone(), Some(sandbox.0.join("not-a-dir")), None);
        std::fs::write(&paths.config_home, "blocked").unwrap();
        let mut app = App::new(paths, Config::default(), Args::default());
        app.tab = Tab::Tokenmaxx;
        app.handle_key(key(KeyCode::Char('a')));
        assert!(app.status_error);
        assert!(app.build_config().unwrap().tokenmaxx.auto);
    }

    #[test]
    fn app_new_and_first_frame_state_do_no_background_work() {
        let mut app = test_app();
        assert!(app.quota.auth_dir.is_none());
        assert!(app.quota.accounts.is_empty());
        assert!(app.quota.last_refresh.is_none());
        assert!(!app.quota.refreshing);
        assert!(app.tokenmaxx.timer.is_none());
        assert!(app.tokenmaxx.log_lines.is_empty());

        let commands = app.startup_commands();
        assert_eq!(commands.len(), 3);
        assert!(matches!(commands[0], Command::RefreshQuota));
        assert!(matches!(commands[1], Command::RefreshTimer));
        assert!(matches!(commands[2], Command::RefreshLog));
        assert!(app.quota.refreshing);
        assert!(app.tokenmaxx.status_pending);
        assert!(app.tokenmaxx.log_pending);
    }

    #[test]
    fn quota_navigation_clamps_and_keeps_selection_valid() {
        let mut app = test_app();
        app.apply_message(quota_message(vec![
            sample_account("a.json", "codex", "a@example.org", Some(10.0), None),
            sample_account("b.json", "codex", "b@example.org", Some(20.0), None),
            sample_account("c.json", "claude", "c@example.org", Some(30.0), None),
        ]));
        app.quota.selected = 2;
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.quota.selected, 2);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.quota.selected, 1);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.quota.selected, 0);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.quota.selected, 0);
    }

    #[test]
    fn tab_help_details_escape_and_quit_keys() {
        let mut app = test_app();
        app.apply_message(quota_message(vec![sample_account(
            "a.json",
            "codex",
            "a@example.org",
            Some(10.0),
            None,
        )]));

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.overlay, Overlay::Details);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.overlay, Overlay::None);

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Tokenmaxx);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Quotas);

        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.overlay, Overlay::Help);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.overlay, Overlay::None);

        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn ctrl_c_quits_even_while_editing() {
        let mut app = test_app();
        app.tab = Tab::Tokenmaxx;
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.edit.is_some());
        app.handle_key(ctrl('c'));
        assert!(app.quit);
    }

    #[test]
    fn tokenmaxx_toggles_and_edits_fields_with_commit_and_cancel() {
        let mut app = test_app();
        app.tab = Tab::Tokenmaxx;

        app.handle_key(key(KeyCode::Enter));
        assert!(!app.tokenmaxx.draft.enabled);
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.tokenmaxx.draft.enabled);

        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.is_editing(Setting::Anchor));
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Backspace));
        }
        for c in "06:30".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.is_editing(Setting::Anchor));
        assert_eq!(app.tokenmaxx.draft.anchor, "06:30");

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.tokenmaxx.selected, 2);
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Char('7')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.tokenmaxx.draft.interval_hours, "7");

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.tokenmaxx.selected, 3);
        let model = app.tokenmaxx.draft.model.clone();
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.tokenmaxx.draft.model, model);
        assert!(!app.is_editing(Setting::Model));

        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.tokenmaxx.draft.log_path_dirty);

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.tokenmaxx.selected, 5);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.edit.is_none());
        assert!(app.status.contains("read-only"));
    }

    #[test]
    fn invalid_draft_is_rejected_before_save_dispatch() {
        let mut app = test_app();
        app.tab = Tab::Tokenmaxx;

        app.tokenmaxx.draft.anchor = "24:00".into();
        assert!(app.handle_key(key(KeyCode::Char('s'))).is_empty());
        assert!(!app.tokenmaxx.save_pending);
        assert!(app.status_error);
        assert!(app.status.contains("anchor"));

        app.tokenmaxx.draft.anchor = "05:00".into();
        app.tokenmaxx.draft.interval_hours = "not-a-number".into();
        assert!(app.handle_key(key(KeyCode::Char('s'))).is_empty());
        assert!(!app.tokenmaxx.save_pending);
        assert!(app.status_error);

        app.tokenmaxx.draft.interval_hours = "5".into();
        app.tokenmaxx.draft.model = "  ".into();
        assert!(app.handle_key(key(KeyCode::Char('s'))).is_empty());
        assert!(!app.tokenmaxx.save_pending);
    }

    #[test]
    fn save_and_run_now_are_single_flight_background_jobs() {
        let mut app = test_app();
        app.tab = Tab::Tokenmaxx;

        let commands = app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(commands.len(), 1);
        let config = match commands.into_iter().next().unwrap() {
            Command::Apply(config) => config,
            _ => panic!("expected apply command"),
        };
        assert_eq!(config.tokenmaxx.anchor, "05:00");
        assert!(app.tokenmaxx.save_pending);
        assert!(app.handle_key(key(KeyCode::Char('s'))).is_empty());
        assert!(app.status.contains("progress"));

        let commands = app.apply_message(WorkerMessage::Applied {
            config,
            result: Ok(()),
        });
        assert!(!app.tokenmaxx.save_pending);
        assert!(commands.iter().any(|c| matches!(c, Command::RefreshTimer)));
        assert!(commands.iter().any(|c| matches!(c, Command::RefreshLog)));

        app.apply_message(WorkerMessage::Timer(Ok(TimerStatus::default())));
        app.apply_message(WorkerMessage::Log {
            lines: Vec::new(),
            error: None,
        });

        let commands = app.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(commands.first(), Some(Command::RunNow)));
        assert!(app.tokenmaxx.run_pending);
        assert!(app.handle_key(key(KeyCode::Char('x'))).is_empty());
        let commands = app.apply_message(WorkerMessage::RunNow(Ok(())));
        assert!(!app.tokenmaxx.run_pending);
        assert!(commands.iter().any(|c| matches!(c, Command::RefreshTimer)));
    }

    #[test]
    fn refresh_is_single_flight_and_tokenmaxx_refresh_includes_status() {
        let mut app = test_app();
        assert_eq!(app.handle_key(key(KeyCode::Char('r'))).len(), 1);
        assert!(app.quota.refreshing);
        assert!(app.handle_key(key(KeyCode::Char('r'))).is_empty());

        app.apply_message(quota_message(vec![sample_account(
            "a.json",
            "codex",
            "a@example.org",
            Some(10.0),
            None,
        )]));
        assert!(!app.quota.refreshing);
        app.tab = Tab::Tokenmaxx;
        let commands = app.handle_key(key(KeyCode::Char('r')));
        assert!(commands.iter().any(|c| matches!(c, Command::RefreshQuota)));
        assert!(commands.iter().any(|c| matches!(c, Command::RefreshTimer)));
        assert!(commands.iter().any(|c| matches!(c, Command::RefreshLog)));
    }

    #[test]
    fn worker_messages_update_quota_timer_and_log_state() {
        let mut app = test_app();
        app.quota.refreshing = true;
        app.apply_message(quota_message(vec![
            sample_account("c.json", "claude", "c@example.org", Some(30.0), None),
            sample_account("a.json", "codex", "a@example.org", Some(10.0), None),
        ]));
        assert_eq!(app.quota.accounts[0].provider, "claude");
        assert_eq!(app.quota.accounts[1].provider, "codex");
        assert_eq!(app.quota.last_refresh, Some(1_700_000_000));
        assert!(app.quota.auth_dir.is_some());
        assert!(!app.quota.refreshing);

        app.quota.refreshing = true;
        app.apply_message(WorkerMessage::QuotaError {
            auth_dir: None,
            error: "Auth directory not found. Tried: /a, /b".into(),
            at: 1_700_000_001,
        });
        assert!(app.quota.error.as_deref().unwrap().contains("Tried"));
        assert_eq!(app.quota.accounts.len(), 2);
        assert!(app.status_error);
        assert!(!app.quota.refreshing);

        app.tokenmaxx.status_pending = true;
        app.apply_message(WorkerMessage::Timer(Ok(TimerStatus {
            enabled: "enabled".into(),
            active: "active".into(),
            next_run: "next".into(),
            last_run: "last".into(),
        })));
        assert!(!app.tokenmaxx.status_pending);
        assert_eq!(app.tokenmaxx.timer.as_ref().unwrap().next_run, "next");

        app.tokenmaxx.status_pending = true;
        app.apply_message(WorkerMessage::Timer(Err(
            "Authorization: Bearer topsecret".into()
        )));
        assert!(!app.tokenmaxx.status_pending);
        assert!(
            !app.tokenmaxx
                .timer_error
                .as_deref()
                .unwrap()
                .contains("topsecret")
        );

        app.tokenmaxx.log_pending = true;
        app.apply_message(WorkerMessage::Log {
            lines: vec!["line one".into(), "line two".into()],
            error: None,
        });
        assert!(!app.tokenmaxx.log_pending);
        assert_eq!(app.tokenmaxx.log_lines.len(), 2);
    }

    #[test]
    fn tokenmaxx_view_values_and_read_only_fields() {
        let mut app = test_app();
        app.apply_message(WorkerMessage::Timer(Ok(TimerStatus {
            enabled: "enabled".into(),
            active: "inactive".into(),
            next_run: "Tue 05:00".into(),
            last_run: "Mon 05:00".into(),
        })));
        assert_eq!(app.setting_value(Setting::Enabled), "yes");
        assert_eq!(app.setting_value(Setting::Anchor), "05:00");
        assert_eq!(app.setting_value(Setting::Proxy), "http://127.0.0.1:8317");
        assert!(app.setting_value(Setting::Timer).contains("inactive"));
        assert_eq!(app.setting_value(Setting::NextRun), "Tue 05:00");
        assert_eq!(app.setting_value(Setting::LastRun), "Mon 05:00");
        assert!(!Setting::Proxy.editable());
        assert!(Setting::Anchor.editable());
    }

    #[test]
    fn config_builder_preserves_proxy_auth_and_pings() {
        let mut app = test_app();
        app.base_config.proxy.api_key = "fixture".into();
        app.base_config.tokenmaxx.pings_per_account = 3;
        app.base_config.tokenmaxx.probe_minutes = 0;
        app.tokenmaxx.draft.anchor = "06:30".into();
        app.tokenmaxx.draft.interval_hours = "7".into();
        app.tokenmaxx.draft.model = "gpt-test".into();
        app.tokenmaxx.draft.enabled = false;

        let config = app.build_config().unwrap();
        assert_eq!(config.proxy.api_key, "fixture");
        assert_eq!(config.tokenmaxx.pings_per_account, 3);
        assert_eq!(config.tokenmaxx.probe_minutes, 0);
        assert_eq!(config.tokenmaxx.anchor, "06:30");
        assert_eq!(config.tokenmaxx.interval_hours, 7);
        assert_eq!(config.tokenmaxx.model, "gpt-test");
        assert!(!config.tokenmaxx.enabled);
        assert!(config.tokenmaxx.log_path.is_none());
    }

    #[test]
    fn invalid_config_is_rejected_before_any_systemd_or_file_write() {
        struct FakeRunner {
            calls: Mutex<Vec<String>>,
        }
        impl Runner for FakeRunner {
            fn run(&self, args: &[&str]) -> Result<String, String> {
                self.calls.lock().unwrap().push(args.join(" "));
                Ok(String::new())
            }
        }

        let sandbox = Sandbox::default();
        let paths = Paths::new(
            sandbox.0.clone(),
            Some(sandbox.0.join("config")),
            Some(sandbox.0.join("state")),
        );
        let executable = sandbox.0.join("bin/cpaq");
        let runner = FakeRunner {
            calls: Mutex::new(Vec::new()),
        };
        let mut config = Config::default();
        config.tokenmaxx.anchor = "99:99".into();

        assert!(apply_validated(&config, &paths, &executable, &runner).is_err());
        assert!(!paths.config_file().exists());
        assert!(!paths.unit_dir().exists());
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn proxy_urls_are_safe_to_display_and_credentials_are_redacted() {
        assert_eq!(
            safe_proxy_url("http://user:pass@127.0.0.1:8317/v1?api_key=secret"),
            "http://127.0.0.1:8317/v1"
        );
        assert_eq!(
            safe_proxy_url("http://127.0.0.1:8317"),
            "http://127.0.0.1:8317"
        );

        let text = sanitize("Authorization: Bearer topsecret and sk-abcdefghijkl");
        assert!(text.contains("[redacted]"));
        assert!(!text.contains("topsecret"));
        assert!(!text.contains("abcdefghijkl"));
    }
}
