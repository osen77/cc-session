//! Configuration sync handler
//!
//! Syncs Claude Code configuration files across devices:
//! - settings.json (without hooks)
//! - CLAUDE.md (with platform tag filtering)
//! - hooks/ (optional)
//! - plugins/skills list

use anyhow::{Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::platform_filter::{has_platform_blocks, merge_claude_md, Platform};
use crate::scm;
use crate::sync::SyncState;
use crate::BINARY_NAME;

// Re-export ConfigSyncSettings from filter module
pub use crate::filter::ConfigSyncSettings;

/// Sync metadata for a device
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceSyncInfo {
    pub device: String,
    pub platform: String,
    #[serde(rename = "lastSync")]
    pub last_sync: String,
}

/// Skills list format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsList {
    pub skills: HashMap<String, String>,
}

/// Get the Claude config directory
fn claude_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot find home directory")?;
    Ok(home.join(".claude"))
}

/// Get the configs subdirectory in sync repo
fn configs_dir(sync_repo: &Path) -> PathBuf {
    sync_repo.join("_configs")
}

/// Get device config directory in sync repo
fn device_config_dir(sync_repo: &Path, device_name: &str) -> PathBuf {
    configs_dir(sync_repo).join(device_name)
}

/// Push configuration to sync repository (only copy files, no commit/push)
/// Returns the list of synced files
pub fn push_config_files(settings: &ConfigSyncSettings) -> Result<Vec<String>> {
    let device_name = settings.get_device_name();
    log::info!("Pushing configuration files for device: {}", device_name);

    let sync_state = SyncState::load()?;
    let sync_repo = sync_state.sync_repo_path.clone();
    let claude = claude_dir()?;
    let target_dir = device_config_dir(&sync_repo, &device_name);

    // Create target directory
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("Failed to create config dir: {}", target_dir.display()))?;

    let mut synced_files = Vec::new();

    // Sync settings.json (without hooks)
    if settings.sync_settings {
        let settings_path = claude.join("settings.json");
        if settings_path.exists() {
            let content = fs::read_to_string(&settings_path)?;

            // Parse and remove hooks
            if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) {
                // Save full version with hooks
                let full_path = target_dir.join("settings-full.json");
                fs::write(&full_path, &content)?;
                synced_files.push("settings-full.json".to_string());

                // Remove hooks for portable version
                if let Some(obj) = json.as_object_mut() {
                    obj.remove("hooks");
                }
                let portable_content = serde_json::to_string_pretty(&json)?;
                let portable_path = target_dir.join("settings.json");
                fs::write(&portable_path, portable_content)?;
                synced_files.push("settings.json".to_string());
            } else {
                // Just copy as-is if not valid JSON
                fs::copy(&settings_path, target_dir.join("settings.json"))?;
                synced_files.push("settings.json".to_string());
            }
        }
    }

    // Sync CLAUDE.md
    if settings.sync_claude_md {
        let claude_md_path = claude.join("CLAUDE.md");
        if claude_md_path.exists() {
            fs::copy(&claude_md_path, target_dir.join("CLAUDE.md"))?;
            synced_files.push("CLAUDE.md".to_string());
        }
    }

    // Sync hooks folder
    if settings.sync_hooks {
        let hooks_dir = claude.join("hooks");
        if hooks_dir.exists() && hooks_dir.is_dir() {
            let target_hooks = target_dir.join("hooks");
            if target_hooks.exists() {
                fs::remove_dir_all(&target_hooks)?;
            }
            copy_dir_recursive(&hooks_dir, &target_hooks)?;
            synced_files.push("hooks/".to_string());
        }
    }

    // Sync skills list
    if settings.sync_skills_list {
        let skills_dir = claude.join("skills");
        if skills_dir.exists() && skills_dir.is_dir() {
            let skills_list = generate_skills_list(&skills_dir)?;
            let skills_json = serde_json::to_string_pretty(&skills_list)?;
            fs::write(target_dir.join("installed_skills.json"), skills_json)?;
            synced_files.push("installed_skills.json".to_string());
        }

        // Also sync plugins list if exists
        let plugins_path = claude.join("plugins/installed_plugins.json");
        if plugins_path.exists() {
            fs::copy(&plugins_path, target_dir.join("installed_plugins.json"))?;
            synced_files.push("installed_plugins.json".to_string());
        }
    }

    // Save sync metadata
    let sync_info = DeviceSyncInfo {
        device: device_name.clone(),
        platform: Platform::current().to_string(),
        last_sync: chrono::Utc::now().to_rfc3339(),
    };
    let info_json = serde_json::to_string_pretty(&sync_info)?;
    fs::write(target_dir.join(".sync-info.json"), info_json)?;

    Ok(synced_files)
}

