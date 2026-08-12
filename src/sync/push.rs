use anyhow::{Context, Result};
use colored::Colorize;
use inquire::Confirm;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ConfigManager;
use crate::filter::FilterConfig;
use crate::history::{
    ConversationSummary, OperationHistory, OperationRecord, OperationType, SyncOperation,
};
use crate::interactive_conflict;
use crate::path_security::{
    safe_join_within_root, safe_join_within_sync_projects_root, safe_project_relative_path,
    safe_relative_path_within_root, validate_directory_candidate, validate_directory_root,
    validate_regular_candidate, validate_sync_projects_root,
};
use crate::scm;
use crate::session_maintenance::state::MaintenanceState;
use crate::BINARY_NAME;

use super::discovery::{
    check_directory_structure_consistency, claude_projects_dir, discover_sessions,
    find_colliding_projects,
};
use super::state::SyncState;
use super::MAX_CONVERSATIONS_TO_DISPLAY;

/// Scan the repo worktree for jsonl files containing git conflict markers.
///
/// Called while a rebase is in progress (before aborting it), so the working
/// tree still has the conflict markers embedded.  After this scan the caller
/// should abort the rebase to restore a clean working tree.
fn find_rebase_conflict_files(repo_path: &Path) -> Vec<PathBuf> {
    let mut conflicts = Vec::new();
    let dirs_to_scan = vec![repo_path.to_path_buf()];

    for dir in dirs_to_scan {
        scan_for_conflict_files(&dir, &mut conflicts);
    }

    conflicts
}

