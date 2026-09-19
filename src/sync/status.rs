use anyhow::Result;
use colored::Colorize;
use std::path::Path;

use crate::config::ConfigManager;
use crate::filter::FilterConfig;
use crate::path_security::validate_sync_projects_root;
use crate::scm;

use super::discovery::{
    claude_projects_dir, count_unique_sessions, count_unique_sessions_in_roots,
    discover_local_sessions,
};
use super::state::SyncState;

/// Show sync status
pub fn show_status(show_conflicts: bool, show_files: bool) -> Result<()> {
    let state = SyncState::load()?;
    let repo = scm::open(&state.sync_repo_path)?;
    let filter = FilterConfig::load()?;
    let root_mappings = filter.root_mappings()?;
    let claude_dir = claude_projects_dir()?;
    let local_roots = crate::project_roots::enumerate(&claude_dir, &root_mappings)?;

    println!("{}", "=== Claude Code Sync Status ===".bold().cyan());
    println!();

    // Installation info
    println!("{}", "安装信息:".bold());
    if let Ok(exe_path) = std::env::current_exe() {
        println!("  二进制: {}", exe_path.display().to_string().dimmed());
    }
    if let Ok(config_dir) = ConfigManager::config_dir() {
        println!("  配置目录: {}", config_dir.display().to_string().dimmed());
    }
    println!();

    // Claude Code info
    println!("{}", "Claude Code:".bold());
    if let Some(parent) = claude_dir.parent() {
        println!("  目录: {}", parent.display().to_string().dimmed());
    }
    println!();

    // Repository info
    println!("{}", "同步仓库:".bold());
    println!("  本地路径: {}", state.sync_repo_path.display());
    let backend = scm::detect_backend(&state.sync_repo_path)
        .map(|b| format!("{:?}", b))
        .unwrap_or_else(|| "Unknown".to_string());
    println!("  后端: {}", backend);

    // Show remote URL if configured
    if state.has_remote {
        if let Ok(remote_url) = repo.get_remote_url("origin") {
            println!("  远程仓库: {}", remote_url.cyan());
        } else {
            println!("  远程仓库: {}", "已配置".green());
        }
    } else {
        println!("  远程仓库: {}", "未配置".yellow());
    }

    if let Ok(branch) = repo.current_branch() {
        println!("  分支: {}", branch.cyan());
    }

    if let Ok(has_changes) = repo.has_changes() {
        println!(
            "  未提交变更: {}",
            if has_changes {
                "是".yellow()
            } else {
                "否".green()
            }
        );
    }

    // Session counts. These are display-only, so use the streaming counter
    // instead of fully parsing every conversation file.
    println!();
    println!("{}", "对话历史:".bold());
    let local_session_count = count_unique_sessions_in_roots(&local_roots, &filter)?;
    println!("  本地: {} 个会话", local_session_count.to_string().cyan());

    let remote_projects_dir = state.sync_repo_path.join(&filter.sync_subdirectory);
    if remote_projects_dir.exists() {
        validate_sync_projects_root(&state.sync_repo_path, &remote_projects_dir)?;
        let remote_session_count = count_unique_sessions(&remote_projects_dir, &filter)?;
        println!(
            "  同步仓库: {} 个会话",
            remote_session_count.to_string().cyan()
        );
    }

    // Config sync info
    println!();
    println!("{}", "配置同步:".bold());
    let config_sync = &filter.config_sync;
    println!(
        "  状态: {}",
        if config_sync.enabled {
            "已启用".green()
        } else {
            "已禁用".yellow()
        }
    );
    println!("  设备名: {}", config_sync.get_device_name().cyan());

    // Show what's being synced
    let mut sync_items = Vec::new();
    if config_sync.sync_settings {
        sync_items.push("settings.json");
    }
    if config_sync.sync_claude_md {
        sync_items.push("CLAUDE.md");
    }
    if config_sync.sync_skills_list {
        sync_items.push("skills");
    }
    if config_sync.sync_hooks {
        sync_items.push("hooks");
    }
    if !sync_items.is_empty() {
        println!("  同步项: {}", sync_items.join(", "));
    }
    println!(
        "  自动应用 CLAUDE.md: {}",
        if config_sync.auto_apply_claude_md {
            "是".green()
        } else {
            "否".dimmed()
        }
    );

    // Check for configs directory
    let configs_dir = state.sync_repo_path.join("_configs");
    if configs_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&configs_dir) {
            let devices: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            if !devices.is_empty() {
                println!("  可用设备: {}", devices.join(", ").dimmed());
            }
        }
    }

    // Show files if requested; only this detail view needs the full parse.
    if show_files {
        println!();
        println!("{}", "本地会话文件:".bold());
        let local_sessions = discover_local_sessions(&claude_dir, &filter, false)?;
        for session in local_sessions.iter().take(20) {
            let relative = Path::new(&session.file_path)
                .strip_prefix(&claude_dir)
                .unwrap_or(Path::new(&session.file_path));
            println!(
                "  {} ({} 条消息)",
                relative.display(),
                session.message_count()
            );
        }
        if local_sessions.len() > 20 {
            println!("  ... 还有 {} 个", local_sessions.len() - 20);
        }
    }

    // Show conflicts if requested
    if show_conflicts {
        println!();
        if let Ok(report) = crate::report::load_latest_report() {
            if report.total_conflicts > 0 {
                report.print_summary();
            } else {
                println!("{}", "上次同步无冲突".green());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_DIR_ENV;
    use crate::project_roots::{with_test_volume, ExternalProjectsRoot, VolumeIdentity};
    use crate::sync::state::SyncState;
    use serial_test::serial;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    struct EnvGuard {
        home: Option<std::ffi::OsString>,
        config: Option<std::ffi::OsString>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.config.take() {
                Some(value) => std::env::set_var(CONFIG_DIR_ENV, value),
                None => std::env::remove_var(CONFIG_DIR_ENV),
            }
        }
    }

    #[test]
    #[serial]
    fn external_root_status_is_readable_and_fail_closed_without_state_changes() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let config_dir = temp.path().join("config");
        let logical = home.join(".claude/projects");
        let mount = temp.path().join("volume");
        let trusted = mount.join("Claude");
        let target = trusted.join("projects");
        let project = target.join("project");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(logical.parent().unwrap()).unwrap();
        fs::create_dir_all(project.join("memory")).unwrap();
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            project.join("session.jsonl"),
            br#"{"type":"user","sessionId":"sid","cwd":"/work/project","message":{"role":"user","content":"hello"}}
"#,
        )
        .unwrap();
        symlink(&target, &logical).unwrap();
        crate::scm::init(&repo_path).unwrap();

        let guard = EnvGuard {
            home: std::env::var_os("HOME"),
            config: std::env::var_os(CONFIG_DIR_ENV),
        };
        std::env::set_var("HOME", &home);
        std::env::set_var(CONFIG_DIR_ENV, &config_dir);
        let state = SyncState {
            sync_repo_path: repo_path,
            has_remote: false,
            is_cloned_repo: false,
            last_synced_commit: None,
        };
        state.save().unwrap();
        let external = ExternalProjectsRoot {
            target: target.clone(),
            trusted_root: trusted,
            volume_uuid: "AF9C9871-18AE-40C9-8D18-624E415520C6".into(),
        };
        let mut filter = FilterConfig::default();
        filter.external_projects_root = Some(external.clone());
        filter.save().unwrap();
        let state_before = fs::read(ConfigManager::state_file_path().unwrap()).unwrap();
        let config_before = fs::read(ConfigManager::filter_config_path().unwrap()).unwrap();
        let volume = VolumeIdentity {
            mount_point: mount,
            volume_uuid: external.volume_uuid.clone(),
            device_id: {
                use std::os::unix::fs::MetadataExt;
                fs::metadata(temp.path()).unwrap().dev()
            },
        };

        with_test_volume(volume, || {
            show_status(false, false).unwrap();
            fs::remove_dir_all(&target).unwrap();
            assert!(show_status(false, false).is_err());
            assert_eq!(fs::read(ConfigManager::state_file_path().unwrap()).unwrap(), state_before);
            assert_eq!(fs::read(ConfigManager::filter_config_path().unwrap()).unwrap(), config_before);
        });
        drop(guard);
    }
}
