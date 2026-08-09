use anyhow::{bail, Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::scm::Backend;

/// Configuration sync settings stored in FilterConfig
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSyncSettings {
    /// Enable configuration sync
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Sync settings.json
    #[serde(default = "default_true")]
    pub sync_settings: bool,

    /// Sync CLAUDE.md
    #[serde(default = "default_true")]
    pub sync_claude_md: bool,

    /// Sync hooks folder
    #[serde(default)]
    pub sync_hooks: bool,

    /// Sync plugins/skills list
    #[serde(default = "default_true")]
    pub sync_skills_list: bool,

    /// Auto-apply CLAUDE.md from the most recently updated device on pull
    #[serde(default = "default_true")]
    pub auto_apply_claude_md: bool,

    /// Push device config automatically when running push command
    #[serde(default = "default_true")]
    pub push_with_config: bool,

    /// Device name (defaults to hostname)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for ConfigSyncSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            sync_settings: true,
            sync_claude_md: true,
            sync_hooks: false,
            sync_skills_list: true,
            auto_apply_claude_md: false,
            push_with_config: true,
            device_name: None,
        }
    }
}

impl ConfigSyncSettings {
    /// Get the device name (from config or friendly system name)
    pub fn get_device_name(&self) -> String {
        if let Some(ref name) = self.device_name {
            return sanitize_device_name(name);
        }

        #[cfg(not(test))]
        {
            // Try to get friendly computer name
            if let Some(name) = get_friendly_computer_name() {
                return sanitize_device_name(&name);
            }

            // Fallback to hostname
            if let Ok(name) = hostname::get() {
                if let Some(name_str) = name.to_str() {
                    return sanitize_device_name(name_str);
                }
            }
        }

        // Fallback
        "unknown-device".to_string()
    }
}

/// Auto memory sync settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoMemorySettings {
    /// Enable auto memory sync
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for AutoMemorySettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Safety controls for automatic session maintenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMaintenanceSettings {
    /// Enable automatic maintenance actions.
    ///
    /// Defaults to on. The explicit `default` function matters for configs written
    /// before this section existed: plain `#[serde(default)]` would resolve a missing
    /// field to `false` and silently keep maintenance off for every upgrading user.
    #[serde(default = "default_maintenance_enabled")]
    pub enabled: bool,

    /// Classifier policy used to decide whether a session is eligible.
    #[serde(default = "default_maintenance_classifier")]
    pub classifier: String,

    /// Hide sessions after this many hours without activity.
    #[serde(default = "default_hide_after_hours")]
    pub hide_after_hours: u64,

    /// Move hidden sessions to recycle storage after this many days.
    #[serde(default = "default_recycle_after_days")]
    pub recycle_after_days: u64,

    /// Permanently purge recycled sessions after this many days.
    #[serde(default = "default_purge_after_days")]
    pub purge_after_days: u64,

    /// Maximum number of maintenance actions allowed in one run.
    #[serde(default = "default_max_maintenance_actions")]
    pub max_actions_per_run: usize,
}

fn default_maintenance_enabled() -> bool {
    true
}

fn default_maintenance_classifier() -> String {
    "conservative".to_string()
}

fn default_hide_after_hours() -> u64 {
    24
}

fn default_recycle_after_days() -> u64 {
    7
}

fn default_purge_after_days() -> u64 {
    30
}

fn default_max_maintenance_actions() -> usize {
    50
}

impl Default for SessionMaintenanceSettings {
    fn default() -> Self {
        Self {
            enabled: default_maintenance_enabled(),
            classifier: default_maintenance_classifier(),
            hide_after_hours: default_hide_after_hours(),
            recycle_after_days: default_recycle_after_days(),
            purge_after_days: default_purge_after_days(),
            max_actions_per_run: default_max_maintenance_actions(),
        }
    }
}

/// Sanitize device name: replace non-ASCII and special characters with `-`
fn sanitize_device_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Remove consecutive dashes and trim
    let mut result = String::new();
    let mut last_was_dash = false;
    for c in sanitized.chars() {
        if c == '-' {
            if !last_was_dash && !result.is_empty() {
                result.push(c);
                last_was_dash = true;
            }
        } else {
            result.push(c);
            last_was_dash = false;
        }
    }

    // Trim trailing dash
    result.trim_end_matches('-').to_string()
}

