use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ConfigManager;

/// Configuration file for non-interactive initialization.
///
/// This struct can be loaded from a TOML file to initialize ccs
/// without requiring interactive prompts. Useful for automation, CI/CD, and
/// headless environments.
///
/// # Example TOML file
///
/// ```toml
/// # Required: Path to the local git repository
/// repo_path = "~/claude-history-sync"
///
/// # Optional: Remote git URL for syncing
/// remote_url = "https://github.com/user/claude-history.git"
///
/// # Optional: Clone from remote (default: false)
/// # Set to true if the repo doesn't exist locally and should be cloned
/// clone = true
///
/// # Optional: Exclude file attachments (default: false)
/// exclude_attachments = true
///
/// # Optional: Exclude conversations older than N days
/// exclude_older_than_days = 30
///
/// # Optional: Enable Git LFS for large files (default: false)
/// enable_lfs = true
///
/// # Optional: SCM backend - "git" or "mercurial" (default: "git")
/// scm_backend = "git"
///
/// # Optional: Subdirectory for storing projects (default: "projects")
/// sync_subdirectory = "claude-history"
///
/// # Optional: Use only project name for multi-device sync (default: false)
/// use_project_name_only = true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitConfig {
    /// Path to the local git repository for storing conversation history.
    pub repo_path: String,

    /// Optional remote git repository URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,

    /// Whether to clone from the remote URL (default: false).
    #[serde(default)]
    pub clone: bool,

    /// Whether to exclude file attachments (default: false).
    #[serde(default)]
    pub exclude_attachments: bool,

    /// Exclude conversations older than N days.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_older_than_days: Option<u32>,

    /// Enable Git LFS for large files (default: false).
    #[serde(default)]
    pub enable_lfs: bool,

    /// SCM backend: "git" or "mercurial" (default: "git").
    #[serde(default = "default_scm_backend")]
    pub scm_backend: String,

    /// Subdirectory within sync repo for storing projects (default: "projects").
    #[serde(default = "default_sync_subdirectory")]
    pub sync_subdirectory: String,

    /// Use only project name instead of full path (default: false).
    /// Enables multi-device sync compatibility.
    #[serde(default)]
    pub use_project_name_only: bool,
}

fn default_scm_backend() -> String {
    "git".to_string()
}

fn default_sync_subdirectory() -> String {
    "projects".to_string()
}

impl InitConfig {
    /// Load configuration from a TOML file.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read config file: {}", path.as_ref().display()))?;

        let config: InitConfig =
            toml::from_str(&content).context("Failed to parse init config file")?;

        // Validate the config
        config.validate()?;

        Ok(config)
    }

    /// Load configuration from the default location.
    ///
    /// Checks the following locations in order:
    /// 1. `CLAUDE_CODE_SYNC_INIT_CONFIG` environment variable
    /// 2. `~/.claude-code-sync-init.toml`
    /// 3. Config directory: `init.toml`
    pub fn load_default() -> Result<Option<Self>> {
        // Check environment variable first
        if let Ok(path) = std::env::var("CLAUDE_CODE_SYNC_INIT_CONFIG") {
            let path = PathBuf::from(&path);
            if path.exists() {
                log::info!(
                    "Loading init config from CLAUDE_CODE_SYNC_INIT_CONFIG: {}",
                    path.display()
                );
                return Ok(Some(Self::load(&path)?));
            }
        }

        // Check ~/.claude-code-sync-init.toml
        if let Some(home) = dirs::home_dir() {
            let home_config = home.join(".claude-code-sync-init.toml");
            if home_config.exists() {
                log::info!("Loading init config from: {}", home_config.display());
                return Ok(Some(Self::load(&home_config)?));
            }
        }

        // Check config directory
        if let Ok(config_dir) = ConfigManager::config_dir() {
            let config_path = config_dir.join("init.toml");
            if config_path.exists() {
                log::info!("Loading init config from: {}", config_path.display());
                return Ok(Some(Self::load(&config_path)?));
            }
        }

        Ok(None)
    }

    /// Validate the configuration.
    fn validate(&self) -> Result<()> {
        // Validate remote URL if provided
        if let Some(ref url) = self.remote_url {
            if !is_valid_git_url(url) {
                return Err(anyhow::anyhow!(
                    "Invalid git URL '{}'. Must start with 'https://', 'http://', 'git@', or 'ssh://'",
                    url
                ));
            }
        }

        // If clone is true, remote_url must be provided
        if self.clone && self.remote_url.is_none() {
            return Err(anyhow::anyhow!(
                "clone = true requires remote_url to be set"
            ));
        }

        // Validate SCM backend
        let backend = self.scm_backend.to_lowercase();
        if backend != "git" && backend != "mercurial" && backend != "hg" {
            return Err(anyhow::anyhow!(
                "Invalid scm_backend '{}'. Use 'git' or 'mercurial'.",
                self.scm_backend
            ));
        }

        // LFS only works with git
        if self.enable_lfs && backend != "git" {
            return Err(anyhow::anyhow!(
                "enable_lfs = true requires scm_backend = 'git'"
            ));
        }

        Ok(())
    }

    /// Convert to OnboardingConfig for use with existing initialization flow.
    pub fn to_onboarding_config(&self) -> Result<OnboardingConfig> {
        let repo_path = expand_tilde(&self.repo_path)?;

        Ok(OnboardingConfig {
            repo_path,
            remote_url: self.remote_url.clone(),
            is_cloned: self.clone,
        })
    }
}

