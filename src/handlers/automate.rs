//! One-click automation setup
//!
//! This module provides a simple command to set up automatic synchronization
//! for Claude Code conversations.

use anyhow::Result;
use colored::Colorize;

use crate::BINARY_NAME;

use super::hooks::{are_hooks_installed, handle_hooks_install, handle_hooks_uninstall};
use super::wrapper::{
    get_wrapper_path, handle_wrapper_install, handle_wrapper_uninstall, is_wrapper_installed,
};

/// Set up automatic synchronization (one-click setup)
pub fn handle_automate_setup() -> Result<()> {
    println!("{}", "Setting up Claude Code auto-sync...".cyan().bold());
    println!();

    // Step 1: Install hooks
    println!("{}", "Step 1: Installing Hooks".cyan());
    println!("{}", "─".repeat(40).dimmed());
    handle_hooks_install()?;
    println!();

    // Step 2: Create wrapper
    println!("{}", "Step 2: Creating Wrapper Script".cyan());
    println!("{}", "─".repeat(40).dimmed());
    let wrapper_path = handle_wrapper_install(false)?;
    println!();

    // Step 3: Print usage instructions
    print_success_message(&wrapper_path)?;

    Ok(())
}

/// Show automation configuration status
pub fn handle_automate_status() -> Result<()> {
    println!("{}", "Claude Code Auto-Sync Status".cyan().bold());
    println!("{}", "═".repeat(40).dimmed());
    println!();

    // Check hooks
    let hooks_installed = are_hooks_installed()?;
    if hooks_installed {
        println!("{} {}", "Hooks:".bold(), "INSTALLED".green());
        println!("  {} SessionEnd (sync on exit)", "•".green());
        println!("  {} UserPromptSubmit (new project detection)", "•".green());
    } else {
        println!("{} {}", "Hooks:".bold(), "NOT INSTALLED".yellow());
    }
    println!();

    // Check wrapper
    let wrapper_installed = is_wrapper_installed()?;
    if wrapper_installed {
        let wrapper_path = get_wrapper_path()?;
        println!("{} {}", "Wrapper:".bold(), "INSTALLED".green());
        println!("  Path: {}", wrapper_path.display().to_string().cyan());
    } else {
        println!("{} {}", "Wrapper:".bold(), "NOT INSTALLED".yellow());
    }
    println!();

    // Overall status
    if hooks_installed && wrapper_installed {
        println!("{}", "═".repeat(40).dimmed());
        println!("{}", "Auto-sync is fully configured!".green().bold());
        println!();
        println!("Usage: Use '{}' to start Claude Code", "claude-sync".cyan());
    } else {
        println!("{}", "═".repeat(40).dimmed());
        println!("{}", "Auto-sync is not fully configured.".yellow());
        println!();
        println!(
            "Run '{}' to complete setup.",
            format!("{} automate", BINARY_NAME).cyan()
        );
    }

    Ok(())
}

/// Remove all automation configuration
pub fn handle_automate_uninstall() -> Result<()> {
    println!(
        "{}",
        "Removing Claude Code auto-sync configuration..."
            .cyan()
            .bold()
    );
    println!();

    // Remove hooks
    println!("{}", "Step 1: Removing Hooks".cyan());
    println!("{}", "─".repeat(40).dimmed());
    handle_hooks_uninstall()?;
    println!();

    // Remove wrapper
    println!("{}", "Step 2: Removing Wrapper Script".cyan());
    println!("{}", "─".repeat(40).dimmed());
    handle_wrapper_uninstall()?;
    println!();

    println!("{}", "═".repeat(40).dimmed());
    println!("{}", "Auto-sync configuration removed.".green().bold());

    Ok(())
}

fn print_success_message(wrapper_path: &std::path::Path) -> Result<()> {
    println!("{}", "═".repeat(50).dimmed());
    println!("{}", "Auto-sync setup complete!".green().bold());
    println!("{}", "═".repeat(50).dimmed());
    println!();

    println!("{}", "How to use:".bold());
    println!();

    #[cfg(unix)]
    {
        println!(
            "  1. Use '{}' instead of 'claude' to start Claude Code",
            "claude-sync".cyan()
        );
        println!();
        println!("  2. Or add an alias to your shell profile (~/.bashrc or ~/.zshrc):");
        println!(
            "     {}",
            format!("alias claude='{}'", wrapper_path.display()).cyan()
        );
    }

    #[cfg(windows)]
    {
        let _ = wrapper_path;
        println!(
            "  1. Use '{}' instead of 'claude' to start Claude Code",
            "claude-sync".cyan()
        );
        println!();
        println!("  2. In PowerShell, you can also use:");
        println!("     {}", ".\\claude-sync.ps1".cyan());
    }

    println!();
    println!("{}", "What happens:".bold());
    println!(
        "  {} On startup: Pull latest conversation history from remote",
        "•".cyan()
    );
    println!(
        "  {} New project: Detect and pull remote history on first message",
        "•".cyan()
    );
    println!("  {} On exit: Sync conversations to remote", "•".cyan());
    println!();

    println!("{}", "Commands:".bold());
    println!(
        "  {} - Show automation status",
        format!("{} automate --status", BINARY_NAME).dimmed()
    );
    println!(
        "  {} - Remove automation",
        format!("{} automate --uninstall", BINARY_NAME).dimmed()
    );

    Ok(())
}