/// Get friendly computer name from the system
#[cfg(not(test))]
fn get_friendly_computer_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        // macOS: use scutil --get ComputerName
        let output = std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
            .ok()?;

        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout);
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Windows: use COMPUTERNAME environment variable (usually friendly name)
        if let Ok(name) = std::env::var("COMPUTERNAME") {
            if !name.is_empty() {
                return Some(name);
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Linux: try /etc/hostname or hostname command
        if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
            let name = name.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    None
}

/// Filter configuration for syncing Claude Code history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    /// Exclude projects older than N days
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_older_than_days: Option<u32>,

    /// Include only these project path patterns (glob-style)
    #[serde(default)]
    pub include_patterns: Vec<String>,

    /// Exclude these project path patterns (glob-style)
    #[serde(default)]
    pub exclude_patterns: Vec<String>,

    /// Maximum file size in bytes (default: 10MB)
    #[serde(default = "default_max_file_size")]
    pub max_file_size_bytes: u64,

    /// Exclude file attachments (images, PDFs, etc.)
    #[serde(default)]
    pub exclude_attachments: bool,

    /// Enable Git LFS for large files
    /// When enabled, files matching lfs_patterns will be stored via LFS
    #[serde(default)]
    pub enable_lfs: bool,

    /// File patterns to track with LFS (e.g., "*.jsonl", "*.png")
    /// Only used when enable_lfs is true
    #[serde(default = "default_lfs_patterns")]
    pub lfs_patterns: Vec<String>,

    /// SCM backend to use: "git" or "mercurial" (default: "git")
    #[serde(default = "default_scm_backend")]
    pub scm_backend: String,

    /// Subdirectory within sync repo to store projects (default: "projects")
    /// Useful when using an existing repo and want to store history in a specific path
    #[serde(default = "default_sync_subdirectory")]
    pub sync_subdirectory: String,

    /// Use only the project name (not full path) when syncing
    /// When enabled, stores conversations using only the project directory name
    /// instead of the full encoded path. This enables multi-device compatibility
    /// when usernames or paths differ across machines.
    /// Default: true (multi-device mode)
    #[serde(default = "default_use_project_name_only")]
    pub use_project_name_only: bool,

    /// Configuration sync settings (settings.json, CLAUDE.md, hooks, etc.)
    #[serde(default)]
    pub config_sync: ConfigSyncSettings,

    /// Auto memory sync settings (memory/ directory)
    #[serde(default)]
    pub auto_memory: AutoMemorySettings,

    /// Automatic session maintenance safety settings.
    #[serde(default)]
    pub session_maintenance: SessionMaintenanceSettings,
}

fn default_lfs_patterns() -> Vec<String> {
    vec!["*.jsonl".to_string()]
}

fn default_max_file_size() -> u64 {
    10 * 1024 * 1024 // 10MB
}

fn default_scm_backend() -> String {
    "git".to_string()
}

fn default_sync_subdirectory() -> String {
    "projects".to_string()
}

fn default_use_project_name_only() -> bool {
    true
}

impl Default for FilterConfig {
    fn default() -> Self {
        FilterConfig {
            exclude_older_than_days: None,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            max_file_size_bytes: default_max_file_size(),
            exclude_attachments: false,
            enable_lfs: false,
            lfs_patterns: default_lfs_patterns(),
            scm_backend: default_scm_backend(),
            sync_subdirectory: default_sync_subdirectory(),
            use_project_name_only: true, // Default to multi-device mode
            config_sync: ConfigSyncSettings::default(),
            auto_memory: AutoMemorySettings::default(),
            session_maintenance: SessionMaintenanceSettings::default(),
        }
    }
}

impl FilterConfig {
    /// Create a default config with no file-size limit.
    ///
    /// Useful for session scanning where we want to read all files
    /// regardless of size.
    pub fn no_size_limit() -> Self {
        FilterConfig {
            max_file_size_bytes: u64::MAX,
            ..Self::default()
        }
    }

    /// Load configuration from file
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path()?;

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

