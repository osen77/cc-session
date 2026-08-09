mod atomic_file;
mod codex;
mod config;
mod conflict;
mod filter;
mod handlers;
mod history;
mod interactive_conflict;
mod logger;
mod merge;
mod omp;
mod onboarding;
mod parser;
mod path_security;
mod report;
mod scm;
mod session_cache;
mod session_diagnostics;
mod session_maintenance;
mod session_model;
mod sync;
mod undo;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;
use std::path::PathBuf;

// Import all handler functions
use handlers::*;

// Import VerbosityLevel from lib
use claude_code_sync::VerbosityLevel;

// Re-export BINARY_NAME so child modules can access it via crate::BINARY_NAME
pub use claude_code_sync::BINARY_NAME;

#[derive(Parser)]
#[command(name = "ccs")]
#[command(about = "Sync Claude Code conversation history with git repositories", long_about = None)]
#[command(version)]
struct Cli {
    /// Enable debug logging for console and file output
    #[arg(long, global = true)]
    debug: bool,

    /// Override the platform log file path
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new sync repository
    Init {
        /// Local filesystem path where the sync repository will be stored
        #[arg(short, long)]
        local: Option<PathBuf>,

        /// Remote git URL for cloning or pushing (e.g., git@github.com:user/repo.git)
        #[arg(short, long)]
        remote: Option<String>,

        /// Clone from the remote URL instead of initializing a new local repo
        #[arg(long)]
        clone: bool,

        /// Path to a TOML configuration file for non-interactive setup
        #[arg(short, long)]
        config: Option<PathBuf>,
    },

    /// Push local Claude Code history to the sync repository
    Push {
        /// Commit message (optional)
        #[arg(short, long)]
        message: Option<String>,

        /// Push to remote after committing
        #[arg(long, default_value_t = true)]
        push_remote: bool,

        /// Branch to push to (default: current branch)
        #[arg(short, long)]
        branch: Option<String>,

        /// Exclude file attachments (images, etc.) from sync
        #[arg(long)]
        exclude_attachments: bool,

        /// Do not sync device configuration (settings.json, CLAUDE.md, etc.)
        #[arg(long)]
        no_config: bool,

        /// Force-delete sync-repo sessions that are missing locally (escape
        /// hatch for accidental-deletion protection). By default missing
        /// sessions are protected and kept in the repo.
        #[arg(long)]
        prune: bool,

        /// Interactive mode - preview changes and confirm before pushing
        #[arg(short, long)]
        interactive: bool,

        /// Show detailed verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Show minimal quiet output
        #[arg(short, long, conflicts_with = "verbose")]
        quiet: bool,
    },

    /// Pull and merge history from the sync repository
    Pull {
        /// Pull from remote before merging
        #[arg(long, default_value_t = true)]
        fetch_remote: bool,

        /// Branch to pull from (default: current branch)
        #[arg(short, long)]
        branch: Option<String>,

        /// Interactive mode - preview changes and confirm before pulling
        #[arg(short, long)]
        interactive: bool,

        /// Show detailed verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Show minimal quiet output
        #[arg(short, long, conflicts_with = "verbose")]
        quiet: bool,
    },

    /// Sync bidirectionally (pull then push)
    Sync {
        /// Commit message for push (optional)
        #[arg(short, long)]
        message: Option<String>,

        /// Branch to sync with (default: current branch)
        #[arg(short, long)]
        branch: Option<String>,

        /// Exclude file attachments (images, etc.) from sync
        #[arg(long)]
        exclude_attachments: bool,

        /// Force-delete sync-repo sessions that are missing locally during the
        /// push phase (escape hatch for accidental-deletion protection).
        #[arg(long)]
        prune: bool,

        /// Interactive mode - preview changes and confirm before syncing
        #[arg(short, long)]
        interactive: bool,

        /// Show detailed verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Show minimal quiet output
        #[arg(short, long, conflicts_with = "verbose")]
        quiet: bool,
    },

    /// Show sync status and conflicts
    Status {
        /// Show detailed conflict information
        #[arg(long)]
        show_conflicts: bool,

        /// Show which files would be synced
        #[arg(long)]
        show_files: bool,
    },

