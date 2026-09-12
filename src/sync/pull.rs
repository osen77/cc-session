use anyhow::{Context, Result};
use colored::Colorize;
use inquire::Confirm;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ConfigManager;
use crate::conflict::ConflictDetector;
use crate::filter::FilterConfig;
use crate::history::{
    ConversationSummary, OperationHistory, OperationRecord, OperationType, SyncOperation,
};
use crate::interactive_conflict;
use crate::parser::ConversationSession;
use crate::path_security::{
    prepare_regular_file_destination, safe_join_within_root, safe_join_within_sync_projects_root,
    validate_directory_candidate, validate_directory_root, validate_project_component,
    validate_regular_candidate, validate_sync_projects_root,
};
use crate::report::{save_conflict_report, ConflictReport};
use crate::scm;
use crate::session_cache::fingerprint_file;
use crate::session_maintenance::state::{LifecycleState, StateStore};
use crate::session_maintenance::{suppression_for_remote, SuppressionDecision};
use crate::session_model::{SessionIdentity, SessionSource};
use crate::sync::pull_guard::{PullGuardEntry, PullGuardRegistry};
#[cfg(test)]
use crate::sync::session_write::write_new_session_noclobber_with_hook;
use crate::sync::session_write::{
    append_merged_entries_guarded, write_new_session_noclobber, AppendSessionWriteOutcome,
    NewSessionWriteOutcome, SessionBaseline,
};
use crate::sync::tombstone::TombstoneRegistry;
use crate::undo::Snapshot;
use crate::BINARY_NAME;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullIncompleteSession {
    pub(crate) session_id: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullIncomplete {
    pub(crate) sessions: Vec<PullIncompleteSession>,
}

impl std::fmt::Display for PullIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pull incomplete: {} session(s) were deferred safely",
            self.sessions.len()
        )
    }
}

impl std::error::Error for PullIncomplete {}