        let config: FilterConfig =
            toml::from_str(&content).context("Failed to parse config file")?;

        Ok(config)
    }

    /// Save configuration to file
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path()?;

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory: {}", parent.display())
            })?;
        }

        let content = toml::to_string_pretty(self).context("Failed to serialize config")?;

        fs::write(&config_path, content)
            .with_context(|| format!("Failed to write config file: {}", config_path.display()))?;

        Ok(())
    }

    /// Get the path to the config file
    fn config_path() -> Result<PathBuf> {
        crate::config::ConfigManager::filter_config_path()
    }

    /// Check if a file should be included based on filters
    pub fn should_include(&self, file_path: &Path) -> bool {
        // Only process .jsonl files (exclude attachments if configured)
        if self.exclude_attachments {
            if let Some(ext) = file_path.extension() {
                if ext != "jsonl" {
                    // This is an attachment (image, PDF, etc.)
                    return false;
                }
            }
        }

        // Check file size
        if let Ok(metadata) = fs::metadata(file_path) {
            if metadata.len() > self.max_file_size_bytes {
                return false;
            }
        }

        let path_str = file_path.to_string_lossy();

        // Check exclude patterns first
        if !self.exclude_patterns.is_empty() {
            for pattern in &self.exclude_patterns {
                if glob_match(pattern, &path_str) {
                    return false;
                }
            }
        }

        // Check include patterns (if any are specified)
        if !self.include_patterns.is_empty() {
            let mut matches_include = false;
            for pattern in &self.include_patterns {
                if glob_match(pattern, &path_str) {
                    matches_include = true;
                    break;
                }
            }
            if !matches_include {
                return false;
            }
        }

        // Check age filter
        if let Some(max_days) = self.exclude_older_than_days {
            if let Ok(metadata) = fs::metadata(file_path) {
                if let Ok(modified) = metadata.modified() {
                    let age = std::time::SystemTime::now()
                        .duration_since(modified)
                        .unwrap_or_default();

                    let max_age = std::time::Duration::from_secs((max_days as u64) * 24 * 60 * 60);
                    if age > max_age {
                        return false;
                    }
                }
            }
        }

        true
    }

    /// Get the configured SCM backend.
    #[allow(dead_code)]
    pub fn backend(&self) -> Result<Backend> {
        match self.scm_backend.to_lowercase().as_str() {
            "git" => Ok(Backend::Git),
            "mercurial" | "hg" => Ok(Backend::Mercurial),
            other => bail!(
                "Unknown SCM backend: '{}'. Use 'git' or 'mercurial'.",
                other
            ),
        }
    }

    /// Validate the configuration.
    ///
    /// Returns an error if LFS is enabled with a non-git backend.
    pub fn validate(&self) -> Result<()> {
        if self.enable_lfs && self.scm_backend.to_lowercase() != "git" {
            bail!(
                "Git LFS is only supported with the 'git' backend. \
                 Current backend: '{}'",
                self.scm_backend
            );
        }
        Ok(())
    }
}

/// Simple glob pattern matching
fn glob_match(pattern: &str, text: &str) -> bool {
    // Simple implementation - for production, use the `glob` crate
    if pattern.contains('*') {
        let parts: Vec<_> = pattern.split('*').collect();
        if parts.len() == 2 {
            text.starts_with(parts[0]) && text.ends_with(parts[1])
        } else {
            // Simplified multi-wildcard support
            let mut pos = 0;
            for (i, part) in parts.iter().enumerate() {
                if part.is_empty() {
                    continue;
                }
                if i == 0 {
                    if !text[pos..].starts_with(part) {
                        return false;
                    }
                    pos += part.len();
                } else if i == parts.len() - 1 {
                    return text[pos..].ends_with(part);
                } else if let Some(idx) = text[pos..].find(part) {
                    pos += idx + part.len();
                } else {
                    return false;
                }
            }
            true
        }
    } else {
        text.contains(pattern)
    }
}

