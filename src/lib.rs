pub mod auth;
pub mod auto;
pub mod cli;
pub mod config;
pub mod docker;
pub mod paths;
pub mod providers;
pub mod quota;
pub mod systemd;
pub mod time;
pub mod tokenmaxx;
pub mod tui;

#[cfg(test)]
pub mod test_support {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    pub struct Sandbox(pub PathBuf);
    impl Default for Sandbox {
        fn default() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/test-state")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