/// Push configuration to sync repository (with commit and push)
pub fn handle_config_push(settings: &ConfigSyncSettings) -> Result<()> {
    let device_name = settings.get_device_name();

    let synced_files = push_config_files(settings)?;

    // Commit and push
    if !synced_files.is_empty() {
        let sync_state = SyncState::load()?;
        let sync_repo = sync_state.sync_repo_path.clone();
        let message = format!("Sync config from {}", device_name);
        let repo = scm::open(&sync_repo)?;

        // Stage all changes
        repo.stage_all()?;

        // Check if there are changes to commit
        if repo.has_changes()? {
            repo.commit(&message)?;

            // Push to remote if available
            if sync_state.has_remote {
                let branch = repo.current_branch()?;
                repo.push("origin", &branch)?;
            }

            println!("{}", "✓ 配置已推送".green());
            for file in &synced_files {
                println!("  - {}", file);
            }
        } else {
            println!("{}", "配置无变化".dimmed());
        }
    } else {
        println!("{}", "⚠️  没有找到可同步的配置文件".yellow());
    }

    Ok(())
}

/// List available device configurations
pub fn handle_config_list() -> Result<()> {
    let sync_state = SyncState::load()?;
    let configs = configs_dir(&sync_state.sync_repo_path);

    if !configs.exists() {
        println!("{}", "没有找到配置同步目录".yellow());
        println!(
            "运行 {} 推送当前设备配置",
            format!("{} config push", BINARY_NAME).cyan()
        );
        return Ok(());
    }

    let current_device = ConfigSyncSettings::default().get_device_name();

    println!("{}", "可用的设备配置:".bold());
    println!();

    let mut found_any = false;
    for entry in fs::read_dir(&configs)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let device_name = entry.file_name().to_string_lossy().to_string();
        found_any = true;

        // Read sync info
        let info_path = entry.path().join(".sync-info.json");
        let sync_info: Option<DeviceSyncInfo> = if info_path.exists() {
            fs::read_to_string(&info_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
        } else {
            None
        };

        // Display device
        if device_name == current_device {
            println!("  {} (当前设备)", device_name.green());
        } else {
            println!("  {}", device_name.cyan());
        }

        if let Some(info) = sync_info {
            println!("    平台: {}", info.platform);
            println!("    最后同步: {}", info.last_sync);
        }

        // Show available files
        let dir = entry.path();
        let files = [
            "settings.json",
            "settings-full.json",
            "CLAUDE.md",
            "installed_skills.json",
        ];
        let mut available = Vec::new();
        for file in files {
            if dir.join(file).exists() {
                available.push(file);
            }
        }
        if dir.join("hooks").exists() {
            available.push("hooks/");
        }

        if !available.is_empty() {
            println!("    文件: {}", available.join(", ").dimmed());
        }
        println!();
    }

    if !found_any {
        println!("{}", "  没有找到设备配置".dimmed());
        println!();
        println!(
            "运行 {} 推送当前设备配置",
            format!("{} config push", BINARY_NAME).cyan()
        );
    }

    Ok(())
}

