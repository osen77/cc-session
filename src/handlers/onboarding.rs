//! Onboarding and initialization handlers
//!
//! Handles first-time setup detection, non-interactive initialization from
//! config files, and recovery of existing repositories.
//!
//! The interactive setup wizard lives in `handlers/setup.rs`.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;

use crate::config;
use crate::filter;
use crate::onboarding::InitConfig;
use crate::scm;
use crate::sync;

/// Check if ccs has been initialized
pub fn is_initialized() -> Result<bool> {
    let state_path = config::ConfigManager::state_file_path()?;
    Ok(state_path.exists())
}

/// Run initialization from a config file (non-interactive).
///
/// This is used when:
/// - A config file is explicitly provided via `--config`
/// - A config file exists at a default location
/// - The environment variable `CLAUDE_CODE_SYNC_INIT_CONFIG` is set
pub fn run_init_from_config<P: AsRef<Path>>(config_path: Option<P>) -> Result<()> {
    // Load config from explicit path or default locations
    let init_config = if let Some(path) = config_path {
        log::info!("Loading init config from: {}", path.as_ref().display());
        InitConfig::load(path.as_ref())?
    } else {
        InitConfig::load_default()?.ok_or_else(|| anyhow::anyhow!("No init config file found"))?
    };

    println!("{}", "📄 Initializing from config file...".cyan().bold());

    // Convert to onboarding config
    let onboarding_config = init_config.to_onboarding_config()?;

    // Handle cloning if needed
    if onboarding_config.is_cloned {
        if let Some(ref remote_url) = onboarding_config.remote_url {
            println!("  {} {}", "Cloning from:".cyan(), remote_url);

            scm::clone(remote_url, &onboarding_config.repo_path)
                .context("Failed to clone repository")?;

            println!("{}", "  ✓ Repository cloned".green());
        }
    }

    // Initialize sync state
    sync::init_from_onboarding(
        &onboarding_config.repo_path,
        onboarding_config.remote_url.as_deref(),
        onboarding_config.is_cloned,
    )
    .context("Failed to initialize sync state")?;

    // Save filter configuration with all settings from init config
    let filter_config = filter::FilterConfig {
        exclude_attachments: init_config.exclude_attachments,
        exclude_older_than_days: init_config.exclude_older_than_days,
        enable_lfs: init_config.enable_lfs,
        scm_backend: init_config.scm_backend.clone(),
        sync_subdirectory: init_config.sync_subdirectory.clone(),
        ..Default::default()
    };
    filter_config
        .save()
        .context("Failed to save filter configuration")?;

    println!("{}", "✓ Initialization complete!".green().bold());
    println!(
        "  {} {}",
        "Repo:".cyan(),
        onboarding_config.repo_path.display()
    );
    if let Some(ref url) = onboarding_config.remote_url {
        println!("  {} {}", "Remote:".cyan(), url);
    }
    println!("  {} {}", "Backend:".cyan(), init_config.scm_backend);
    if init_config.enable_lfs {
        println!("  {} enabled", "LFS:".cyan());
    }
    println!();

    Ok(())
}

/// Try to run non-interactive initialization if a config file exists.
///
/// Returns Ok(true) if initialization was performed, Ok(false) if no config found.
pub fn try_init_from_config() -> Result<bool> {
    match InitConfig::load_default()? {
        Some(_) => {
            run_init_from_config::<&Path>(None)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Try to recover an existing repository when state.json is missing.
///
/// This scans common locations where users might have a sync repository:
/// - Default location: ~/.../claude-code-sync/repo
/// - Home directory patterns: ~/claude-*, ~/.*claude*, etc.
///
/// Returns Ok(true) if recovery was successful, Ok(false) if no repo found.
pub fn try_recover_existing_repo() -> Result<bool> {
    use crate::sync::{MultiRepoState, RepoConfig};
    use std::collections::HashMap;

    // Collect candidate paths to check
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    // 1. Default repo location
    if let Ok(default_repo) = config::ConfigManager::default_repo_dir() {
        candidates.push(default_repo);
    }

    // 2. Scan home directory for common patterns
    if let Some(home) = dirs::home_dir() {
        // Common naming patterns for sync repos
        let patterns = [
            "claude-history-backup",
            "claude-code-sync-repo",
            "claude-sync",
            "claude-backup",
            ".claude-sync",
        ];

        for pattern in &patterns {
            let path = home.join(pattern);
            if !candidates.contains(&path) {
                candidates.push(path);
            }
        }

        // Also check Documents folder
        let docs = home.join("Documents");
        if docs.exists() {
            for pattern in &patterns {
                let path = docs.join(pattern);
                if !candidates.contains(&path) {
                    candidates.push(path);
                }
            }
        }
    }

    // Check each candidate
    for repo_path in candidates {
        if !repo_path.exists() {
            continue;
        }

        // Must be a git/hg repo with a projects subdirectory
        if !scm::is_repo(&repo_path) {
            continue;
        }

        let projects_dir = repo_path.join("projects");
        if !projects_dir.exists() || !projects_dir.is_dir() {
            continue;
        }

        // Found a valid repo! Try to recover
        log::info!("Found existing sync repo at: {}", repo_path.display());

        let (has_remote, remote_url) = match scm::open(&repo_path) {
            Ok(repo) => {
                let has_remote = repo.has_remote("origin");
                let remote_url = if has_remote {
                    repo.get_remote_url("origin").ok()
                } else {
                    None
                };
                (has_remote, remote_url)
            }
            Err(_) => (false, None),
        };

        println!(
            "{} Found existing sync repository at: {}",
            "!".yellow(),
            repo_path.display()
        );
        if let Some(ref url) = remote_url {
            println!("  Remote: {}", url.cyan());
        }
        println!("  Recovering configuration...");

        // Create and save the recovered state
        let repo_config = RepoConfig {
            name: "default".to_string(),
            sync_repo_path: repo_path,
            has_remote,
            is_cloned_repo: has_remote, // Assume cloned if has remote
            remote_url,
            description: Some("Recovered from existing repository".to_string()),
        };

        let mut repos = HashMap::new();
        repos.insert("default".to_string(), repo_config);

        let state = MultiRepoState {
            version: 2,
            active_repo: "default".to_string(),
            repos,
        };

        state.save().context("Failed to save recovered state")?;

        println!("{}", "✓ Configuration recovered successfully!".green());
        println!();

        return Ok(true);
    }

    Ok(false)
}