/// Update the filter configuration
#[allow(clippy::too_many_arguments)]
pub fn update_config(
    exclude_older_than: Option<u32>,
    include_projects: Option<String>,
    exclude_projects: Option<String>,
    exclude_attachments: Option<bool>,
    enable_lfs: Option<bool>,
    lfs_patterns: Option<String>,
    scm_backend: Option<String>,
    sync_subdirectory: Option<String>,
    use_project_name_only: Option<bool>,
) -> Result<()> {
    let mut config = FilterConfig::load()?;

    if let Some(days) = exclude_older_than {
        config.exclude_older_than_days = Some(days);
        println!(
            "{}",
            format!("Set exclude_older_than_days to {days} days").green()
        );
    }

    if let Some(includes) = include_projects {
        config.include_patterns = includes
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        println!(
            "{}",
            format!("Set include patterns: {:?}", config.include_patterns).green()
        );
    }

    if let Some(excludes) = exclude_projects {
        config.exclude_patterns = excludes
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        println!(
            "{}",
            format!("Set exclude patterns: {:?}", config.exclude_patterns).green()
        );
    }

    if let Some(exclude_att) = exclude_attachments {
        config.exclude_attachments = exclude_att;
        println!("{}", format!("Exclude attachments: {exclude_att}").green());
    }

    if let Some(lfs) = enable_lfs {
        config.enable_lfs = lfs;
        println!(
            "{}",
            format!("Git LFS: {}", if lfs { "enabled" } else { "disabled" }).green()
        );
    }

    if let Some(patterns) = lfs_patterns {
        config.lfs_patterns = patterns
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        println!(
            "{}",
            format!("Set LFS patterns: {:?}", config.lfs_patterns).green()
        );
    }

    if let Some(backend) = scm_backend {
        let backend_lower = backend.to_lowercase();
        if backend_lower != "git" && backend_lower != "mercurial" && backend_lower != "hg" {
            bail!(
                "Invalid SCM backend: '{}'. Use 'git' or 'mercurial'.",
                backend
            );
        }
        config.scm_backend = backend_lower;
        println!(
            "{}",
            format!("Set SCM backend: {}", config.scm_backend).green()
        );
    }

    if let Some(subdir) = sync_subdirectory {
        let subdir_trimmed = subdir.trim().to_string();
        if subdir_trimmed.is_empty() {
            bail!("Sync subdirectory cannot be empty");
        }
        config.sync_subdirectory = subdir_trimmed;
        println!(
            "{}",
            format!("Set sync subdirectory: {}", config.sync_subdirectory).green()
        );
    }

    if let Some(project_name_only) = use_project_name_only {
        // Check if mode is actually changing
        if config.use_project_name_only != project_name_only {
            println!();
            println!("{}", "⚠️  同步模式切换警告".yellow().bold());
            println!("{}", "─".repeat(50).dimmed());

            if project_name_only {
                // Switching to multi-device mode
                println!(
                    "{}",
                    "正在从「单设备备份」切换到「多设备同步」模式".yellow()
                );
                println!();
                println!("影响：");
                println!(
                    "  {} 新推送的文件将使用项目名格式（如 {}）",
                    "•".cyan(),
                    "n8n-workflow/".green()
                );
                println!(
                    "  {} 已有的完整路径格式目录（如 {}）不会自动清理",
                    "•".cyan(),
                    "-Users-.../".dimmed()
                );
                println!();
                println!("建议：手动清理同步仓库中的完整路径格式目录，避免数据重复。");
            } else {
                // Switching to single-device mode
                println!(
                    "{}",
                    "正在从「多设备同步」切换到「单设备备份」模式".yellow()
                );
                println!();
                println!("影响：");
                println!(
                    "  {} 新推送的文件将使用完整路径格式（如 {}）",
                    "•".cyan(),
                    "-Users-xxx-project/".dimmed()
                );
                println!(
                    "  {} 已有的项目名格式目录（如 {}）不会自动清理",
                    "•".cyan(),
                    "n8n-workflow/".green()
                );
                println!();
                println!("注意：此模式不支持跨设备同步（路径不同会被视为不同项目）。");
            }
            println!("{}", "─".repeat(50).dimmed());
            println!();
        }

        config.use_project_name_only = project_name_only;
        println!(
            "{}",
            format!(
                "Use project name only: {}",
                if project_name_only {
                    "enabled (multi-device mode)"
                } else {
                    "disabled (full path mode)"
                }
            )
            .green()
        );
    }

    // Validate configuration before saving
    config.validate()?;

    config.save()?;
    println!("{}", "Configuration saved successfully!".green().bold());

    Ok(())
}

