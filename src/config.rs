use anyhow::{Context, Result};
use std::path::PathBuf;

/// Cross-platform configuration directory manager
pub struct ConfigManager;

/// Environment variable name for overriding the config directory (used by tests on all platforms)
pub const CONFIG_DIR_ENV: &str = "CLAUDE_CODE_SYNC_CONFIG_DIR";

impl ConfigManager {
    /// Get the main configuration directory path following platform conventions:
    /// - Linux: $XDG_CONFIG_HOME/claude-code-sync or ~/.config/claude-code-sync
    /// - macOS: ~/Library/Application Support/claude-code-sync
    /// - Windows: %APPDATA%\claude-code-sync
    pub fn config_dir() -> Result<PathBuf> {
        // Allow override via CLAUDE_CODE_SYNC_CONFIG_DIR (used by tests on all platforms)
        if let Ok(override_dir) = std::env::var(CONFIG_DIR_ENV) {
            return Ok(PathBuf::from(override_dir));
        }

        #[cfg(target_os = "linux")]
        {
            // Follow XDG Base Directory Specification
            if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
                Ok(PathBuf::from(xdg_config).join("claude-code-sync"))
            } else {
                let home = dirs::home_dir().context("Failed to get home directory")?;
                Ok(home.join(".config").join("claude-code-sync"))
            }
        }

        #[cfg(target_os = "macos")]
        {
            // Follow macOS conventions
            let home = dirs::home_dir().context("Failed to get home directory")?;
            Ok(home
                .join("Library")
                .join("Application Support")
                .join("claude-code-sync"))
        }

        #[cfg(target_os = "windows")]
        {
            // Use Windows APPDATA
            Ok(dirs::config_dir()
                .context("Failed to get Windows config directory")?
                .join("claude-code-sync"))
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            // Fallback for other platforms
            let home = dirs::home_dir().context("Failed to get home directory")?;
            Ok(home.join(".claude-code-sync"))
        }
    }

    /// Get the state file path (state.json)
    pub fn state_file_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("state.json"))
    }

    /// Get the filter config file path (config.toml)
    pub fn filter_config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    /// Get the operation history file path
    pub fn operation_history_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("operation-history.json"))
    }

    /// Get the snapshots directory path
    pub fn snapshots_dir() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("snapshots"))
    }

    /// Get the default repository clone directory
    pub fn default_repo_dir() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("repo"))
    }

    /// Get the latest conflict report path
    #[allow(dead_code)]
    pub fn conflict_report_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("latest-conflict-report.json"))
    }

    /// Get the log file path
    pub fn log_file_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("claude-code-sync.log"))
    }

    /// Get the user data file path (user_data.json)
    pub fn user_data_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("user_data.json"))
    }

    /// Get the delete-unlock window state file path (delete-unlock.json)
    pub fn delete_unlock_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("delete-unlock.json"))
    }

    /// Get the session maintenance settings file path.
    #[allow(dead_code)]
    pub fn session_maintenance_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("session-maintenance.json"))
    }

    /// Get the session maintenance lock file path.
    #[allow(dead_code)]
    pub fn session_maintenance_lock_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("session-maintenance.lock"))
    }

    /// Get the session recycle directory path.
    #[allow(dead_code)]
    pub fn session_recycle_dir() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("session-recycle"))
    }

    /// Ensure the configuration directory exists
    pub fn ensure_config_dir() -> Result<PathBuf> {
        let config_dir = Self::config_dir()?;
        std::fs::create_dir_all(&config_dir).with_context(|| {
            format!(
                "Failed to create config directory: {}",
                config_dir.display()
            )
        })?;
        Ok(config_dir)
    }

    /// Ensure the snapshots directory exists
    #[allow(dead_code)]
    pub fn ensure_snapshots_dir() -> Result<PathBuf> {
        let snapshots_dir = Self::snapshots_dir()?;
        std::fs::create_dir_all(&snapshots_dir).with_context(|| {
            format!(
                "Failed to create snapshots directory: {}",
                snapshots_dir.display()
            )
        })?;
        Ok(snapshots_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct EnvGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.original.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    #[serial]
    fn test_config_paths() {
        // Just ensure they don't panic and return valid paths
        let config_dir = ConfigManager::config_dir().unwrap();
        assert!(config_dir.to_string_lossy().contains("claude-code-sync"));

        let state_path = ConfigManager::state_file_path().unwrap();
        assert!(state_path.to_string_lossy().contains("state.json"));

        let filter_path = ConfigManager::filter_config_path().unwrap();
        assert!(filter_path.to_string_lossy().contains("config.toml"));

        let history_path = ConfigManager::operation_history_path().unwrap();
        assert!(history_path
            .to_string_lossy()
            .contains("operation-history.json"));

        let snapshots = ConfigManager::snapshots_dir().unwrap();
        assert!(snapshots.to_string_lossy().contains("snapshots"));

        let repo = ConfigManager::default_repo_dir().unwrap();
        assert!(repo.to_string_lossy().contains("repo"));

        let conflict = ConfigManager::conflict_report_path().unwrap();
        assert!(conflict
            .to_string_lossy()
            .contains("latest-conflict-report.json"));

        let log = ConfigManager::log_file_path().unwrap();
        assert!(log.to_string_lossy().contains("claude-code-sync.log"));

        let unlock = ConfigManager::delete_unlock_path().unwrap();
        assert!(unlock.to_string_lossy().contains("delete-unlock.json"));

        let maintenance = ConfigManager::session_maintenance_path().unwrap();
        assert_eq!(maintenance, config_dir.join("session-maintenance.json"));

        let maintenance_lock = ConfigManager::session_maintenance_lock_path().unwrap();
        assert_eq!(
            maintenance_lock,
            config_dir.join("session-maintenance.lock")
        );

        let recycle = ConfigManager::session_recycle_dir().unwrap();
        assert_eq!(recycle, config_dir.join("session-recycle"));
    }

    #[test]
    #[serial]
    #[cfg(target_os = "linux")]
    fn test_xdg_config_home_respected() {
        let _guard = EnvGuard::set("XDG_CONFIG_HOME", "/tmp/test-xdg-config");
        let config_dir = ConfigManager::config_dir().unwrap();
        assert!(config_dir
            .to_string_lossy()
            .contains("/tmp/test-xdg-config/claude-code-sync"));
    }

    #[test]
    #[serial]
    #[cfg(target_os = "macos")]
    fn test_macos_library_path() {
        let config_dir = ConfigManager::config_dir().unwrap();
        assert!(config_dir
            .to_string_lossy()
            .contains("Library/Application Support/claude-code-sync"));
    }

    #[test]
    #[serial]
    fn test_config_dir_override() {
        let _guard = EnvGuard::set(CONFIG_DIR_ENV, "/tmp/test-override");
        let config_dir = ConfigManager::config_dir().unwrap();
        assert_eq!(config_dir, PathBuf::from("/tmp/test-override"));
    }
}
