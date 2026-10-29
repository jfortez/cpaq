use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub state_home: PathBuf,
    pub env_auth: Option<PathBuf>,
    pub docker_container: String,
    pub container_auth_dir: String,
}

impl Paths {
    pub fn from_env() -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .ok_or("HOME is not set")?;
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let state = std::env::var_os("XDG_STATE_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let mut paths = Self::new(home, config, state);
        paths.env_auth = std::env::var_os("CPA_AUTH_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        paths.docker_container =
            std::env::var("CPA_DOCKER_CONTAINER").unwrap_or_else(|_| "cli-proxy-api".into());
        paths.container_auth_dir = std::env::var("CPA_CONTAINER_AUTH_DIR")
            .unwrap_or_else(|_| "/root/.cli-proxy-api".into());
        Ok(paths)
    }
    pub fn new(home: PathBuf, config: Option<PathBuf>, state: Option<PathBuf>) -> Self {
        Self {
            config_home: config.unwrap_or_else(|| home.join(".config")),
            state_home: state.unwrap_or_else(|| home.join(".local/state")),
            home,
            env_auth: None,
            docker_container: "cli-proxy-api".into(),
            container_auth_dir: "/root/.cli-proxy-api".into(),
        }
    }
    pub fn config_file(&self) -> PathBuf {
        self.config_home.join("cpaq/config.toml")
    }
    pub fn log_file(&self) -> PathBuf {
        self.state_home.join("tokenmaxx/ping.log")
    }
    pub fn auto_state_file(&self) -> PathBuf {
        self.state_home.join("tokenmaxx/auto.json")
    }
    pub fn unit_dir(&self) -> PathBuf {
        self.config_home.join("systemd/user")
    }
    pub fn expand(&self, path: &Path) -> PathBuf {
        if path == Path::new("~") {
            return self.home.clone();
        }
        path.strip_prefix("~/")
            .map_or_else(|_| path.to_owned(), |p| self.home.join(p))
    }
    pub fn shorten(&self, path: &Path) -> String {
        path.strip_prefix(&self.home).map_or_else(
            |_| path.display().to_string(),
            |p| format!("~/{}", p.display()),
        )
    }
    pub fn auth_candidates(&self, cli: Option<&Path>, config: Option<&Path>) -> Vec<PathBuf> {
        if let Some(path) = cli.or(self.env_auth.as_deref()).or(config) {
            return vec![self.expand(path)];
        }
        vec![
            self.config_home.join("cli-proxy-api/auths"),
            self.config_home.join("cpa/auths"),
        ]
    }
    pub fn resolve_auth(
        &self,
        cli: Option<&Path>,
        config: Option<&Path>,
    ) -> Result<PathBuf, String> {
        let candidates = self.auth_candidates(cli, config);
        candidates
            .iter()
            .find(|p| p.is_dir())
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Auth directory not found. Tried: {}. Set --auth-dir or CPA_AUTH_DIR.",
                    candidates
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Sandbox;
    #[test]
    fn explicit_auth_precedence_does_not_silently_fall_through() {
        let s = Sandbox::default();
        let mut p = Paths::new(s.0.clone(), None, None);
        p.env_auth = Some(s.0.join("env"));
        assert_eq!(
            p.auth_candidates(Some(Path::new("cli")), Some(Path::new("config"))),
            vec![PathBuf::from("cli")]
        );
        assert_eq!(
            p.auth_candidates(None, Some(Path::new("config"))),
            vec![s.0.join("env")]
        );
        assert!(p.resolve_auth(Some(Path::new("absent")), None).is_err());
        p.env_auth = None;
        assert_eq!(
            p.auth_candidates(None, Some(Path::new("~/auth"))),
            vec![s.0.join("auth")]
        );
    }
    #[test]
    fn xdg_discovery_and_legacy_fallback() {
        let s = Sandbox::default();
        let p = Paths::new(
            s.0.join("home"),
            Some(s.0.join("config")),
            Some(s.0.join("state")),
        );
        std::fs::create_dir_all(p.config_home.join("cpa/auths")).unwrap();
        assert_eq!(
            p.resolve_auth(None, None).unwrap(),
            p.config_home.join("cpa/auths")
        );
        std::fs::create_dir_all(p.config_home.join("cli-proxy-api/auths")).unwrap();
        assert_eq!(
            p.resolve_auth(None, None).unwrap(),
            p.config_home.join("cli-proxy-api/auths")
        );
        assert_eq!(p.log_file(), s.0.join("state/tokenmaxx/ping.log"));
        assert_eq!(p.unit_dir(), s.0.join("config/systemd/user"));
    }
}
