use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

use crate::atomic_file::sync_parent_directory;
use crate::parser::{ConversationEntry, ConversationSession};
use crate::path_security::prepare_regular_file_destination;

#[derive(Debug, Clone)]
pub(crate) struct SessionBaseline {
    bytes: Vec<u8>,
    digest: blake3::Hash,
    canonical_path: PathBuf,
    session_id: String,
}

impl SessionBaseline {
    pub(crate) fn from_snapshot(path: &Path, session_id: &str, bytes: Vec<u8>) -> Result<Self> {
        let canonical_path = std::fs::canonicalize(path).with_context(|| {
            format!("failed to bind snapshot baseline path: {}", path.display())
        })?;
        let digest = blake3::hash(&bytes);
        Ok(Self {
            bytes,
            digest,
            canonical_path,
            session_id: session_id.to_string(),
        })
    }

    pub(crate) fn parse_session(
        &self,
        path: &Path,
        expected_session_id: &str,
    ) -> Result<ConversationSession> {
        let canonical_path = std::fs::canonicalize(path).with_context(|| {
            format!(
                "failed to revalidate snapshot baseline path: {}",
                path.display()
            )
        })?;
        if canonical_path != self.canonical_path || expected_session_id != self.session_id {
            bail!(
                "snapshot baseline binding changed: expected {} at {}, got {} at {}",
                self.session_id,
                self.canonical_path.display(),
                expected_session_id,
                canonical_path.display()
            );
        }
        let outcome = ConversationSession::from_bytes_with_report(&self.bytes, path)?;
        if outcome.malformed_lines > 0 {
            bail!(
                "snapshot baseline contains {} malformed non-empty JSONL line(s): {}",
                outcome.malformed_lines,
                path.display()
            );
        }
        if outcome.value.session_id != expected_session_id {
            bail!(
                "snapshot baseline session identity changed: expected {}, found {} at {}",
                expected_session_id,
                outcome.value.session_id,
                path.display()
            );
        }
        Ok(outcome.value)
    }

    fn matches_prefix(&self, bytes: &[u8]) -> bool {
        bytes.len() >= self.bytes.len()
            && blake3::hash(&bytes[..self.bytes.len()]) == self.digest
            && bytes.starts_with(&self.bytes)
    }