/// Apply configuration from another device
pub fn handle_config_apply(
    source_device: &str,
    with_hooks: bool,
    settings: &ConfigSyncSettings,
) -> Result<()> {
    let sync_state = SyncState::load()?;
    let source_dir = device_config_dir(&sync_state.sync_repo_path, source_device);

    if !source_dir.exists() {
        return Err(anyhow::anyhow!(
            "设备配置不存在: {}\n运行 `{} config list` 查看可用配置",
            source_device,
            BINARY_NAME
        ));
    }

    let claude = claude_dir()?;
    let current_platform = Platform::current();
    let mut applied_files = Vec::new();

    println!("{}", format!("从 {} 应用配置...", source_device).cyan());

    // Apply settings.json
    if settings.sync_settings {
        let settings_file = if with_hooks {
            "settings-full.json"
        } else {
            "settings.json"
        };

        let source_settings = source_dir.join(settings_file);
        if source_settings.exists() {
            let target_settings = claude.join("settings.json");

            // Backup current settings
            if target_settings.exists() {
                let backup = claude.join("settings.json.backup");
                fs::copy(&target_settings, &backup)?;
                println!("  {} 已备份到 settings.json.backup", "ℹ".blue());
            }

            if with_hooks {
                // Copy full version directly
                fs::copy(&source_settings, &target_settings)?;
            } else {
                // Merge: keep local hooks, use remote settings
                let source_content = fs::read_to_string(&source_settings)?;
                let target_content = if target_settings.exists() {
                    fs::read_to_string(&target_settings)?
                } else {
                    "{}".to_string()
                };

                let source_json: serde_json::Value = serde_json::from_str(&source_content)?;
                let target_json: serde_json::Value = serde_json::from_str(&target_content)?;

                // Merge: source settings + local hooks
                let mut merged = source_json.clone();
                if let (Some(merged_obj), Some(target_obj)) =
                    (merged.as_object_mut(), target_json.as_object())
                {
                    if let Some(hooks) = target_obj.get("hooks") {
                        merged_obj.insert("hooks".to_string(), hooks.clone());
                    }
                }

                let merged_content = serde_json::to_string_pretty(&merged)?;
                fs::write(&target_settings, merged_content)?;
            }

            applied_files.push(format!(
                "{} ({})",
                "settings.json",
                if with_hooks {
                    "含 hooks"
                } else {
                    "保留本地 hooks"
                }
            ));
        }
    }

    // Apply CLAUDE.md with platform filtering and merging
    if settings.sync_claude_md {
        let source_claude_md = source_dir.join("CLAUDE.md");
        if source_claude_md.exists() {
            let source_content = fs::read_to_string(&source_claude_md)?;
            let target_claude_md = claude.join("CLAUDE.md");

            // Backup
            if target_claude_md.exists() {
                let backup = claude.join("CLAUDE.md.backup");
                fs::copy(&target_claude_md, &backup)?;
            }

            // Read target content (if exists)
            let target_content = if target_claude_md.exists() {
                fs::read_to_string(&target_claude_md)?
            } else {
                String::new()
            };

            // Merge: source common content + target's current platform block
            let final_content = if has_platform_blocks(&source_content)
                || has_platform_blocks(&target_content)
            {
                let merged = merge_claude_md(&source_content, &target_content, current_platform);
                println!(
                    "  {} 已合并 CLAUDE.md（保留本地 {} 平台内容）",
                    "ℹ".blue(),
                    current_platform
                );
                merged
            } else {
                // No platform blocks, just use source
                source_content
            };

            fs::write(&target_claude_md, final_content)?;
            applied_files.push("CLAUDE.md".to_string());
        }
    }

    // Apply hooks if requested
    if with_hooks && settings.sync_hooks {
        let source_hooks = source_dir.join("hooks");
        if source_hooks.exists() && source_hooks.is_dir() {
            let target_hooks = claude.join("hooks");
            fs::create_dir_all(&target_hooks)?;

            for entry in fs::read_dir(&source_hooks)? {
                let entry = entry?;
                let target_file = target_hooks.join(entry.file_name());
                fs::copy(entry.path(), &target_file)?;

                // Make executable on Unix
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = fs::metadata(&target_file)?.permissions();
                    perms.set_mode(0o755);
                    fs::set_permissions(&target_file, perms)?;
                }
            }

            applied_files.push("hooks/".to_string());

            println!();
            println!(
                "{}",
                "⚠️  Hooks 已应用，请检查以下路径是否适用于本设备:".yellow()
            );
            println!("    - ~/.claude/hooks/ 中的脚本内容");
            println!("    - settings.json 中的 hooks 命令路径");
        }
    }

    // Show skills to install
    let skills_path = source_dir.join("installed_skills.json");
    if skills_path.exists() {
        let content = fs::read_to_string(&skills_path)?;
        if let Ok(skills_list) = serde_json::from_str::<SkillsList>(&content) {
            if !skills_list.skills.is_empty() {
                println!();
                println!("{}", "Skills 安装命令:".cyan());
                for url in skills_list.skills.values() {
                    println!("  claude skill install {}", url);
                }
            }
        }
    }

    // Show plugins to install
    let plugins_path = source_dir.join("installed_plugins.json");
    if plugins_path.exists() {
        if let Ok(content) = fs::read_to_string(&plugins_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(plugins) = json.get("plugins").and_then(|p| p.as_object()) {
                    if !plugins.is_empty() {
                        println!();
                        println!("{}", "Plugins 安装命令:".cyan());
                        for name in plugins.keys() {
                            println!("  claude plugin install {}", name);
                        }
                    }
                }
            }
        }
    }

    println!();
    if !applied_files.is_empty() {
        println!("{}", "✓ 配置已应用".green());
        for file in &applied_files {
            println!("  - {}", file);
        }
        println!();
        println!("{}", "请重启 Claude Code 使配置生效".cyan());
    } else {
        println!("{}", "没有应用任何配置".yellow());
    }

    Ok(())
}

