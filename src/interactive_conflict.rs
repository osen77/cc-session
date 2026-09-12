use anyhow::{Context, Result};
use colored::Colorize;
use inquire::{Confirm, Select};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::conflict::{Conflict, ConflictResolution};
use crate::parser::ConversationSession;
use crate::sync::session_write::{
    append_merged_entries_guarded, write_new_session_noclobber, AppendSessionWriteOutcome,
    NewSessionWriteOutcome, SessionBaseline,
};

/// Resolution action chosen by the user
#[derive(Debug, Clone)]
pub enum ResolutionAction {
    /// Intelligently merge both versions (default/recommended)
    SmartMerge,
    /// Keep the local version and discard the remote changes
    KeepLocal,
    /// Request the remote version as a separate safe copy; the active local file is not overwritten
    KeepRemote,
    /// Keep both versions by saving the remote file with a conflict suffix
    KeepBoth,
    /// View detailed comparison of the conflicting files (does not resolve the conflict)
    ViewDetails,
}

impl std::fmt::Display for ResolutionAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolutionAction::SmartMerge => {
                write!(f, "Smart Merge (combine both versions - recommended)")
            }
            ResolutionAction::KeepLocal => write!(f, "Keep Local Version (discard remote)"),
            ResolutionAction::KeepRemote => {
                write!(
                    f,
                    "Save Remote as Separate Copy (active local stays unchanged)"
                )
            }
            ResolutionAction::KeepBoth => {
                write!(f, "Keep Both (save remote with conflict suffix)")
            }
            ResolutionAction::ViewDetails => write!(f, "View Detailed Comparison"),
        }
    }
}

/// Result of interactive conflict resolution
#[derive(Debug)]
pub struct ResolutionResult {
    /// Conflicts resolved via smart merge
    pub smart_merge: Vec<Conflict>,
    /// Conflicts that should keep local version (discard remote)
    pub keep_local: Vec<Conflict>,
    /// Conflicts requesting a safe remote copy without overwriting the active local file
    pub keep_remote: Vec<Conflict>,
    /// Conflicts that should keep both versions (rename remote)
    pub keep_both: Vec<Conflict>,
}

impl Default for ResolutionResult {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolutionResult {
    /// Creates a new empty ResolutionResult with all conflict vectors initialized
    pub fn new() -> Self {
        ResolutionResult {
            smart_merge: Vec::new(),
            keep_local: Vec::new(),
            keep_remote: Vec::new(),
            keep_both: Vec::new(),
        }
    }