#[cfg(test)]
thread_local! {
    static AFTER_SNAPSHOT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn set_after_snapshot_hook(hook: impl FnOnce() + 'static) {
    AFTER_SNAPSHOT_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn run_after_snapshot_hook() {
    AFTER_SNAPSHOT_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingSuppressionClear {
    identity: SessionIdentity,
    expected_fingerprint: String,
    expected_lifecycle: LifecycleState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SuppressionRevalidation {
    NotSuppressed,
    SkipSameRevision,
    Restore(PendingSuppressionClear),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuppressionApplyOutcome {
    Written,
    Unchanged,
    SkippedNoLocalProject,
    Cancelled,
    WriteFailed,
    SkippedChanged,
    SkippedExistingTarget,
}

fn should_clear_suppression(outcome: SuppressionApplyOutcome) -> bool {
    matches!(
        outcome,
        SuppressionApplyOutcome::Written | SuppressionApplyOutcome::Unchanged
    )
}

fn revalidate_suppression_for_remote(
    store: &StateStore,
    identity: &SessionIdentity,
    remote_fingerprint: &str,
) -> anyhow::Result<SuppressionRevalidation> {
    store.transaction(|locked| {
        let Some(entry) =
            crate::session_maintenance::maintenance_state_for(&locked.state, identity)
        else {
            return Ok(SuppressionRevalidation::NotSuppressed);
        };
        match suppression_for_remote(&locked.state, identity, remote_fingerprint) {
            SuppressionDecision::NotSuppressed => Ok(SuppressionRevalidation::NotSuppressed),
            SuppressionDecision::SkipSameRevision => Ok(SuppressionRevalidation::SkipSameRevision),
            SuppressionDecision::RestoreNewRevision => {
                Ok(SuppressionRevalidation::Restore(PendingSuppressionClear {
                    identity: identity.clone(),
                    expected_fingerprint: entry.fingerprint.clone(),
                    expected_lifecycle: entry.lifecycle,
                }))
            }
        }
    })
}

fn clear_pending_suppression_after_outcome(
    store: Option<&StateStore>,
    pending: &HashMap<SessionIdentity, PendingSuppressionClear>,
    identity: &SessionIdentity,
    outcome: SuppressionApplyOutcome,
) {
    if !should_clear_suppression(outcome) {
        return;
    }
    let (Some(store), Some(token)) = (store, pending.get(identity)) else {
        return;
    };
    match store.transaction(|locked| {
        let cleared = locked.state.clear_suppression_if_matches(
            &token.identity,
            &token.expected_fingerprint,
            token.expected_lifecycle,
        );
        if cleared {
            locked.persist()?;
        }
        Ok(cleared)
    }) {
        Ok(true) => {}
        Ok(false) => log::debug!(
            "Suppression clear skipped because maintenance state changed for {}",
            identity.session_id
        ),
        Err(error) => log::warn!(
            "Failed to clear suppression after successful session restore safely: {}",
            error
        ),
    }
}

use super::discovery::{
    claude_projects_dir, discover_sessions, find_local_project_by_name, warn_large_files,
};
use super::state::SyncState;
use super::MAX_CONVERSATIONS_TO_DISPLAY;

fn prepare_local_session_destination(local_root: &Path, relative: &Path) -> Result<PathBuf> {
    prepare_regular_file_destination(local_root, relative)
}

fn parse_snapshot_session(
    conflict: &crate::conflict::Conflict,
    baseline: &SessionBaseline,
) -> Result<ConversationSession> {
    baseline.parse_session(&conflict.local_file, &conflict.session_id)
}

fn revalidate_unchanged_session(
    discovery_local: &ConversationSession,
    remote: &ConversationSession,
    local_root: &Path,
    relative: &Path,
) -> Result<bool> {
    let destination = prepare_local_session_destination(local_root, relative)?;
    let outcome = match ConversationSession::from_file_with_report(&destination) {
        Ok(outcome) => outcome,
        Err(error) => {
            log::warn!(
                "Unchanged session revalidation failed for {}: {}",
                destination.display(),
                error
            );
            return Ok(false);
        }
    };
    if outcome.malformed_lines > 0
        || outcome.value.session_id != remote.session_id
        || discovery_local.session_id != remote.session_id
    {
        return Ok(false);
    }
    let current_hash = outcome.value.content_hash();
    Ok(current_hash == discovery_local.content_hash() && current_hash == remote.content_hash())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AddedSessionWriteOutcome {
    Written(PathBuf),
    SkippedExisting(PathBuf),
}

fn map_added_outcome(outcome: NewSessionWriteOutcome) -> AddedSessionWriteOutcome {
    match outcome {
        NewSessionWriteOutcome::Written { path, .. } => AddedSessionWriteOutcome::Written(path),
        NewSessionWriteOutcome::SkippedExisting(path) => {
            AddedSessionWriteOutcome::SkippedExisting(path)
        }
    }
}

fn write_added_session_without_overwrite(
    session: &ConversationSession,
    local_root: &Path,
    relative: &Path,
) -> Result<AddedSessionWriteOutcome> {
    Ok(map_added_outcome(write_new_session_noclobber(
        session, local_root, relative,
    )?))
}

#[cfg(test)]
fn write_added_session_without_overwrite_with_hook<F>(
    session: &ConversationSession,
    local_root: &Path,
    relative: &Path,
    before_commit: F,
) -> Result<AddedSessionWriteOutcome>
where
    F: FnOnce(&Path) -> Result<()>,
{
    Ok(map_added_outcome(write_new_session_noclobber_with_hook(
        session,
        local_root,
        relative,
        before_commit,
    )?))
}

fn propagate_tombstones(local_projects_root: &Path, registry: &TombstoneRegistry) -> Result<usize> {
    validate_directory_root(local_projects_root)?;
    let mut propagated = 0;
    let entries = match fs::read_dir(local_projects_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };

    for entry in entries {
        let entry = entry?;
        let local_project_dir = entry.path();
        let metadata = fs::symlink_metadata(&local_project_dir)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            log::warn!(
                "Skipping unsafe local project while applying tombstones: {}",
                local_project_dir.display()
            );
            continue;
        }
        if let Err(error) = validate_directory_candidate(local_projects_root, &local_project_dir) {
            log::warn!(
                "Skipping unsafe local project while applying tombstones: {}",
                error
            );
            continue;
        }

        for file in fs::read_dir(&local_project_dir)? {
            let file = file?;
            let file_name = file.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if !name.ends_with(".jsonl") {
                continue;
            }

            let Some(session_id) =
                crate::session_model::claude_session_id_from_path(Path::new(name))
            else {
                continue;
            };
            if !registry.contains(&session_id) {
                continue;
            }

            let file_path = file.path();
            let metadata = fs::symlink_metadata(&file_path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                log::warn!(
                    "Skipping unsafe tombstone deletion candidate: {}",
                    file_path.display()
                );
                continue;
            }
            validate_regular_candidate(local_projects_root, &file_path)?;
            let relative = file_path
                .strip_prefix(local_projects_root)
                .with_context(|| {
                    format!(
                        "local session path is outside projects root: {}",
                        file_path.display()
                    )
                })?;
            let candidate = safe_join_within_root(local_projects_root, relative)?;
            validate_regular_candidate(local_projects_root, &candidate)?;
            fs::remove_file(&candidate)
                .with_context(|| format!("failed to propagate tombstone for {session_id}"))?;
            propagated += 1;
            log::debug!("Propagated remote deletion: {}", candidate.display());
        }
    }

    Ok(propagated)
}

fn sync_auto_memory_from_remote(
    sync_repo_path: &Path,
    remote_projects_dir: &Path,
    local_projects_root: &Path,
    use_project_name_only: bool,
) -> Result<Vec<String>> {
    validate_sync_projects_root(sync_repo_path, remote_projects_dir)?;
    validate_directory_root(local_projects_root)?;
    let mut synced_projects = Vec::new();

    for entry in fs::read_dir(remote_projects_dir)? {
        let entry = entry?;
        let project_name = match entry.file_name().to_str() {
            Some(name) if !name.starts_with('.') && !name.is_empty() => name.to_string(),
            _ => continue,
        };
        validate_project_component(&project_name)?;

        let project_relative = PathBuf::from(&project_name);
        let sync_project_dir = safe_join_within_sync_projects_root(
            sync_repo_path,
            remote_projects_dir,
            &project_relative,
        )?;
        let project_metadata = fs::symlink_metadata(&sync_project_dir)?;
        if project_metadata.file_type().is_symlink() {
            anyhow::bail!("remote auto-memory project path must not be a symlink");
        }
        if !project_metadata.is_dir() {
            continue;
        }
        validate_directory_candidate(remote_projects_dir, &sync_project_dir)?;

        let memory_relative = project_relative.join("memory");
        let remote_memory_path = safe_join_within_sync_projects_root(
            sync_repo_path,
            remote_projects_dir,
            &memory_relative,
        )?;
        let remote_memory_metadata = match fs::symlink_metadata(&remote_memory_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if remote_memory_metadata.file_type().is_symlink() || !remote_memory_metadata.is_dir() {
            anyhow::bail!("remote auto-memory path must be a non-symlink directory");
        }
        validate_directory_candidate(remote_projects_dir, &remote_memory_path)?;

        let local_project_dir = if use_project_name_only {
            find_local_project_by_name(local_projects_root, &project_name)
        } else {
            let local_path = safe_join_within_root(local_projects_root, &project_relative)?;
            match fs::symlink_metadata(&local_path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    Some(local_path)
                }
                Ok(_) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            }
        };

        let Some(local_project_dir) = local_project_dir else {
            log::debug!(
                "No local project found for '{}', skipping memory sync",
                project_name
            );
            continue;
        };
        validate_directory_candidate(local_projects_root, &local_project_dir)?;

        let local_memory_path = safe_join_within_root(&local_project_dir, Path::new("memory"))?;
        match fs::symlink_metadata(&local_memory_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                anyhow::bail!("local auto-memory path must be a non-symlink directory");
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&local_memory_path)?;
            }
            Err(error) => return Err(error.into()),
        }
        validate_directory_candidate(local_projects_root, &local_memory_path)?;

        for memory_entry in fs::read_dir(&remote_memory_path)? {
            let memory_entry = memory_entry?;
            let source = memory_entry.path();
            let source_metadata = fs::symlink_metadata(&source)?;
            if source_metadata.file_type().is_symlink() {
                anyhow::bail!("remote auto-memory contains a symlink file");
            }
            if !source_metadata.is_file() {
                continue;
            }
            validate_regular_candidate(remote_projects_dir, &source)?;

            let destination =
                safe_join_within_root(&local_memory_path, Path::new(&memory_entry.file_name()))?;
            if let Ok(metadata) = fs::symlink_metadata(&destination) {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    anyhow::bail!("local auto-memory destination is not a regular file");
                }
            }

            validate_regular_candidate(remote_projects_dir, &source)?;
            let destination =
                safe_join_within_root(&local_memory_path, Path::new(&memory_entry.file_name()))?;
            fs::copy(&source, &destination)?;
        }
        synced_projects.push(project_name);
    }

    Ok(synced_projects)
}

/// Pull and merge history from sync repository
pub fn pull_history(
    fetch_remote: bool,
    branch: Option<&str>,
    interactive: bool,
    verbosity: crate::VerbosityLevel,
) -> Result<()> {
    use crate::VerbosityLevel;

    if verbosity != VerbosityLevel::Quiet {
        println!("{}", "Pulling Claude Code history...".cyan().bold());
    }

    let state = SyncState::load()?;

    // Serialize against any other process writing this repository — a pull
    // rebases and rewrites the working tree, so it cannot overlap a push.
    let Some(_repo_lock) = crate::sync::repo_lock::RepoLock::acquire_or_report(
        &state.sync_repo_path,
        "拉取",
        verbosity,
    )?
    else {
        return Ok(());
    };

    let repo = scm::open(&state.sync_repo_path)?;
    let filter = FilterConfig::load()?;
    let claude_dir = claude_projects_dir()?;
    validate_directory_root(&claude_dir)?;

    // Get the current branch name for operation record
    let branch_name = branch
        .map(|s| s.to_string())
        .or_else(|| repo.current_branch().ok())
        .unwrap_or_else(|| "main".to_string());

    // Fetch from remote if configured
    if fetch_remote && state.has_remote {
        println!("  {} from remote...", "Fetching".cyan());

        match repo.pull("origin", &branch_name) {
            Ok(_) => println!("  {} Pulled from origin/{}", "✓".green(), branch_name),
            Err(e) => {
                log::warn!("Failed to pull: {}", e);
                log::info!("Continuing with local sync repository state...");
            }
        }
    }

    // ============================================================================
    // PROPAGATE INTENTIONAL DELETIONS
    // ============================================================================
    // Before discovering local sessions, check if the sync repo has any
    // registered tombstones that we haven't applied locally yet.
    match TombstoneRegistry::load(&state.sync_repo_path) {
        Ok(registry) if !registry.is_empty() => {
            if verbosity != VerbosityLevel::Quiet {
                println!("  {} tombstones...", "Checking".cyan());
            }
            let propagated_deletes = propagate_tombstones(&claude_dir, &registry)?;
            if propagated_deletes > 0 && verbosity != VerbosityLevel::Quiet {
                println!(
                    "  {} Propagated {} intentional deletion(s) from other devices",
                    "✓".green(),
                    propagated_deletes
                );
            }
        }
        Ok(_) => {}
        Err(error) => {
            log::warn!("Failed to load tombstone registry safely: {}", error);
        }
    }

    // Discover local sessions.
    println!("  {} local sessions...", "Discovering".cyan());
    let local_sessions = discover_sessions(&claude_dir, &filter)?;
    println!(
        "  {} {} local sessions",
        "Found".green(),
        local_sessions.len()
    );

    // Discover remote sessions
    let remote_projects_dir = state.sync_repo_path.join(&filter.sync_subdirectory);
    validate_sync_projects_root(&state.sync_repo_path, &remote_projects_dir)?;
    println!("  {} remote sessions...", "Discovering".cyan());
    let discovered_remote_sessions = discover_sessions(&remote_projects_dir, &filter)?;
    let maintenance_store = ConfigManager::config_dir().ok().map(|config_dir| {
        crate::session_maintenance::state::StateStore::from_config_dir(&config_dir)
    });
    let maintenance_state = match maintenance_store.as_ref().map(|store| store.load()) {
        Some(Ok(state)) => Some(state),
        Some(Err(error)) => {
            log::warn!(
                "Failed to load session maintenance state; restoring remote sessions safely: {}",
                error
            );
            None
        }
        None => {
            log::warn!(
                "Failed to locate session maintenance state; restoring remote sessions safely"
            );
            None
        }
    };
    let mut suppressed_remote_count = 0usize;
    let mut pending_suppression_clears = HashMap::new();
    let mut remote_sessions = Vec::with_capacity(discovered_remote_sessions.len());
    for remote_session in discovered_remote_sessions {
        let identity = SessionIdentity {
            source: SessionSource::Claude,
            session_id: remote_session.session_id.clone(),
        };
        let Some(state) = maintenance_state.as_ref() else {
            remote_sessions.push(remote_session);
            continue;
        };
        if crate::session_maintenance::maintenance_state_for(state, &identity).is_none() {
            remote_sessions.push(remote_session);
            continue;
        }
        let fingerprint = match fingerprint_file(Path::new(&remote_session.file_path)) {
            Ok(fingerprint) => fingerprint.digest,
            Err(error) => {
                log::warn!(
                    "Failed to fingerprint remote session safely; restoring it: {}",
                    error
                );
                remote_sessions.push(remote_session);
                continue;
            }
        };
        match suppression_for_remote(state, &identity, &fingerprint) {
            SuppressionDecision::SkipSameRevision => {
                suppressed_remote_count += 1;
            }
            SuppressionDecision::RestoreNewRevision => {
                let revalidated = maintenance_store
                    .as_ref()
                    .map(|store| revalidate_suppression_for_remote(store, &identity, &fingerprint));
                match revalidated {
                    Some(Ok(SuppressionRevalidation::SkipSameRevision)) => {
                        suppressed_remote_count += 1;
                    }
                    Some(Ok(SuppressionRevalidation::Restore(token))) => {
                        pending_suppression_clears.insert(identity, token);
                        remote_sessions.push(remote_session);
                    }
                    Some(Ok(SuppressionRevalidation::NotSuppressed)) | None => {
                        remote_sessions.push(remote_session);
                    }
                    Some(Err(error)) => {
                        log::warn!(
                            "Failed to revalidate remote suppression safely; restoring it without clearing state: {}",
                            error
                        );
                        remote_sessions.push(remote_session);
                    }
                }
            }
            SuppressionDecision::NotSuppressed => remote_sessions.push(remote_session),
        }
    }
    println!(
        "  {} {} remote sessions",
        "Found".green(),
        remote_sessions.len()
    );
    let remote_revisions: HashMap<String, (PathBuf, String)> = remote_sessions
        .iter()
        .map(|session| {
            let path = Path::new(&session.file_path);
            let relative = path
                .strip_prefix(&remote_projects_dir)
                .context("remote session is outside sync projects root")?
                .to_path_buf();
            let fingerprint = fingerprint_file(path)?.digest;
            Ok((session.session_id.clone(), (relative, fingerprint)))
        })
        .collect::<Result<_>>()?;
    if suppressed_remote_count > 0 && verbosity != VerbosityLevel::Quiet {
        println!(
            "  {} Suppressed {} unchanged locally recycled session(s)",
            "✓".green(),
            suppressed_remote_count
        );
    }

    // ============================================================================
    // CONFLICT DETECTION (moved before snapshot for efficiency)
    // ============================================================================
    // Detect conflicts FIRST so we only backup files that will be modified
    if verbosity != VerbosityLevel::Quiet {
        println!("  {} conflicts...", "Detecting".cyan());
    }
    let mut detector = ConflictDetector::new();
    detector.detect(&local_sessions, &remote_sessions);

    // ============================================================================
    // SHOW SUMMARY AND INTERACTIVE CONFIRMATION
    // ============================================================================
    if verbosity != VerbosityLevel::Quiet {
        println!();
        println!("{}", "Pull Summary:".bold().cyan());
        println!("  {} Local sessions: {}", "•".cyan(), local_sessions.len());
        println!(
            "  {} Remote sessions: {}",
            "•".cyan(),
            remote_sessions.len()
        );
        println!();
    }

    if verbosity == VerbosityLevel::Verbose {
        println!("{}", "Remote sessions to be pulled:".bold());
        for (idx, session) in remote_sessions.iter().enumerate().take(20) {
            let relative_path = Path::new(&session.file_path)
                .strip_prefix(&remote_projects_dir)
                .unwrap_or(Path::new(&session.file_path));
            println!(
                "  {}. {} ({} messages)",
                idx + 1,
                relative_path.display(),
                session.message_count()
            );
        }
        if remote_sessions.len() > 20 {
            println!("  ... and {} more", remote_sessions.len() - 20);
        }
        println!();
    }

    // Confirm before capturing baselines so interactive waiting cannot make the
    // snapshot stale before conflict resolution even begins.
    if interactive && interactive_conflict::is_interactive() {
        let confirm =
            Confirm::new("Do you want to proceed with pulling and merging these changes?")
                .with_default(true)
                .with_help_message(
                    "This will merge remote sessions into your local Claude Code history",
                )
                .prompt()
                .context("Failed to get confirmation")?;
        if !confirm {
            println!("\n{}", "Pull cancelled.".yellow());
            for token in pending_suppression_clears.values() {
                clear_pending_suppression_after_outcome(
                    maintenance_store.as_ref(),
                    &pending_suppression_clears,
                    &token.identity,
                    SuppressionApplyOutcome::Cancelled,
                );
            }
            return Ok(());
        }
    }

    // ============================================================================
    // SNAPSHOT CREATION AND EXACT CONFLICT BASELINES
    // ============================================================================
    let mut conflict_baselines: HashMap<String, SessionBaseline> = HashMap::new();
    let snapshot_path = if detector.has_conflicts() {
        println!(
            "  {} snapshot of {} conflicting files...",
            "Creating".cyan(),
            detector.conflict_count()
        );
        let conflicting_file_paths: Vec<PathBuf> = detector
            .conflicts()
            .iter()
            .map(|conflict| conflict.local_file.clone())
            .collect();
        warn_large_files(&conflicting_file_paths);
        let snapshot = Snapshot::create(OperationType::Pull, conflicting_file_paths.iter(), None)
            .context("Failed to create snapshot before pull")?;
        for conflict in detector.conflicts() {
            let key = conflict.local_file.to_string_lossy();
            if let Some(bytes) = snapshot.files.get(key.as_ref()) {
                match SessionBaseline::from_snapshot(
                    &conflict.local_file,
                    &conflict.session_id,
                    bytes.clone(),
                ) {
                    Ok(baseline) => {
                        conflict_baselines.insert(conflict.session_id.clone(), baseline);
                    }
                    Err(error) => log::warn!(
                        "Snapshot baseline path binding failed for {}: {}",
                        conflict.local_file.display(),
                        error
                    ),
                }
            } else {
                log::warn!(
                    "Snapshot did not capture conflict baseline {}; this session will be skipped",
                    conflict.local_file.display()
                );
            }
        }
        let path = snapshot
            .save_to_disk(None)
            .context("Failed to save snapshot to disk")?;
        if verbosity != VerbosityLevel::Quiet {
            println!(
                "  {} Snapshot created: {} ({} files)",
                "✓".green(),
                path.file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string()),
                conflicting_file_paths.len()
            );
        }
        Some(path)
    } else {
        println!("  {} No conflicts - skipping snapshot", "✓".green());
        None
    };

    #[cfg(test)]
    run_after_snapshot_hook();

    // ============================================================================
    // CONFLICT RESOLUTION (detection already done above)
    // ============================================================================
    // Track affected conversations for operation record
    let mut affected_conversations: Vec<ConversationSummary> = Vec::new();
    let mut skipped_changed_count = 0usize;
    let mut incomplete_reasons: HashMap<String, String> = HashMap::new();
    let mut completed_sessions: HashSet<String> = HashSet::new();

    if detector.has_conflicts() {
        println!(
            "  {} {} conflicts detected",
            "!".yellow(),
            detector.conflict_count()
        );

        // ============================================================================
        // ATTEMPT SMART MERGE FIRST
        // ============================================================================
        println!("  {} smart merge...", "Attempting".cyan());

        let remote_map: HashMap<_, _> = remote_sessions
            .iter()
            .map(|session| (session.session_id.clone(), session))
            .collect();

        let mut smart_merge_success_count = 0;
        let mut smart_merge_failed_conflicts = Vec::new();
        let mut baseline_sessions = HashMap::new();

        for conflict in detector.conflicts_mut() {
            let identity = SessionIdentity {
                source: SessionSource::Claude,
                session_id: conflict.session_id.clone(),
            };
            let Some(baseline) = conflict_baselines.get(&conflict.session_id) else {
                skipped_changed_count += 1;
                incomplete_reasons.insert(
                    conflict.session_id.clone(),
                    "snapshot baseline unavailable".to_string(),
                );
                println!(
                    "  {} Skipped {} because its exact local snapshot baseline is unavailable",
                    "!".yellow(),
                    conflict.session_id
                );
                clear_pending_suppression_after_outcome(
                    maintenance_store.as_ref(),
                    &pending_suppression_clears,
                    &identity,
                    SuppressionApplyOutcome::SkippedChanged,
                );
                continue;
            };
            let baseline_session = match parse_snapshot_session(conflict, baseline) {
                Ok(session) => session,
                Err(error) => {
                    skipped_changed_count += 1;
                    incomplete_reasons.insert(
                        conflict.session_id.clone(),
                        "malformed or mismatched snapshot baseline".to_string(),
                    );
                    log::warn!(
                        "Skipping conflict {} because its exact snapshot baseline is malformed: {}",
                        conflict.session_id,
                        error
                    );
                    println!(
                        "  {} Skipped {} because its local snapshot contains malformed JSONL",
                        "!".yellow(),
                        conflict.session_id
                    );
                    clear_pending_suppression_after_outcome(
                        maintenance_store.as_ref(),
                        &pending_suppression_clears,
                        &identity,
                        SuppressionApplyOutcome::SkippedChanged,
                    );
                    continue;
                }
            };
            baseline_sessions.insert(conflict.session_id.clone(), baseline_session.clone());
            let Some(remote_session) = remote_map.get(&conflict.session_id) else {
                continue;
            };

            match conflict.try_smart_merge(&baseline_session, remote_session) {
                Ok(()) => {
                    let crate::conflict::ConflictResolution::SmartMerge {
                        ref merged_entries,
                        ref stats,
                    } = conflict.resolution
                    else {
                        unreachable!("successful smart merge must set SmartMerge resolution")
                    };
                    let merged_session = ConversationSession {
                        session_id: conflict.session_id.clone(),
                        entries: merged_entries.clone(),
                        file_path: conflict.local_file.to_string_lossy().to_string(),
                    };
                    let local_relative = conflict
                        .local_file
                        .strip_prefix(&claude_dir)
                        .context("conflict destination is outside Claude projects root")?;
                    match append_merged_entries_guarded(
                        &claude_dir,
                        local_relative,
                        baseline,
                        &merged_session.entries,
                    ) {
                        Ok(AppendSessionWriteOutcome::Written {
                            entries_added,
                            durability,
                        }) => {
                            smart_merge_success_count += 1;
                            completed_sessions.insert(conflict.session_id.clone());
                            incomplete_reasons.remove(&conflict.session_id);
                            println!(
                                "  {} Smart merged {} by appending {} complete JSONL entr{} ({}/{} UUIDs, durability: {:?})",
                                "✓".green(),
                                conflict.session_id,
                                entries_added,
                                if entries_added == 1 { "y" } else { "ies" },
                                stats.emitted_uuid_count,
                                stats.expected_uuid_count,
                                durability
                            );
                            clear_pending_suppression_after_outcome(
                                maintenance_store.as_ref(),
                                &pending_suppression_clears,
                                &identity,
                                SuppressionApplyOutcome::Written,
                            );
                            match ConversationSummary::new(
                                conflict.session_id.clone(),
                                local_relative.to_string_lossy().to_string(),
                                merged_session.latest_timestamp(),
                                merged_session.message_count(),
                                SyncOperation::Conflict,
                            ) {
                                Ok(summary) => affected_conversations.push(summary),
                                Err(error) => log::warn!(
                                    "Failed to record append-only SmartMerge {}: {}",
                                    conflict.session_id,
                                    error
                                ),
                            }
                        }
                        Ok(AppendSessionWriteOutcome::Noop) => {
                            smart_merge_success_count += 1;
                            completed_sessions.insert(conflict.session_id.clone());
                            incomplete_reasons.remove(&conflict.session_id);
                            println!(
                                "  {} Smart merge for {} required no local write ({}/{} UUIDs)",
                                "✓".green(),
                                conflict.session_id,
                                stats.emitted_uuid_count,
                                stats.expected_uuid_count
                            );
                            clear_pending_suppression_after_outcome(
                                maintenance_store.as_ref(),
                                &pending_suppression_clears,
                                &identity,
                                SuppressionApplyOutcome::Unchanged,
                            );
                            match ConversationSummary::new(
                                conflict.session_id.clone(),
                                local_relative.to_string_lossy().to_string(),
                                merged_session.latest_timestamp(),
                                merged_session.message_count(),
                                SyncOperation::Conflict,
                            ) {
                                Ok(summary) => affected_conversations.push(summary),
                                Err(error) => log::warn!(
                                    "Failed to record no-op SmartMerge {}: {}",
                                    conflict.session_id,
                                    error
                                ),
                            }
                        }
                        Ok(AppendSessionWriteOutcome::SkippedChanged) => {
                            conflict.resolution = crate::conflict::ConflictResolution::Pending;
                            log::warn!(
                                "Append-only SmartMerge preconditions changed for {}; falling back to KeepBoth",
                                conflict.session_id
                            );
                            clear_pending_suppression_after_outcome(
                                maintenance_store.as_ref(),
                                &pending_suppression_clears,
                                &identity,
                                SuppressionApplyOutcome::SkippedChanged,
                            );
                            smart_merge_failed_conflicts.push(conflict.clone());
                        }
                        Err(error) => {
                            conflict.resolution = crate::conflict::ConflictResolution::Pending;
                            log::warn!(
                                "SmartMerge for {} requires rewriting existing entries or append failed; falling back to KeepBoth: {}",
                                conflict.session_id,
                                error
                            );
                            clear_pending_suppression_after_outcome(
                                maintenance_store.as_ref(),
                                &pending_suppression_clears,
                                &identity,
                                SuppressionApplyOutcome::WriteFailed,
                            );
                            smart_merge_failed_conflicts.push(conflict.clone());
                        }
                    }
                }
                Err(error) => {
                    log::warn!("Smart merge failed for {}: {}", conflict.session_id, error);
                    log::info!("Falling back to manual resolution...");
                    clear_pending_suppression_after_outcome(
                        maintenance_store.as_ref(),
                        &pending_suppression_clears,
                        &identity,
                        SuppressionApplyOutcome::WriteFailed,
                    );
                    smart_merge_failed_conflicts.push(conflict.clone());
                }
            }
        }

        println!(
            "  {} Successfully smart merged {}/{} conflicts",
            "✓".green(),
            smart_merge_success_count,
            detector.conflict_count()
        );

        // If some smart merges failed, handle them with interactive/keep-both resolution
        let renames = if !smart_merge_failed_conflicts.is_empty() {
            println!(
                "  {} {} conflicts require manual resolution",
                "!".yellow(),
                smart_merge_failed_conflicts.len()
            );

            let use_interactive = crate::interactive_conflict::is_interactive();
            let resolution_result = if use_interactive {
                println!(
                    "\n{} Running in interactive mode for remaining conflicts",
                    "→".cyan()
                );
                let baseline_map: HashMap<_, _> = baseline_sessions
                    .iter()
                    .map(|(session_id, session)| (session_id.clone(), session))
                    .collect();
                crate::interactive_conflict::resolve_conflicts_interactive_with_sessions(
                    &mut smart_merge_failed_conflicts,
                    Some(&baseline_map),
                    Some(&remote_map),
                )?
            } else {
                println!(
                    "\n{} Using automatic conflict resolution (keep both versions)",
                    "→".cyan()
                );
                let mut result = crate::interactive_conflict::ResolutionResult::new();
                result.keep_both = smart_merge_failed_conflicts.clone();
                result
            };

            let applied = match crate::interactive_conflict::apply_resolutions_guarded(
                &resolution_result,
                &remote_sessions,
                &claude_dir,
                &remote_projects_dir,
                &conflict_baselines,
            ) {
                Ok(applied) => applied,
                Err(error) => {
                    log::warn!(
                        "Conflict fallback could not write a local remote copy; remote remains protected in the sync repository: {}",
                        error
                    );
                    for conflict in &smart_merge_failed_conflicts {
                        incomplete_reasons.insert(
                            conflict.session_id.clone(),
                            format!("conflict fallback write failed: {error}"),
                        );
                    }
                    crate::interactive_conflict::GuardedApplyResult::default()
                }
            };
            for (session_id, outcome) in &applied.outcomes {
                let identity = SessionIdentity {
                    source: SessionSource::Claude,
                    session_id: session_id.clone(),
                };
                let suppression_outcome = match outcome {
                    crate::interactive_conflict::GuardedApplyOutcome::Written { .. } => {
                        completed_sessions.insert(session_id.clone());
                        incomplete_reasons.remove(session_id);
                        SuppressionApplyOutcome::Written
                    }
                    crate::interactive_conflict::GuardedApplyOutcome::CreatedConflictCopy {
                        ..
                    } => {
                        incomplete_reasons.insert(
                            session_id.clone(),
                            "remote preserved as KeepBoth after incomplete merge".to_string(),
                        );
                        SuppressionApplyOutcome::WriteFailed
                    }
                    crate::interactive_conflict::GuardedApplyOutcome::SkippedChanged => {
                        skipped_changed_count += 1;
                        incomplete_reasons.insert(
                            session_id.clone(),
                            "local session changed during conflict resolution".to_string(),
                        );
                        println!(
                            "  {} Local session {} changed during conflict resolution; preserved local file",
                            "!".yellow(),
                            session_id
                        );
                        SuppressionApplyOutcome::SkippedChanged
                    }
                    crate::interactive_conflict::GuardedApplyOutcome::KeptLocal => {
                        completed_sessions.insert(session_id.clone());
                        incomplete_reasons.remove(session_id);
                        SuppressionApplyOutcome::Cancelled
                    }
                };
                clear_pending_suppression_after_outcome(
                    maintenance_store.as_ref(),
                    &pending_suppression_clears,
                    &identity,
                    suppression_outcome,
                );
            }

            crate::interactive_conflict::update_reported_resolutions(
                detector.conflicts_mut(),
                &resolution_result,
                &applied,
            );
            let report = ConflictReport::from_conflicts(detector.conflicts());
            save_conflict_report(&report)?;
            applied.renames
        } else {
            // All conflicts resolved via smart merge
            Vec::new()
        };

        // Track all conflicts in affected conversations
        for (_original_path, renamed_path) in &renames {
            let relative_path = renamed_path
                .strip_prefix(&claude_dir)
                .unwrap_or(renamed_path)
                .to_string_lossy()
                .to_string();

            // Find the session ID from the renamed path
            if let Some(session) = remote_sessions.iter().find(|s| {
                let session_file = Path::new(&s.file_path).file_name();
                let renamed_file = renamed_path.file_name();
                // Try to match based on session ID in filename
                session_file
                    .and_then(|f| f.to_str())
                    .and_then(|name| name.split('-').next())
                    == renamed_file
                        .and_then(|f| f.to_str())
                        .and_then(|name| name.split('-').next())
            }) {
                match ConversationSummary::new(
                    session.session_id.clone(),
                    relative_path.clone(),
                    session.latest_timestamp(),
                    session.message_count(),
                    SyncOperation::Conflict,
                ) {
                    Ok(summary) => affected_conversations.push(summary),
                    Err(e) => log::warn!(
                        "Failed to create summary for conflict {}: {}",
                        relative_path,
                        e
                    ),
                }
            }
        }

        println!(
            "\n{} View details with: {} report",
            "Hint:".cyan(),
            BINARY_NAME
        );
    } else {
        println!("  {} No conflicts detected", "✓".green());
    }

    // ============================================================================
    // MERGE NON-CONFLICTING SESSIONS
    // ============================================================================
    println!("  {} non-conflicting sessions...", "Merging".cyan());
    let local_map: HashMap<_, _> = local_sessions
        .iter()
        .map(|s| (s.session_id.clone(), s))
        .collect();

    let mut merged_count = 0;
    let mut added_count = 0;
    let mut unchanged_count = 0;
    let mut skipped_no_local_match = 0;
    let mut skipped_existing_target = 0;

    for remote_session in &remote_sessions {
        // Skip if conflicts were detected
        if detector
            .conflicts()
            .iter()
            .any(|c| c.session_id == remote_session.session_id)
        {
            continue;
        }

        let relative_path_for_tracking = if filter.use_project_name_only {
            // Extract project name and session filename from remote path.
            let remote_relative = Path::new(&remote_session.file_path)
                .strip_prefix(&remote_projects_dir)
                .ok()
                .unwrap_or_else(|| Path::new(&remote_session.file_path));
            let project_name = remote_relative
                .components()
                .next()
                .and_then(|component| component.as_os_str().to_str())
                .unwrap_or("unknown");
            validate_project_component(project_name)?;

            let Some(local_project_dir) = find_local_project_by_name(&claude_dir, project_name)
            else {
                log::debug!(
                    "No matching local project found for '{}', skipping",
                    project_name
                );
                skipped_no_local_match += 1;
                incomplete_reasons.insert(
                    remote_session.session_id.clone(),
                    "no matching local project".to_string(),
                );
                let identity = SessionIdentity {
                    source: SessionSource::Claude,
                    session_id: remote_session.session_id.clone(),
                };
                clear_pending_suppression_after_outcome(
                    maintenance_store.as_ref(),
                    &pending_suppression_clears,
                    &identity,
                    SuppressionApplyOutcome::SkippedNoLocalProject,
                );
                continue;
            };
            validate_directory_candidate(&claude_dir, &local_project_dir)?;

            let Some(filename) = remote_relative.file_name() else {
                log::warn!(
                    "Could not extract filename from remote path: {:?}",
                    remote_relative
                );
                skipped_no_local_match += 1;
                incomplete_reasons.insert(
                    remote_session.session_id.clone(),
                    "no matching local project".to_string(),
                );
                let identity = SessionIdentity {
                    source: SessionSource::Claude,
                    session_id: remote_session.session_id.clone(),
                };
                clear_pending_suppression_after_outcome(
                    maintenance_store.as_ref(),
                    &pending_suppression_clears,
                    &identity,
                    SuppressionApplyOutcome::SkippedNoLocalProject,
                );
                continue;
            };
            let local_project_relative = local_project_dir
                .strip_prefix(&claude_dir)
                .context("matched local project is outside Claude projects root")?;
            local_project_relative.join(filename)
        } else {
            Path::new(&remote_session.file_path)
                .strip_prefix(&remote_projects_dir)
                .context("remote session is outside the sync projects root")?
                .to_path_buf()
        };

        if let Err(error) =
            prepare_local_session_destination(&claude_dir, &relative_path_for_tracking)
        {
            incomplete_reasons.insert(
                remote_session.session_id.clone(),
                format!("unsafe local destination: {error}"),
            );
            let identity = SessionIdentity {
                source: SessionSource::Claude,
                session_id: remote_session.session_id.clone(),
            };
            clear_pending_suppression_after_outcome(
                maintenance_store.as_ref(),
                &pending_suppression_clears,
                &identity,
                SuppressionApplyOutcome::WriteFailed,
            );
            continue;
        }

        // Determine operation type based on local state
        let operation = if let Some(local) = local_map.get(&remote_session.session_id) {
            if local.content_hash() == remote_session.content_hash() {
                SyncOperation::Unchanged
            } else {
                SyncOperation::Modified
            }
        } else {
            SyncOperation::Added
        };

        // A remote-only discovery result does not prove the destination is absent:
        // local filtering or parse errors may have hidden a physical file. Never
        // replace such a file through the Added path.
        if operation == SyncOperation::Added {
            match write_added_session_without_overwrite(
                remote_session,
                &claude_dir,
                &relative_path_for_tracking,
            ) {
                Ok(AddedSessionWriteOutcome::Written(_)) => {
                    added_count += 1;
                    merged_count += 1;
                    completed_sessions.insert(remote_session.session_id.clone());
                    incomplete_reasons.remove(&remote_session.session_id);
                }
                Ok(AddedSessionWriteOutcome::SkippedExisting(destination)) => {
                    log::warn!(
                        "Skipping remote session {} because its local destination already exists but was not discovered; preserving local file: {}",
                        remote_session.session_id,
                        destination.display()
                    );
                    skipped_existing_target += 1;
                    incomplete_reasons.insert(
                        remote_session.session_id.clone(),
                        "existing local target was not safely discoverable".to_string(),
                    );
                    let identity = SessionIdentity {
                        source: SessionSource::Claude,
                        session_id: remote_session.session_id.clone(),
                    };
                    clear_pending_suppression_after_outcome(
                        maintenance_store.as_ref(),
                        &pending_suppression_clears,
                        &identity,
                        SuppressionApplyOutcome::SkippedExistingTarget,
                    );
                    continue;
                }
                Err(error) => {
                    incomplete_reasons.insert(
                        remote_session.session_id.clone(),
                        format!("Added write failed: {error}"),
                    );
                    let identity = SessionIdentity {
                        source: SessionSource::Claude,
                        session_id: remote_session.session_id.clone(),
                    };
                    clear_pending_suppression_after_outcome(
                        maintenance_store.as_ref(),
                        &pending_suppression_clears,
                        &identity,
                        SuppressionApplyOutcome::WriteFailed,
                    );
                    continue;
                }
            }
        } else if operation == SyncOperation::Modified {
            // Any differing session present on both sides should have been a
            // conflict. Treat reaching this branch as an invariant failure and
            // preserve the local file rather than overwriting it.
            log::warn!(
                "Skipping non-conflict Modified session {} to preserve the local file",
                remote_session.session_id
            );
            skipped_changed_count += 1;
            incomplete_reasons.insert(
                remote_session.session_id.clone(),
                "unexpected non-conflict Modified classification".to_string(),
            );
            let identity = SessionIdentity {
                source: SessionSource::Claude,
                session_id: remote_session.session_id.clone(),
            };
            clear_pending_suppression_after_outcome(
                maintenance_store.as_ref(),
                &pending_suppression_clears,
                &identity,
                SuppressionApplyOutcome::SkippedChanged,
            );
            continue;
        }

        if operation == SyncOperation::Unchanged {
            let Some(discovery_local) = local_map.get(&remote_session.session_id) else {
                unreachable!("Unchanged session must have a discovery baseline")
            };
            if !revalidate_unchanged_session(
                discovery_local,
                remote_session,
                &claude_dir,
                &relative_path_for_tracking,
            )? {
                skipped_changed_count += 1;
                incomplete_reasons.insert(
                    remote_session.session_id.clone(),
                    "local session changed before Unchanged revalidation".to_string(),
                );
                let identity = SessionIdentity {
                    source: SessionSource::Claude,
                    session_id: remote_session.session_id.clone(),
                };
                clear_pending_suppression_after_outcome(
                    maintenance_store.as_ref(),
                    &pending_suppression_clears,
                    &identity,
                    SuppressionApplyOutcome::SkippedChanged,
                );
                continue;
            }
            unchanged_count += 1;
            completed_sessions.insert(remote_session.session_id.clone());
            incomplete_reasons.remove(&remote_session.session_id);
        }

        let identity = SessionIdentity {
            source: SessionSource::Claude,
            session_id: remote_session.session_id.clone(),
        };
        clear_pending_suppression_after_outcome(
            maintenance_store.as_ref(),
            &pending_suppression_clears,
            &identity,
            if operation == SyncOperation::Unchanged {
                SuppressionApplyOutcome::Unchanged
            } else {
                SuppressionApplyOutcome::Written
            },
        );

        // Track all sessions (including unchanged) in affected conversations
        let relative_path_str = relative_path_for_tracking.to_string_lossy().to_string();
        match ConversationSummary::new(
            remote_session.session_id.clone(),
            relative_path_str.clone(),
            remote_session.latest_timestamp(),
            remote_session.message_count(),
            operation,
        ) {
            Ok(summary) => affected_conversations.push(summary),
            Err(e) => log::warn!("Failed to create summary for {}: {}", relative_path_str, e),
        }
    }

    println!("  {} Merged {} sessions", "✓".green(), merged_count);

    let protected_entries = incomplete_reasons
        .iter()
        .filter_map(|(session_id, reason)| {
            remote_revisions
                .get(session_id)
                .map(|(relative, fingerprint)| PullGuardEntry {
                    session_id: session_id.clone(),
                    remote_relative_path: relative.clone(),
                    remote_fingerprint: fingerprint.clone(),
                    reason: reason.clone(),
                })
        })
        .collect::<Vec<_>>();
    let completed_entries = completed_sessions
        .iter()
        .filter_map(|session_id| {
            remote_revisions
                .get(session_id)
                .map(|(relative, fingerprint)| {
                    (session_id.clone(), relative.clone(), fingerprint.clone())
                })
        })
        .collect::<Vec<_>>();
    PullGuardRegistry::reconcile(protected_entries, completed_entries)?;

    // ============================================================================
    // CREATE AND SAVE OPERATION RECORD
    // ============================================================================
    let mut operation_record = OperationRecord::new(
        OperationType::Pull,
        Some(branch_name.clone()),
        affected_conversations.clone(),
    );

    // Attach the snapshot path to the operation record (only if we created one)
    operation_record.snapshot_path = snapshot_path;

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
        log::info!("Pull completed successfully, but history was not updated.");
    }

    // ============================================================================
    // DISPLAY SUMMARY TO USER
    // ============================================================================
    println!("\n{}", "=== Pull Summary ===".bold().cyan());

    // Show operation statistics
    let conflict_count = detector.conflict_count();
    let stats_msg = format!(
        "  {} Added    {} Conflicts    {} Unchanged    {} Deferred",
        format!("{added_count}").green(),
        format!("{conflict_count}").yellow(),
        format!("{unchanged_count}").dimmed(),
        format!("{skipped_changed_count}").cyan(),
    );
    println!("{stats_msg}");
    if filter.use_project_name_only && skipped_no_local_match > 0 {
        println!(
            "  {} Skipped (no local match): {}",
            "!".yellow(),
            skipped_no_local_match
        );
    }
    if skipped_existing_target > 0 {
        println!(
            "  {} Skipped (existing local target): {}",
            "!".yellow(),
            skipped_existing_target
        );
    }
    if skipped_changed_count > 0 {
        println!(
            "  {} Skipped (local session changed or unsafe baseline): {}",
            "!".yellow(),
            skipped_changed_count
        );
    }
    println!();

    // Group conversations by project (top-level directory)
    let mut by_project: HashMap<String, Vec<&ConversationSummary>> = HashMap::new();
    for conv in &affected_conversations {
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
        println!("{}", "Affected Conversations:".bold());

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

    println!("\n{}", "Pull complete!".green().bold());

    // Clean up old snapshots automatically
    if let Err(e) = crate::undo::cleanup_old_snapshots(None, false) {
        log::warn!("Failed to cleanup old snapshots: {}", e);
    }

    // ============================================================================
    // SYNC AUTO MEMORY DIRECTORIES
    // ============================================================================
    if filter.auto_memory.enabled {
        println!("  {} auto memory directories...", "Syncing".cyan());

        let synced_projects = sync_auto_memory_from_remote(
            &state.sync_repo_path,
            &remote_projects_dir,
            &claude_dir,
            filter.use_project_name_only,
        )?;
        if verbosity == VerbosityLevel::Verbose {
            for project_name in &synced_projects {
                println!("    {} {}/memory", "←".cyan(), project_name);
            }
        }
        let synced_count = synced_projects.len();

        if verbosity != VerbosityLevel::Quiet {
            println!(
                "  {} Synced {} memory directories",
                "✓".green(),
                synced_count
            );
        }
    }

    // Auto-apply CLAUDE.md if enabled
    if filter.config_sync.enabled && filter.config_sync.auto_apply_claude_md {
        if let Err(e) = crate::handlers::config_sync::auto_apply_claude_md(&filter.config_sync) {
            log::debug!("Failed to auto-apply CLAUDE.md: {}", e);
        }
    }

    if !incomplete_reasons.is_empty() {
        let mut sessions = incomplete_reasons
            .into_iter()
            .map(|(session_id, reason)| PullIncompleteSession { session_id, reason })
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        return Err(PullIncomplete { sessions }.into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppression_decision_skips_same_revision_and_restores_changed_revision() {
        use crate::session_maintenance::state::{
            identity_key, LifecycleState, MaintenanceEntry, MaintenanceState,
        };
        use crate::session_model::{SessionIdentity, SessionSource};
        use std::path::PathBuf;

        let identity = SessionIdentity {
            source: SessionSource::Claude,
            session_id: "session-1".to_string(),
        };
        let mut state = MaintenanceState::default();
        state.entries.insert(
            identity_key(&identity),
            MaintenanceEntry {
                identity: identity.clone(),
                original_relative_path: PathBuf::from("project/session-1.jsonl"),
                project_name: "project".to_string(),
                fingerprint: "same".to_string(),
                lifecycle: LifecycleState::Recycled,
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

        assert_eq!(
            crate::session_maintenance::suppression_for_remote(&state, &identity, "same"),
            crate::session_maintenance::SuppressionDecision::SkipSameRevision
        );
        assert_eq!(
            crate::session_maintenance::suppression_for_remote(&state, &identity, "changed"),
            crate::session_maintenance::SuppressionDecision::RestoreNewRevision
        );

        for source in [SessionSource::Codex, SessionSource::Omp] {
            let other = SessionIdentity {
                source,
                session_id: "session-1".to_string(),
            };
            assert_eq!(
                crate::session_maintenance::suppression_for_remote(&state, &other, "same"),
                crate::session_maintenance::SuppressionDecision::NotSuppressed
            );
        }
    }

    #[test]
    fn changed_revision_rechecks_latest_state_before_restore() {
        use crate::session_maintenance::state::{
            identity_key, LifecycleState, MaintenanceEntry, StateStore,
        };
        use crate::session_model::{SessionIdentity, SessionSource};
        use std::path::PathBuf;

        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::from_config_dir(temp.path());
        let identity = SessionIdentity {
            source: SessionSource::Claude,
            session_id: "session-recheck".to_string(),
        };
        store
            .update(|state| {
                state.entries.insert(
                    identity_key(&identity),
                    MaintenanceEntry {
                        identity: identity.clone(),
                        original_relative_path: PathBuf::from("project/session-recheck.jsonl"),
                        project_name: "project".to_string(),
                        fingerprint: "old".to_string(),
                        lifecycle: LifecycleState::Recycled,
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
                Ok(())
            })
            .unwrap();

        assert_eq!(
            revalidate_suppression_for_remote(&store, &identity, "new").unwrap(),
            SuppressionRevalidation::Restore(PendingSuppressionClear {
                identity: identity.clone(),
                expected_fingerprint: "old".to_string(),
                expected_lifecycle: LifecycleState::Recycled,
            })
        );

        store
            .update(|state| {
                state
                    .entries
                    .get_mut(&identity_key(&identity))
                    .unwrap()
                    .fingerprint = "new".to_string();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            revalidate_suppression_for_remote(&store, &identity, "new").unwrap(),
            SuppressionRevalidation::SkipSameRevision
        );
    }

    #[test]
    fn cancelled_pull_keeps_suppression_pending() {
        assert!(!should_clear_suppression(
            SuppressionApplyOutcome::Cancelled
        ));
    }

    #[test]
    fn missing_project_keeps_suppression_pending() {
        assert!(!should_clear_suppression(
            SuppressionApplyOutcome::SkippedNoLocalProject
        ));
    }

    #[test]
    fn merge_write_failure_keeps_suppression_pending() {
        assert!(!should_clear_suppression(
            SuppressionApplyOutcome::WriteFailed
        ));
        assert!(!should_clear_suppression(
            SuppressionApplyOutcome::SkippedChanged
        ));
    }

    #[test]
    fn existing_target_skip_keeps_suppression_pending() {
        assert!(!should_clear_suppression(
            SuppressionApplyOutcome::SkippedExistingTarget
        ));
    }

    #[test]
    fn successful_active_write_clears_suppression_pending() {
        assert!(should_clear_suppression(SuppressionApplyOutcome::Written));
        assert!(should_clear_suppression(SuppressionApplyOutcome::Unchanged));
    }

    #[test]
    fn suppression_clear_happens_only_after_successful_active_outcome() {
        use crate::session_maintenance::state::{
            identity_key, LifecycleState, MaintenanceEntry, StateStore,
        };
        use std::path::PathBuf;

        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::from_config_dir(temp.path());
        let identity = SessionIdentity {
            source: SessionSource::Claude,
            session_id: "session-boundary".to_string(),
        };
        store
            .update(|state| {
                state.entries.insert(
                    identity_key(&identity),
                    MaintenanceEntry {
                        identity: identity.clone(),
                        original_relative_path: PathBuf::from("project/session-boundary.jsonl"),
                        project_name: "project".to_string(),
                        fingerprint: "old".to_string(),
                        lifecycle: LifecycleState::Recycled,
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
                Ok(())
            })
            .unwrap();
        let token = PendingSuppressionClear {
            identity: identity.clone(),
            expected_fingerprint: "old".to_string(),
            expected_lifecycle: LifecycleState::Recycled,
        };
        let mut pending = HashMap::new();
        pending.insert(identity.clone(), token);

        for outcome in [
            SuppressionApplyOutcome::Cancelled,
            SuppressionApplyOutcome::SkippedNoLocalProject,
            SuppressionApplyOutcome::WriteFailed,
            SuppressionApplyOutcome::SkippedChanged,
            SuppressionApplyOutcome::SkippedExistingTarget,
        ] {
            clear_pending_suppression_after_outcome(Some(&store), &pending, &identity, outcome);
            assert!(store
                .load()
                .unwrap()
                .entries
                .contains_key(&identity_key(&identity)));
        }
        clear_pending_suppression_after_outcome(
            Some(&store),
            &pending,
            &identity,
            SuppressionApplyOutcome::Written,
        );
        assert!(!store
            .load()
            .unwrap()
            .entries
            .contains_key(&identity_key(&identity)));
    }

    #[test]
    fn suppression_state_load_failure_is_not_suppressed() {
        let identity = crate::session_model::SessionIdentity {
            source: crate::session_model::SessionSource::Claude,
            session_id: "session-1".to_string(),
        };
        let state = crate::session_maintenance::state::MaintenanceState::default();
        assert_eq!(
            crate::session_maintenance::suppression_for_remote(&state, &identity, "same"),
            crate::session_maintenance::SuppressionDecision::NotSuppressed
        );
    }

    fn tombstone_record(session_id: &str) -> crate::sync::tombstone::DeletionRecord {
        crate::sync::tombstone::DeletionRecord {
            session_id: session_id.to_string(),
            repo_relative_path: format!("projects/project/{session_id}.jsonl"),
            project_name: "project".to_string(),
            source: "claude".to_string(),
            deleted_at: "2026-08-03T00:00:00Z".to_string(),
            device: "test".to_string(),
            reason: crate::sync::tombstone::DeleteReason::Explicit,
        }
    }

    fn session_json(session_id: &str, content: &str) -> String {
        serde_json::json!({
            "type": "user",
            "sessionId": session_id,
            "timestamp": "2026-09-09T00:00:00Z",
            "cwd": "/tmp/project",
            "message": {"role": "user", "content": content}
        })
        .to_string()
            + "\n"
    }

    #[test]
    fn oversized_local_session_is_not_overwritten_as_added() {
        let temp = tempfile::tempdir().unwrap();
        let local_root = temp.path().join("local");
        let remote_root = temp.path().join("remote");
        let relative = Path::new("project/session-id.jsonl");
        let local_file = local_root.join(relative);
        let remote_file = remote_root.join(relative);
        fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        fs::create_dir_all(remote_file.parent().unwrap()).unwrap();
        let local_bytes = session_json("session-id", &"x".repeat(512)).into_bytes();
        fs::write(&local_file, &local_bytes).unwrap();
        fs::write(&remote_file, session_json("session-id", "remote")).unwrap();
        let filter = FilterConfig {
            max_file_size_bytes: 256,
            ..FilterConfig::default()
        };

        assert!(discover_sessions(&local_root, &filter).unwrap().is_empty());
        let remote_session = discover_sessions(&remote_root, &filter)
            .unwrap()
            .pop()
            .unwrap();
        assert!(matches!(
            write_added_session_without_overwrite(&remote_session, &local_root, relative).unwrap(),
            AddedSessionWriteOutcome::SkippedExisting(path) if path == local_file
        ));
        assert_eq!(fs::read(&local_file).unwrap(), local_bytes);
    }

    #[test]
    fn added_session_write_skips_existing_filtered_target() {
        let temp = tempfile::tempdir().unwrap();
        let local_root = temp.path().join("local");
        let relative = Path::new("project/session-id.jsonl");
        let destination = local_root.join(relative);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"filtered-local-original").unwrap();
        let session = ConversationSession {
            session_id: "session-id".to_string(),
            entries: Vec::new(),
            file_path: String::new(),
        };

        assert!(matches!(
            write_added_session_without_overwrite(&session, &local_root, relative).unwrap(),
            AddedSessionWriteOutcome::SkippedExisting(path) if path == destination
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"filtered-local-original");
    }

    #[test]
    fn added_session_write_loses_create_race_without_overwriting_winner() {
        let temp = tempfile::tempdir().unwrap();
        let local_root = temp.path().join("local");
        fs::create_dir_all(&local_root).unwrap();
        let relative = Path::new("project/session-id.jsonl");
        let destination = local_root.join(relative);
        let session = ConversationSession {
            session_id: "session-id".to_string(),
            entries: Vec::new(),
            file_path: String::new(),
        };

        assert!(matches!(
            write_added_session_without_overwrite_with_hook(
                &session,
                &local_root,
                relative,
                |path| {
                    fs::write(path, b"race-winner")?;
                    Ok(())
                },
            )
            .unwrap(),
            AddedSessionWriteOutcome::SkippedExisting(path) if path == destination
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"race-winner");
    }

    #[test]
    fn added_session_write_creates_absent_target() {
        let temp = tempfile::tempdir().unwrap();
        let local_root = temp.path().join("local");
        fs::create_dir_all(&local_root).unwrap();
        let relative = Path::new("project/session-id.jsonl");
        let destination = local_root.join(relative);
        let source = temp.path().join("source.jsonl");
        fs::write(&source, session_json("session-id", "remote")).unwrap();
        let session = ConversationSession::from_file(&source).unwrap();

        assert_eq!(
            write_added_session_without_overwrite(&session, &local_root, relative).unwrap(),
            AddedSessionWriteOutcome::Written(destination.clone())
        );
        assert_eq!(
            ConversationSession::from_file(&destination)
                .unwrap()
                .session_id,
            "session-id"
        );
    }

    #[cfg(unix)]
    #[test]
    fn guarded_session_write_rejects_project_and_file_symlinks() {
        use std::os::unix::fs::symlink;

        for mode in ["root", "project", "file"] {
            let temp = tempfile::tempdir().unwrap();
            let local_root = temp.path().join("local");
            let outside = temp.path().join("outside");
            let outside_file = if mode == "root" {
                outside.join("project/session-id.jsonl")
            } else {
                outside.join("session-id.jsonl")
            };
            fs::create_dir_all(outside_file.parent().unwrap()).unwrap();
            fs::write(&outside_file, b"outside-marker").unwrap();

            match mode {
                "root" => symlink(&outside, &local_root).unwrap(),
                "project" => {
                    fs::create_dir_all(&local_root).unwrap();
                    symlink(&outside, local_root.join("project")).unwrap();
                }
                "file" => {
                    fs::create_dir_all(local_root.join("project")).unwrap();
                    symlink(&outside_file, local_root.join("project/session-id.jsonl")).unwrap();
                }
                _ => unreachable!(),
            }

            let session = ConversationSession {
                session_id: "session-id".to_string(),
                entries: Vec::new(),
                file_path: String::new(),
            };
            assert!(write_added_session_without_overwrite(
                &session,
                &local_root,
                Path::new("project/session-id.jsonl"),
            )
            .is_err());
            assert_eq!(fs::read(&outside_file).unwrap(), b"outside-marker");
        }
    }

    #[test]
    fn tombstone_propagation_deletes_regular_local_session() {
        let temp = tempfile::tempdir().unwrap();
        let local_root = temp.path().join("local");
        let local_file = local_root.join("project/session-abc-id.jsonl");
        fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        fs::write(&local_file, b"session").unwrap();
        let mut registry = TombstoneRegistry::default();
        registry.add(tombstone_record("session-abc-id"));

        assert_eq!(propagate_tombstones(&local_root, &registry).unwrap(), 1);
        assert!(!local_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn tombstone_propagation_rejects_project_and_file_symlinks() {
        use std::os::unix::fs::symlink;

        for mode in ["root", "project", "file"] {
            let temp = tempfile::tempdir().unwrap();
            let local_root = temp.path().join("local");
            let outside = temp.path().join("outside");
            let outside_file = if mode == "root" {
                outside.join("project/abc-id.jsonl")
            } else {
                outside.join("abc-id.jsonl")
            };
            fs::create_dir_all(outside_file.parent().unwrap()).unwrap();
            fs::write(&outside_file, b"outside-marker").unwrap();

            match mode {
                "root" => symlink(&outside, &local_root).unwrap(),
                "project" => {
                    fs::create_dir_all(&local_root).unwrap();
                    symlink(&outside, local_root.join("project")).unwrap();
                }
                "file" => {
                    fs::create_dir_all(local_root.join("project")).unwrap();
                    symlink(&outside_file, local_root.join("project/abc-id.jsonl")).unwrap();
                }
                _ => unreachable!(),
            }

            let mut registry = TombstoneRegistry::default();
            registry.add(tombstone_record("abc-id"));
            let result = propagate_tombstones(&local_root, &registry);
            if mode == "root" {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap(), 0);
            }
            assert_eq!(fs::read(&outside_file).unwrap(), b"outside-marker");
        }
    }

    #[test]
    fn auto_memory_pull_copies_normal_file_within_guarded_roots() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let remote_projects = repo.join("projects");
        let remote_memory = remote_projects.join("project/memory");
        let local_projects = temp.path().join("local");
        let local_project = local_projects.join("project");
        fs::create_dir_all(&remote_memory).unwrap();
        fs::create_dir_all(&local_project).unwrap();
        fs::write(remote_memory.join("note.md"), b"remote memory").unwrap();

        let synced =
            sync_auto_memory_from_remote(&repo, &remote_projects, &local_projects, false).unwrap();
        assert_eq!(synced, vec!["project"]);
        assert_eq!(
            fs::read(local_project.join("memory/note.md")).unwrap(),
            b"remote memory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn auto_memory_pull_rejects_remote_and_local_symlink_boundaries() {
        use std::os::unix::fs::symlink;

        for mode in [
            "remote-root",
            "remote-project",
            "remote-memory",
            "remote-file",
            "local-root",
            "local-project",
            "local-memory",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            let remote_projects = repo.join("projects");
            let local_projects = temp.path().join("local");
            let local_project = local_projects.join("project");
            let outside = temp.path().join("outside");
            let outside_memory = outside.join("memory");
            let outside_file = outside.join("secret.md");
            fs::create_dir_all(&repo).unwrap();
            fs::create_dir_all(&outside_memory).unwrap();
            fs::write(&outside_file, b"must not copy").unwrap();

            if mode == "remote-root" {
                fs::create_dir_all(&local_project).unwrap();
                symlink(&outside, &remote_projects).unwrap();
            } else {
                let remote_memory = remote_projects.join("project/memory");
                fs::create_dir_all(&remote_projects).unwrap();
                match mode {
                    "remote-project" => {
                        symlink(&outside, remote_projects.join("project")).unwrap();
                    }
                    "remote-memory" => {
                        fs::create_dir_all(remote_projects.join("project")).unwrap();
                        symlink(&outside, &remote_memory).unwrap();
                    }
                    "remote-file" => {
                        fs::create_dir_all(&remote_memory).unwrap();
                        symlink(&outside_file, remote_memory.join("secret.md")).unwrap();
                    }
                    _ => {
                        fs::create_dir_all(&remote_memory).unwrap();
                        fs::write(remote_memory.join("note.md"), b"remote memory").unwrap();
                    }
                }

                match mode {
                    "local-root" => {
                        symlink(&outside, &local_projects).unwrap();
                    }
                    "local-project" => {
                        fs::create_dir_all(&local_projects).unwrap();
                        symlink(&outside, &local_project).unwrap();
                    }
                    "local-memory" => {
                        fs::create_dir_all(&local_project).unwrap();
                        symlink(&outside_memory, local_project.join("memory")).unwrap();
                    }
                    _ => fs::create_dir_all(&local_project).unwrap(),
                }
            }

            assert!(
                sync_auto_memory_from_remote(&repo, &remote_projects, &local_projects, false)
                    .is_err(),
                "mode={mode}"
            );
            assert!(!local_project.join("memory/secret.md").exists());
            assert_eq!(fs::read(&outside_file).unwrap(), b"must not copy");
        }
    }
    #[test]
    fn smart_merge_uses_snapshot_bytes_instead_of_stale_discovery_session() {
        fn entry(uuid: &str) -> crate::parser::ConversationEntry {
            crate::parser::ConversationEntry {
                entry_type: "user".to_string(),
                uuid: Some(uuid.to_string()),
                parent_uuid: None,
                session_id: Some("session".to_string()),
                timestamp: None,
                message: None,
                cwd: None,
                version: None,
                git_branch: None,
                custom_title: None,
                extra: serde_json::Value::Null,
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let local_path = temp.path().join("session.jsonl");
        let stale = ConversationSession {
            session_id: "session".to_string(),
            entries: vec![entry("stale-discovery")],
            file_path: local_path.to_string_lossy().to_string(),
        };
        let remote = ConversationSession {
            session_id: "session".to_string(),
            entries: vec![entry("remote")],
            file_path: temp
                .path()
                .join("remote.jsonl")
                .to_string_lossy()
                .to_string(),
        };
        let snapshot_bytes = format!(
            "{}\n",
            serde_json::to_string(&entry("snapshot-local")).unwrap()
        )
        .into_bytes();
        std::fs::write(&local_path, &snapshot_bytes).unwrap();
        let baseline =
            SessionBaseline::from_snapshot(&local_path, "session", snapshot_bytes).unwrap();
        let mut conflict = crate::conflict::Conflict::new(&stale, &remote);

        let baseline_session = parse_snapshot_session(&conflict, &baseline).unwrap();
        conflict
            .try_smart_merge(&baseline_session, &remote)
            .unwrap();

        assert_eq!(
            baseline_session.entries[0].uuid.as_deref(),
            Some("snapshot-local")
        );
        let crate::conflict::ConflictResolution::SmartMerge { merged_entries, .. } =
            conflict.resolution
        else {
            panic!("expected SmartMerge resolution");
        };
        let uuids = merged_entries
            .iter()
            .filter_map(|entry| entry.uuid.as_deref())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            uuids,
            std::collections::HashSet::from(["snapshot-local", "remote"])
        );
        assert!(!uuids.contains("stale-discovery"));
    }
    #[test]
    fn unchanged_revalidation_detects_local_change_before_state_and_history_updates() {
        fn entry(uuid: &str) -> crate::parser::ConversationEntry {
            crate::parser::ConversationEntry {
                entry_type: "user".to_string(),
                uuid: Some(uuid.to_string()),
                parent_uuid: None,
                session_id: Some("session".to_string()),
                timestamp: None,
                message: None,
                cwd: None,
                version: None,
                git_branch: None,
                custom_title: None,
                extra: serde_json::Value::Null,
            }
        }
        let root = tempfile::tempdir().unwrap();
        let relative = Path::new("project/session.jsonl");
        let target = root.path().join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let discovery = ConversationSession {
            session_id: "session".to_string(),
            entries: vec![entry("same")],
            file_path: target.to_string_lossy().to_string(),
        };
        let remote = discovery.clone();
        std::fs::write(
            &target,
            format!("{}\n", serde_json::to_string(&entry("changed")).unwrap()),
        )
        .unwrap();

        assert!(
            !revalidate_unchanged_session(&discovery, &remote, root.path(), relative,).unwrap()
        );
    }
    #[test]
    fn automatic_smart_merge_rejects_snapshot_with_different_session_identity() {
        let temp = tempfile::tempdir().unwrap();
        let local_path = temp.path().join("session.jsonl");
        let wrong = b"{\"type\":\"user\",\"uuid\":\"other\",\"sessionId\":\"other-session\"}\n";
        std::fs::write(&local_path, wrong).unwrap();
        let baseline =
            SessionBaseline::from_snapshot(&local_path, "session", wrong.to_vec()).unwrap();
        let local = ConversationSession {
            session_id: "session".to_string(),
            entries: vec![],
            file_path: local_path.to_string_lossy().to_string(),
        };
        let remote = ConversationSession {
            session_id: "session".to_string(),
            entries: vec![],
            file_path: temp
                .path()
                .join("remote.jsonl")
                .to_string_lossy()
                .to_string(),
        };
        let conflict = crate::conflict::Conflict::new(&local, &remote);

        let error = parse_snapshot_session(&conflict, &baseline).unwrap_err();
        assert!(error.to_string().contains("identity changed"));
    }
    #[test]
    #[serial_test::serial]
    fn production_append_only_pull_keeps_concurrent_local_entry_visible_and_pushes_union() {
        struct EnvGuard {
            home: Option<std::ffi::OsString>,
            userprofile: Option<std::ffi::OsString>,
            config: Option<std::ffi::OsString>,
        }
        impl EnvGuard {
            fn set(home: &Path, config: &Path) -> Self {
                let guard = Self {
                    home: std::env::var_os("HOME"),
                    userprofile: std::env::var_os("USERPROFILE"),
                    config: std::env::var_os(crate::config::CONFIG_DIR_ENV),
                };
                std::env::set_var("HOME", home);
                std::env::set_var("USERPROFILE", home);
                std::env::set_var(crate::config::CONFIG_DIR_ENV, config);
                guard
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for (name, value) in [
                    ("HOME", self.home.take()),
                    ("USERPROFILE", self.userprofile.take()),
                    (crate::config::CONFIG_DIR_ENV, self.config.take()),
                ] {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let config = temp.path().join("config");
        let repo_path = temp.path().join("repo");
        let local_file = home.join(".claude/projects/project/session.jsonl");
        let remote_file = repo_path.join("projects/project/session.jsonl");
        std::fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(remote_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        let _guard = EnvGuard::set(&home, &config);

        let line = |uuid: &str, parent: Option<&str>| {
            serde_json::json!({
                "type": "user",
                "uuid": uuid,
                "parentUuid": parent,
                "sessionId": "session",
                "cwd": "/tmp/project",
                "message": {"role": "user", "content": uuid}
            })
            .to_string()
        };
        let local_bytes = format!("{}\n", line("a", None));
        let remote_bytes = format!("{}\n{}\n", line("a", None), line("b", Some("a")));
        std::fs::write(&local_file, &local_bytes).unwrap();
        std::fs::write(&remote_file, &remote_bytes).unwrap();

        crate::scm::init(&repo_path).unwrap();
        let repo = crate::scm::open(&repo_path).unwrap();
        repo.stage_all().unwrap();
        repo.commit("remote baseline").unwrap();
        SyncState {
            sync_repo_path: repo_path.clone(),
            has_remote: false,
            is_cloned_repo: false,
            last_synced_commit: None,
        }
        .save()
        .unwrap();
        let mut filter = FilterConfig {
            use_project_name_only: false,
            ..FilterConfig::default()
        };
        filter.session_maintenance.enabled = false;
        filter.config_sync.enabled = false;
        filter.auto_memory.enabled = false;
        filter.save().unwrap();
        std::fs::write(
            config.join("config.toml"),
            "[session_maintenance]\nenabled = false\n",
        )
        .unwrap();

        let changed_path = local_file.clone();
        let concurrent_line = line("c", Some("a"));
        set_after_snapshot_hook(move || {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&changed_path)
                .unwrap();
            writeln!(file, "{concurrent_line}").unwrap();
            file.sync_all().unwrap();
        });

        pull_history(false, None, false, crate::VerbosityLevel::Quiet).unwrap();

        let local_contents = std::fs::read_to_string(&local_file).unwrap();
        assert!(local_contents.contains("\"b\""));
        assert!(local_contents.contains("\"c\""));
        let history: OperationHistory =
            serde_json::from_slice(&std::fs::read(config.join("operation-history.json")).unwrap())
                .unwrap();
        assert!(history.operations.iter().any(|operation| {
            operation
                .affected_conversations
                .iter()
                .any(|conversation| conversation.operation == SyncOperation::Conflict)
        }));

        crate::sync::push_history(
            None,
            false,
            None,
            false,
            false,
            false,
            false,
            crate::VerbosityLevel::Quiet,
        )
        .unwrap();
        let remote_contents = std::fs::read_to_string(&remote_file).unwrap();
        assert!(remote_contents.contains("\"b\""));
        assert!(remote_contents.contains("\"c\""));

        let guard = PullGuardRegistry::load().unwrap();
        assert!(!guard
            .suppresses_push("session", Path::new("project/session.jsonl"), &remote_file)
            .unwrap());

        let remote_r2 = std::fs::read(&remote_file).unwrap();
        PullGuardRegistry::reconcile(
            [PullGuardEntry {
                session_id: "session".to_string(),
                remote_relative_path: PathBuf::from("project/session.jsonl"),
                remote_fingerprint: blake3::hash(b"older remote R1").to_hex().to_string(),
                reason: "R1 pull incomplete".to_string(),
            }],
            [],
        )
        .unwrap();
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&local_file)
                .unwrap();
            writeln!(file, "{}", line("e", Some("c"))).unwrap();
            file.sync_all().unwrap();
        }
        crate::sync::push_history(
            None,
            false,
            None,
            false,
            false,
            false,
            false,
            crate::VerbosityLevel::Quiet,
        )
        .unwrap();
        assert_eq!(std::fs::read(&remote_file).unwrap(), remote_r2);
        assert!(PullGuardRegistry::load()
            .unwrap()
            .suppresses_push("session", Path::new("project/session.jsonl"), &remote_file)
            .unwrap());
    }
    #[test]
    #[serial_test::serial]
    fn production_unchanged_revalidation_defers_mutation_without_history_or_suppression_clear() {
        struct EnvGuard {
            home: Option<std::ffi::OsString>,
            config: Option<std::ffi::OsString>,
        }
        impl EnvGuard {
            fn set(home: &Path, config: &Path) -> Self {
                let guard = Self {
                    home: std::env::var_os("HOME"),
                    config: std::env::var_os(crate::config::CONFIG_DIR_ENV),
                };
                std::env::set_var("HOME", home);
                std::env::set_var(crate::config::CONFIG_DIR_ENV, config);
                guard
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.home.take() {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match self.config.take() {
                    Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                    None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
                }
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let config = temp.path().join("config");
        let repo_path = temp.path().join("repo");
        let local_file = home.join(".claude/projects/project/session.jsonl");
        let remote_file = repo_path.join("projects/project/session.jsonl");
        std::fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(remote_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        let _guard = EnvGuard::set(&home, &config);
        let baseline = serde_json::json!({
            "type": "user",
            "uuid": "same",
            "sessionId": "session",
            "cwd": "/tmp/project",
            "message": {"role": "user", "content": "same"}
        })
        .to_string();
        std::fs::write(&local_file, format!("{baseline}\n")).unwrap();
        std::fs::write(&remote_file, format!("{baseline}\n")).unwrap();
        crate::scm::init(&repo_path).unwrap();
        let repo = crate::scm::open(&repo_path).unwrap();
        repo.stage_all().unwrap();
        repo.commit("same baseline").unwrap();
        SyncState {
            sync_repo_path: repo_path.clone(),
            has_remote: false,
            is_cloned_repo: false,
            last_synced_commit: None,
        }
        .save()
        .unwrap();
        let mut filter = FilterConfig {
            use_project_name_only: false,
            ..FilterConfig::default()
        };
        filter.session_maintenance.enabled = false;
        filter.config_sync.enabled = false;
        filter.auto_memory.enabled = false;
        filter.save().unwrap();
        std::fs::write(
            config.join("config.toml"),
            "[session_maintenance]\nenabled = false\n",
        )
        .unwrap();

        let changed_path = local_file.clone();
        set_after_snapshot_hook(move || {
            use std::io::Write;
            let changed = serde_json::json!({
                "type": "user",
                "uuid": "changed",
                "parentUuid": "same",
                "sessionId": "session",
                "cwd": "/tmp/project"
            });
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(changed_path)
                .unwrap();
            writeln!(file, "{changed}").unwrap();
            file.sync_all().unwrap();
        });

        let error = pull_history(false, None, false, crate::VerbosityLevel::Quiet).unwrap_err();
        assert!(error.downcast_ref::<PullIncomplete>().is_some());
        assert!(std::fs::read_to_string(&local_file)
            .unwrap()
            .contains("changed"));
        let history = OperationHistory::load().unwrap();
        assert!(history.operations[0].affected_conversations.is_empty());
        assert!(PullGuardRegistry::load()
            .unwrap()
            .suppresses_push("session", Path::new("project/session.jsonl"), &remote_file)
            .unwrap());
    }
}