    /// Configure sync settings
    Config {
        /// Exclude projects older than N days
        #[arg(long)]
        exclude_older_than: Option<u32>,

        /// Include only specific project paths (comma-separated patterns)
        #[arg(long)]
        include_projects: Option<String>,

        /// Exclude specific project paths (comma-separated patterns)
        #[arg(long)]
        exclude_projects: Option<String>,

        /// Exclude file attachments (images, etc.) from sync
        #[arg(long)]
        exclude_attachments: Option<bool>,

        /// Enable Git LFS for large files
        #[arg(long)]
        enable_lfs: Option<bool>,

        /// File patterns to track with LFS (comma-separated, e.g., "*.jsonl,*.png")
        #[arg(long)]
        lfs_patterns: Option<String>,

        /// SCM backend: git or mercurial (default: git)
        #[arg(long)]
        scm_backend: Option<String>,

        /// Subdirectory within sync repo for storing projects (default: "projects")
        #[arg(long)]
        sync_subdirectory: Option<String>,

        /// Use only project name instead of full path (enables multi-device sync)
        #[arg(long)]
        use_project_name_only: Option<bool>,

        /// Show current configuration
        #[arg(long)]
        show: bool,

        /// Interactive configuration menu (select settings to modify)
        #[arg(short, long)]
        interactive: bool,

        /// Step-by-step configuration wizard
        #[arg(short, long)]
        wizard: bool,
    },

    /// View conflict reports
    Report {
        /// Output format: json or markdown
        #[arg(short, long, default_value = "markdown")]
        format: String,

        /// Output file (default: print to stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Manage git remote configuration
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },

    /// Undo the last sync operation
    Undo {
        #[command(subcommand)]
        operation: UndoOperation,

        /// Show detailed verbose output
        #[arg(short, long, global = true)]
        verbose: bool,

        /// Show minimal quiet output
        #[arg(short, long, global = true, conflicts_with = "verbose")]
        quiet: bool,
    },

    /// View and manage operation history
    History {
        #[command(subcommand)]
        action: HistoryAction,
    },

    /// Interactive setup wizard for first-time configuration
    Setup {
        /// Skip the initial sync after setup
        #[arg(long)]
        skip_sync: bool,
    },

    /// Check for updates and update to the latest version
    Update {
        /// Check for updates without installing
        #[arg(long)]
        check_only: bool,
    },

    /// Uninstall ccs and clean up all artifacts
    Uninstall {
        /// Skip confirmation prompts
        #[arg(long)]
        force: bool,
    },

    /// Clean up old snapshot files
    CleanupSnapshots {
        /// Show what would be deleted without actually deleting
        #[arg(long)]
        dry_run: bool,

        /// Maximum number of snapshots to keep per operation type
        #[arg(long, default_value_t = 5)]
        max_count: usize,

        /// Maximum age of snapshots to keep (in days)
        #[arg(long, default_value_t = 7)]
        max_age_days: i64,

        /// Interactive mode with detailed confirmation
        #[arg(short, long)]
        interactive: bool,

        /// Show detailed verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Show minimal quiet output
        #[arg(short, long, conflicts_with = "verbose")]
        quiet: bool,
    },

    /// Manage Claude Code hooks for automatic sync
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },

    /// Manage wrapper script for automatic pull on startup
    Wrapper {
        #[command(subcommand)]
        action: WrapperAction,
    },

    /// One-click setup for automatic synchronization
    Automate {
        /// Show automation configuration status
        #[arg(long)]
        status: bool,

        /// Remove all automation configuration
        #[arg(long)]
        uninstall: bool,
    },

    /// Sync Claude Code configuration files across devices
    ConfigSync {
        #[command(subcommand)]
        action: ConfigSyncAction,
    },

    /// Internal command for UserPromptSubmit hook (new project detection)
    #[command(hide = true)]
    HookNewProjectCheck,

    /// Internal command for SessionStart hook (pull on startup)
    #[command(hide = true)]
    HookSessionStart,

    /// Internal command for Stop hook (push after each response)
    #[command(hide = true)]
    HookStop,

    /// Manage Claude Code conversation sessions
    Session {
        #[command(subcommand)]
        action: Option<SessionAction>,

        /// Filter by project name (skip project selection)
        #[arg(short, long, global = true)]
        project: Option<String>,

        /// Filter by session source (all, claude, codex, omp)
        #[arg(short, long, global = true, default_value = "all")]
        source: SessionSourceArg,

        /// Include hidden and recycled maintenance entries
        #[arg(long, global = true)]
        include_hidden: bool,
    },

    /// Temporarily allow push to sync session deletions to the cloud
    UnlockDelete {
        /// Window duration in minutes (default: 15)
        #[arg(long, default_value_t = 15)]
        minutes: u64,

        /// Close the window now
        #[arg(long, conflicts_with = "status")]
        off: bool,

        /// Show remaining time / whether the window is active
        #[arg(long)]
        status: bool,
    },
}

#[derive(Subcommand)]
enum RemoteAction {
    /// Show current remote URL
    Show,