    /// Total number of conflicts resolved
    #[allow(dead_code)]
    pub fn total(&self) -> usize {
        self.smart_merge.len()
            + self.keep_local.len()
            + self.keep_remote.len()
            + self.keep_both.len()
    }
}

/// Check if we're running in an interactive terminal
pub fn is_interactive() -> bool {
    atty::is(atty::Stream::Stdin) && atty::is(atty::Stream::Stdout)
}

/// Display detailed conflict information
fn display_conflict_details(conflict: &Conflict) {
    println!("\n{}", "=".repeat(80).cyan());
    println!("{}", "Conflict Details".bold().cyan());
    println!("{}", "=".repeat(80).cyan());

    println!("\n{} {}", "Session ID:".bold(), conflict.session_id.cyan());

    println!(
        "\n{} {}",
        "Local File:".bold().green(),
        conflict.local_file.display()
    );
    println!(
        "  {} messages",
        conflict.local_message_count.to_string().green()
    );
    if let Some(ts) = &conflict.local_timestamp {
        println!("  Last updated: {}", ts.dimmed());
    }
    println!("  Content hash: {}", &conflict.local_hash[..16].dimmed());

    println!(
        "\n{} {}",
        "Remote File:".bold().yellow(),
        conflict.remote_file.display()
    );
    println!(
        "  {} messages",
        conflict.remote_message_count.to_string().yellow()
    );
    if let Some(ts) = &conflict.remote_timestamp {
        println!("  Last updated: {}", ts.dimmed());
    }
    println!("  Content hash: {}", &conflict.remote_hash[..16].dimmed());

    // Highlight the differences
    let msg_diff = conflict.remote_message_count as i32 - conflict.local_message_count as i32;
    if msg_diff > 0 {
        println!(
            "\n{} Remote has {} more messages",
            "→".yellow(),
            msg_diff.to_string().yellow().bold()
        );
    } else if msg_diff < 0 {
        println!(
            "\n{} Local has {} more messages",
            "→".green(),
            (-msg_diff).to_string().green().bold()
        );
    } else {
        println!(
            "\n{} Both have the same number of messages, but content differs",
            "→".cyan()
        );
    }

    println!("{}", "=".repeat(80).cyan());
}

/// Interactively resolve a single conflict
fn resolve_conflict_interactive(conflict: &Conflict) -> Result<ResolutionAction> {
    loop {
        println!("\n{}", "Conflict Detected!".yellow().bold());
        println!("  {}", conflict.description().dimmed());

        let options = vec![
            ResolutionAction::SmartMerge,
            ResolutionAction::KeepLocal,
            ResolutionAction::KeepRemote,
            ResolutionAction::KeepBoth,
            ResolutionAction::ViewDetails,
        ];

        let action = Select::new("How would you like to resolve this conflict?", options)
            .with_help_message(
                "Use arrow keys to navigate. Remote preference is saved as a separate copy; active local history is never overwritten",
            )
            .prompt()
            .context("Failed to get resolution action")?;

        match action {
            ResolutionAction::ViewDetails => {
                display_conflict_details(conflict);
                // Loop back to ask again
                continue;
            }
            _ => return Ok(action),
        }
    }
}

/// Interactively resolve all conflicts
///
/// This function presents each conflict to the user one at a time,
/// allowing them to choose how to resolve it.
///
/// # Arguments
/// * `conflicts` - Mutable slice of conflicts to resolve
/// * `local_sessions` - Optional map of local sessions (for smart merge)
/// * `remote_sessions` - Optional map of remote sessions (for smart merge)
///
/// # Returns
/// A `ResolutionResult` containing the categorized conflicts
pub fn resolve_conflicts_interactive_with_sessions(
    conflicts: &mut [Conflict],
    local_sessions: Option<&std::collections::HashMap<String, &ConversationSession>>,
    remote_sessions: Option<&std::collections::HashMap<String, &ConversationSession>>,
) -> Result<ResolutionResult> {
    if conflicts.is_empty() {
        return Ok(ResolutionResult::new());
    }

    let total_conflicts = conflicts.len();

    println!(
        "\n{}",
        format!("Found {total_conflicts} conflicts to resolve")
            .yellow()
            .bold()
    );
    println!("{}", "Let's resolve them one by one...".cyan());

    let mut result = ResolutionResult::new();

    for (idx, conflict) in conflicts.iter_mut().enumerate() {
        println!(
            "\n{} Conflict {} of {}",
            ">>>".yellow().bold(),
            (idx + 1).to_string().cyan(),
            total_conflicts.to_string().cyan()
        );

        loop {
            let action = resolve_conflict_interactive(conflict)?;

            match action {
                ResolutionAction::SmartMerge => {
                    // Attempt smart merge
                    if let (Some(local_map), Some(remote_map)) = (local_sessions, remote_sessions) {
                        if let (Some(&local_session), Some(&remote_session)) = (
                            local_map.get(&conflict.session_id),
                            remote_map.get(&conflict.session_id),
                        ) {
                            match conflict.try_smart_merge(local_session, remote_session) {
                                Ok(()) => {
                                    if let ConflictResolution::SmartMerge { ref stats, .. } =
                                        conflict.resolution
                                    {
                                        println!(
                                        "  {} Smart merged ({} local + {} remote = {} total, {} branches)",
                                        "✓".green(),
                                        stats.local_messages,
                                        stats.remote_messages,
                                        stats.merged_messages,
                                        stats.branches_detected
                                    );
                                    }
                                    result.smart_merge.push(conflict.clone());
                                }
                                Err(e) => {
                                    eprintln!("  {} Smart merge failed: {}", "✗".red(), e);
                                    eprintln!("  Please choose another resolution method...");
                                    // Don't add to result, user will be prompted again
                                    continue;
                                }
                            }
                        } else {
                            eprintln!("  {} Cannot find local or remote session", "✗".red());
                            eprintln!("  Please choose another resolution method...");
                            continue;
                        }
                    } else {
                        eprintln!("  {} Session maps not provided", "✗".red());
                        eprintln!("  Please choose another resolution method...");
                        continue;
                    }
                }
                ResolutionAction::KeepLocal => {
                    println!("  {} Keeping local version", "✓".green());
                    conflict.resolution = ConflictResolution::KeepLocal;
                    result.keep_local.push(conflict.clone());
                }
                ResolutionAction::KeepRemote => {
                    println!(
                        "  {} Saving remote as a separate conflict copy; active local stays unchanged",
                        "✓".yellow()
                    );
                    conflict.resolution = ConflictResolution::KeepRemote;
                    result.keep_remote.push(conflict.clone());
                }
                ResolutionAction::KeepBoth => {
                    println!(
                        "  {} Keeping both versions (remote will be saved with conflict suffix)",
                        "✓".cyan()
                    );
                    // Keep both is handled later with proper renaming
                    result.keep_both.push(conflict.clone());
                }
                ResolutionAction::ViewDetails => {
                    unreachable!("ViewDetails should be handled in the loop")
                }
            }
            break;
        }
    }

    println!("\n{}", "=".repeat(80).green());
    println!("{}", "Resolution Summary".bold().green());
    println!("{}", "=".repeat(80).green());
    println!(
        "  Smart Merge: {}",
        result.smart_merge.len().to_string().cyan()
    );
    println!(
        "  Keep Local:  {}",
        result.keep_local.len().to_string().green()
    );
    println!(
        "  Safe Remote Copy: {}",
        result.keep_remote.len().to_string().yellow()
    );
    println!(
        "  Keep Both:   {}",
        result.keep_both.len().to_string().cyan()
    );
    println!("{}", "=".repeat(80).green());

    // Final confirmation
    let confirm = Confirm::new("Apply these resolutions?")
        .with_default(true)
        .prompt()
        .context("Failed to get confirmation")?;

    if !confirm {
        return Err(anyhow::anyhow!(
            "Resolution cancelled by user. No changes were made."
        ));
    }

    Ok(result)
}

/// Backward-compatible version of resolve_conflicts_interactive without session maps
///
/// This version doesn't support SmartMerge since it requires session data.
/// Use `resolve_conflicts_interactive_with_sessions` for full functionality.
#[allow(dead_code)]
pub fn resolve_conflicts_interactive(conflicts: &mut [Conflict]) -> Result<ResolutionResult> {
    resolve_conflicts_interactive_with_sessions(conflicts, None, None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GuardedApplyOutcome {
    Written {
        entries_added: usize,
        durability: Option<crate::sync::session_write::CommitDurability>,
    },
    SkippedChanged,
    KeptLocal,
    CreatedConflictCopy {
        path: PathBuf,
        durability: crate::sync::session_write::CommitDurability,
    },
}

#[derive(Debug, Default)]
pub(crate) struct GuardedApplyResult {
    pub(crate) renames: Vec<(PathBuf, PathBuf)>,
    pub(crate) outcomes: Vec<(String, GuardedApplyOutcome)>,
}

fn relative_local_path<'a>(root: &Path, destination: &'a Path) -> Result<&'a Path> {
    destination.strip_prefix(root).with_context(|| {
        format!(
            "conflict destination is outside root: {}",
            destination.display()
        )
    })
}

fn write_conflict_copy_noclobber(
    conflict: &Conflict,
    remote_session: &ConversationSession,
    claude_dir: &Path,
) -> Result<(PathBuf, crate::sync::session_write::CommitDurability)> {
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    for attempt in 0..1000usize {
        let suffix = if attempt == 0 {
            format!("conflict-{timestamp}")
        } else {
            format!("conflict-{timestamp}-{attempt}")
        };
        let renamed_path = conflict.clone().resolve_keep_both(&suffix)?;
        let relative = relative_local_path(claude_dir, &renamed_path)?;
        match write_new_session_noclobber(remote_session, claude_dir, relative)? {
            NewSessionWriteOutcome::Written { path, durability } => {
                return Ok((path, durability));
            }
            NewSessionWriteOutcome::SkippedExisting(_) => continue,
        }
    }
    anyhow::bail!(
        "could not allocate a no-clobber conflict copy for {}",
        conflict.session_id
    )
}

/// Apply resolutions using snapshot-derived local baselines. Existing files are
/// replaced only if they still match the exact bytes observed before the user
/// made a resolution choice.
pub(crate) fn apply_resolutions_guarded(
    result: &ResolutionResult,
    remote_sessions: &[ConversationSession],
    claude_dir: &Path,
    _remote_projects_dir: &Path,
    baselines: &HashMap<String, SessionBaseline>,
) -> Result<GuardedApplyResult> {
    let mut applied = GuardedApplyResult::default();

    for conflict in &result.smart_merge {
        let Some(baseline) = baselines.get(&conflict.session_id) else {
            applied.outcomes.push((
                conflict.session_id.clone(),
                GuardedApplyOutcome::SkippedChanged,
            ));
            continue;
        };
        if let ConflictResolution::SmartMerge {
            ref merged_entries, ..
        } = conflict.resolution
        {
            let merged_session = ConversationSession {
                session_id: conflict.session_id.clone(),
                entries: merged_entries.clone(),
                file_path: conflict.local_file.to_string_lossy().to_string(),
            };
            let relative = relative_local_path(claude_dir, &conflict.local_file)?;
            match append_merged_entries_guarded(
                claude_dir,
                relative,
                baseline,
                &merged_session.entries,
            ) {
                Ok(AppendSessionWriteOutcome::Written {
                    entries_added,
                    durability,
                }) => applied.outcomes.push((
                    conflict.session_id.clone(),
                    GuardedApplyOutcome::Written {
                        entries_added,
                        durability: Some(durability),
                    },
                )),
                Ok(AppendSessionWriteOutcome::Noop) => applied.outcomes.push((
                    conflict.session_id.clone(),
                    GuardedApplyOutcome::Written {
                        entries_added: 0,
                        durability: None,
                    },
                )),
                Ok(AppendSessionWriteOutcome::SkippedChanged) | Err(_) => {
                    let remote_session = remote_sessions
                        .iter()
                        .find(|session| session.session_id == conflict.session_id)
                        .with_context(|| {
                            format!(
                                "remote session is unavailable for SmartMerge fallback: {}",
                                conflict.session_id
                            )
                        })?;
                    let (path, durability) =
                        write_conflict_copy_noclobber(conflict, remote_session, claude_dir)?;
                    applied
                        .renames
                        .push((conflict.remote_file.clone(), path.clone()));
                    applied.outcomes.push((
                        conflict.session_id.clone(),
                        GuardedApplyOutcome::CreatedConflictCopy { path, durability },
                    ));
                }
            }
        }
    }

    for conflict in &result.keep_remote {
        let remote_session = remote_sessions
            .iter()
            .find(|session| session.session_id == conflict.session_id)
            .with_context(|| {
                format!(
                    "remote session is unavailable for KeepRemote resolution: {}",
                    conflict.session_id
                )
            })?;
        let (path, durability) =
            write_conflict_copy_noclobber(conflict, remote_session, claude_dir)?;
        applied
            .renames
            .push((conflict.remote_file.clone(), path.clone()));
        applied.outcomes.push((
            conflict.session_id.clone(),
            GuardedApplyOutcome::CreatedConflictCopy { path, durability },
        ));
    }

    for conflict in &result.keep_both {
        let remote_session = remote_sessions
            .iter()
            .find(|session| session.session_id == conflict.session_id)
            .with_context(|| {
                format!(
                    "remote session is unavailable for KeepBoth resolution: {}",
                    conflict.session_id
                )
            })?;
        let (renamed_path, durability) =
            write_conflict_copy_noclobber(conflict, remote_session, claude_dir)?;
        applied
            .renames
            .push((conflict.remote_file.clone(), renamed_path.clone()));
        applied.outcomes.push((
            conflict.session_id.clone(),
            GuardedApplyOutcome::CreatedConflictCopy {
                path: renamed_path,
                durability,
            },
        ));
    }

    for conflict in &result.keep_local {
        applied
            .outcomes
            .push((conflict.session_id.clone(), GuardedApplyOutcome::KeptLocal));
    }

    Ok(applied)
}

pub(crate) fn update_reported_resolutions(
    conflicts: &mut [Conflict],
    result: &ResolutionResult,
    applied: &GuardedApplyResult,
) {
    for conflict in conflicts {
        let Some((_, outcome)) = applied
            .outcomes
            .iter()
            .find(|(session_id, _)| session_id == &conflict.session_id)
        else {
            continue;
        };
        conflict.resolution = match outcome {
            GuardedApplyOutcome::Written { .. } => result
                .smart_merge
                .iter()
                .find(|item| item.session_id == conflict.session_id)
                .map(|item| item.resolution.clone())
                .unwrap_or(ConflictResolution::KeepRemote),
            GuardedApplyOutcome::CreatedConflictCopy { path, .. } => ConflictResolution::KeepBoth {
                renamed_remote_file: path.clone(),
            },
            GuardedApplyOutcome::KeptLocal => ConflictResolution::KeepLocal,
            GuardedApplyOutcome::SkippedChanged => ConflictResolution::Pending,
        };
    }
}

/// Apply resolution results. This compatibility wrapper captures each current
/// local file as its immediate baseline, then delegates to the guarded writer.
#[allow(dead_code)]
pub fn apply_resolutions(
    result: &ResolutionResult,
    remote_sessions: &[ConversationSession],
    claude_dir: &Path,
    remote_projects_dir: &Path,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut baselines = HashMap::new();
    for conflict in result.smart_merge.iter().chain(result.keep_remote.iter()) {
        let bytes = std::fs::read(&conflict.local_file).with_context(|| {
            format!(
                "failed to read compatibility resolution baseline: {}",
                conflict.local_file.display()
            )
        })?;
        baselines.insert(
            conflict.session_id.clone(),
            SessionBaseline::from_snapshot(&conflict.local_file, &conflict.session_id, bytes)?,
        );
    }
    let applied = apply_resolutions_guarded(
        result,
        remote_sessions,
        claude_dir,
        remote_projects_dir,
        &baselines,
    )?;
    let outcome_for = |session_id: &str| {
        applied
            .outcomes
            .iter()
            .find(|(applied_id, _)| applied_id == session_id)
            .map(|(_, outcome)| outcome)
    };
    for conflict in &result.smart_merge {
        if !matches!(
            outcome_for(&conflict.session_id),
            Some(GuardedApplyOutcome::Written { .. })
        ) {
            anyhow::bail!(
                "SmartMerge resolution was not applied for {}",
                conflict.session_id
            );
        }
    }
    for conflict in &result.keep_remote {
        if !matches!(
            outcome_for(&conflict.session_id),
            Some(GuardedApplyOutcome::Written { .. })
        ) {
            anyhow::bail!(
                "KeepRemote resolution was not applied for {}",
                conflict.session_id
            );
        }
    }
    for conflict in &result.keep_both {
        if !matches!(
            outcome_for(&conflict.session_id),
            Some(GuardedApplyOutcome::CreatedConflictCopy { .. })
        ) {
            anyhow::bail!(
                "KeepBoth resolution was not applied for {}",
                conflict.session_id
            );
        }
    }
    for conflict in &result.keep_local {
        if !matches!(
            outcome_for(&conflict.session_id),
            Some(GuardedApplyOutcome::KeptLocal)
        ) {
            anyhow::bail!(
                "KeepLocal resolution was not applied for {}",
                conflict.session_id
            );
        }
    }
    Ok(applied.renames)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;

    #[test]
    fn test_resolution_result() {
        let result = ResolutionResult::new();
        assert_eq!(result.total(), 0);
        assert_eq!(result.keep_local.len(), 0);
        assert_eq!(result.keep_remote.len(), 0);
        assert_eq!(result.keep_both.len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn smart_merge_rejects_root_project_and_file_symlink_destinations() {
        use std::os::unix::fs::symlink;

        for mode in ["root", "project", "file"] {
            let temp = tempfile::tempdir().unwrap();
            let real_root = temp.path().join("local");
            let outside = temp.path().join("outside");
            let outside_file = outside.join("session.jsonl");
            fs::create_dir_all(&outside).unwrap();
            fs::write(&outside_file, b"outside-marker").unwrap();

            let claude_dir = if mode == "root" {
                let root_link = temp.path().join("local-link");
                symlink(&outside, &root_link).unwrap();
                root_link
            } else {
                fs::create_dir_all(&real_root).unwrap();
                if mode == "project" {
                    symlink(&outside, real_root.join("project")).unwrap();
                } else {
                    fs::create_dir_all(real_root.join("project")).unwrap();
                    symlink(&outside_file, real_root.join("project/session.jsonl")).unwrap();
                }
                real_root.clone()
            };

            let local_file = if mode == "root" {
                claude_dir.join("session.jsonl")
            } else {
                claude_dir.join("project/session.jsonl")
            };
            let conflict = Conflict {
                session_id: "session".to_string(),
                local_file,
                remote_file: temp.path().join("remote/session.jsonl"),
                local_timestamp: None,
                remote_timestamp: None,
                local_message_count: 0,
                remote_message_count: 0,
                local_hash: "local".to_string(),
                remote_hash: "remote".to_string(),
                resolution: ConflictResolution::SmartMerge {
                    merged_entries: Vec::new(),
                    stats: crate::merge::MergeStats::default(),
                },
            };
            let mut result = ResolutionResult::new();
            result.smart_merge.push(conflict);

            assert!(apply_resolutions(&result, &[], &claude_dir, temp.path()).is_err());
            assert_eq!(fs::read(&outside_file).unwrap(), b"outside-marker");
        }
    }

    #[test]
    fn test_display_resolution_action() {
        let action = ResolutionAction::KeepLocal;
        assert_eq!(action.to_string(), "Keep Local Version (discard remote)");

        let action = ResolutionAction::KeepRemote;
        assert_eq!(
            action.to_string(),
            "Save Remote as Separate Copy (active local stays unchanged)"
        );

        let action = ResolutionAction::KeepBoth;
        assert_eq!(
            action.to_string(),
            "Keep Both (save remote with conflict suffix)"
        );
    }
    fn test_session(path: &Path, uuid: &str) -> ConversationSession {
        ConversationSession {
            session_id: "session".to_string(),
            file_path: path.to_string_lossy().to_string(),
            entries: vec![crate::parser::ConversationEntry {
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
            }],
        }
    }

    fn test_conflict(local_file: PathBuf, remote_file: PathBuf) -> Conflict {
        Conflict {
            session_id: "session".to_string(),
            local_file,
            remote_file,
            local_timestamp: None,
            remote_timestamp: None,
            local_message_count: 1,
            remote_message_count: 1,
            local_hash: "local".to_string(),
            remote_hash: "remote".to_string(),
            resolution: ConflictResolution::KeepRemote,
        }
    }

    #[test]
    fn compatibility_apply_resolutions_errors_when_baseline_cannot_be_read() {
        let root = tempfile::tempdir().unwrap();
        let local_file = root.path().join("project/missing.jsonl");
        let remote_file = root.path().join("remote/session.jsonl");
        let mut result = ResolutionResult::new();
        result
            .keep_remote
            .push(test_conflict(local_file, remote_file.clone()));
        let remote = test_session(&remote_file, "remote");

        let error = apply_resolutions(&result, &[remote], root.path(), root.path()).unwrap_err();

        assert!(error.to_string().contains("baseline"));
    }

    #[test]
    fn compatibility_apply_resolutions_errors_when_keep_remote_source_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let local_file = root.path().join("project/session.jsonl");
        std::fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        std::fs::write(
            &local_file,
            b"{\"type\":\"user\",\"uuid\":\"local\",\"sessionId\":\"session\"}\n",
        )
        .unwrap();
        let mut result = ResolutionResult::new();
        result.keep_remote.push(test_conflict(
            local_file,
            root.path().join("remote/session.jsonl"),
        ));

        let error = apply_resolutions(&result, &[], root.path(), root.path()).unwrap_err();

        assert!(error.to_string().contains("remote session"));
    }

    #[test]
    fn compatibility_apply_resolutions_errors_when_guarded_write_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        let local_file = root.path().join("project/session.jsonl");
        std::fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        std::fs::write(
            &local_file,
            b"{\"type\":\"user\",\"uuid\":\"other\",\"sessionId\":\"other-session\"}\n",
        )
        .unwrap();
        let remote_file = root.path().join("remote/session.jsonl");
        let remote = test_session(&remote_file, "remote");
        let mut result = ResolutionResult::new();
        result
            .keep_remote
            .push(test_conflict(local_file, remote_file));

        let error = apply_resolutions(&result, &[remote], root.path(), root.path()).unwrap_err();

        assert!(error.to_string().contains("not applied"));
    }

    #[test]
    fn guarded_keep_remote_downgrades_to_keep_both_without_touching_changed_local() {
        let root = tempfile::tempdir().unwrap();
        let local_file = root.path().join("project/session.jsonl");
        std::fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        let original = b"{\"type\":\"user\",\"uuid\":\"local\",\"sessionId\":\"session\"}\n";
        std::fs::write(&local_file, original).unwrap();
        let conflict = test_conflict(local_file.clone(), root.path().join("remote/session.jsonl"));
        let mut result = ResolutionResult::new();
        result.keep_remote.push(conflict);
        let remote = test_session(&root.path().join("remote/session.jsonl"), "remote");
        let baselines = std::collections::HashMap::from([(
            "session".to_string(),
            crate::sync::session_write::SessionBaseline::from_snapshot(
                &local_file,
                "session",
                original.to_vec(),
            )
            .unwrap(),
        )]);
        std::fs::write(&local_file, b"changed while waiting\n").unwrap();

        let applied =
            apply_resolutions_guarded(&result, &[remote], root.path(), root.path(), &baselines)
                .unwrap();

        assert!(matches!(
            applied.outcomes[0].1,
            GuardedApplyOutcome::CreatedConflictCopy { .. }
        ));
        assert_eq!(
            std::fs::read(&local_file).unwrap(),
            b"changed while waiting\n"
        );
        assert!(applied.renames[0].1.is_file());
    }

    #[test]
    fn guarded_smart_merge_falls_back_to_keep_both_when_local_is_not_append_only() {
        let root = tempfile::tempdir().unwrap();
        let local_file = root.path().join("project/session.jsonl");
        std::fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        let original = b"{\"type\":\"user\",\"uuid\":\"local\",\"sessionId\":\"session\"}\n";
        std::fs::write(&local_file, original).unwrap();
        let mut conflict =
            test_conflict(local_file.clone(), root.path().join("remote/session.jsonl"));
        conflict.resolution = ConflictResolution::SmartMerge {
            merged_entries: test_session(&local_file, "merged").entries,
            stats: crate::merge::MergeStats::default(),
        };
        let mut result = ResolutionResult::new();
        result.smart_merge.push(conflict);
        let baselines = std::collections::HashMap::from([(
            "session".to_string(),
            crate::sync::session_write::SessionBaseline::from_snapshot(
                &local_file,
                "session",
                original.to_vec(),
            )
            .unwrap(),
        )]);
        std::fs::write(&local_file, b"changed while waiting\n").unwrap();

        let remote = test_session(&root.path().join("remote/session.jsonl"), "remote");
        let applied =
            apply_resolutions_guarded(&result, &[remote], root.path(), root.path(), &baselines)
                .unwrap();

        assert!(matches!(
            applied.outcomes[0].1,
            GuardedApplyOutcome::CreatedConflictCopy { .. }
        ));
        assert_eq!(
            std::fs::read(&local_file).unwrap(),
            b"changed while waiting\n"
        );
    }

    #[test]
    fn keep_both_allocates_unique_no_clobber_copies() {
        let root = tempfile::tempdir().unwrap();
        let local_file = root.path().join("project/session.jsonl");
        std::fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        std::fs::write(&local_file, b"local\n").unwrap();
        let conflict = test_conflict(local_file.clone(), root.path().join("remote/session.jsonl"));
        let mut result = ResolutionResult::new();
        result.keep_both.push(conflict);
        let remote = test_session(&root.path().join("remote/session.jsonl"), "remote");

        let first = apply_resolutions_guarded(
            &result,
            std::slice::from_ref(&remote),
            root.path(),
            root.path(),
            &std::collections::HashMap::new(),
        )
        .unwrap();
        let second = apply_resolutions_guarded(
            &result,
            &[remote],
            root.path(),
            root.path(),
            &std::collections::HashMap::new(),
        )
        .unwrap();

        assert_ne!(first.renames[0].1, second.renames[0].1);
        assert!(first.renames[0].1.exists());
        assert!(second.renames[0].1.exists());
        assert_eq!(std::fs::read(&local_file).unwrap(), b"local\n");
    }
    #[test]
    fn guarded_keep_remote_downgrades_without_using_unsafe_snapshot_identity() {
        let root = tempfile::tempdir().unwrap();
        let local_file = root.path().join("project/session.jsonl");
        std::fs::create_dir_all(local_file.parent().unwrap()).unwrap();
        let local = b"{\"type\":\"user\",\"uuid\":\"local\",\"sessionId\":\"session\"}\n";
        std::fs::write(&local_file, local).unwrap();
        let conflict = test_conflict(local_file.clone(), root.path().join("remote/session.jsonl"));
        let mut result = ResolutionResult::new();
        result.keep_remote.push(conflict);
        let remote = test_session(&root.path().join("remote/session.jsonl"), "remote");
        let wrong_identity =
            b"{\"type\":\"user\",\"uuid\":\"other\",\"sessionId\":\"other-session\"}\n";
        let baselines = std::collections::HashMap::from([(
            "session".to_string(),
            crate::sync::session_write::SessionBaseline::from_snapshot(
                &local_file,
                "session",
                wrong_identity.to_vec(),
            )
            .unwrap(),
        )]);

        let applied =
            apply_resolutions_guarded(&result, &[remote], root.path(), root.path(), &baselines)
                .unwrap();

        assert!(matches!(
            applied.outcomes[0].1,
            GuardedApplyOutcome::CreatedConflictCopy { .. }
        ));
        assert_eq!(std::fs::read(&local_file).unwrap(), local);
        assert!(applied.renames[0].1.is_file());
    }
    #[test]
    fn reported_resolution_reflects_actual_keep_both_and_skips() {
        let root = tempfile::tempdir().unwrap();
        let local = root.path().join("session.jsonl");
        let remote = root.path().join("remote.jsonl");
        let mut conflicts = vec![test_conflict(local.clone(), remote.clone())];
        let copy = root.path().join("session-conflict.jsonl");
        let mut result = ResolutionResult::new();
        result.keep_both.push(conflicts[0].clone());
        let applied = GuardedApplyResult {
            renames: vec![(remote, copy.clone())],
            outcomes: vec![(
                "session".to_string(),
                GuardedApplyOutcome::CreatedConflictCopy {
                    path: copy.clone(),
                    durability: crate::sync::session_write::CommitDurability::Synced,
                },
            )],
        };
        update_reported_resolutions(&mut conflicts, &result, &applied);
        assert!(matches!(
            &conflicts[0].resolution,
            ConflictResolution::KeepBoth { renamed_remote_file } if renamed_remote_file == &copy
        ));

        let skipped = GuardedApplyResult {
            renames: vec![],
            outcomes: vec![("session".to_string(), GuardedApplyOutcome::SkippedChanged)],
        };
        update_reported_resolutions(&mut conflicts, &result, &skipped);
        assert!(matches!(
            conflicts[0].resolution,
            ConflictResolution::Pending
        ));
    }
    #[test]
    fn guarded_write_error_falls_back_to_keep_both_remote_copy() {
        let root = tempfile::tempdir().unwrap();
        let local_file = root.path().join("project/session.jsonl");
        std::fs::create_dir_all(&local_file).unwrap();
        let remote_file = root.path().join("remote/session.jsonl");
        let remote = test_session(&remote_file, "remote");
        let baseline_bytes = b"{\"type\":\"user\",\"uuid\":\"local\",\"sessionId\":\"session\"}\n";
        let baseline = crate::sync::session_write::SessionBaseline::from_snapshot(
            &local_file,
            "session",
            baseline_bytes.to_vec(),
        )
        .unwrap();
        let mut result = ResolutionResult::new();
        result
            .keep_remote
            .push(test_conflict(local_file.clone(), remote_file));
        let baselines = std::collections::HashMap::from([("session".to_string(), baseline)]);

        let applied =
            apply_resolutions_guarded(&result, &[remote], root.path(), root.path(), &baselines)
                .unwrap();

        assert!(local_file.is_dir());
        assert_eq!(applied.renames.len(), 1);
        assert!(applied.renames[0].1.is_file());
        assert!(matches!(
            applied.outcomes[0].1,
            GuardedApplyOutcome::CreatedConflictCopy { .. }
        ));
    }
}
