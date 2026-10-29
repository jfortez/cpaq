use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub auth: AuthConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_dir: Option<PathBuf>,
    pub proxy: ProxyConfig,
    pub tokenmaxx: TokenmaxxConfig,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AuthConfig {
    pub dir: Option<PathBuf>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub base_url: String,
    pub api_key: String,
    pub timeout_secs: u64,
}
impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8317".into(),
            api_key: String::new(),
            timeout_secs: 30,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TokenmaxxConfig {
    pub enabled: bool,
    #[serde(default = "default_auto")]
    pub auto: bool,
    pub anchor: String,
    pub interval_hours: u32,
    pub probe_minutes: u32,
    pub model: String,
    pub pings_per_account: usize,
    pub log_path: Option<PathBuf>,
}
fn default_auto() -> bool {
    true
}

impl Default for TokenmaxxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto: default_auto(),
            anchor: "05:00".into(),
            interval_hours: 5,
            probe_minutes: 15,
            model: "gpt-6-luna".into(),
            pings_per_account: 1,
            log_path: None,
        }
    }
}
impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let config: Self = toml::from_str(&text)
                    .map_err(|_| "Invalid config.toml (check TOML syntax)".to_string())?;
                config.validate()?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => Err("Cannot read config.toml".into()),
        }
    }
    pub fn auth_dir(&self) -> Option<&Path> {
        self.auth.dir.as_deref().or(self.auth_dir.as_deref())
    }
    pub fn log_path(&self, paths: &Paths) -> PathBuf {
        self.tokenmaxx
            .log_path
            .as_deref()
            .map_or_else(|| paths.log_file(), |p| paths.expand(p))
    }
    pub fn validate(&self) -> Result<(), String> {
        let bytes = self.tokenmaxx.anchor.as_bytes();
        if bytes.len() != 5
            || bytes[2] != b':'
            || ![bytes[0], bytes[1], bytes[3], bytes[4]]
                .iter()
                .all(u8::is_ascii_digit)
            || self.tokenmaxx.anchor[..2].parse::<u8>().unwrap_or(255) > 23
            || self.tokenmaxx.anchor[3..].parse::<u8>().unwrap_or(255) > 59
        {
            return Err("anchor must be HH:MM (00:00–23:59)".into());
        }
        if self.tokenmaxx.interval_hours == 0
            || self.tokenmaxx.pings_per_account == 0
            || self.proxy.timeout_secs == 0
        {
            return Err(
                "interval_hours, pings_per_account and timeout_secs must be positive".into(),
            );
        }
        if self.tokenmaxx.model.trim().is_empty() {
            return Err("model must not be empty".into());
        }
        Ok(())
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let text =
            toml::to_string_pretty(self).map_err(|_| "Cannot serialize config".to_string())?;
        atomic_write(path, text.as_bytes()).map_err(|_| "Cannot save config.toml atomically".into())
    }
    pub fn proxy_credentials(&self, paths: &Paths) -> Result<ProxyConfig, String> {
        let mut proxy = self.proxy.clone();
        if proxy.api_key.is_empty()
            && let Ok(text) = fs::read_to_string(paths.home.join(".pi/agent/cliproxyapi.json"))
            && let Ok(value) =
                serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}'))
        {
            proxy.api_key = value["apiKey"].as_str().unwrap_or_default().into();
            if self.proxy.base_url == ProxyConfig::default().base_url
                && let Some(url) = value["baseUrl"].as_str().filter(|s| !s.is_empty())
            {
                proxy.base_url = url.into();
            }
        }
        if proxy.api_key.is_empty() {
            return Err(
                "No API key: configure [proxy].api_key or ~/.pi/agent/cliproxyapi.json".into(),
            );
        }
        if !(proxy.base_url.starts_with("http://") || proxy.base_url.starts_with("https://")) {
            return Err("Proxy base URL must use http:// or https://".into());
        }
        Ok(proxy)
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Sandbox;
    #[test]
    fn auto_defaults_true_for_missing_legacy_and_empty_config_and_persists_false() {
        let sandbox = Sandbox::default();
        let file = sandbox.0.join("config.toml");
        for config in [
            Config::load(&file).unwrap(),
            toml::from_str::<Config>("[tokenmaxx]\nenabled=true").unwrap(),
        ] {
            let value: toml::Value = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(
                value["tokenmaxx"]
                    .get("auto")
                    .and_then(toml::Value::as_bool),
                Some(true)
            );
        }
        let config: Config = toml::from_str("[tokenmaxx]\nauto=false").unwrap();
        config.save(&file).unwrap();
        let value: toml::Value =
            toml::from_str(&toml::to_string(&Config::load(&file).unwrap()).unwrap()).unwrap();
        assert_eq!(value["tokenmaxx"]["auto"].as_bool(), Some(false));
    }
    #[test]
    fn probe_interval_defaults_for_legacy_config_and_zero_round_trips() {
        let sandbox = Sandbox::default();
        let path = sandbox.0.join("config.toml");
        assert_eq!(Config::load(&path).unwrap().tokenmaxx.probe_minutes, 15);
        let mut config: Config = toml::from_str("[tokenmaxx]\ninterval_hours=3").unwrap();
        assert_eq!(config.tokenmaxx.probe_minutes, 15);
        config.tokenmaxx.probe_minutes = 0;
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().tokenmaxx.probe_minutes, 0);
        assert_eq!(Config::load(&path).unwrap().tokenmaxx.interval_hours, 3);
    }

    #[test]
    fn config_round_trip_and_private_atomic_save() {
        use std::os::unix::fs::PermissionsExt;
        let s = Sandbox::default();
        let file = s.0.join("cpaq/config.toml");
        let c = Config::default();
        c.save(&file).unwrap();
        assert_eq!(Config::load(&file).unwrap().tokenmaxx.anchor, "05:00");
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[test]
    fn invalid_save_leaves_previous_config_unchanged() {
        let s = Sandbox::default();
        let file = s.0.join("config.toml");
        let mut c = Config::default();
        c.save(&file).unwrap();
        let before = fs::read(&file).unwrap();
        for anchor in ["24:00", "05:60", "5:00", "a💥"] {
            c.tokenmaxx.anchor = anchor.into();
            assert!(c.save(&file).is_err());
            assert_eq!(fs::read(&file).unwrap(), before);
        }
    }
    #[test]
    fn missing_config_defaults_unknown_keys_and_alias() {
        let s = Sandbox::default();
        assert!(Config::load(&s.0.join("absent")).is_ok());
        let c: Config =
            toml::from_str("unknown = true\nauth_dir = '/legacy'\n[auth]\ndir = '/new'").unwrap();
        assert_eq!(c.auth_dir(), Some(Path::new("/new")));
    }
    #[test]
    fn proxy_fallback_uses_injected_home() {
        let s = Sandbox::default();
        let paths = Paths::new(s.0.clone(), None, None);
        let file = s.0.join(".pi/agent/cliproxyapi.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            file,
            r#"{"baseUrl":"http://localhost:1234","apiKey":"fixture"}"#,
        )
        .unwrap();
        assert_eq!(
            Config::default()
                .proxy_credentials(&paths)
                .unwrap()
                .base_url,
            "http://localhost:1234"
        );
    }
}
