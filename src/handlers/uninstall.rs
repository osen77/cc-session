//! Uninstall handler
//!
//! Removes all ccs artifacts: hooks, wrapper, config directory, sync repo, and binary.

use anyhow::{Context, Result};
use colored::Colorize;
use inquire::Confirm;

use crate::config::ConfigManager;
use crate::sync::MultiRepoState;

/// Run the uninstall flow
pub fn handle_uninstall(force: bool) -> Result<()> {
    println!();
    println!("{}", "🗑️  卸载 Claude Code Sync".red().bold());
    println!("{}", "━━━━━━━━━━━━━━━━━━━━━━━━━".red());
    println!();

    // Gather info about what exists
    let config_dir = ConfigManager::config_dir().ok();
    let sync_repo_path = MultiRepoState::load()
        .ok()
        .and_then(|s| s.active().map(|r| r.sync_repo_path.clone()));
    let current_exe = std::env::current_exe().ok();

    // Show what will be removed
    println!("{}", "将要执行以下操作:".cyan());
    println!("   1. 卸载 Claude Code hooks");
    println!("   2. 删除 wrapper 脚本 (claude-sync)");
    if let Some(ref dir) = config_dir {
        println!("   3. 删除配置目录 ({})", dir.display());
    }
    if let Some(ref repo) = sync_repo_path {
        println!("   4. 删除同步仓库 ({}) [需单独确认]", repo.display());
    }
    if let Some(ref exe) = current_exe {
        println!("   5. 删除 ccs 二进制 ({}) [需单独确认]", exe.display());
    }
    println!();

    if !force {
        let confirm = Confirm::new("确认卸载?")
            .with_default(false)
            .prompt()
            .unwrap_or(false);

        if !confirm {
            println!("{}", "已取消卸载。".yellow());
            return Ok(());
        }
        println!();
    }

    // Step 1: Uninstall hooks
    println!("{}", "1. 卸载 hooks...".cyan());
    match crate::handlers::hooks::handle_hooks_uninstall() {
        Ok(()) => {}
        Err(e) => println!("   {} {}", "跳过:".yellow(), e),
    }

    // Step 2: Remove wrapper
    println!("{}", "2. 删除 wrapper 脚本...".cyan());
    match crate::handlers::wrapper::handle_wrapper_uninstall() {
        Ok(()) => {}
        Err(e) => println!("   {} {}", "跳过:".yellow(), e),
    }

    // Step 3: Remove config directory
    if let Some(ref dir) = config_dir {
        println!("{}", "3. 删除配置目录...".cyan());
        if dir.exists() {
            std::fs::remove_dir_all(dir)
                .with_context(|| format!("删除配置目录失败: {}", dir.display()))?;
            println!("   {} {}", "✓".green(), dir.display());
        } else {
            println!("   {} 配置目录不存在，跳过", "!".yellow());
        }
    }

    // Step 4: Remove sync repo (requires separate confirmation)
    if let Some(ref repo) = sync_repo_path {
        if repo.exists() {
            println!();
            println!("{}", "⚠️  同步仓库可能包含未推送的对话历史".yellow().bold());
            println!("   路径: {}", repo.display());

            let delete_repo = if force {
                true
            } else {
                Confirm::new("是否删除同步仓库?")
                    .with_default(false)
                    .prompt()
                    .unwrap_or(false)
            };

            if delete_repo {
                std::fs::remove_dir_all(repo)
                    .with_context(|| format!("删除同步仓库失败: {}", repo.display()))?;
                println!("   {} 同步仓库已删除", "✓".green());
            } else {
                println!("   {} 保留同步仓库", "!".yellow());
            }
        }
    }

    // Step 5: Remove binary (requires separate confirmation)
    if let Some(ref exe) = current_exe {
        println!();
        let delete_binary = if force {
            true
        } else {
            Confirm::new(&format!("是否删除 ccs 二进制 ({})?", exe.display()))
                .with_default(false)
                .prompt()
                .unwrap_or(false)
        };

        if delete_binary {
            // Also remove wrapper in same directory (in case step 2 missed it)
            let exe_dir = exe.parent();
            if let Some(dir) = exe_dir {
                for name in &["claude-sync", "claude-sync.bat", "claude-sync.ps1"] {
                    let wrapper = dir.join(name);
                    if wrapper.exists() {
                        std::fs::remove_file(&wrapper).ok();
                    }
                }
            }

            std::fs::remove_file(exe)
                .with_context(|| format!("删除二进制失败: {}", exe.display()))?;
            println!("   {} ccs 二进制已删除", "✓".green());
        } else {
            println!("   {} 保留 ccs 二进制", "!".yellow());
        }
    }

    println!();
    println!("{}", "✓ 卸载完成".green().bold());
    println!();

    Ok(())
}