/// Show the current filter configuration
pub fn show_config() -> Result<()> {
    let config = FilterConfig::load()?;

    println!("{}", "Current Filter Configuration:".bold());
    println!(
        "  {}: {}",
        "Exclude older than".cyan(),
        config
            .exclude_older_than_days
            .map(|d| format!("{d} days"))
            .unwrap_or_else(|| "Not set".to_string())
    );
    println!(
        "  {}: {}",
        "Include patterns".cyan(),
        if config.include_patterns.is_empty() {
            "None (all included)".to_string()
        } else {
            config.include_patterns.join(", ")
        }
    );
    println!(
        "  {}: {}",
        "Exclude patterns".cyan(),
        if config.exclude_patterns.is_empty() {
            "None".to_string()
        } else {
            config.exclude_patterns.join(", ")
        }
    );
    println!(
        "  {}: {} bytes ({:.2} MB)",
        "Max file size".cyan(),
        config.max_file_size_bytes,
        config.max_file_size_bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "  {}: {}",
        "Exclude attachments".cyan(),
        if config.exclude_attachments {
            "Yes (only .jsonl files)".green()
        } else {
            "No (all files)".yellow()
        }
    );
    println!(
        "  {}: {}",
        "Git LFS".cyan(),
        if config.enable_lfs {
            format!("Enabled (patterns: {})", config.lfs_patterns.join(", ")).green()
        } else {
            "Disabled".yellow()
        }
    );
    println!("  {}: {}", "SCM backend".cyan(), config.scm_backend.green());
    println!(
        "  {}: {}",
        "Sync subdirectory".cyan(),
        config.sync_subdirectory.green()
    );
    println!(
        "  {}: {}",
        "Use project name only".cyan(),
        if config.use_project_name_only {
            "Yes (multi-device mode)".green()
        } else {
            "No (full path mode)".yellow()
        }
    );

    // Show config sync settings
    println!();
    println!("{}", "Configuration Sync Settings:".bold());
    println!(
        "  {}: {}",
        "Config sync enabled".cyan(),
        if config.config_sync.enabled {
            "Yes".green()
        } else {
            "No".yellow()
        }
    );
    if config.config_sync.enabled {
        println!(
            "  {}: {}",
            "Device name".cyan(),
            config.config_sync.get_device_name().green()
        );
        println!(
            "  {}: {}",
            "Push with config".cyan(),
            if config.config_sync.push_with_config {
                "Yes (auto-sync on push)".green()
            } else {
                "No (manual only)".dimmed()
            }
        );
        println!(
            "  {}: {}",
            "Sync settings.json".cyan(),
            if config.config_sync.sync_settings {
                "Yes"
            } else {
                "No"
            }
        );
        println!(
            "  {}: {}",
            "Sync CLAUDE.md".cyan(),
            if config.config_sync.sync_claude_md {
                "Yes"
            } else {
                "No"
            }
        );
        println!(
            "  {}: {}",
            "Sync hooks".cyan(),
            if config.config_sync.sync_hooks {
                "Yes"
            } else {
                "No"
            }
        );
        println!(
            "  {}: {}",
            "Sync skills list".cyan(),
            if config.config_sync.sync_skills_list {
                "Yes"
            } else {
                "No"
            }
        );
    }

    // Show auto memory settings
    println!();
    println!("{}", "Auto Memory Settings:".bold());
    println!(
        "  {}: {}",
        "Auto memory sync".cyan(),
        if config.auto_memory.enabled {
            "Enabled".green()
        } else {
            "Disabled".yellow()
        }
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*test*", "this is a test"));
        assert!(glob_match("test*", "testing"));
        assert!(glob_match("*test", "this is a test"));
        assert!(!glob_match("test*", "no match"));
    }

    #[test]
    fn test_filter_config_default() {
        let config = FilterConfig::default();
        assert_eq!(config.exclude_older_than_days, None);
        assert!(config.include_patterns.is_empty());
        assert!(config.exclude_patterns.is_empty());
        assert!(!config.exclude_attachments);
    }

    #[test]
    fn maintenance_is_enabled_by_default_with_conservative_timings() {
        let config = FilterConfig::default();
        assert!(config.session_maintenance.enabled);
        assert_eq!(config.session_maintenance.classifier, "conservative");
        assert_eq!(config.session_maintenance.hide_after_hours, 24);
        assert_eq!(config.session_maintenance.recycle_after_days, 7);
        assert_eq!(config.session_maintenance.purge_after_days, 30);
        assert_eq!(config.session_maintenance.max_actions_per_run, 50);
    }

    #[test]
    fn legacy_toml_without_session_maintenance_uses_safe_defaults() {
        let config: FilterConfig = toml::from_str("exclude_attachments = true\n").unwrap();
        assert_eq!(
            config.session_maintenance,
            SessionMaintenanceSettings::default()
        );
    }

    #[test]
    fn explicitly_disabled_maintenance_survives_the_new_default() {
        // An existing config that opted out must not be re-enabled on upgrade.
        let config: FilterConfig =
            toml::from_str("[session_maintenance]\nenabled = false\n").unwrap();
        assert!(!config.session_maintenance.enabled);
    }

    #[test]
    fn partial_session_maintenance_toml_fills_remaining_defaults() {
        let config: FilterConfig =
            toml::from_str("[session_maintenance]\nenabled = true\n").unwrap();
        assert!(config.session_maintenance.enabled);
        assert_eq!(config.session_maintenance.classifier, "conservative");
        assert_eq!(config.session_maintenance.hide_after_hours, 24);
        assert_eq!(config.session_maintenance.recycle_after_days, 7);
        assert_eq!(config.session_maintenance.purge_after_days, 30);
        assert_eq!(config.session_maintenance.max_actions_per_run, 50);
    }

    #[test]
    fn test_exclude_attachments_filter() {
        use std::path::PathBuf;

        // Config with exclude_attachments = false (default)
        let config_include_all = FilterConfig::default();

        // Should include .jsonl files
        assert!(config_include_all.should_include(&PathBuf::from("session.jsonl")));

        // Should also include other files when exclude_attachments is false
        assert!(config_include_all.should_include(&PathBuf::from("image.png")));
        assert!(config_include_all.should_include(&PathBuf::from("document.pdf")));

        // Config with exclude_attachments = true
        let config_exclude = FilterConfig {
            exclude_attachments: true,
            ..Default::default()
        };

        // Should include .jsonl files
        assert!(config_exclude.should_include(&PathBuf::from("session.jsonl")));

        // Should exclude non-.jsonl files
        assert!(!config_exclude.should_include(&PathBuf::from("image.png")));
        assert!(!config_exclude.should_include(&PathBuf::from("image.jpg")));
        assert!(!config_exclude.should_include(&PathBuf::from("document.pdf")));
        assert!(!config_exclude.should_include(&PathBuf::from("archive.zip")));
    }

    #[test]
    fn test_exclude_attachments_with_patterns() {
        use std::path::PathBuf;

        let config = FilterConfig {
            exclude_attachments: true,
            exclude_patterns: vec!["*test*".to_string()],
            ..Default::default()
        };

        // Should exclude based on attachment filter
        assert!(!config.should_include(&PathBuf::from("image.png")));

        // Should exclude based on pattern even for .jsonl
        assert!(!config.should_include(&PathBuf::from("/path/test/session.jsonl")));

        // Should include .jsonl that doesn't match exclude pattern
        assert!(config.should_include(&PathBuf::from("/path/prod/session.jsonl")));
    }

    #[test]
    fn test_filter_config_serialization() {
        let config = FilterConfig {
            exclude_attachments: true,
            exclude_older_than_days: Some(30),
            ..Default::default()
        };

        // Test that it can be serialized
        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains("exclude_attachments"));

        // Test that it can be deserialized
        let deserialized: FilterConfig = toml::from_str(&serialized).unwrap();
        assert!(deserialized.exclude_attachments);
        assert_eq!(deserialized.exclude_older_than_days, Some(30));
    }
}