    #[cfg(test)]
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitDurability {
    #[cfg_attr(windows, allow(dead_code))]
    Synced,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppendSessionWriteOutcome {
    Written {
        entries_added: usize,
        durability: CommitDurability,
    },
    Noop,
    SkippedChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NewSessionWriteOutcome {
    Written {
        path: PathBuf,
        durability: CommitDurability,
    },
    SkippedExisting(PathBuf),
}

fn serialized_session(session: &ConversationSession) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for entry in &session.entries {
        serde_json::to_writer(&mut bytes, entry)
            .context("Failed to serialize conversation entry")?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn serialized_entry(entry: &ConversationEntry) -> Result<Vec<u8>> {
    let mut line = serde_json::to_vec(entry).context("Failed to serialize conversation entry")?;
    line.push(b'\n');
    Ok(line)
}

fn entry_key(entry: &ConversationEntry) -> Result<Vec<u8>> {
    serde_json::to_vec(entry).context("Failed to compare conversation entry")
}

fn append_only_candidates(
    baseline: &ConversationSession,
    merged_entries: &[ConversationEntry],
) -> Result<Vec<ConversationEntry>> {
    let mut matched = vec![false; merged_entries.len()];
    for local_entry in &baseline.entries {
        let local_key = entry_key(local_entry)?;
        let match_index = if let Some(uuid) = local_entry.uuid.as_deref() {
            let same_uuid = merged_entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.uuid.as_deref() == Some(uuid))
                .collect::<Vec<_>>();
            if same_uuid.len() != 1 || entry_key(same_uuid[0].1)? != local_key {
                bail!("smart merge requires rewriting existing UUID entry {uuid}");
            }
            same_uuid[0].0
        } else {
            let mut found = None;
            for (index, entry) in merged_entries.iter().enumerate() {
                if !matched[index] && entry_key(entry)? == local_key {
                    found = Some(index);
                    break;
                }
            }
            found.context("smart merge removes or rewrites an existing non-UUID entry")?
        };
        if matched[match_index] {
            bail!("smart merge duplicates an existing local entry identity");
        }
        matched[match_index] = true;
    }
    Ok(merged_entries
        .iter()
        .zip(matched)
        .filter_map(|(entry, matched)| (!matched).then_some(entry.clone()))
        .collect())
}

fn current_allows_candidates(
    baseline: &ConversationSession,
    current: &ConversationSession,
    candidates: &[ConversationEntry],
) -> Result<Option<Vec<ConversationEntry>>> {
    if current.session_id != baseline.session_id
        || append_only_candidates(baseline, &current.entries).is_err()
    {
        return Ok(None);
    }
    let mut append = Vec::new();
    for candidate in candidates {
        if let Some(uuid) = candidate.uuid.as_deref() {
            let existing = current
                .entries
                .iter()
                .find(|entry| entry.uuid.as_deref() == Some(uuid));
            match existing {
                Some(entry) if entry_key(entry)? == entry_key(candidate)? => continue,
                Some(_) => return Ok(None),
                None => append.push(candidate.clone()),
            }
        } else {
            let key = entry_key(candidate)?;
            let mut exists = false;
            for entry in &current.entries {
                if entry.uuid.is_none() && entry_key(entry)? == key {
                    exists = true;
                    break;
                }
            }
            if !exists {
                append.push(candidate.clone());
            }
        }
    }
    Ok(Some(append))
}

fn durability_after_commit(parent: &Path) -> CommitDurability {
    #[cfg(windows)]
    {
        let _ = sync_parent_directory(parent);
        log::warn!(
            "Session entries were appended on Windows, where directory fsync is unavailable; file sync completed but crash durability is weaker"
        );
        CommitDurability::Warning
    }
    #[cfg(not(windows))]
    {
        match sync_parent_directory(parent) {
            Ok(()) => CommitDurability::Synced,
            Err(error) => {
                log::warn!(
                    "Session entries were appended, but parent directory durability could not be confirmed for {}: {}",
                    parent.display(),
                    error
                );
                CommitDurability::Warning
            }
        }
    }
}

pub(crate) fn append_merged_entries_guarded(
    root: &Path,
    relative: &Path,
    baseline: &SessionBaseline,
    merged_entries: &[ConversationEntry],
) -> Result<AppendSessionWriteOutcome> {
    append_merged_entries_guarded_with_hook(root, relative, baseline, merged_entries, |_| Ok(()))
}

pub(crate) fn append_merged_entries_guarded_with_hook<F>(
    root: &Path,
    relative: &Path,
    baseline: &SessionBaseline,
    merged_entries: &[ConversationEntry],
    after_validation: F,
) -> Result<AppendSessionWriteOutcome>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let destination = prepare_regular_file_destination(root, relative)?;
    let baseline_session = baseline.parse_session(&destination, &baseline.session_id)?;
    let candidates = append_only_candidates(&baseline_session, merged_entries)?;
    if candidates.is_empty() {
        return Ok(AppendSessionWriteOutcome::Noop);
    }

    let current_bytes = std::fs::read(&destination).with_context(|| {
        format!(
            "failed to read current session before append: {}",
            destination.display()
        )
    })?;
    if !baseline.matches_prefix(&current_bytes)
        || (!current_bytes.is_empty() && !current_bytes.ends_with(b"\n"))
    {
        return Ok(AppendSessionWriteOutcome::SkippedChanged);
    }
    let current_outcome =
        ConversationSession::from_bytes_with_report(&current_bytes, &destination)?;
    if current_outcome.malformed_lines > 0 {
        return Ok(AppendSessionWriteOutcome::SkippedChanged);
    }
    let Some(candidates) =
        current_allows_candidates(&baseline_session, &current_outcome.value, &candidates)?
    else {
        return Ok(AppendSessionWriteOutcome::SkippedChanged);
    };
    if candidates.is_empty() {
        return Ok(AppendSessionWriteOutcome::Noop);
    }

    let destination = prepare_regular_file_destination(root, relative)?;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&destination)
        .with_context(|| {
            format!(
                "failed to open session for append: {}",
                destination.display()
            )
        })?;
    after_validation(&destination)?;
    for entry in &candidates {
        let line = serialized_entry(entry)?;
        let written = file.write(&line)?;
        if written != line.len() {
            bail!(
                "short atomic JSONL append for {}: wrote {} of {} bytes",
                destination.display(),
                written,
                line.len()
            );
        }
    }
    file.flush()?;
    file.sync_all()?;
    let parent = destination
        .parent()
        .context("session destination has no parent")?;
    Ok(AppendSessionWriteOutcome::Written {
        entries_added: candidates.len(),
        durability: durability_after_commit(parent),
    })
}

fn stage_session(parent: &Path, session: &ConversationSession) -> Result<NamedTempFile> {
    let bytes = serialized_session(session)?;
    let mut staged = NamedTempFile::new_in(parent)
        .with_context(|| format!("Failed to stage session in {}", parent.display()))?;
    staged.write_all(&bytes)?;
    staged.flush()?;
    staged.as_file().sync_all()?;
    Ok(staged)
}

pub(crate) fn write_new_session_noclobber(
    session: &ConversationSession,
    root: &Path,
    relative: &Path,
) -> Result<NewSessionWriteOutcome> {
    write_new_session_noclobber_with_hook(session, root, relative, |_| Ok(()))
}

pub(crate) fn write_new_session_noclobber_with_hook<F>(
    session: &ConversationSession,
    root: &Path,
    relative: &Path,
    before_commit: F,
) -> Result<NewSessionWriteOutcome>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let destination = prepare_regular_file_destination(root, relative)?;
    let parent = destination
        .parent()
        .context("session destination has no parent")?;
    let staged = stage_session(parent, session)?;
    let destination = prepare_regular_file_destination(root, relative)?;
    before_commit(&destination)?;
    match staged.persist_noclobber(&destination) {
        Ok(_) => Ok(NewSessionWriteOutcome::Written {
            path: destination,
            durability: durability_after_commit(parent),
        }),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(NewSessionWriteOutcome::SkippedExisting(destination))
        }
        Err(error) => Err(error.error)
            .with_context(|| format!("Failed to persist session {}", destination.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn session(path: &Path, uuid: &str) -> ConversationSession {
        ConversationSession {
            session_id: "session".to_string(),
            file_path: path.to_string_lossy().to_string(),
            entries: vec![ConversationEntry {
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
                extra: Value::Null,
            }],
        }
    }

    #[test]
    fn append_only_merge_keeps_concurrent_local_and_remote_entries_in_canonical_file() {
        let root = tempfile::tempdir().unwrap();
        let relative = Path::new("project/session.jsonl");
        let target = root.path().join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let original = b"{\"type\":\"user\",\"uuid\":\"local\",\"sessionId\":\"session\"}\n";
        std::fs::write(&target, original).unwrap();
        let baseline =
            SessionBaseline::from_snapshot(&target, "session", original.to_vec()).unwrap();
        let local = session(&target, "local");
        let mut merged_entries = local.entries.clone();
        merged_entries.extend(session(&target, "remote").entries);
        #[cfg(unix)]
        let original_inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&target).unwrap().ino()
        };
        let mut claude_writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&target)
            .unwrap();
        let concurrent = b"{\"type\":\"user\",\"uuid\":\"concurrent\",\"sessionId\":\"session\"}\n";

