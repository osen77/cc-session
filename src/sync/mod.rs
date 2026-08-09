// Module declarations
pub mod delete_unlock;
pub(crate) mod discovery;
mod init;
mod pull;
mod push;
mod remote;
mod state;
mod status;
pub mod tombstone;

// Re-export public types and functions
pub use init::{init_from_onboarding, init_sync_repo};
pub use pull::pull_history;
pub use push::push_history;
pub use remote::{remove_remote, set_remote, show_remote};
pub use state::{MultiRepoState, RepoConfig, SyncState};
pub use status::show_status;

use anyhow::Result;
use colored::Colorize;

/// Maximum number of conversations to display per project in summary
const MAX_CONVERSATIONS_TO_DISPLAY: usize = 10;

/// Bidirectional sync: pull remote changes, then push local changes
pub fn sync_bidirectional(
    commit_message: Option<&str>,
    branch: Option<&str>,
    exclude_attachments: bool,
    interactive: bool,
    prune: bool,
    verbosity: crate::VerbosityLevel,
) -> Result<()> {
    use crate::VerbosityLevel;

    if verbosity != VerbosityLevel::Quiet {
        println!("{}", "=== Bidirectional Sync ===".bold().cyan());
        println!();
        println!("{}", "Step 1: Pulling remote changes...".bold());
    }

    // First, pull remote changes
    pull_history(true, branch, interactive, verbosity)?;

    if verbosity != VerbosityLevel::Quiet {
        println!();
        println!("{}", "Step 2: Pushing local changes...".bold());
    }

    // Then, push local changes (sync_config = true by default)
    push_history(
        commit_message,
        true,
        branch,
        exclude_attachments,
        true,
        interactive,
        prune,
        verbosity,
    )?;

    if verbosity == VerbosityLevel::Quiet {
        println!("Sync complete");
    } else {
        println!();
        println!("{}", "=== Sync Complete ===".green().bold());
        println!(
            "  {} Your local and remote histories are now in sync",
            "✓".green()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_DIR_ENV;
    use crate::filter::FilterConfig;
    use crate::scm;
    use serial_test::serial;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    #[serial]
    fn test_url_validation() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path().join("test-repo");

        // Initialize a test repo
        scm::init(&repo_path).unwrap();

        // Isolate config directory to avoid overwriting real state.json
        let config_dir = temp_dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::env::set_var(CONFIG_DIR_ENV, &config_dir);

        // Save a test state
        let state = SyncState {
            sync_repo_path: repo_path.clone(),
            has_remote: false,
            is_cloned_repo: false,
            last_synced_commit: None,
        };

        let state_file = crate::config::ConfigManager::state_file_path().unwrap();
        std::fs::write(&state_file, serde_json::to_string(&state).unwrap()).unwrap();

        // Valid HTTPS URL
        let result = set_remote("origin", "https://github.com/user/repo.git");
        assert!(result.is_ok());

        // Valid HTTP URL
        let result = set_remote("origin", "http://gitlab.com/user/repo.git");
        assert!(result.is_ok());

        // Valid SSH URL
        let result = set_remote("origin", "git@github.com:user/repo.git");
        assert!(result.is_ok());

        // Invalid URL (missing protocol)
        let result = set_remote("origin", "github.com/user/repo.git");
        assert!(result.is_err());
        if let Err(e) = result {
            let error_msg = e.to_string();
            assert!(error_msg.contains("Invalid URL format"));
        }

        // Cleanup
        std::env::remove_var(CONFIG_DIR_ENV);
    }

    #[test]
    fn test_filter_with_attachments() {
        let filter = FilterConfig {
            exclude_attachments: true,
            ..Default::default()
        };

        // JSONL files should be included
        assert!(filter.should_include(Path::new("session.jsonl")));
        assert!(filter.should_include(Path::new("/path/to/session.jsonl")));

        // Non-JSONL files should be excluded
        assert!(!filter.should_include(Path::new("image.png")));
        assert!(!filter.should_include(Path::new("document.pdf")));
        assert!(!filter.should_include(Path::new("archive.zip")));
        assert!(!filter.should_include(Path::new("/path/to/file.jpg")));
    }

    #[test]
    fn test_filter_without_attachments_exclusion() {
        let filter = FilterConfig::default();
        // By default, exclude_attachments is false

        // All files should be included (subject to other filters)
        assert!(filter.should_include(Path::new("session.jsonl")));
        assert!(filter.should_include(Path::new("image.png")));
        assert!(filter.should_include(Path::new("document.pdf")));
    }
}