/// Onboarding configuration result
///
/// Contains repository settings gathered from config file initialization.
/// Filter settings (exclude_attachments, etc.) are read directly from `InitConfig`.
#[derive(Debug)]
pub struct OnboardingConfig {
    /// Path to the local git repository for storing conversation history.
    pub repo_path: PathBuf,

    /// Optional remote git repository URL for syncing conversations.
    pub remote_url: Option<String>,

    /// Whether the repository should be cloned from the remote URL.
    pub is_cloned: bool,
}

/// Validate git URL format
pub fn is_valid_git_url(url: &str) -> bool {
    url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("git@")
        || url.starts_with("ssh://")
}

/// Expand tilde in path
pub fn expand_tilde(path: &str) -> Result<PathBuf> {
    if path.starts_with("~/") || path == "~" {
        let home = dirs::home_dir().context("Failed to get home directory")?;
        if path == "~" {
            Ok(home)
        } else {
            Ok(home.join(&path[2..]))
        }
    } else {
        Ok(PathBuf::from(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_git_url() {
        assert!(is_valid_git_url("https://github.com/user/repo.git"));
        assert!(is_valid_git_url("http://gitlab.com/user/repo.git"));
        assert!(is_valid_git_url("git@github.com:user/repo.git"));
        assert!(is_valid_git_url("ssh://git@github.com/user/repo.git"));
        assert!(!is_valid_git_url("invalid-url"));
        assert!(!is_valid_git_url("/local/path"));
    }

    #[test]
    fn test_expand_tilde() {
        let home = dirs::home_dir().unwrap();

        // Test tilde expansion
        let expanded = expand_tilde("~/test").unwrap();
        assert_eq!(expanded, home.join("test"));

        // Test just tilde
        let expanded = expand_tilde("~").unwrap();
        assert_eq!(expanded, home);

        // Test non-tilde path
        let expanded = expand_tilde("/absolute/path").unwrap();
        assert_eq!(expanded, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_init_config_parse_minimal() {
        let toml = r#"
            repo_path = "/tmp/test-repo"
        "#;
        let config: InitConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.repo_path, "/tmp/test-repo");
        assert!(config.remote_url.is_none());
        assert!(!config.clone);
        assert!(!config.exclude_attachments);
        assert!(!config.enable_lfs);
        assert_eq!(config.scm_backend, "git");
        assert_eq!(config.sync_subdirectory, "projects");
    }

    #[test]
    fn test_init_config_parse_full() {
        let toml = r#"
            repo_path = "~/claude-sync"
            remote_url = "https://github.com/user/repo.git"
            clone = true
            exclude_attachments = true
            exclude_older_than_days = 30
            enable_lfs = true
            scm_backend = "git"
            sync_subdirectory = "history"
        "#;
        let config: InitConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.repo_path, "~/claude-sync");
        assert_eq!(
            config.remote_url,
            Some("https://github.com/user/repo.git".to_string())
        );
        assert!(config.clone);
        assert!(config.exclude_attachments);
        assert_eq!(config.exclude_older_than_days, Some(30));
        assert!(config.enable_lfs);
        assert_eq!(config.scm_backend, "git");
        assert_eq!(config.sync_subdirectory, "history");
    }

    #[test]
    fn test_init_config_validate_clone_requires_remote() {
        let config = InitConfig {
            repo_path: "/tmp/test".to_string(),
            remote_url: None,
            clone: true,
            exclude_attachments: false,
            exclude_older_than_days: None,
            enable_lfs: false,
            scm_backend: "git".to_string(),
            sync_subdirectory: "projects".to_string(),
            use_project_name_only: false,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_init_config_validate_lfs_requires_git() {
        let config = InitConfig {
            repo_path: "/tmp/test".to_string(),
            remote_url: None,
            clone: false,
            exclude_attachments: false,
            exclude_older_than_days: None,
            enable_lfs: true,
            scm_backend: "mercurial".to_string(),
            sync_subdirectory: "projects".to_string(),
            use_project_name_only: false,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_init_config_validate_invalid_backend() {
        let config = InitConfig {
            repo_path: "/tmp/test".to_string(),
            remote_url: None,
            clone: false,
            exclude_attachments: false,
            exclude_older_than_days: None,
            enable_lfs: false,
            scm_backend: "svn".to_string(),
            sync_subdirectory: "projects".to_string(),
            use_project_name_only: false,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_init_config_to_onboarding_config() {
        let config = InitConfig {
            repo_path: "/tmp/test".to_string(),
            remote_url: Some("https://github.com/user/repo.git".to_string()),
            clone: true,
            exclude_attachments: true,
            exclude_older_than_days: Some(30),
            enable_lfs: true,
            scm_backend: "git".to_string(),
            sync_subdirectory: "projects".to_string(),
            use_project_name_only: false,
        };
        let onboarding = config.to_onboarding_config().unwrap();
        assert_eq!(onboarding.repo_path, PathBuf::from("/tmp/test"));
        assert_eq!(
            onboarding.remote_url,
            Some("https://github.com/user/repo.git".to_string())
        );
        assert!(onboarding.is_cloned);
    }
}