        let outcome = append_merged_entries_guarded_with_hook(
            root.path(),
            relative,
            &baseline,
            &merged_entries,
            |_| {
                claude_writer.write_all(concurrent)?;
                claude_writer.sync_all()?;
                Ok(())
            },
        )
        .unwrap();

        assert!(matches!(
            outcome,
            AppendSessionWriteOutcome::Written {
                entries_added: 1,
                ..
            }
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(&target).unwrap().ino(), original_inode);
        }
        let contents = std::fs::read_to_string(&target).unwrap();
        assert!(contents.contains("\"uuid\":\"local\""));
        assert!(contents.contains("\"uuid\":\"concurrent\""));
        assert!(contents.contains("\"uuid\":\"remote\""));
    }

    #[test]
    fn append_only_merge_rejects_edit_of_existing_entry_without_touching_canonical() {
        let root = tempfile::tempdir().unwrap();
        let relative = Path::new("project/session.jsonl");
        let target = root.path().join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let original = b"{\"type\":\"user\",\"uuid\":\"local\",\"sessionId\":\"session\"}\n";
        std::fs::write(&target, original).unwrap();
        let baseline =
            SessionBaseline::from_snapshot(&target, "session", original.to_vec()).unwrap();
        let mut edited = session(&target, "local").entries;
        edited[0].message = Some(serde_json::json!({"text": "edited"}));

        let error =
            append_merged_entries_guarded(root.path(), relative, &baseline, &edited).unwrap_err();

        assert!(error.to_string().contains("rewriting"));
        assert_eq!(std::fs::read(&target).unwrap(), original);
    }

    #[test]
    fn append_only_merge_skips_when_snapshot_is_no_longer_a_raw_prefix() {
        let root = tempfile::tempdir().unwrap();
        let relative = Path::new("project/session.jsonl");
        let target = root.path().join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let original = b"{\"type\":\"user\",\"uuid\":\"local\",\"sessionId\":\"session\"}\n";
        std::fs::write(&target, original).unwrap();
        let baseline =
            SessionBaseline::from_snapshot(&target, "session", original.to_vec()).unwrap();
        let mut merged = session(&target, "local").entries;
        merged.extend(session(&target, "remote").entries);
        std::fs::write(
            &target,
            b"{\"type\":\"user\",\"uuid\":\"changed\",\"sessionId\":\"session\"}\n",
        )
        .unwrap();

        assert_eq!(
            append_merged_entries_guarded(root.path(), relative, &baseline, &merged).unwrap(),
            AppendSessionWriteOutcome::SkippedChanged
        );
        assert!(std::fs::read_to_string(&target)
            .unwrap()
            .contains("\"changed\""));
    }

    #[test]
    fn new_session_writer_never_clobbers_existing_destination() {
        let root = tempfile::tempdir().unwrap();
        let relative = Path::new("project/session.jsonl");
        let target = root.path().join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"existing\n").unwrap();

        let outcome =
            write_new_session_noclobber(&session(&target, "remote"), root.path(), relative)
                .unwrap();

        assert_eq!(
            outcome,
            NewSessionWriteOutcome::SkippedExisting(target.clone())
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"existing\n");
    }

    #[test]
    fn malformed_snapshot_baseline_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("session.jsonl");
        std::fs::write(&target, b"not-json\n").unwrap();
        let baseline =
            SessionBaseline::from_snapshot(&target, "session", b"not-json\n".to_vec()).unwrap();
        assert_eq!(baseline.bytes(), b"not-json\n");
        let error = baseline.parse_session(&target, "session").unwrap_err();
        assert!(error.to_string().contains("malformed"));
    }

    #[test]
    fn committed_append_reports_parent_sync_warning_without_becoming_write_failed() {
        let root = tempfile::tempdir().unwrap();
        let relative = Path::new("project/session.jsonl");
        let target = root.path().join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let original = b"{\"type\":\"user\",\"uuid\":\"local\",\"sessionId\":\"session\"}\n";
        std::fs::write(&target, original).unwrap();
        let baseline =
            SessionBaseline::from_snapshot(&target, "session", original.to_vec()).unwrap();
        let mut merged = session(&target, "local").entries;
        merged.extend(session(&target, "remote").entries);
        crate::atomic_file::test_force_parent_sync_failure(true);
        let outcome = append_merged_entries_guarded(root.path(), relative, &baseline, &merged);
        crate::atomic_file::test_force_parent_sync_failure(false);

        assert!(matches!(
            outcome.unwrap(),
            AppendSessionWriteOutcome::Written {
                durability: CommitDurability::Warning,
                ..
            }
        ));
        assert!(std::fs::read_to_string(&target)
            .unwrap()
            .contains("\"uuid\":\"remote\""));
    }
}