    /// Set or update remote URL
    Set {
        /// Remote name (default: origin)
        #[arg(short, long, default_value = "origin")]
        name: String,

        /// Remote URL (e.g., https://github.com/user/repo.git)
        url: String,
    },

    /// Remove remote
    Remove {
        /// Remote name (default: origin)
        #[arg(short, long, default_value = "origin")]
        name: String,
    },
}

#[derive(Subcommand)]
enum UndoOperation {
    /// Undo the last pull operation
    Pull {
        /// Preview the undo without executing it
        #[arg(long)]
        preview: bool,
    },

    /// Undo the last push operation
    Push {
        /// Preview the undo without executing it
        #[arg(long)]
        preview: bool,
    },
}

#[derive(Subcommand)]
enum HistoryAction {
    /// List recent sync operations
    List {
        /// Number of operations to show (default: 10)
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },

    /// Show details of the last operation
    Last {
        /// Filter by operation type (pull or push)
        #[arg(short = 't', long)]
        operation_type: Option<String>,
    },

    /// Interactively review and select operations to view details
    Review {
        /// Number of operations to show for selection (default: 10)
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },

    /// Clear all operation history
    Clear,
}

#[derive(Subcommand)]
enum HooksAction {
    /// Install SessionEnd and UserPromptSubmit hooks
    Install,

    /// Remove installed hooks
    Uninstall,

    /// Show current hooks configuration status
    Show,
}

#[derive(Subcommand)]
enum WrapperAction {
    /// Create wrapper script (claude-sync)
    Install {
        /// Overwrite existing wrapper script
        #[arg(long)]
        force: bool,
    },

    /// Remove wrapper script
    Uninstall,

    /// Show wrapper script path and status
    Show,
}

#[derive(Subcommand)]
enum ConfigSyncAction {
    /// Push local configuration to sync repository
    Push,

    /// List available device configurations
    List,

    /// Apply configuration from another device
    Apply {
        /// Device name to apply configuration from
        device: String,

        /// Also apply hooks configuration (check paths!)
        #[arg(long)]
        with_hooks: bool,
    },

    /// Show configuration sync status
    Status,
}

#[derive(Subcommand)]
enum SessionAction {
    /// Run or inspect automatic session maintenance
    Maintain {
        /// Enable automatic maintenance
        #[arg(long, conflicts_with_all = ["disable", "status", "dry_run", "run"])]
        enable: bool,

        /// Disable automatic maintenance
        #[arg(long, conflicts_with_all = ["enable", "status", "dry_run", "run"])]
        disable: bool,

        /// Show maintenance status without changing anything
        #[arg(long, conflicts_with_all = ["enable", "disable", "dry_run", "run"])]
        status: bool,

        /// Preview maintenance without changing state or files
        #[arg(long, conflicts_with_all = ["enable", "disable", "status", "run"])]
        dry_run: bool,

        /// Apply maintenance lifecycle and file actions now
        #[arg(long, conflicts_with_all = ["enable", "disable", "status", "dry_run"])]
        run: bool,
    },

    /// Explain maintenance lifecycle state for one session
    Explain {
        /// Session ID
        session_id: String,

        /// Output JSON
        #[arg(long)]
        json: bool,
    },

    /// Keep a session visible and protected from maintenance
    Keep { session_id: String },

    /// Remove the keep protection from a session
    Unkeep { session_id: String },

    /// Mark a session as an explicit test candidate
    MarkTest { session_id: String },

    /// Remove the explicit test marker from a session
    UnmarkTest { session_id: String },

    /// List all sessions (non-interactive output)
    List {
        /// Filter by project name
        #[arg(short, long)]
        project: Option<String>,

        /// Show session IDs
        #[arg(long)]
        show_ids: bool,
    },

    /// Search sessions and memory files by keyword (multiple words = AND match)
    Search {
        /// Search keywords (multiple words are AND-matched)
        keyword: Vec<String>,

        /// Filter by project name
        #[arg(short, long)]
        project: Option<String>,

        /// Only search sessions active within this duration (e.g., "1d", "3h", "1w")
        #[arg(long)]
        since: Option<String>,

        /// Context chars around each match (default: 100)
        #[arg(short, long, default_value_t = 100)]
        context: usize,

        /// Maximum number of match results (default: 10)
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,

        /// Search only user messages (default: both user and assistant)
        #[arg(long)]
        user_only: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Search only sessions currently visible to maintenance
        #[arg(long)]
        active_only: bool,

        /// Treat the whole query as one phrase instead of whitespace-separated keywords
        #[arg(long)]
        phrase: bool,
    },

