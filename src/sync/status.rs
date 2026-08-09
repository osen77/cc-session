use anyhow::Result;
use colored::Colorize;
use std::path::Path;

use crate::config::ConfigManager;
use crate::filter::FilterConfig;
use crate::path_security::{validate_directory_root, validate_sync_projects_root};
use crate::scm;

use super::discovery::{claude_projects_dir, discover_sessions};
use super::state::SyncState;

/// Show sync status
pub fn show_status(show_conflicts: bool, show_files: bool) -> Result<()> {
    let state = SyncState::load()?;
    let repo = scm::open(&state.sync_repo_path)?;
    let filter = FilterConfig::load()?;
    let claude_dir = claude_projects_dir()?;

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

    // Session counts
    println!();
    println!("{}", "对话历史:".bold());
    validate_directory_root(&claude_dir)?;
    let local_sessions = discover_sessions(&claude_dir, &filter)?;
    println!("  本地: {} 个会话", local_sessions.len().to_string().cyan());

    let remote_projects_dir = state.sync_repo_path.join(&filter.sync_subdirectory);
    if remote_projects_dir.exists() {
        validate_sync_projects_root(&state.sync_repo_path, &remote_projects_dir)?;
        let remote_sessions = discover_sessions(&remote_projects_dir, &filter)?;
        println!(
            "  同步仓库: {} 个会话",
            remote_sessions.len().to_string().cyan()
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

    // Show files if requested
    if show_files {
        println!();
        println!("{}", "本地会话文件:".bold());
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