fn scan_for_conflict_files(dir: &Path, conflicts: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            // Skip hidden directories (.git, etc.)
            let is_hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.'))
                .unwrap_or(false);
            if !is_hidden {
                scan_for_conflict_files(&path, conflicts);
            }
        } else if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if content.contains("<<<<<<<") || content.contains(">>>>>>>") {
                    log::info!("Found rebase conflict file: {}", path.display());
                    conflicts.push(path.to_path_buf());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Push orchestration types and helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum PushResult {
    Clean,
    Degraded {
        conflicts: Vec<PathBuf>,
    },
    #[allow(dead_code)]
    NothingToPush,
}

fn has_last_synced_commit_drift(
    last_synced_commit: Option<&str>,
    current_head: &str,
    is_ancestor: bool,
) -> bool {
    match last_synced_commit {
        Some(last) => last != current_head && !is_ancestor,
        None => false,
    }
}

fn git_is_ancestor(repo_path: &Path, older: &str, newer: &str) -> bool {
    std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", older, newer])
        .current_dir(repo_path)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn ensure_clean_rebase_state(repo: &dyn scm::Scm) -> Result<()> {
    if repo.is_rebase_in_progress()? {
        log::warn!("Detected stale rebase state, aborting before push");
        repo.rebase_abort()?;
    }
    Ok(())
}

/// Try to push with automatic rebase-and-retry on non-fast-forward rejection.
///
/// Loop up to 3 times: push -> if non-fast-forward, fetch & rebase -> retry.
/// Returns `Clean` on success, `Degraded` if rebase conflicts were encountered,
/// or `NothingToPush` if there was nothing to push.
fn push_with_rebase_auto_heal(
    repo: &dyn scm::Scm,
    repo_path: &Path,
    state: &mut SyncState,
    branch_name: &str,
    verbosity: crate::VerbosityLevel,
) -> Result<PushResult> {
    ensure_clean_rebase_state(repo)?;

    // Drift detection
    let current_head = repo.current_commit_hash().ok();
    if let (Some(last), Some(head)) = (state.last_synced_commit.as_deref(), current_head.as_deref())
    {
        let drift =
            has_last_synced_commit_drift(Some(last), head, git_is_ancestor(repo_path, last, head));
        if drift {
            log::warn!("Detected sync drift before push; auto-heal path will be used if needed");
        }
    }

    // Bounded retry loop (max 3)
    for attempt in 1..=3 {
        match repo.push_classified("origin", branch_name) {
            Ok(()) => {
                state.last_synced_commit = repo.current_commit_hash().ok();
                state.save()?;
                if verbosity != crate::VerbosityLevel::Quiet && attempt > 1 {
                    println!(
                        "  {} Rebased and pushed on attempt {}",
                        "✓".green(),
                        attempt
                    );
                }
                return Ok(PushResult::Clean);
            }
            Err(scm::PushError::NonFastForward) => {
                repo.fetch("origin")?;
                match repo.rebase(&format!("origin/{branch_name}"))? {
                    scm::RebaseOutcome::Completed => continue,
                    scm::RebaseOutcome::InProgress => {
                        // Scan for conflict markers while the rebase is still
                        // in progress (aborting would remove them from disk).
                        let conflicts = find_rebase_conflict_files(repo_path);
                        repo.rebase_abort()?;
                        return Ok(PushResult::Degraded { conflicts });
                    }
                }
            }
            Err(scm::PushError::Other(e)) => return Err(e.context("Push failed")),
        }
    }
    Err(anyhow::anyhow!(
        "Remote remained busy after 3 push attempts"
    ))
}

/// How to handle sessions present in the sync repo but missing locally.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MissingAction {
    /// Keep them in the repo (accidental-loss protection).
    Protect,
    /// User passed `--prune`: physical sync, "Pruned N" wording.
    PruneManual,
    /// Delete-unlock window active: prune + 🔓 wording. Carries remaining minutes.
    PruneUnlock(u64),
}

/// Decide the action for locally-missing sessions.
/// Explicit `--prune` always wins over the window (and keeps the plain wording).
pub(crate) fn decide_missing_action(prune: bool, unlock_remaining: Option<u64>) -> MissingAction {
    if prune {
        MissingAction::PruneManual
    } else if let Some(secs) = unlock_remaining {
        MissingAction::PruneUnlock(secs / 60)
    } else {
        MissingAction::Protect
    }
}

/// Scan the sync repo's project dirs and return sessions that exist in the
/// repo but are missing locally.
///
/// Only sync-repo project dirs that have a corresponding local project are
/// considered, so sessions pushed by other devices for projects absent on
/// this machine are never flagged. The two layout modes
/// (`use_project_name_only` vs full-path) share the same collection logic;
/// only the local-file grouping differs.
fn collect_missing_repo_sessions(
    projects_dir: &Path,
    filter: &FilterConfig,
    sessions: &[crate::parser::ConversationSession],
    local_files_by_project: &HashMap<String, std::collections::HashSet<String>>,
) -> Vec<PathBuf> {
    let mut missing = Vec::new();

    // Return only validated relative paths. Callers must reconstruct the
    // destination under the guarded projects root before deleting anything.
    let mut collect_files = |sync_project_dir: &Path,
                             local_files: &std::collections::HashSet<String>,
                             project_name: &str| {
        let Ok(project_metadata) = fs::symlink_metadata(sync_project_dir) else {
            return;
        };
        if project_metadata.file_type().is_symlink() || !project_metadata.is_dir() {
            return;
        }
        let Ok(files) = fs::read_dir(sync_project_dir) else {
            return;
        };
        for file in files.filter_map(|f| f.ok()) {
            let file_path = file.path();
            let Ok(file_name) = file.file_name().into_string() else {
                continue;
            };
            if !file_name.ends_with(".jsonl") || local_files.contains(&file_name) {
                continue;
            }
            if validate_regular_candidate(projects_dir, &file_path).is_err() {
                continue;
            }
            if let Ok(relative) = safe_relative_path_within_root(projects_dir, &file_path) {
                if relative.starts_with(project_name) {
                    missing.push(relative);
                }
            }
        }
    };

    if filter.use_project_name_only {
        let mut local_files_by_name: HashMap<String, std::collections::HashSet<String>> =
            HashMap::new();
        let mut project_name_has_local = std::collections::HashSet::new();

        for session in sessions {
            if let Some(pname) = session.project_name() {
                project_name_has_local.insert(pname.to_string());
                let fname = Path::new(&session.file_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                local_files_by_name
                    .entry(pname.to_string())
                    .or_default()
                    .insert(fname);
            }
        }

        let Ok(entries) = fs::read_dir(projects_dir) else {
            return missing;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let sync_project_dir = entry.path();
            let Some(project_name) = sync_project_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !project_name_has_local.contains(project_name) {
                continue;
            }
            let local_files = local_files_by_name
                .get(project_name)
                .cloned()
                .unwrap_or_default();
            collect_files(&sync_project_dir, &local_files, project_name);
        }
    } else {
        let Ok(entries) = fs::read_dir(projects_dir) else {
            return missing;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let sync_project_dir = entry.path();
            let Some(dir_name) = sync_project_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(local_files) = local_files_by_project.get(dir_name) else {
                continue;
            };
            collect_files(&sync_project_dir, local_files, dir_name);
        }
    }

    missing
}

fn partition_missing_repo_sessions(
    missing: &[PathBuf],
    maintenance: Option<&MaintenanceState>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    missing.iter().cloned().partition(|relative| {
        maintenance.is_some_and(|state| state.is_suppressed_missing_session(relative))
    })
}

fn missing_for_action(
    missing: &[PathBuf],
    maintenance: Option<&MaintenanceState>,
    action: &MissingAction,
) -> Vec<PathBuf> {
    let (suppressed, ordinary) = partition_missing_repo_sessions(missing, maintenance);
    match action {
        MissingAction::Protect => Vec::new(),
        MissingAction::PruneUnlock(_) => ordinary,
        MissingAction::PruneManual => suppressed.into_iter().chain(ordinary).collect(),
    }
}

fn prune_missing_repo_sessions(
    sync_repo_path: &Path,
    projects_dir: &Path,
    missing: &[PathBuf],
) -> Result<usize> {
    let _ = validate_sync_projects_root(sync_repo_path, projects_dir)?;
    let mut deleted = 0;
    for relative in missing {
        let file_path =
            safe_join_within_sync_projects_root(sync_repo_path, projects_dir, relative)?;
        let canonical_root = validate_sync_projects_root(sync_repo_path, projects_dir)?;
        validate_regular_candidate(&canonical_root, &file_path)?;
        fs::remove_file(&file_path)
            .with_context(|| format!("failed to prune session: {}", relative.display()))?;
        deleted += 1;
    }
    Ok(deleted)
}

fn sync_auto_memory_directories(
    sync_repo_path: &Path,
    projects_dir: &Path,
    local_projects_root: &Path,
    project_dir_to_sync: &HashMap<PathBuf, PathBuf>,
) -> Result<(usize, usize)> {
    validate_sync_projects_root(sync_repo_path, projects_dir)?;
    validate_directory_root(local_projects_root)?;

    let mut synced_count = 0;
    let mut local_memory_by_sync: HashMap<PathBuf, std::collections::HashSet<std::ffi::OsString>> =
        HashMap::new();

    for (local_dir, sync_project) in project_dir_to_sync {
        validate_directory_candidate(local_projects_root, local_dir)?;
        let local_memory = local_dir.join("memory");
        let local_memory_metadata = match fs::symlink_metadata(&local_memory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if local_memory_metadata.file_type().is_symlink() {
            anyhow::bail!("local auto-memory path must not be a symlink");
        }
        if !local_memory_metadata.is_dir() {
            continue;
        }
        validate_directory_candidate(local_projects_root, &local_memory)?;

        let memory_relative = sync_project.join("memory");
        let dest_memory_dir =
            safe_join_within_sync_projects_root(sync_repo_path, projects_dir, &memory_relative)?;
        fs::create_dir_all(&dest_memory_dir)?;

        // Recheck the created directory and all of its parents before reading
        // or writing below it.
        let dest_memory_dir =
            safe_join_within_sync_projects_root(sync_repo_path, projects_dir, &memory_relative)?;
        let memory_metadata = fs::symlink_metadata(&dest_memory_dir)?;
        if memory_metadata.file_type().is_symlink() || !memory_metadata.is_dir() {
            anyhow::bail!(
                "auto-memory destination is not a regular directory: {}",
                memory_relative.display()
            );
        }

        let file_set = local_memory_by_sync
            .entry(sync_project.clone())
            .or_default();
        for entry in fs::read_dir(&local_memory)? {
            let entry = entry?;
            let source_path = entry.path();
            let source_metadata = fs::symlink_metadata(&source_path)?;
            if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
                continue;
            }

            let file_name = entry.file_name();
            file_set.insert(file_name.clone());
            let relative = memory_relative.join(&file_name);
            let destination =
                safe_join_within_sync_projects_root(sync_repo_path, projects_dir, &relative)?;
            if let Ok(metadata) = fs::symlink_metadata(&destination) {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    anyhow::bail!(
                        "auto-memory destination is not a regular file: {}",
                        relative.display()
                    );
                }
            }

            // Final fail-safe revalidation immediately before copy.
            let destination =
                safe_join_within_sync_projects_root(sync_repo_path, projects_dir, &relative)?;
            if let Ok(metadata) = fs::symlink_metadata(&destination) {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    anyhow::bail!(
                        "auto-memory destination is not a regular file: {}",
                        relative.display()
                    );
                }
            }
            fs::copy(&source_path, &destination)?;
        }

        synced_count += 1;
    }

    let mut deleted_memory_count = 0;
    for (sync_project, local_files) in &local_memory_by_sync {
        let memory_relative = sync_project.join("memory");
        let remote_memory =
            safe_join_within_sync_projects_root(sync_repo_path, projects_dir, &memory_relative)?;
        let metadata = match fs::symlink_metadata(&remote_memory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!(
                "auto-memory destination is not a regular directory: {}",
                memory_relative.display()
            );
        }

        for entry in fs::read_dir(&remote_memory)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path)?;
            if metadata.file_type().is_symlink() {
                anyhow::bail!(
                    "auto-memory destination contains a symlink: {}",
                    file_name.to_string_lossy()
                );
            }
            if !metadata.is_file() || local_files.contains(&file_name) {
                continue;
            }

            let relative = memory_relative.join(&file_name);
            let canonical_root = validate_sync_projects_root(sync_repo_path, projects_dir)?;
            let candidate =
                safe_join_within_sync_projects_root(sync_repo_path, projects_dir, &relative)?;
            validate_regular_candidate(&canonical_root, &candidate)?;
            // Final fail-safe revalidation immediately before unlink.
            let canonical_root = validate_sync_projects_root(sync_repo_path, projects_dir)?;
            let candidate =
                safe_join_within_sync_projects_root(sync_repo_path, projects_dir, &relative)?;
            validate_regular_candidate(&canonical_root, &candidate)?;
            fs::remove_file(candidate)?;
            deleted_memory_count += 1;
        }
    }

    Ok((synced_count, deleted_memory_count))
}