    /// Show session details (supports drill-down with --tail/--head/--around)
    Show {
        /// Session ID
        session_id: String,

        /// Show last N messages
        #[arg(long)]
        tail: Option<usize>,

        /// Show first N messages
        #[arg(long)]
        head: Option<usize>,

        /// Show N messages around keyword match
        #[arg(long)]
        around: Option<String>,

        /// Number of messages before/after for --around (default: 5)
        #[arg(short = 'n', long, default_value_t = 5)]
        num: usize,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Show full content without truncation (default for interactive detail view)
        #[arg(long)]
        full: bool,
    },

    /// Rename session (change title)
    Rename {
        /// Session ID
        session_id: String,

        /// New title
        title: String,
    },

    /// Delete session
    Delete {
        /// Session ID
        session_id: String,

        /// Skip confirmation
        #[arg(short, long)]
        force: bool,
    },

    /// Restore sessions deleted by accident (present in sync repo, missing locally)
    Restore {
        /// Specific session ID to restore (restores all if omitted)
        session_id: Option<String>,
    },

    /// List all projects (non-interactive)
    Projects,

    /// Overview of all projects with recent session context (for agent consumption)
    Overview {
        /// Number of recent sessions per project (default: 3)
        #[arg(long, default_value_t = 3)]
        recent: usize,

        /// Only show projects active within this duration (e.g., "7d", "1w")
        #[arg(long)]
        since: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SessionSourceArg {
    All,
    Claude,
    Codex,
    Omp,
}

impl From<SessionSourceArg> for handlers::session::SessionSourceFilter {
    fn from(value: SessionSourceArg) -> Self {
        match value {
            SessionSourceArg::All => Self::All,
            SessionSourceArg::Claude => Self::Claude,
            SessionSourceArg::Codex => Self::Codex,
            SessionSourceArg::Omp => Self::Omp,
        }
    }
}

fn resolve_command(command: Option<Commands>) -> Commands {
    command.unwrap_or(Commands::Session {
        action: None,
        project: None,
        source: SessionSourceArg::All,
        include_hidden: false,
    })
}

/// Surface handler failures as JSON on stdout when the caller asked for `--json`.
/// Without this a failing `--json` run leaves stdout empty and the real message on
/// stderr, so piped consumers only see a parse error.
fn finish_json_aware(result: Result<()>, json: bool) -> Result<()> {
    use std::io::Write;

    let Err(err) = result else {
        return Ok(());
    };
    if !json {
        return Err(err);
    }

    let payload = serde_json::json!({
        "schema_version": 1,
        "error": {
            "message": err.to_string(),
            "chain": err.chain().map(|cause| cause.to_string()).collect::<Vec<_>>(),
        },
    });
    match serde_json::to_string(&payload) {
        Ok(text) => println!("{text}"),
        Err(_) => {
            println!(
                r#"{{"schema_version":1,"error":{{"message":"internal serialization failure"}}}}"#
            )
        }
    }
    // process::exit skips buffer teardown, so flush before leaving.
    let _ = std::io::stdout().flush();
    std::process::exit(1);
}

fn main() -> Result<()> {
    // Parse CLI arguments before initializing logging so logger options are available.
    let cli = Cli::parse();
    let rust_log = std::env::var("RUST_LOG").ok();
    let logger_status = logger::init_logger_with_options(logger::LoggerOptions::new(
        cli.debug,
        cli.log_file.clone(),
        rust_log.as_deref(),
    )?)?;

    if let Some(warning) = &logger_status.warning {
        eprintln!("WARNING: {warning}");
    }

    let command = resolve_command(cli.command);

    log::debug!("ccs started");

    // Local commands, including every session action, must not start a network update check.
    let is_update_command = matches!(command, Commands::Update { .. });
    let is_local_command = matches!(
        command,
        Commands::Session { .. }
            | Commands::Config { .. }
            | Commands::Status { .. }
            | Commands::Report { .. }
            | Commands::History { .. }
    );
    let update_check_handle = (!is_update_command && !is_local_command)
        .then(|| std::thread::spawn(check_for_update_silent));

    if let Some(update_check_handle) = update_check_handle {
        if let Ok(Some(new_version)) = update_check_handle.join() {
            print_update_notification(&new_version);
        }
    }

    // Check if initialization is needed (before processing any command)
    let needs_onboarding = !is_initialized()?;

    // Check if this is a command that should skip auto-onboarding
    let is_init_command = matches!(command, Commands::Init { .. });
    let is_config_command = matches!(command, Commands::Config { .. });
    let is_session_command = matches!(command, Commands::Session { .. });
    let is_setup_command = matches!(command, Commands::Setup { .. });
    let is_update_command = matches!(command, Commands::Update { .. });
    let is_uninstall_command = matches!(command, Commands::Uninstall { .. });
    let is_unlock_delete_command = matches!(command, Commands::UnlockDelete { .. });

    // Run onboarding if needed (skip for commands that don't require sync repo)
    if needs_onboarding
        && !is_init_command
        && !is_config_command
        && !is_session_command
        && !is_setup_command
        && !is_update_command
        && !is_uninstall_command
        && !is_unlock_delete_command
    {
        log::info!("Running onboarding flow - first time setup detected");

        // Try non-interactive init first (from config file)
        let mut initialized = try_init_from_config().unwrap_or(false);

        // If no config file, try to recover existing repo
        if !initialized {
            initialized = handlers::onboarding::try_recover_existing_repo().unwrap_or(false);
        }

        if !initialized {
            // Fall back to interactive setup wizard
            handle_setup(false)?;
        }

        log::info!("Onboarding completed successfully");
    }

    match command {
        Commands::Init {
            local,
            remote,
            clone,
            config,
        } => {
            // If config file is provided, use non-interactive init
            if config.is_some() {
                run_init_from_config(config)?;
            } else if clone {
                // Clone mode: requires remote URL
                let remote_url = remote.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("--clone requires --remote <URL> to be specified")
                })?;

                // Determine clone destination
                let clone_path = if let Some(ref path) = local {
                    path.clone()
                } else {
                    config::ConfigManager::default_repo_dir()?
                };

                println!(
                    "{}",
                    format!("Cloning from {} to {}...", remote_url, clone_path.display()).cyan()
                );

                scm::clone(remote_url, &clone_path)?;
                sync::init_from_onboarding(&clone_path, Some(remote_url), true)?;

                // Save default filter configuration if it doesn't exist
                let filter_config_path = config::ConfigManager::filter_config_path()?;
                if !filter_config_path.exists() {
                    filter::FilterConfig::default().save()?;
                }

                println!("{}", "Clone and initialization complete!".green().bold());
            } else if let Some(local_path) = local {
                // Use CLI args for init (local path)
                sync::init_sync_repo(&local_path, remote.as_deref())?;
            } else if let Some(remote_url) = remote {
                // Just --remote provided: clone to default location
                let default_path = config::ConfigManager::default_repo_dir()?;

                println!(
                    "{}",
                    format!(
                        "Cloning from {} to {}...",
                        remote_url,
                        default_path.display()
                    )
                    .cyan()
                );

                scm::clone(&remote_url, &default_path)?;
                sync::init_from_onboarding(&default_path, Some(&remote_url), true)?;

                // Save default filter configuration if it doesn't exist
                let filter_config_path = config::ConfigManager::filter_config_path()?;
                if !filter_config_path.exists() {
                    filter::FilterConfig::default().save()?;
                }

                println!("{}", "Clone and initialization complete!".green().bold());
            } else {
                // No args provided, try config file first, then fall back to setup wizard
                if !try_init_from_config()? {
                    handle_setup(false)?;
                }
            }
        }
        Commands::Push {
            message,
            push_remote,
            branch,
            exclude_attachments,
            no_config,
            prune,
            interactive,
            verbose,
            quiet,
        } => {
            // Determine verbosity level
            let verbosity = if verbose {
                VerbosityLevel::Verbose
            } else if quiet {
                VerbosityLevel::Quiet
            } else {
                VerbosityLevel::Normal
            };

            sync::push_history(
                message.as_deref(),
                push_remote,
                branch.as_deref(),
                exclude_attachments,
                !no_config, // sync_config = !no_config
                interactive,
                prune,
                verbosity,
            )?;
        }
        Commands::UnlockDelete {
            minutes,
            off,
            status,
        } => {
            handle_unlock_delete(minutes, off, status)?;
        }
        Commands::Pull {
            fetch_remote,
            branch,
            interactive,
            verbose,
            quiet,
        } => {
            // Determine verbosity level
            let verbosity = if verbose {
                VerbosityLevel::Verbose
            } else if quiet {
                VerbosityLevel::Quiet
            } else {
                VerbosityLevel::Normal
            };

            sync::pull_history(fetch_remote, branch.as_deref(), interactive, verbosity)?;
        }
        Commands::Sync {
            message,
            branch,
            exclude_attachments,
            prune,
            interactive,
            verbose,
            quiet,
        } => {
            // Determine verbosity level
            let verbosity = if verbose {
                VerbosityLevel::Verbose
            } else if quiet {
                VerbosityLevel::Quiet
            } else {
                VerbosityLevel::Normal
            };

            sync::sync_bidirectional(
                message.as_deref(),
                branch.as_deref(),
                exclude_attachments,
                interactive,
                prune,
                verbosity,
            )?;
        }
        Commands::Status {
            show_conflicts,
            show_files,
        } => {
            sync::show_status(show_conflicts, show_files)?;
        }
        Commands::Config {
            exclude_older_than,
            include_projects,
            exclude_projects,
            exclude_attachments,
            enable_lfs,
            lfs_patterns,
            scm_backend,
            sync_subdirectory,
            use_project_name_only,
            show,
            interactive,
            wizard,
        } => {
            // Check if ANY flag was provided
            let has_any_flag = exclude_older_than.is_some()
                || include_projects.is_some()
                || exclude_projects.is_some()
                || exclude_attachments.is_some()
                || enable_lfs.is_some()
                || lfs_patterns.is_some()
                || scm_backend.is_some()
                || sync_subdirectory.is_some()
                || use_project_name_only.is_some()
                || show
                || interactive
                || wizard;

            if !has_any_flag {
                // No args provided - show repo selector menu
                handle_repo_selector()?;
            } else if interactive {
                handle_config_interactive()?;
            } else if wizard {
                handle_config_wizard()?;
            } else if show {
                filter::show_config()?;
            } else {
                filter::update_config(
                    exclude_older_than,
                    include_projects,
                    exclude_projects,
                    exclude_attachments,
                    enable_lfs,
                    lfs_patterns,
                    scm_backend,
                    sync_subdirectory,
                    use_project_name_only,
                )?;
            }
        }
        Commands::Report { format, output } => {
            report::generate_report(&format, output.as_deref())?;
        }
        Commands::Remote { action } => match action {
            RemoteAction::Show => {
                sync::show_remote()?;
            }
            RemoteAction::Set { name, url } => {
                sync::set_remote(&name, &url)?;
            }
            RemoteAction::Remove { name } => {
                sync::remove_remote(&name)?;
            }
        },
        Commands::Undo {
            operation,
            verbose,
            quiet,
        } => {
            // Determine verbosity level
            let verbosity = if verbose {
                VerbosityLevel::Verbose
            } else if quiet {
                VerbosityLevel::Quiet
            } else {
                VerbosityLevel::Normal
            };

            match operation {
                UndoOperation::Pull { preview } => {
                    handle_undo_pull(preview, verbosity)?;
                }
                UndoOperation::Push { preview } => {
                    handle_undo_push(preview, verbosity)?;
                }
            }
        }
        Commands::History { action } => match action {
            HistoryAction::List { limit } => {
                handle_history_list(limit)?;
            }
            HistoryAction::Last { operation_type } => {
                handle_history_last(operation_type.as_deref())?;
            }
            HistoryAction::Review { limit } => {
                handle_history_review(limit)?;
            }
            HistoryAction::Clear => {
                handle_history_clear()?;
            }
        },
        Commands::Setup { skip_sync } => {
            handle_setup(skip_sync)?;
        }
        Commands::Update { check_only } => {
            handle_update(check_only)?;
        }
        Commands::Uninstall { force } => {
            handle_uninstall(force)?;
        }
        Commands::CleanupSnapshots {
            dry_run,
            max_count,
            max_age_days,
            interactive,
            verbose,
            quiet,
        } => {
            // Determine verbosity level
            let verbosity = if verbose {
                VerbosityLevel::Verbose
            } else if quiet {
                VerbosityLevel::Quiet
            } else {
                VerbosityLevel::Normal
            };

            handle_cleanup_snapshots(dry_run, max_count, max_age_days, interactive, verbosity)?;
        }
        Commands::Hooks { action } => match action {
            HooksAction::Install => {
                handle_hooks_install()?;
            }
            HooksAction::Uninstall => {
                handle_hooks_uninstall()?;
            }
            HooksAction::Show => {
                handle_hooks_show()?;
            }
        },
        Commands::Wrapper { action } => match action {
            WrapperAction::Install { force } => {
                handle_wrapper_install(force)?;
            }
            WrapperAction::Uninstall => {
                handle_wrapper_uninstall()?;
            }
            WrapperAction::Show => {
                handle_wrapper_show()?;
            }
        },
        Commands::Automate { status, uninstall } => {
            if status {
                handle_automate_status()?;
            } else if uninstall {
                handle_automate_uninstall()?;
            } else {
                handle_automate_setup()?;
            }
        }
        Commands::HookNewProjectCheck => {
            handle_new_project_check()?;
        }
        Commands::HookSessionStart => {
            handle_session_start()?;
        }
        Commands::HookStop => {
            handle_stop()?;
        }
        Commands::ConfigSync { action } => {
            let filter_config = filter::FilterConfig::load()?;
            match action {
                ConfigSyncAction::Push => {
                    handle_config_push(&filter_config.config_sync)?;
                }
                ConfigSyncAction::List => {
                    handle_config_list()?;
                }
                ConfigSyncAction::Apply { device, with_hooks } => {
                    handle_config_apply(&device, with_hooks, &filter_config.config_sync)?;
                }
                ConfigSyncAction::Status => {
                    handle_config_status(&filter_config.config_sync)?;
                }
            }
        }
        Commands::Session {
            action,
            project,
            source,
            include_hidden,
        } => {
            match action {
                Some(SessionAction::Maintain {
                    enable,
                    disable,
                    status,
                    dry_run,
                    run,
                }) => {
                    handle_session_maintain(enable, disable, status, dry_run, run, source.into())?;
                }
                Some(SessionAction::Explain { session_id, json }) => {
                    finish_json_aware(
                        handle_session_explain(&session_id, json, source.into()),
                        json,
                    )?;
                }
                Some(SessionAction::Keep { session_id }) => {
                    handle_session_keep(&session_id, true, source.into())?;
                }
                Some(SessionAction::Unkeep { session_id }) => {
                    handle_session_keep(&session_id, false, source.into())?;
                }
                Some(SessionAction::MarkTest { session_id }) => {
                    handle_session_mark_test(&session_id, true, source.into())?;
                }
                Some(SessionAction::UnmarkTest { session_id }) => {
                    handle_session_mark_test(&session_id, false, source.into())?;
                }
                None => {
                    // Interactive mode
                    handle_session_interactive(project.as_deref(), source.into(), include_hidden)?;
                }
                Some(SessionAction::List {
                    project: list_project,
                    show_ids,
                }) => {
                    // Use subcommand project filter if provided, otherwise use global
                    let filter = list_project.as_deref().or(project.as_deref());
                    handle_session_list(filter, show_ids, source.into(), include_hidden)?;
                }
                Some(SessionAction::Search {
                    keyword,
                    project: search_project,
                    since,
                    context,
                    limit,
                    user_only,
                    json,
                    active_only,
                    phrase,
                }) => {
                    let filter = search_project.as_deref().or(project.as_deref());
                    let keywords: Vec<&str> = keyword.iter().map(|s| s.as_str()).collect();
                    finish_json_aware(
                        handle_session_search(
                            &keywords,
                            filter,
                            since.as_deref(),
                            context,
                            limit,
                            user_only,
                            json,
                            active_only,
                            source.into(),
                            phrase,
                        ),
                        json,
                    )?;
                }
                Some(SessionAction::Show {
                    session_id,
                    tail,
                    head,
                    around,
                    num,
                    json,
                    full,
                }) => {
                    finish_json_aware(
                        handle_session_show(
                            &session_id,
                            tail,
                            head,
                            around.as_deref(),
                            num,
                            json,
                            full,
                            source.into(),
                        ),
                        json,
                    )?;
                }
                Some(SessionAction::Rename { session_id, title }) => {
                    handle_session_rename_with_source(&session_id, &title, source.into())?;
                }
                Some(SessionAction::Delete { session_id, force }) => {
                    handle_session_delete_with_source(&session_id, force, source.into())?;
                }
                Some(SessionAction::Restore { session_id }) => {
                    handle_session_restore_with_source(session_id.as_deref(), source.into())?;
                }
                Some(SessionAction::Projects) => {
                    handle_session_projects(source.into(), include_hidden)?;
                }
                Some(SessionAction::Overview {
                    recent,
                    since,
                    json,
                }) => {
                    finish_json_aware(
                        handle_session_overview(
                            recent,
                            since.as_deref(),
                            json,
                            source.into(),
                            include_hidden,
                        ),
                        json,
                    )?;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_command_is_interactive_session() {
        let cli = Cli::try_parse_from(["ccs"]).expect("bare ccs should parse");

        match resolve_command(cli.command) {
            Commands::Session {
                action,
                project,
                source,
                include_hidden,
            } => {
                assert!(action.is_none());
                assert!(project.is_none());
                assert_eq!(source, SessionSourceArg::All);
                assert!(!include_hidden);
            }
            _ => panic!("bare ccs should default to interactive session mode"),
        }
    }

    fn parse_session_source(args: &[&str]) -> SessionSourceArg {
        let cli = Cli::try_parse_from(args).expect("CLI should parse");
        match cli.command {
            Some(Commands::Session { source, .. }) => source,
            _ => panic!("expected session command"),
        }
    }

    #[test]
    fn test_global_logger_flags_parse_before_and_after_subcommand() {
        let before = Cli::try_parse_from([
            "ccs",
            "--debug",
            "--log-file",
            "/tmp/ccs.log",
            "session",
            "projects",
        ])
        .unwrap();
        assert!(before.debug);
        assert_eq!(before.log_file, Some(PathBuf::from("/tmp/ccs.log")));

        let after = Cli::try_parse_from([
            "ccs",
            "session",
            "projects",
            "--debug",
            "--log-file",
            "/tmp/ccs.log",
        ])
        .unwrap();
        assert!(after.debug);
        assert_eq!(after.log_file, Some(PathBuf::from("/tmp/ccs.log")));
    }

    #[test]
    fn test_session_source_before_subcommand() {
        assert_eq!(
            parse_session_source(&["ccs", "session", "--source", "codex", "list"]),
            SessionSourceArg::Codex
        );
    }

    #[test]
    fn test_session_source_after_subcommand() {
        assert_eq!(
            parse_session_source(&["ccs", "session", "list", "--source", "codex"]),
            SessionSourceArg::Codex
        );
    }

    #[test]
    fn test_session_source_defaults_to_all() {
        assert_eq!(
            parse_session_source(&["ccs", "session", "list"]),
            SessionSourceArg::All
        );
    }

    #[test]
    fn test_session_source_applies_to_mutations() {
        assert_eq!(
            parse_session_source(&[
                "ccs",
                "session",
                "--source",
                "omp",
                "delete",
                "session-id",
                "--force",
            ]),
            SessionSourceArg::Omp
        );
        assert_eq!(
            parse_session_source(&[
                "ccs",
                "session",
                "rename",
                "session-id",
                "new-title",
                "--source",
                "codex",
            ]),
            SessionSourceArg::Codex
        );
    }

    fn assert_query_actions_have_no_local_source(action: SessionAction) {
        match action {
            SessionAction::List {
                project: _,
                show_ids: _,
            } => {}
            SessionAction::Search {
                keyword: _,
                project: _,
                since: _,
                context: _,
                limit: _,
                user_only: _,
                json: _,
                active_only: _,
                phrase: _,
            } => {}
            SessionAction::Show {
                session_id: _,
                tail: _,
                head: _,
                around: _,
                num: _,
                json: _,
                full: _,
            } => {}
            SessionAction::Projects => {}
            SessionAction::Overview {
                recent: _,
                since: _,
                json: _,
            } => {}
            _ => panic!("expected a session query action"),
        }
    }

    #[test]
    fn test_session_query_actions_use_outer_source_only() {
        let cli = Cli::try_parse_from(["ccs", "session", "--source", "codex", "list"])
            .expect("CLI should parse");
        match cli.command {
            Some(Commands::Session {
                action: Some(action),
                source,
                ..
            }) => {
                assert_eq!(source, SessionSourceArg::Codex);
                assert_query_actions_have_no_local_source(action);
            }
            _ => panic!("expected session list command"),
        }
    }

    #[test]
    fn test_session_maintenance_actions_parse() {
        for flag in ["--enable", "--disable", "--status", "--dry-run", "--run"] {
            let cli = Cli::try_parse_from(["ccs", "session", "maintain", flag])
                .expect("maintenance action should parse");
            assert!(matches!(cli.command, Some(Commands::Session { .. })));
        }
    }

    #[test]
    fn test_session_maintenance_defaults_to_status() {
        let cli = Cli::try_parse_from(["ccs", "session", "maintain"])
            .expect("maintenance without action should parse");
        assert!(matches!(cli.command, Some(Commands::Session { .. })));
    }

    #[test]
    fn test_session_maintenance_actions_are_mutually_exclusive() {
        let result = Cli::try_parse_from(["ccs", "session", "maintain", "--enable", "--run"]);
        let error = match result {
            Ok(_) => panic!("maintenance actions must conflict"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn test_session_maintenance_explain_keep_and_mark_parse() {
        for args in [
            vec!["ccs", "session", "explain", "abc", "--json"],
            vec!["ccs", "session", "keep", "abc"],
            vec!["ccs", "session", "unkeep", "abc"],
            vec!["ccs", "session", "mark-test", "abc"],
            vec!["ccs", "session", "unmark-test", "abc"],
        ] {
            Cli::try_parse_from(args).expect("maintenance session action should parse");
        }
    }

    #[test]
    fn test_session_visibility_and_search_flags_parse_before_or_after_actions() {
        for args in [
            vec!["ccs", "session", "--include-hidden", "list"],
            vec!["ccs", "session", "list", "--include-hidden"],
        ] {
            Cli::try_parse_from(args).expect("include-hidden should parse globally");
        }
        let cli = Cli::try_parse_from(["ccs", "session", "search", "test", "--active-only"])
            .expect("active-only should parse");
        assert!(matches!(cli.command, Some(Commands::Session { .. })));
    }
}
