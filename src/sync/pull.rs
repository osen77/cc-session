use anyhow::{Context, Result};
use colored::Colorize;
use inquire::Confirm;
use std::collections::HashMap;
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
use crate::sync::tombstone::TombstoneRegistry;
use crate::undo::Snapshot;
use crate::BINARY_NAME;

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

fn write_session_within_local_root(
    session: &ConversationSession,
    local_root: &Path,
    relative: &Path,
) -> Result<PathBuf> {
    prepare_local_session_destination(local_root, relative)?;
    // Rebuild and revalidate immediately before the write. A same-UID
    // replacement after this check remains the documented narrow TOCTOU residual.
    let destination = prepare_local_session_destination(local_root, relative)?;
    session.write_to_file(&destination)?;
    Ok(destination)
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

    // Discover local sessions
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
    // SNAPSHOT CREATION: Only backup files that have conflicts
    // ============================================================================
    // Optimization: Only backup local files that have conflicts and will be merged.
    // Files that are new (remote-only) or unchanged don't need backup.
    // This reduces snapshot size from potentially gigabytes to typically <1MB.
    let snapshot_path = if detector.has_conflicts() {
        println!(
            "  {} snapshot of {} conflicting files...",
            "Creating".cyan(),
            detector.conflict_count()
        );

        // Only collect paths for files that have conflicts
        let conflicting_file_paths: Vec<PathBuf> = detector
            .conflicts()
            .iter()
            .map(|c| c.local_file.clone())
            .collect();

        // Check for large conversation files and warn users
        warn_large_files(&conflicting_file_paths);

        // Create snapshot of ONLY conflicting files
        let snapshot = Snapshot::create(
            OperationType::Pull,
            conflicting_file_paths.iter(),
            None, // No git manager needed for pull snapshots
        )
        .context("Failed to create snapshot before pull")?;

        // Save snapshot to disk
        let path = snapshot
            .save_to_disk(None)
            .context("Failed to save snapshot to disk")?;

        if verbosity != VerbosityLevel::Quiet {
            println!(
                "  {} Snapshot created: {} ({} files)",
                "✓".green(),
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string()),
                conflicting_file_paths.len()
            );
        }

        Some(path)
    } else {
        println!("  {} No conflicts - skipping snapshot", "✓".green());
        None
    };

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

    // Show detailed file list in verbose mode
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

    // Interactive confirmation
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
    // CONFLICT RESOLUTION (detection already done above)
    // ============================================================================
    // Track affected conversations for operation record
    let mut affected_conversations: Vec<ConversationSummary> = Vec::new();

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

        let local_map: HashMap<_, _> = local_sessions
            .iter()
            .map(|s| (s.session_id.clone(), s))
            .collect();

        let remote_map: HashMap<_, _> = remote_sessions
            .iter()
            .map(|s| (s.session_id.clone(), s))
            .collect();

        let mut smart_merge_success_count = 0;
        let mut smart_merge_failed_conflicts = Vec::new();

        for conflict in detector.conflicts_mut() {
            // Find local and remote sessions
            if let (Some(local_session), Some(remote_session)) = (
                local_map.get(&conflict.session_id),
                remote_map.get(&conflict.session_id),
            ) {
                // Try smart merge
                match conflict.try_smart_merge(local_session, remote_session) {
                    Ok(()) => {
                        smart_merge_success_count += 1;
                        // Write merged result to local file
                        if let crate::conflict::ConflictResolution::SmartMerge {
                            ref merged_entries,
                            ref stats,
                        } = conflict.resolution
                        {
                            // Create a new session with merged entries
                            let merged_session = ConversationSession {
                                session_id: conflict.session_id.clone(),
                                entries: merged_entries.clone(),
                                file_path: conflict.local_file.to_string_lossy().to_string(),
                            };

                            // Rebuild the destination from the trusted local root.
                            let local_relative = conflict
                                .local_file
                                .strip_prefix(&claude_dir)
                                .context("conflict destination is outside Claude projects root")?;
                            if let Err(e) = write_session_within_local_root(
                                &merged_session,
                                &claude_dir,
                                local_relative,
                            ) {
                                log::warn!(
                                    "Failed to write merged session {}: {}",
                                    conflict.session_id,
                                    e
                                );
                                let identity = SessionIdentity {
                                    source: SessionSource::Claude,
                                    session_id: conflict.session_id.clone(),
                                };
                                clear_pending_suppression_after_outcome(
                                    maintenance_store.as_ref(),
                                    &pending_suppression_clears,
                                    &identity,
                                    SuppressionApplyOutcome::WriteFailed,
                                );
                                smart_merge_failed_conflicts.push(conflict.clone());
                            } else {
                                println!(
                                    "  {} Smart merged {} ({} local + {} remote = {} total, {} branches)",
                                    "✓".green(),
                                    conflict.session_id,
                                    stats.local_messages,
                                    stats.remote_messages,
                                    stats.merged_messages,
                                    stats.branches_detected
                                );
                                let identity = SessionIdentity {
                                    source: SessionSource::Claude,
                                    session_id: conflict.session_id.clone(),
                                };
                                clear_pending_suppression_after_outcome(
                                    maintenance_store.as_ref(),
                                    &pending_suppression_clears,
                                    &identity,
                                    SuppressionApplyOutcome::Written,
                                );
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("Smart merge failed for {}: {}", conflict.session_id, e);
                        log::info!("Falling back to manual resolution...");
                        let identity = SessionIdentity {
                            source: SessionSource::Claude,
                            session_id: conflict.session_id.clone(),
                        };
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

            // Check if we can run interactively
            let use_interactive = crate::interactive_conflict::is_interactive();

            if use_interactive {
                // Interactive conflict resolution for failed merges
                println!(
                    "\n{} Running in interactive mode for remaining conflicts",
                    "→".cyan()
                );

                let resolution_result = crate::interactive_conflict::resolve_conflicts_interactive(
                    &mut smart_merge_failed_conflicts,
                )?;

                // Apply the resolutions
                let renames = crate::interactive_conflict::apply_resolutions(
                    &resolution_result,
                    &remote_sessions,
                    &claude_dir,
                    &remote_projects_dir,
                )?;

                for conflict in resolution_result
                    .smart_merge
                    .iter()
                    .chain(resolution_result.keep_remote.iter())
                    .chain(resolution_result.keep_both.iter())
                {
                    let identity = SessionIdentity {
                        source: SessionSource::Claude,
                        session_id: conflict.session_id.clone(),
                    };
                    clear_pending_suppression_after_outcome(
                        maintenance_store.as_ref(),
                        &pending_suppression_clears,
                        &identity,
                        SuppressionApplyOutcome::Written,
                    );
                }

                // Save conflict report
                let report = ConflictReport::from_conflicts(detector.conflicts());
                save_conflict_report(&report)?;

                renames
            } else {
                // Non-interactive mode: use "keep both" strategy for failed merges
                println!(
                    "\n{} Using automatic conflict resolution (keep both versions)",
                    "→".cyan()
                );

                let mut renames = Vec::new();
                let mut successful_keep_both_ids = Vec::new();

                println!("\n{}", "Conflict Resolution:".yellow().bold());
                for conflict in &smart_merge_failed_conflicts {
                    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                    let conflict_suffix = format!("conflict-{timestamp}");

                    if let Ok(renamed_path) = conflict.clone().resolve_keep_both(&conflict_suffix) {
                        let relative_renamed = renamed_path
                            .strip_prefix(&claude_dir)
                            .unwrap_or(&renamed_path);
                        println!(
                            "  {} remote version saved as: {}",
                            "→".yellow(),
                            relative_renamed.display().to_string().cyan()
                        );

                        // Find and write the remote session through the trusted local root.
                        if let Some(session) = remote_sessions
                            .iter()
                            .find(|s| s.session_id == conflict.session_id)
                        {
                            let renamed_relative = renamed_path.strip_prefix(&claude_dir).context(
                                "conflict copy destination is outside Claude projects root",
                            )?;
                            write_session_within_local_root(
                                session,
                                &claude_dir,
                                renamed_relative,
                            )?;
                            successful_keep_both_ids.push(conflict.session_id.clone());
                        }

                        renames.push((conflict.remote_file.clone(), renamed_path));
                    }
                }

                for session_id in successful_keep_both_ids {
                    let identity = SessionIdentity {
                        source: SessionSource::Claude,
                        session_id,
                    };
                    clear_pending_suppression_after_outcome(
                        maintenance_store.as_ref(),
                        &pending_suppression_clears,
                        &identity,
                        SuppressionApplyOutcome::Written,
                    );
                }

                // Save conflict report
                let report = ConflictReport::from_conflicts(detector.conflicts());
                save_conflict_report(&report)?;

                renames
            }
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
    let mut modified_count = 0;
    let mut unchanged_count = 0;
    let mut skipped_no_local_match = 0;

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
            return Err(error);
        }

        // Determine operation type based on local state
        let operation = if let Some(local) = local_map.get(&remote_session.session_id) {
            if local.content_hash() == remote_session.content_hash() {
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

        // Copy file if it's not unchanged
        if operation != SyncOperation::Unchanged {
            if let Err(error) = write_session_within_local_root(
                remote_session,
                &claude_dir,
                &relative_path_for_tracking,
            ) {
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
                return Err(error);
            }
            merged_count += 1;
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
        "  {} Added    {} Modified    {} Conflicts    {} Unchanged",
        format!("{added_count}").green(),
        format!("{modified_count}").cyan(),
        format!("{conflict_count}").yellow(),
        format!("{unchanged_count}").dimmed(),
    );
    println!("{stats_msg}");
    if filter.use_project_name_only && skipped_no_local_match > 0 {
        println!(
            "  {} Skipped (no local match): {}",
            "!".yellow(),
            skipped_no_local_match
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

    #[test]
    fn guarded_session_write_creates_normal_destination() {
        let temp = tempfile::tempdir().unwrap();
        let local_root = temp.path().join("local");
        fs::create_dir_all(&local_root).unwrap();
        let session = ConversationSession {
            session_id: "session-id".to_string(),
            entries: Vec::new(),
            file_path: String::new(),
        };

        let destination = write_session_within_local_root(
            &session,
            &local_root,
            Path::new("project/session-id.jsonl"),
        )
        .unwrap();
        assert_eq!(destination, local_root.join("project/session-id.jsonl"));
        assert!(destination.is_file());
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
            assert!(write_session_within_local_root(
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
}