/// Show config sync status
pub fn handle_config_status(settings: &ConfigSyncSettings) -> Result<()> {
    let device_name = settings.get_device_name();
    let claude = claude_dir()?;

    println!("{}", "配置同步状态".bold());
    println!("{}", "━".repeat(40));
    println!();

    println!("设备名称: {}", device_name.cyan());
    println!("平台: {}", Platform::current().to_string().cyan());
    println!();

    println!("{}", "本地配置文件:".bold());
    let files = [
        ("settings.json", claude.join("settings.json")),
        ("CLAUDE.md", claude.join("CLAUDE.md")),
        ("hooks/", claude.join("hooks")),
    ];

    for (name, path) in files {
        let status = if path.exists() {
            "✓".green()
        } else {
            "✗".red()
        };
        println!("  {} {}", status, name);
    }

    // Check for skills
    let skills_dir = claude.join("skills");
    if skills_dir.exists() {
        let count = fs::read_dir(&skills_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .count();
        println!("  {} skills: {} 个", "✓".green(), count);
    }

    // Show sync settings
    println!();
    println!("{}", "同步设置:".bold());
    println!(
        "  配置同步: {}",
        if settings.enabled {
            "启用".green()
        } else {
            "禁用".red()
        }
    );
    println!(
        "  同步 settings.json: {}",
        if settings.sync_settings {
            "是".green()
        } else {
            "否".dimmed()
        }
    );
    println!(
        "  同步 CLAUDE.md: {}",
        if settings.sync_claude_md {
            "是".green()
        } else {
            "否".dimmed()
        }
    );
    println!(
        "  同步 hooks: {}",
        if settings.sync_hooks {
            "是".green()
        } else {
            "否".dimmed()
        }
    );
    println!(
        "  同步 skills 列表: {}",
        if settings.sync_skills_list {
            "是".green()
        } else {
            "否".dimmed()
        }
    );

    Ok(())
}

/// Generate skills list from skills directory
fn generate_skills_list(skills_dir: &Path) -> Result<SkillsList> {
    let mut skills = HashMap::new();

    for entry in fs::read_dir(skills_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let skill_name = entry.file_name().to_string_lossy().to_string();
        let git_dir = entry.path().join(".git");

        if git_dir.exists() {
            // Try to get git remote origin URL
            if let Ok(output) = std::process::Command::new("git")
                .args(["remote", "get-url", "origin"])
                .current_dir(entry.path())
                .output()
            {
                if output.status.success() {
                    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !url.is_empty() {
                        skills.insert(skill_name, url);
                    }
                }
            }
        }
    }

    Ok(SkillsList { skills })
}

/// Recursively copy a directory
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

/// Find the most recently updated device config (excluding current device)
#[allow(dead_code)]
pub fn find_latest_device_config(sync_repo: &Path, current_device: &str) -> Option<String> {
    find_latest_device_config_with_time(sync_repo, current_device).map(|(name, _)| name)
}

/// Find the most recently synced device config (excluding current device),
/// returning both device name and its sync timestamp.
fn find_latest_device_config_with_time(
    sync_repo: &Path,
    current_device: &str,
) -> Option<(String, chrono::DateTime<chrono::Utc>)> {
    let configs = configs_dir(sync_repo);
    if !configs.exists() {
        return None;
    }

    let mut latest: Option<(String, chrono::DateTime<chrono::Utc>)> = None;

    for entry in fs::read_dir(&configs).ok()?.filter_map(|e| e.ok()) {
        if !entry.path().is_dir() {
            continue;
        }

        let device_name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };

        // Skip current device
        if device_name == current_device {
            continue;
        }

        // Read .sync-info.json
        let info_path = entry.path().join(".sync-info.json");
        if let Ok(content) = fs::read_to_string(&info_path) {
            if let Ok(info) = serde_json::from_str::<DeviceSyncInfo>(&content) {
                if let Ok(sync_time) = chrono::DateTime::parse_from_rfc3339(&info.last_sync) {
                    let sync_time = sync_time.with_timezone(&chrono::Utc);
                    if latest.is_none() || sync_time > latest.as_ref().unwrap().1 {
                        latest = Some((device_name, sync_time));
                    }
                }
            }
        }
    }

    latest
}