/// Push local Claude Code history to sync repository
///
/// `prune` controls the accidental-deletion policy:
/// - `false` (default): sessions present in the sync repo but missing locally
///   are treated as accidental loss and **protected** — they are kept in the
///   repo and a warning is printed. Use `ccs restore` to recover them.
/// - `true`: the missing sessions are force-deleted from the repo (physical
///   prune), which is the escape hatch for users who deliberately removed
///   files outside `ccs` and want the deletion propagated.
#[allow(clippy::too_many_arguments)]
pub fn push_history(
    commit_message: Option<&str>,
    push_remote: bool,
    branch: Option<&str>,
    exclude_attachments: bool,
    sync_config: bool,
    interactive: bool,
    prune: bool,
    verbosity: crate::VerbosityLevel,
) -> Result<()> {
    use crate::VerbosityLevel;

    if verbosity != VerbosityLevel::Quiet {
        println!("{}", "Pushing Claude Code history...".cyan().bold());
    }

    let mut state = SyncState::load()?;

    // Serialize against any other process writing this repository. Held for the
    // rest of the function; a busy repository means someone else is already
    // syncing, so skipping is correct and must not be reported as a failure.
    let Some(_repo_lock) = crate::sync::repo_lock::RepoLock::acquire_or_report(
        &state.sync_repo_path,
        "推送",
        verbosity,
    )?
    else {
        return Ok(());
    };

    let repo = scm::open(&state.sync_repo_path)?;
    let mut filter = FilterConfig::load()?;

    // Override exclude_attachments if specified in command
    if exclude_attachments {
        filter.exclude_attachments = true;
    }

    // Set up LFS if enabled
    if filter.enable_lfs {
        if verbosity != VerbosityLevel::Quiet {
            println!("  {} Git LFS...", "Configuring".cyan());
        }
        scm::lfs::setup(&state.sync_repo_path, &filter.lfs_patterns)
            .context("Failed to set up Git LFS")?;
    }

    let claude_dir = claude_projects_dir()?;
    validate_directory_root(&claude_dir)?;

    // Check directory structure consistency before pushing. The projects root
    // is an untrusted checkout boundary: it must be a real directory inside
    // the sync repository, never a symlink to an external tree.
    let projects_dir = state.sync_repo_path.join(&filter.sync_subdirectory);
    if !projects_dir.exists() {
        fs::create_dir_all(&projects_dir)?;
    }
    let projects_dir = validate_sync_projects_root(&state.sync_repo_path, &projects_dir)?;
    {
        let structure_check =
            check_directory_structure_consistency(&projects_dir, filter.use_project_name_only);

        if !structure_check.is_consistent {
            if let Some(warning) = &structure_check.warning {
                if verbosity != VerbosityLevel::Quiet {
                    println!();
                    println!("{}", "⚠️  目录结构不一致警告".yellow().bold());
                    println!("{}", "─".repeat(50).dimmed());
                    println!("{}", warning.yellow());
                    println!();
                }

                if interactive && interactive_conflict::is_interactive() {
                    let proceed = Confirm::new("是否继续推送？")
                        .with_default(false)
                        .with_help_message("建议先清理目录结构再继续")
                        .prompt()
                        .context("取消确认")?;

                    if !proceed {
                        println!("\n{}", "推送已取消。".yellow());
                        println!(
                            "提示：使用 '{}' 可以切换同步模式",
                            format!(
                                "{} config --use-project-name-only <true|false>",
                                BINARY_NAME
                            )
                            .cyan()
                        );
                        return Ok(());
                    }
                } else if verbosity != VerbosityLevel::Quiet {
                    println!(
                        "{}",
                        "使用 --interactive 选项可以在不一致时选择是否继续".dimmed()
                    );
                }
            }
        }
    }

    // Get the current branch name for operation record
    let branch_name = branch
        .map(|s| s.to_string())
        .or_else(|| repo.current_branch().ok())
        .unwrap_or_else(|| "main".to_string());

    // Discover all sessions
    if verbosity != VerbosityLevel::Quiet {
        println!("  {} conversation sessions...", "Discovering".cyan());
    }
    let sessions = discover_sessions(&claude_dir, &filter)?;
    if verbosity != VerbosityLevel::Quiet {
        println!("  {} {} sessions", "Found".green(), sessions.len());
    }

    // Check for project name collisions when using project-name-only mode
    if filter.use_project_name_only {
        let collisions = find_colliding_projects(&claude_dir);
        if !collisions.is_empty() && verbosity != VerbosityLevel::Quiet {
            println!();
            println!(
                "{}",
                "Warning: Multiple projects map to the same name:"
                    .yellow()
                    .bold()
            );
            for (name, paths) in &collisions {
                println!("  {} -> {} locations:", name.cyan(), paths.len());
                for path in paths.iter().take(3) {
                    let display_path = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown");
                    println!("    - {}", display_path);
                }
                if paths.len() > 3 {
                    println!("    ... and {} more", paths.len() - 3);
                }
            }
            println!();
            println!(
                "{}",
                "Sessions from colliding projects will be merged into the same directory.".yellow()
            );
            println!();
        }
    }

    // ============================================================================
    // COPY SESSIONS AND TRACK CHANGES
    // ============================================================================
    // Note: projects_dir was validated above as the trusted sync root.

    // Discover existing sessions in sync repo to determine operation type
    if verbosity != VerbosityLevel::Quiet {
        println!("  {} sessions to sync repository...", "Copying".cyan());
    }
    let existing_sessions = discover_sessions(&projects_dir, &filter)?;
    let existing_map: HashMap<_, _> = existing_sessions
        .iter()
        .map(|s| (s.session_id.clone(), s))
        .collect();

    // Track pushed conversations for operation record
    let mut pushed_conversations: Vec<ConversationSummary> = Vec::new();
    let mut added_count = 0;
    let mut modified_count = 0;
    let mut unchanged_count = 0;

    // Track sessions skipped due to missing cwd
    let mut skipped_no_cwd = 0;

    // Mapping from local project dir -> sync repo project dir (for memory sync)
    let mut project_dir_to_sync: HashMap<PathBuf, PathBuf> = HashMap::new();

    // Closure to compute the relative path for a session, respecting use_project_name_only
    let compute_relative_path =
        |session: &crate::parser::ConversationSession| -> Result<Option<PathBuf>> {
            if filter.use_project_name_only {
                let filename = Path::new(&session.file_path)
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("session file has no filename"))?;
                let Some(project_name) = session.project_name() else {
                    return Ok(None);
                };
                Ok(Some(safe_project_relative_path(project_name, filename)?))
            } else {
                Ok(Some(safe_relative_path_within_root(
                    &claude_dir,
                    Path::new(&session.file_path),
                )?))
            }
        };

    for session in &sessions {
        let relative_path = match compute_relative_path(session)? {
            Some(path) => path,
            None => {
                skipped_no_cwd += 1;
                log::debug!("Skipping session {} (no cwd)", session.session_id);
                continue;
            }
        };

        // Build project dir mapping for memory sync (amortized during session loop)
        if let Some(sync_project_dir) = relative_path.parent() {
            if !sync_project_dir.as_os_str().is_empty() {
                let local_project_dir = Path::new(&session.file_path)
                    .parent()
                    .unwrap_or(Path::new(""));
                project_dir_to_sync
                    .entry(local_project_dir.to_path_buf())
                    .or_insert_with(|| sync_project_dir.to_path_buf());
            }
        }

        let dest_path = safe_join_within_root(&projects_dir, &relative_path)?;

        // Determine operation type based on existing state
        let operation = if let Some(existing) = existing_map.get(&session.session_id) {
            if existing.content_hash() == session.content_hash() {
                unchanged_count += 1;
                SyncOperation::Unchanged
            } else {
                modified_count += 1;
                SyncOperation::Modified
            }
        } else {
            added_count += 1;
            SyncOperation::Added
        };

        // Write the session file
        session.write_to_file(&dest_path)?;

        // Track this session in pushed conversations
        let relative_path_str = relative_path.to_string_lossy().to_string();
        match ConversationSummary::new(
            session.session_id.clone(),
            relative_path_str.clone(),
            session.latest_timestamp(),
            session.message_count(),
            operation,
        ) {
            Ok(summary) => pushed_conversations.push(summary),
            Err(e) => log::warn!("Failed to create summary for {}: {}", relative_path_str, e),
        }
    }

    // ============================================================================
    // SHOW SUMMARY AND INTERACTIVE CONFIRMATION
    // ============================================================================
    if verbosity != VerbosityLevel::Quiet {
        println!();
        println!("{}", "Push Summary:".bold().cyan());
        println!("  {} Added: {}", "•".green(), added_count);
        println!("  {} Modified: {}", "•".yellow(), modified_count);
        println!("  {} Unchanged: {}", "•".dimmed(), unchanged_count);
        let total_with_cwd = sessions.len().saturating_sub(skipped_no_cwd);
        println!("  {} Skipped (no cwd): {}", "•".dimmed(), skipped_no_cwd);
        println!(
            "  {} Sessions (with project context): {}",
            "•".cyan(),
            total_with_cwd
        );
        println!();
    }

    // Show detailed file list in verbose mode
    if verbosity == VerbosityLevel::Verbose {
        println!("{}", "Files to be pushed:".bold());
        for (idx, session) in sessions.iter().enumerate().take(20) {
            let Some(relative_path) = compute_relative_path(session)? else {
                continue;
            };

            let status = if let Some(existing) = existing_map.get(&session.session_id) {
                if existing.content_hash() == session.content_hash() {
                    "unchanged".dimmed()
                } else {
                    "modified".yellow()
                }
            } else {
                "new".green()
            };

            println!("  {}. {} [{}]", idx + 1, relative_path.display(), status);
        }
        if sessions.len() > 20 {
            println!("  ... and {} more", sessions.len() - 20);
        }
        println!();
    }

    // Interactive confirmation
    if interactive && interactive_conflict::is_interactive() {
        let confirm = Confirm::new("Do you want to proceed with pushing these changes?")
            .with_default(true)
            .with_help_message("This will commit and push to the sync repository")
            .prompt()
            .context("Failed to get confirmation")?;

        if !confirm {
            println!("\n{}", "Push cancelled.".yellow());
            return Ok(());
        }
    }

    // ============================================================================
    // SYNC DEVICE CONFIGURATION (if enabled)
    // ============================================================================
    if sync_config && filter.config_sync.enabled && filter.config_sync.push_with_config {
        if verbosity != VerbosityLevel::Quiet {
            println!();
            println!("  {} device configuration...", "Syncing".cyan());
        }

        // Use config_sync handler to push configuration files (no commit)
        match crate::handlers::config_sync::push_config_files(&filter.config_sync) {
            Ok(synced_files) => {
                if !synced_files.is_empty() {
                    if verbosity != VerbosityLevel::Quiet {
                        println!("  {} Device configuration synced:", "✓".green());
                        for file in &synced_files {
                            println!("    - {}", file.dimmed());
                        }
                    }
                } else if verbosity == VerbosityLevel::Verbose {
                    println!("  {} No configuration files to sync", "ℹ".dimmed());
                }
            }
            Err(e) => {
                log::warn!("Failed to sync device configuration: {}", e);
                if verbosity != VerbosityLevel::Quiet {
                    println!(
                        "  {} Failed to sync device configuration: {}",
                        "⚠".yellow(),
                        e
                    );
                }
            }
        }
    }

    // ============================================================================
    // DETECT LOCALLY-MISSING SESSIONS IN SYNC REPO
    // ============================================================================
    // Compare sync repo files against local files to find sessions that exist
    // in the repo but are missing locally. Only consider sync-repo project
    // dirs that have a corresponding local project dir — this prevents
    // touching sessions pushed by other devices for projects absent here.
    //
    // These missing sessions are either:
    //   * accidental local loss → protected by default (kept in repo),
    //   * force-pruned when `--prune` is set.
    let mut deleted_from_repo = 0;

    let missing_in_repo: Vec<PathBuf> = {
        // Build a set of local session file names grouped by project dir name
        // (the encoded directory name under ~/.claude/projects/)
        let mut local_files_by_project: HashMap<String, std::collections::HashSet<String>> =
            HashMap::new();

        if let Ok(entries) = fs::read_dir(&claude_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let local_project_dir = entry.path();
                if !local_project_dir.is_dir() {
                    continue;
                }
                let dir_name = local_project_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                if dir_name.starts_with('.') {
                    continue;
                }

                let mut file_names = std::collections::HashSet::new();
                if let Ok(files) = fs::read_dir(&local_project_dir) {
                    for file in files.filter_map(|f| f.ok()) {
                        if let Some(name) = file.file_name().to_str() {
                            if name.ends_with(".jsonl") {
                                file_names.insert(name.to_string());
                            }
                        }
                    }
                }
                local_files_by_project.insert(dir_name, file_names);
            }
        }

        collect_missing_repo_sessions(&projects_dir, &filter, &sessions, &local_files_by_project)
    };

    // Delete-unlock window: when active, treat ordinary locally-missing sessions as
    // intentional deletions (same as --prune, no tombstone). Maintenance-suppressed
    // Claude sessions remain protected unless the user explicitly passes --prune.
    let unlock_remaining = crate::sync::delete_unlock::status().ok().flatten();
    let action = decide_missing_action(prune, unlock_remaining);
    let maintenance_state = if missing_in_repo.is_empty() {
        None
    } else {
        match ConfigManager::config_dir().and_then(|config_dir| {
            crate::session_maintenance::state::StateStore::from_config_dir(&config_dir).load()
        }) {
            Ok(state) => Some(state),
            Err(error) => {
                if matches!(action, MissingAction::PruneUnlock(_)) {
                    return Err(error).context(
                        "maintenance state is unavailable; refusing delete-unlock prune. "
                            .to_string()
                            + "Repair the state file or use explicit --prune",
                    );
                }
                log::warn!(
                    "Failed to load maintenance state; continuing without suppression classification: {}",
                    error
                );
                None
            }
        }
    };
    let (_suppressed_missing, ordinary_missing) =
        partition_missing_repo_sessions(&missing_in_repo, maintenance_state.as_ref());

    if missing_in_repo.is_empty() {
        // Nothing missing locally — no protection or pruning needed.
    } else {
        match action {
            MissingAction::PruneManual | MissingAction::PruneUnlock(_) => {
                let actionable_missing =
                    missing_for_action(&missing_in_repo, maintenance_state.as_ref(), &action);
                // Physical sync of the deletion. No tombstone is written —
                // prune/window are physical syncs, not intentional-delete
                // registrations.
                match prune_missing_repo_sessions(
                    &state.sync_repo_path,
                    &projects_dir,
                    &actionable_missing,
                ) {
                    Ok(count) => {
                        deleted_from_repo = count;
                        for relative in &actionable_missing {
                            log::debug!("Pruned missing session: {}", relative.display());
                        }
                    }
                    Err(error) => {
                        log::warn!("Failed to prune missing sessions safely: {}", error);
                    }
                }
                if verbosity != VerbosityLevel::Quiet {
                    match action {
                        MissingAction::PruneUnlock(mins) => {
                            println!(
                                "  {} 删除放行窗口生效中，已同步删除 {} 个 session（剩余 {} 分钟）",
                                "🔓".yellow(),
                                deleted_from_repo,
                                mins
                            );
                        }
                        _ => {
                            println!(
                                "  {} Pruned {} missing sessions from sync repo",
                                "✓".green(),
                                deleted_from_repo
                            );
                        }
                    }
                }
            }
            MissingAction::Protect => {
                // Protection mode: refuse to propagate the local absence. The
                // repo keeps these sessions so they survive as a recoverable
                // backup. Suppressed maintenance sessions are intentionally
                // omitted from this warning because their absence is expected.
                if !ordinary_missing.is_empty() && verbosity != VerbosityLevel::Quiet {
                    println!(
                        "  {} Detected {} session(s) missing locally but present in sync repo — protected from deletion.",
                        "⚠".yellow(),
                        ordinary_missing.len()
                    );
                    println!(
                        "    {} Use '{}' to recover them, or '{}' to force-delete.",
                        "→".cyan(),
                        format!("{} session restore", BINARY_NAME).cyan(),
                        format!("{} push --prune", BINARY_NAME).cyan()
                    );
                }
                if !ordinary_missing.is_empty() {
                    log::info!(
                        "Protected {} missing sessions from deletion (use --prune or unlock-delete to force)",
                        ordinary_missing.len()
                    );
                }
            }
        }
    }

    // ============================================================================
    // SYNC AUTO MEMORY DIRECTORIES
    // ============================================================================
    if filter.auto_memory.enabled {
        if verbosity != VerbosityLevel::Quiet {
            println!();
            println!("  {} auto memory directories...", "Syncing".cyan());
        }

        let (synced_count, deleted_memory_count) = sync_auto_memory_directories(
            &state.sync_repo_path,
            &projects_dir,
            &claude_dir,
            &project_dir_to_sync,
        )?;

        if synced_count > 0 {
            if verbosity != VerbosityLevel::Quiet {
                println!(
                    "  {} Synced {} memory directories",
                    "✓".green(),
                    synced_count
                );
            }
        } else if verbosity == VerbosityLevel::Verbose {
            println!("  {} No memory directories found", "ℹ".dimmed());
        }

        if deleted_memory_count > 0 && verbosity != VerbosityLevel::Quiet {
            println!(
                "  {} Removed {} deleted memory files from sync repo",
                "✓".green(),
                deleted_memory_count
            );
        }
    }

    // ============================================================================
    // COMMIT AND PUSH CHANGES
    // ============================================================================
    repo.stage_all()?;

    let has_changes = repo.has_changes()?;
    if has_changes {
        // Get the current commit hash before making any changes
        // This allows us to undo the push later by resetting to this commit
        // Note: We don't create file snapshots for push - git already has history!
        // Undo push simply does `git reset` to this commit.
        // On a brand new repo with no commits, this will be None (no undo available for first push)
        let commit_before_push = repo.current_commit_hash().ok();

        if let Some(ref hash) = commit_before_push {
            if verbosity != VerbosityLevel::Quiet {
                println!("  {} Recorded commit {} for undo", "✓".green(), &hash[..8]);
            }
        } else if verbosity != VerbosityLevel::Quiet {
            println!(
                "  {} First push - no previous commit to undo to",
                "ℹ".cyan()
            );
        }

        let default_message = format!(
            "Sync {} sessions at {}",
            sessions.len(),
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        );
        let message = commit_message.unwrap_or(&default_message);

        if verbosity != VerbosityLevel::Quiet {
            println!("  {} changes...", "Committing".cyan());
        }
        repo.commit(message)?;
        if verbosity != VerbosityLevel::Quiet {
            println!("  {} Committed: {}", "✓".green(), message);
        }

        // Track whether push failed so we can propagate the error
        // after saving the operation record (undo information).
        let mut push_error: Option<anyhow::Error> = None;

        // Push to remote if configured
        if push_remote && state.has_remote {
            if verbosity != VerbosityLevel::Quiet {
                println!("  {} to remote...", "Pushing".cyan());
            }

            let repo_path = state.sync_repo_path.clone();
            match push_with_rebase_auto_heal(
                repo.as_ref(),
                &repo_path,
                &mut state,
                &branch_name,
                verbosity,
            ) {
                Ok(PushResult::Clean) => {
                    if verbosity != VerbosityLevel::Quiet {
                        println!("  {} Pushed to origin/{}", "✓".green(), branch_name);
                    }
                }
                Ok(PushResult::Degraded { conflicts }) => {
                    if verbosity != VerbosityLevel::Quiet {
                        println!(
                            "  {} Push degraded; kept {} conflict file(s)",
                            "⚠".yellow(),
                            conflicts.len()
                        );
                    }
                }
                Ok(PushResult::NothingToPush) => {}
                Err(e) => {
                    log::warn!("Failed to push: {}", e);
                    if verbosity != VerbosityLevel::Quiet {
                        println!("  {} Failed to push: {}", "⚠".yellow(), e);
                    }
                    push_error = Some(e);
                }
            }
        }

        // ============================================================================
        // CREATE AND SAVE OPERATION RECORD
        // ============================================================================
        let mut operation_record = OperationRecord::new(
            OperationType::Push,
            Some(branch_name.clone()),
            pushed_conversations.clone(),
        );

        // Store commit hash for undo (no file snapshot needed - git has history)
        // On first push (no prior commits), this will be None
        operation_record.commit_hash = commit_before_push;

        // Load operation history and add this operation
        let mut history = match OperationHistory::load() {
            Ok(h) => h,
            Err(e) => {
                log::warn!("Failed to load operation history: {}", e);
                log::info!("Creating new history...");
                OperationHistory::default()
            }
        };

        if let Err(e) = history.add_operation(operation_record) {
            log::warn!("Failed to save operation to history: {}", e);
            log::info!("Push completed successfully, but history was not updated.");
        }

        // If push failed, propagate the error so the process exits with non-zero code.
        // The operation record is already saved above, preserving undo capability.
        if let Some(e) = push_error {
            return Err(e);
        }
    } else if verbosity != VerbosityLevel::Quiet {
        println!("  {} No changes to commit", "Note:".yellow());
    }

    // ============================================================================
    // DISPLAY SUMMARY TO USER
    // ============================================================================
    if verbosity != VerbosityLevel::Quiet {
        println!("\n{}", "=== Push Summary ===".bold().cyan());

        // Show operation statistics
        let stats_msg = if deleted_from_repo > 0 {
            format!(
                "  {} Added    {} Modified    {} Deleted    {} Unchanged",
                format!("{added_count}").green(),
                format!("{modified_count}").cyan(),
                format!("{deleted_from_repo}").red(),
                format!("{unchanged_count}").dimmed(),
            )
        } else {
            format!(
                "  {} Added    {} Modified    {} Unchanged",
                format!("{added_count}").green(),
                format!("{modified_count}").cyan(),
                format!("{unchanged_count}").dimmed(),
            )
        };
        println!("{stats_msg}");
        println!();

        // Group conversations by project (top-level directory)
        let mut by_project: HashMap<String, Vec<&ConversationSummary>> = HashMap::new();
        for conv in &pushed_conversations {
            // Skip unchanged conversations in detailed output
            if conv.operation == SyncOperation::Unchanged {
                continue;
            }

            let project = conv
                .project_path
                .split('/')
                .next()
                .unwrap_or("unknown")
                .to_string();
            by_project.entry(project).or_default().push(conv);
        }

        // Display conversations grouped by project
        if !by_project.is_empty() {
            println!("{}", "Pushed Conversations:".bold());

            let mut projects: Vec<_> = by_project.keys().collect();
            projects.sort();

            for project in projects {
                let conversations = &by_project[project];
                println!("\n  {} {}/", "Project:".bold(), project.cyan());

                for conv in conversations.iter().take(MAX_CONVERSATIONS_TO_DISPLAY) {
                    let operation_str = match conv.operation {
                        SyncOperation::Added => "ADD".green(),
                        SyncOperation::Modified => "MOD".cyan(),
                        SyncOperation::Conflict => "CONFLICT".yellow(),
                        SyncOperation::Unchanged => "---".dimmed(),
                    };

                    let timestamp_str = conv
                        .timestamp
                        .as_ref()
                        .and_then(|t| {
                            // Extract just the date portion for compact display
                            t.split('T').next()
                        })
                        .unwrap_or("unknown");

                    println!(
                        "    {} {} ({}msg, {})",
                        operation_str,
                        conv.project_path,
                        conv.message_count,
                        timestamp_str.dimmed()
                    );
                }

                if conversations.len() > MAX_CONVERSATIONS_TO_DISPLAY {
                    println!(
                        "    {} ... and {} more conversations",
                        "...".dimmed(),
                        conversations.len() - MAX_CONVERSATIONS_TO_DISPLAY
                    );
                }
            }
        }

        println!("\n{}", "Push complete!".green().bold());
    }

    // Clean up old snapshots automatically
    if let Err(e) = crate::undo::cleanup_old_snapshots(None, false) {
        log::warn!("Failed to cleanup old snapshots: {}", e);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_degraded_result_not_error() {
        let result = PushResult::Degraded {
            conflicts: vec![PathBuf::from("session-conflict-1.jsonl")],
        };
        assert!(matches!(result, PushResult::Degraded { .. }));
    }

    #[test]
    fn test_drift_check_returns_false_without_pointer() {
        assert!(!has_last_synced_commit_drift(None, "abc123", true));
    }

    #[test]
    fn test_drift_check_true_when_diverged() {
        assert!(has_last_synced_commit_drift(
            Some("old-hash"),
            "new-hash",
            false
        ));
    }

    #[test]
    fn test_drift_check_false_when_ancestor() {
        assert!(!has_last_synced_commit_drift(
            Some("ancestor"),
            "descendant",
            true
        ));
    }

    #[test]
    fn test_drift_check_false_when_same_hash() {
        assert!(!has_last_synced_commit_drift(
            Some("same-hash"),
            "same-hash",
            true
        ));
    }

    #[test]
    fn test_find_rebase_conflict_files_detects_markers() {
        let dir = tempfile::TempDir::new().unwrap();

        // File with conflict markers
        let conflict_file = dir.path().join("session.jsonl");
        std::fs::write(
            &conflict_file,
            "<<<<<<< HEAD\nline1\n=======\nline2\n>>>>>>> branch\n",
        )
        .unwrap();

        // Normal file without markers
        let normal_file = dir.path().join("other.jsonl");
        std::fs::write(&normal_file, "{\"key\": \"value\"}\n").unwrap();

        // Non-jsonl file with markers (should be skipped)
        let txt_file = dir.path().join("notes.txt");
        std::fs::write(&txt_file, "<<<<<<< HEAD\nconflict\n").unwrap();

        let conflicts = find_rebase_conflict_files(dir.path());
        assert_eq!(conflicts.len(), 1, "should find exactly one conflict file");
        assert_eq!(
            conflicts[0], conflict_file,
            "should find the jsonl file with conflict markers"
        );
    }

    #[test]
    fn test_find_rebase_conflict_files_empty_when_no_conflicts() {
        let dir = tempfile::TempDir::new().unwrap();

        let normal_file = dir.path().join("session.jsonl");
        std::fs::write(&normal_file, "{\"key\": \"value\"}\n").unwrap();

        let conflicts = find_rebase_conflict_files(dir.path());
        assert!(conflicts.is_empty(), "should find no conflict files");
    }

    #[test]
    fn test_find_rebase_conflict_files_skips_hidden_dirs() {
        let dir = tempfile::TempDir::new().unwrap();

        // File in .git dir with conflict markers (should be skipped)
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("conflict.jsonl"), "<<<<<<< HEAD\nconflict\n").unwrap();

        // File in normal dir with conflict markers (should be found)
        let nested = dir.path().join("projects").join("my-project");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("session.jsonl"), "<<<<<<< HEAD\nconflict\n").unwrap();

        let conflicts = find_rebase_conflict_files(dir.path());
        assert_eq!(
            conflicts.len(),
            1,
            "should skip .git but find conflict in projects dir"
        );
        assert!(conflicts[0].ends_with("session.jsonl"));
    }

    #[cfg(unix)]
    #[test]
    fn rebase_conflict_scan_skips_directory_and_file_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_conflict = outside.path().join("conflict.jsonl");
        fs::write(&outside_conflict, "<<<<<<< HEAD\nconflict\n").unwrap();
        symlink(outside.path(), temp.path().join("escape")).unwrap();
        symlink(&outside_conflict, temp.path().join("alias.jsonl")).unwrap();

        assert!(find_rebase_conflict_files(temp.path()).is_empty());
    }

    #[test]
    fn auto_memory_sync_writes_normal_destination_under_guarded_projects_root() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let projects = repo.join("projects");
        let local_project = temp.path().join("local/project");
        let local_memory = local_project.join("memory");
        fs::create_dir_all(&local_memory).unwrap();
        fs::write(local_memory.join("note.md"), b"local memory").unwrap();
        fs::create_dir_all(&projects).unwrap();

        let mut mappings = HashMap::new();
        mappings.insert(local_project, PathBuf::from("project"));

        let (synced, deleted) =
            sync_auto_memory_directories(&repo, &projects, &temp.path().join("local"), &mappings)
                .unwrap();
        assert_eq!((synced, deleted), (1, 0));
        assert_eq!(
            fs::read(projects.join("project/memory/note.md")).unwrap(),
            b"local memory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn auto_memory_sync_rejects_root_project_memory_and_file_symlinks() {
        use std::os::unix::fs::symlink;

        fn run_case(repo: &Path, projects: &Path, local_project: &Path, outside_marker: &Path) {
            let mut mappings = HashMap::new();
            mappings.insert(local_project.to_path_buf(), PathBuf::from("project"));
            assert!(sync_auto_memory_directories(
                repo,
                projects,
                local_project.parent().unwrap(),
                &mappings,
            )
            .is_err());
            assert_eq!(fs::read(outside_marker).unwrap(), b"must survive");
        }

        // A symlink at the projects root must never redefine the sync boundary.
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let projects = repo.join("projects");
        let outside = temp.path().join("outside");
        let outside_marker = outside.join("marker");
        let local_project = temp.path().join("local/project");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(&outside_marker, b"must survive").unwrap();
        fs::create_dir_all(local_project.join("memory")).unwrap();
        fs::write(local_project.join("memory/note.md"), b"local").unwrap();
        symlink(&outside, &projects).unwrap();
        run_case(&repo, &projects, &local_project, &outside_marker);

        // Project, memory, and destination file symlinks are each rejected.
        for mode in ["project", "memory", "file"] {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            let projects = repo.join("projects");
            let outside = temp.path().join("outside");
            let outside_marker = outside.join("marker");
            let local_project = temp.path().join("local/project");
            fs::create_dir_all(&projects).unwrap();
            fs::create_dir_all(&outside).unwrap();
            fs::write(&outside_marker, b"must survive").unwrap();
            fs::create_dir_all(local_project.join("memory")).unwrap();
            fs::write(local_project.join("memory/note.md"), b"local").unwrap();

            match mode {
                "project" => symlink(&outside, projects.join("project")).unwrap(),
                "memory" => {
                    fs::create_dir_all(projects.join("project")).unwrap();
                    symlink(&outside, projects.join("project/memory")).unwrap();
                }
                "file" => {
                    fs::create_dir_all(projects.join("project/memory")).unwrap();
                    symlink(&outside_marker, projects.join("project/memory/note.md")).unwrap();
                }
                _ => unreachable!(),
            }
            run_case(&repo, &projects, &local_project, &outside_marker);
        }
    }

    #[cfg(unix)]
    #[test]
    fn auto_memory_sync_rejects_local_project_and_memory_symlinks_without_copy_or_prune() {
        use std::os::unix::fs::symlink;

        for mode in ["project", "memory"] {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            let projects = repo.join("projects");
            let local_root = temp.path().join("local");
            let local_project = local_root.join("project");
            let outside_project = temp.path().join("outside-project");
            let outside_memory = outside_project.join("memory");
            fs::create_dir_all(&projects).unwrap();
            fs::create_dir_all(&outside_memory).unwrap();
            fs::write(outside_memory.join("secret.md"), b"must not copy").unwrap();

            let remote_memory = projects.join("project/memory");
            fs::create_dir_all(&remote_memory).unwrap();
            let remote_marker = remote_memory.join("keep.md");
            fs::write(&remote_marker, b"must survive").unwrap();

            match mode {
                "project" => {
                    fs::create_dir_all(&local_root).unwrap();
                    symlink(&outside_project, &local_project).unwrap();
                }
                "memory" => {
                    fs::create_dir_all(&local_project).unwrap();
                    symlink(&outside_memory, local_project.join("memory")).unwrap();
                }
                _ => unreachable!(),
            }

            let mut mappings = HashMap::new();
            mappings.insert(local_project, PathBuf::from("project"));
            assert!(
                sync_auto_memory_directories(&repo, &projects, &local_root, &mappings).is_err()
            );
            assert!(!remote_memory.join("secret.md").exists());
            assert_eq!(fs::read(&remote_marker).unwrap(), b"must survive");
        }
    }

    #[cfg(unix)]
    #[test]
    fn auto_memory_sync_rejects_local_projects_root_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let projects = repo.join("projects");
        let outside = temp.path().join("outside");
        let outside_memory = outside.join("project/memory");
        let local_root = temp.path().join("local-link");
        fs::create_dir_all(&projects).unwrap();
        fs::create_dir_all(&outside_memory).unwrap();
        fs::write(outside_memory.join("secret.md"), b"must not copy").unwrap();
        symlink(&outside, &local_root).unwrap();

        let remote_memory = projects.join("project/memory");
        fs::create_dir_all(&remote_memory).unwrap();
        let remote_marker = remote_memory.join("keep.md");
        fs::write(&remote_marker, b"must survive").unwrap();

        let mut mappings = HashMap::new();
        mappings.insert(local_root.join("project"), PathBuf::from("project"));
        assert!(sync_auto_memory_directories(&repo, &projects, &local_root, &mappings).is_err());
        assert!(!remote_memory.join("secret.md").exists());
        assert_eq!(fs::read(&remote_marker).unwrap(), b"must survive");
    }

    #[cfg(unix)]
    #[test]
    fn push_and_prune_reject_projects_root_symlink_without_external_write_or_delete() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let sync_repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&sync_repo).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("demo/session.jsonl");
        fs::create_dir_all(outside_file.parent().unwrap()).unwrap();
        fs::write(&outside_file, b"must survive\n").unwrap();
        let projects = sync_repo.join("projects");
        symlink(&outside, &projects).unwrap();

        assert!(validate_sync_projects_root(&sync_repo, &projects).is_err());
        assert!(prune_missing_repo_sessions(
            &sync_repo,
            &projects,
            &[PathBuf::from("demo/session.jsonl")],
        )
        .is_err());
        assert_eq!(fs::read_to_string(&outside_file).unwrap(), "must survive\n");
    }

    #[test]
    fn test_decide_missing_action_protect() {
        assert_eq!(decide_missing_action(false, None), MissingAction::Protect);
    }

    #[test]
    fn test_decide_missing_action_manual_prune_wins_over_window() {
        assert_eq!(
            decide_missing_action(true, None),
            MissingAction::PruneManual
        );
        assert_eq!(
            decide_missing_action(true, Some(600)),
            MissingAction::PruneManual
        );
    }

    #[test]
    fn test_decide_missing_action_window_prune_reports_minutes() {
        assert_eq!(
            decide_missing_action(false, Some(600)),
            MissingAction::PruneUnlock(10)
        );
        assert_eq!(
            decide_missing_action(false, Some(59)),
            MissingAction::PruneUnlock(0)
        );
    }

    #[test]
    fn suppressed_missing_sessions_follow_protection_policy() {
        use crate::session_maintenance::state::{
            identity_key, LifecycleState, MaintenanceEntry, MaintenanceState,
        };
        use crate::session_model::{SessionIdentity, SessionSource};
        use std::path::PathBuf;

        let identity = SessionIdentity {
            source: SessionSource::Claude,
            session_id: "session-suppressed".to_string(),
        };
        let mut state = MaintenanceState::default();
        state.entries.insert(
            identity_key(&identity),
            MaintenanceEntry {
                identity,
                original_relative_path: PathBuf::from("project/session-suppressed.jsonl"),
                project_name: "project".to_string(),
                fingerprint: "fingerprint".to_string(),
                lifecycle: LifecycleState::PurgedLocal,
                classifier_version: 1,
                score: 0,
                reason_codes: vec![],
                hidden_since: None,
                recycled_at: None,
                purged_at: None,
                keep: false,
                explicit_test: false,
            },
        );
        let missing = vec![
            PathBuf::from("project/session-suppressed.jsonl"),
            PathBuf::from("project/session-ordinary.jsonl"),
        ];

        let (suppressed, ordinary) = partition_missing_repo_sessions(&missing, Some(&state));
        assert_eq!(
            suppressed,
            vec![PathBuf::from("project/session-suppressed.jsonl")]
        );
        assert_eq!(
            ordinary,
            vec![PathBuf::from("project/session-ordinary.jsonl")]
        );
        assert!(missing_for_action(&missing, Some(&state), &MissingAction::Protect).is_empty());
        assert_eq!(
            missing_for_action(&missing, Some(&state), &MissingAction::PruneUnlock(5)),
            ordinary
        );
        assert_eq!(
            missing_for_action(&missing, Some(&state), &MissingAction::PruneManual),
            missing
        );

        let (suppressed_on_error, ordinary_on_error) =
            partition_missing_repo_sessions(&missing, None);
        assert!(suppressed_on_error.is_empty());
        assert_eq!(ordinary_on_error, missing);
    }
}