/// Get the sync timestamp of a specific device from its .sync-info.json.
fn get_device_sync_time(sync_repo: &Path, device: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let info_path = device_config_dir(sync_repo, device).join(".sync-info.json");
    let content = fs::read_to_string(&info_path).ok()?;
    let info: DeviceSyncInfo = serde_json::from_str(&content).ok()?;
    chrono::DateTime::parse_from_rfc3339(&info.last_sync)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// Auto-apply CLAUDE.md from the most recently updated device
/// Only applies CLAUDE.md, not other config files (settings, hooks, skills)
/// Only applies if the other device's config is newer than the current device's config
pub fn auto_apply_claude_md(settings: &ConfigSyncSettings) -> Result<()> {
    if !settings.enabled || !settings.auto_apply_claude_md {
        log::debug!("Auto-apply CLAUDE.md is disabled");
        return Ok(());
    }

    let sync_state = SyncState::load()?;
    let current_device = settings.get_device_name();

    // Find most recently updated device (with timestamp)
    let (latest_device, latest_time) =
        match find_latest_device_config_with_time(&sync_state.sync_repo_path, &current_device) {
            Some(d) => d,
            None => {
                log::debug!("No other device configs found for auto-apply");
                return Ok(());
            }
        };

    // Only apply if the other device's config is newer than current device's
    if let Some(current_time) = get_device_sync_time(&sync_state.sync_repo_path, &current_device) {
        if latest_time <= current_time {
            log::debug!(
                "Current device config ({}) is newer than {} ({}), skipping auto-apply",
                current_time,
                latest_device,
                latest_time
            );
            return Ok(());
        }
    }

    let source_dir = device_config_dir(&sync_state.sync_repo_path, &latest_device);
    let source_claude_md = source_dir.join("CLAUDE.md");

    if !source_claude_md.exists() {
        log::debug!("No CLAUDE.md found in device config: {}", latest_device);
        return Ok(());
    }

    let claude = claude_dir()?;
    let target_claude_md = claude.join("CLAUDE.md");
    let source_content = fs::read_to_string(&source_claude_md)?;

    // Read target content
    let target_content = if target_claude_md.exists() {
        fs::read_to_string(&target_claude_md)?
    } else {
        String::new()
    };

    // Only apply if there are platform blocks to merge
    if has_platform_blocks(&source_content) || has_platform_blocks(&target_content) {
        let current_platform = Platform::current();
        let merged = merge_claude_md(&source_content, &target_content, current_platform);

        // Only write if content changed
        if merged != target_content {
            fs::write(&target_claude_md, &merged)?;
            log::info!("Auto-applied CLAUDE.md from device: {}", latest_device);
        }
    } else {
        // No platform blocks - check if source is different and update
        if source_content != target_content {
            fs::write(&target_claude_md, &source_content)?;
            log::info!("Auto-applied CLAUDE.md from device: {}", latest_device);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_name_fallback() {
        let settings = ConfigSyncSettings::default();
        let name = settings.get_device_name();
        assert!(!name.is_empty());
    }

    #[test]
    fn test_config_sync_settings_default() {
        let settings = ConfigSyncSettings::default();
        assert!(settings.enabled);
        assert!(settings.sync_settings);
        assert!(settings.sync_claude_md);
        assert!(!settings.sync_hooks);
        assert!(settings.sync_skills_list);
        assert!(!settings.auto_apply_claude_md);
    }
}
