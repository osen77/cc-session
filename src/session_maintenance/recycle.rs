#[cfg(debug_assertions)]
use super::state::wait_for_maintenance_test_gate;
use super::state::{
    identity_key, LifecycleState, MaintenanceEntry, PendingOperation, PendingOperationKind,
    StateStore,
};
use crate::atomic_file::sync_parent_directory;
use crate::path_security::{
    prepare_regular_file_destination, safe_join_within_root, validate_directory_root,
    validate_regular_candidate,
};
use crate::session_cache::fingerprint_file;
use crate::session_model::SessionSource;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// Trusted roots used by the local session maintenance transaction layer.
#[derive(Debug, Clone)]
pub(crate) struct MaintenanceRoots {
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub omp: PathBuf,
    pub recycle: PathBuf,
}

impl MaintenanceRoots {
    /// Return the source root for one session source.
    pub(crate) fn source_root(&self, source: SessionSource) -> &Path {
        match source {
            SessionSource::Claude => &self.claude,
            SessionSource::Codex => &self.codex,
            SessionSource::Omp => &self.omp,
        }
    }
}

/// Return the deterministic path used for a recycled session.
pub(crate) fn recycle_relative_path(entry: &MaintenanceEntry) -> PathBuf {
    let session_component = session_component(&entry.identity.session_id);
    let fingerprint_component = safe_component(&entry.fingerprint);
    PathBuf::from(entry.identity.source.as_str())
        .join(session_component)
        .join(format!("{fingerprint_component}.jsonl"))
}

fn verify_recycle_file(roots: &MaintenanceRoots, entry: &MaintenanceEntry) -> Result<()> {
    ensure_recycle_root(&roots.recycle)?;
    let relative = recycle_relative_path(entry);
    let path = safe_join_within_root(&roots.recycle, &relative)?;
    verify_regular_fingerprint_within_root(&roots.recycle, &path, &entry.fingerprint)
}

/// Move a verified source session into the recycle store transactionally.
pub(crate) fn recycle_session(
    store: &StateStore,
    roots: &MaintenanceRoots,
    requested: &MaintenanceEntry,
    now: DateTime<Utc>,
) -> Result<()> {
    store.transaction(|locked| {
        if locked.state.pending.is_some() {
            anyhow::bail!("cannot recycle while another maintenance operation is pending")
        }
        let key = identity_key(&requested.identity);
        let entry = locked
            .state
            .entries
            .get(&key)
            .cloned()
            .with_context(|| format!("maintenance entry not found: {key}"))?;
        if entry.fingerprint != requested.fingerprint
            || entry.original_relative_path != requested.original_relative_path
        {
            anyhow::bail!("stale maintenance entry for {key}");
        }
        if entry.lifecycle == LifecycleState::Recycled {
            verify_recycle_file(roots, &entry)?;
            let final_relative = recycle_relative_path(&entry);
            let staging_path =
                safe_join_within_root(&roots.recycle, &staging_relative_path(&final_relative))?;
            if inspect_file(&roots.recycle, &staging_path, &entry.fingerprint)?.is_some() {
                remove_verified(&roots.recycle, &staging_path, &entry.fingerprint)?;
            }

            let source_root = roots.source_root(entry.identity.source);
            validate_directory_root(source_root)?;
            let source = safe_join_within_root(source_root, &entry.original_relative_path)?;
            if inspect_file(source_root, &source, &entry.fingerprint)?.is_some() {
                remove_verified(source_root, &source, &entry.fingerprint)?;
            }
            return Ok(());
        }
        if entry.lifecycle != LifecycleState::Hidden {
            anyhow::bail!("session {key} is not hidden")
        }

        let source_root = roots.source_root(entry.identity.source);
        validate_directory_root(source_root)?;
        ensure_recycle_root(&roots.recycle)?;
        let source = safe_join_within_root(source_root, &entry.original_relative_path)?;
        validate_regular_candidate(source_root, &source)?;
        verify_fingerprint(&source, &entry.fingerprint)?;

        let final_relative = recycle_relative_path(&entry);
        let staging_relative = staging_relative_path(&final_relative);
        let final_path = prepare_regular_file_destination(&roots.recycle, &final_relative)?;
        let staging_path = prepare_regular_file_destination(&roots.recycle, &staging_relative)?;
        if path_is_regular(&final_path)? {
            verify_fingerprint(&final_path, &entry.fingerprint)?;
        }
        if path_is_regular(&staging_path)? {
            verify_fingerprint(&staging_path, &entry.fingerprint)?;
        }

        let pending = PendingOperation {
            identity: entry.identity.clone(),
            operation: PendingOperationKind::Recycle,
            source_relative_path: entry.original_relative_path.clone(),
            staging_relative_path: staging_relative,
            recycle_relative_path: final_relative,
            expected_fingerprint: entry.fingerprint.clone(),
        };
        locked.state.pending = Some(pending);
        // This is the durability boundary: no operation below may touch source
        // until the journal says how to recover it.
        locked.persist()?;
        #[cfg(debug_assertions)]
        wait_for_maintenance_test_gate(
            "CCS_TEST_MAINTENANCE_AFTER_PENDING_READY",
            "CCS_TEST_MAINTENANCE_AFTER_PENDING_RELEASE",
            "maintenance pending journal",
        )?;

        if path_is_regular(&final_path)? {
            verify_regular_fingerprint_within_root(
                &roots.recycle,
                &final_path,
                &entry.fingerprint,
            )?;
            if path_is_regular(&staging_path)? {
                remove_verified(&roots.recycle, &staging_path, &entry.fingerprint)?;
            }
            remove_verified(source_root, &source, &entry.fingerprint)?;
        } else {
            move_source_to_recycle(
                source_root,
                &source,
                &roots.recycle,
                &staging_path,
                &final_path,
                &entry.fingerprint,
            )?;
        }

        let current = locked
            .state
            .entries
            .get_mut(&key)
            .context("maintenance entry disappeared during recycle")?;
        current.lifecycle = LifecycleState::Recycled;
        current.recycled_at = Some(now);
        locked.state.pending = None;
        locked.persist()
    })
}

/// Restore a recycled session without overwriting different local content.
pub(crate) fn restore_session(
    store: &StateStore,
    roots: &MaintenanceRoots,
    requested: &MaintenanceEntry,
    _now: DateTime<Utc>,
) -> Result<()> {
    store.transaction(|locked| {
        if locked.state.pending.is_some() {
            anyhow::bail!("cannot restore while another maintenance operation is pending")
        }
        let key = identity_key(&requested.identity);
        let entry = locked
            .state
            .entries
            .get(&key)
            .cloned()
            .with_context(|| format!("maintenance entry not found: {key}"))?;
        if entry.fingerprint != requested.fingerprint
            || entry.original_relative_path != requested.original_relative_path
        {
            anyhow::bail!("stale maintenance entry for {key}");
        }
        if entry.lifecycle == LifecycleState::Visible {
            let source_root = roots.source_root(entry.identity.source);
            validate_directory_root(source_root)?;
            let destination = safe_join_within_root(source_root, &entry.original_relative_path)?;
            verify_regular_fingerprint_within_root(source_root, &destination, &entry.fingerprint)?;
            ensure_recycle_root(&roots.recycle)?;
            let final_relative = recycle_relative_path(&entry);
            let final_path = safe_join_within_root(&roots.recycle, &final_relative)?;
            let staging_path =
                safe_join_within_root(&roots.recycle, &staging_relative_path(&final_relative))?;
            let final_present =
                inspect_file(&roots.recycle, &final_path, &entry.fingerprint)?.is_some();
            let staging_present =
                inspect_file(&roots.recycle, &staging_path, &entry.fingerprint)?.is_some();
            if final_present {
                remove_verified(&roots.recycle, &final_path, &entry.fingerprint)?;
            }
            if staging_present {
                remove_verified(&roots.recycle, &staging_path, &entry.fingerprint)?;
            }
            return Ok(());
        }
        if entry.lifecycle != LifecycleState::Recycled {
            anyhow::bail!("session {key} is not recycled")
        }

        let source_root = roots.source_root(entry.identity.source);
        validate_directory_root(source_root)?;
        ensure_recycle_root(&roots.recycle)?;
        let final_relative = recycle_relative_path(&entry);
        let final_path = safe_join_within_root(&roots.recycle, &final_relative)?;
        verify_regular_fingerprint_within_root(&roots.recycle, &final_path, &entry.fingerprint)?;
        let destination = safe_join_within_root(source_root, &entry.original_relative_path)?;
        if path_is_regular(&destination)? {
            verify_regular_fingerprint_within_root(source_root, &destination, &entry.fingerprint)
                .with_context(|| format!("restore conflict: destination differs for {key}"))?;
        } else {
            prepare_regular_file_destination(source_root, &entry.original_relative_path)?;
        }

        let staging_relative = staging_relative_path(&final_relative);
        locked.state.pending = Some(PendingOperation {
            identity: entry.identity.clone(),
            operation: PendingOperationKind::Restore,
            source_relative_path: entry.original_relative_path.clone(),
            staging_relative_path: staging_relative,
            recycle_relative_path: final_relative,
            expected_fingerprint: entry.fingerprint.clone(),
        });
        locked.persist()?;

        if !path_is_regular(&destination)? {
            copy_verified_file(
                &final_path,
                &roots.recycle,
                source_root,
                &entry.original_relative_path,
                &entry.fingerprint,
                maybe_create_restore_destination_target_for_test,
            )?;
        }
        verify_regular_fingerprint_within_root(source_root, &destination, &entry.fingerprint)?;
        maybe_replace_restore_destination_for_test(&destination)?;
        // The recycle copy is the only remaining durable source. Revalidate the
        // destination immediately before deleting it, including the preexisting
        // destination path and the freshly copied path.
        verify_regular_fingerprint_within_root(source_root, &destination, &entry.fingerprint)?;
        remove_verified(&roots.recycle, &final_path, &entry.fingerprint)?;

        let current = locked
            .state
            .entries
            .get_mut(&key)
            .context("maintenance entry disappeared during restore")?;
        current.lifecycle = LifecycleState::Visible;
        current.recycled_at = None;
        current.purged_at = None;
        locked.state.pending = None;
        locked.persist()
    })
}

/// Permanently remove a recycled session while retaining its maintenance audit entry.
pub(crate) fn purge_session(
    store: &StateStore,
    roots: &MaintenanceRoots,
    requested: &MaintenanceEntry,
    now: DateTime<Utc>,
) -> Result<()> {
    store.transaction(|locked| {
        if locked.state.pending.is_some() {
            anyhow::bail!("cannot purge while another maintenance operation is pending")
        }
        let key = identity_key(&requested.identity);
        let entry = locked
            .state
            .entries
            .get(&key)
            .cloned()
            .with_context(|| format!("maintenance entry not found: {key}"))?;
        if entry.fingerprint != requested.fingerprint {
            anyhow::bail!("stale maintenance entry for {key}")
        }
        if entry.lifecycle == LifecycleState::PurgedLocal {
            ensure_recycle_root(&roots.recycle)?;
            let final_relative = recycle_relative_path(&entry);
            let final_path = safe_join_within_root(&roots.recycle, &final_relative)?;
            let staging_path =
                safe_join_within_root(&roots.recycle, &staging_relative_path(&final_relative))?;
            let final_present =
                inspect_file(&roots.recycle, &final_path, &entry.fingerprint)?.is_some();
            let staging_present =
                inspect_file(&roots.recycle, &staging_path, &entry.fingerprint)?.is_some();
            if final_present {
                remove_verified(&roots.recycle, &final_path, &entry.fingerprint)?;
            }
            if staging_present {
                remove_verified(&roots.recycle, &staging_path, &entry.fingerprint)?;
            }
            return Ok(());
        }
        if entry.lifecycle != LifecycleState::Recycled {
            anyhow::bail!("session {key} is not recycled")
        }

        ensure_recycle_root(&roots.recycle)?;
        let final_relative = recycle_relative_path(&entry);
        let final_path = safe_join_within_root(&roots.recycle, &final_relative)?;
        verify_regular_fingerprint_within_root(&roots.recycle, &final_path, &entry.fingerprint)?;
        locked.state.pending = Some(PendingOperation {
            identity: entry.identity.clone(),
            operation: PendingOperationKind::Purge,
            source_relative_path: entry.original_relative_path.clone(),
            staging_relative_path: staging_relative_path(&final_relative),
            recycle_relative_path: final_relative,
            expected_fingerprint: entry.fingerprint.clone(),
        });
        locked.persist()?;
        remove_verified(&roots.recycle, &final_path, &entry.fingerprint)?;

        let current = locked
            .state
            .entries
            .get_mut(&key)
            .context("maintenance entry disappeared during purge")?;
        current.lifecycle = LifecycleState::PurgedLocal;
        current.purged_at = Some(now);
        if current.identity.source == SessionSource::Claude {
            current.project_name.clear();
            current.classifier_version = 0;
            current.score = 0;
            current.reason_codes.clear();
            current.hidden_since = None;
            current.recycled_at = None;
            current.keep = false;
            current.explicit_test = false;
        }
        locked.state.pending = None;
        locked.persist()
    })
}

/// Recover the one journaled maintenance operation after an interrupted process.
pub(crate) fn reconcile_pending(
    store: &StateStore,
    roots: &MaintenanceRoots,
    now: DateTime<Utc>,
) -> Result<()> {
    store.transaction(|locked| {
        let Some(pending) = locked.state.pending.clone() else {
            return Ok(());
        };
        let key = identity_key(&pending.identity);
        let entry = locked
            .state
            .entries
            .get(&key)
            .cloned()
            .with_context(|| format!("pending entry not found: {key}"))?;
        validate_pending_binding(&entry, &pending)?;
        match pending.operation {
            PendingOperationKind::Recycle => {
                reconcile_recycle(locked, roots, &entry, &pending, now)
            }
            PendingOperationKind::Restore => reconcile_restore(locked, roots, &entry, &pending),
            PendingOperationKind::Purge => reconcile_purge(locked, roots, &entry, &pending, now),
        }
    })
}

fn reconcile_recycle(
    locked: &mut super::state::LockedState<'_>,
    roots: &MaintenanceRoots,
    entry: &MaintenanceEntry,
    pending: &PendingOperation,
    now: DateTime<Utc>,
) -> Result<()> {
    let source_root = roots.source_root(entry.identity.source);
    validate_directory_root(source_root)?;
    ensure_recycle_root(&roots.recycle)?;
    let source = safe_join_within_root(source_root, &pending.source_relative_path)?;
    let staging = safe_join_within_root(&roots.recycle, &pending.staging_relative_path)?;
    let final_path = safe_join_within_root(&roots.recycle, &pending.recycle_relative_path)?;
    let mut source_state = inspect_file(source_root, &source, &pending.expected_fingerprint)?;
    let mut staging_state = inspect_file(&roots.recycle, &staging, &pending.expected_fingerprint)?;
    let mut final_state = inspect_file(&roots.recycle, &final_path, &pending.expected_fingerprint)?;

    if let (Some(source_fp), Some(final_fp)) = (&source_state, &final_state) {
        if source_fp != final_fp {
            anyhow::bail!("pending recycle has different source and final content")
        }
    }
    if let (Some(staging_fp), Some(final_fp)) = (&staging_state, &final_state) {
        if staging_fp != final_fp {
            anyhow::bail!("pending recycle has different staging and final content")
        }
    }

    if staging_state.is_some() && final_state.is_none() {
        let final_path =
            prepare_regular_file_destination(&roots.recycle, &pending.recycle_relative_path)?;
        promote_noclobber(
            &roots.recycle,
            &staging,
            &roots.recycle,
            &final_path,
            &pending.expected_fingerprint,
            maybe_create_staging_to_final_target_for_test,
        )?;
        maybe_replace_reconcile_final_for_test(&final_path)?;
        verify_regular_fingerprint_within_root(
            &roots.recycle,
            &final_path,
            &pending.expected_fingerprint,
        )?;
        staging_state = None;
        final_state = Some(pending.expected_fingerprint.clone());
    } else if staging_state.is_some() && final_state.is_some() {
        remove_verified(&roots.recycle, &staging, &pending.expected_fingerprint)?;
        staging_state = None;
    }

    if final_state.is_none() && source_state.is_some() {
        let staging_path =
            prepare_regular_file_destination(&roots.recycle, &pending.staging_relative_path)?;
        let final_path =
            prepare_regular_file_destination(&roots.recycle, &pending.recycle_relative_path)?;
        move_source_to_recycle(
            source_root,
            &source,
            &roots.recycle,
            &staging_path,
            &final_path,
            &pending.expected_fingerprint,
        )?;
        source_state = None;
        final_state = Some(pending.expected_fingerprint.clone());
    }

    if final_state.is_some() && source_state.is_some() {
        remove_verified(source_root, &source, &pending.expected_fingerprint)?;
        source_state = None;
    }
    if final_state.is_none() && staging_state.is_none() && source_state.is_none() {
        anyhow::bail!("pending recycle has no recoverable source or target")
    }
    if final_state.is_none() {
        anyhow::bail!("pending recycle did not produce a final file")
    }

    let current = locked
        .state
        .entries
        .get_mut(&identity_key(&entry.identity))
        .context("maintenance entry disappeared during reconcile")?;
    current.lifecycle = LifecycleState::Recycled;
    current.recycled_at = Some(now);
    locked.state.pending = None;
    locked.persist()
}

fn reconcile_restore(
    locked: &mut super::state::LockedState<'_>,
    roots: &MaintenanceRoots,
    entry: &MaintenanceEntry,
    pending: &PendingOperation,
) -> Result<()> {
    let source_root = roots.source_root(entry.identity.source);
    validate_directory_root(source_root)?;
    ensure_recycle_root(&roots.recycle)?;
    let destination = safe_join_within_root(source_root, &pending.source_relative_path)?;
    let staging = safe_join_within_root(&roots.recycle, &pending.staging_relative_path)?;
    let final_path = safe_join_within_root(&roots.recycle, &pending.recycle_relative_path)?;
    let source_state = inspect_file(source_root, &destination, &pending.expected_fingerprint)?;
    let mut staging_state = inspect_file(&roots.recycle, &staging, &pending.expected_fingerprint)?;
    let final_state = inspect_file(&roots.recycle, &final_path, &pending.expected_fingerprint)?;

    if let (Some(source_fp), Some(final_fp)) = (&source_state, &final_state) {
        if source_fp != final_fp {
            anyhow::bail!("pending restore has a different destination and recycle file")
        }
    }
    if let (Some(staging_fp), Some(final_fp)) = (&staging_state, &final_state) {
        if staging_fp != final_fp {
            anyhow::bail!("pending restore has a different staging and recycle file")
        }
    }
    if source_state.is_none() && final_state.is_none() && staging_state.is_none() {
        anyhow::bail!("pending restore has no recoverable source or target")
    }
    if source_state.is_some() && staging_state.is_some() && final_state.is_none() {
        maybe_replace_restore_staging_for_test(&staging)?;
        verify_regular_fingerprint_within_root(
            &roots.recycle,
            &staging,
            &pending.expected_fingerprint,
        )?;
        remove_verified(&roots.recycle, &staging, &pending.expected_fingerprint)?;
        staging_state = None;
    }
    if source_state.is_none() {
        if staging_state.is_some() && final_state.is_none() {
            promote_noclobber(
                &roots.recycle,
                &staging,
                &roots.recycle,
                &final_path,
                &pending.expected_fingerprint,
                noop_no_clobber_hook,
            )?;
        }
        copy_verified_file(
            &final_path,
            &roots.recycle,
            source_root,
            &pending.source_relative_path,
            &pending.expected_fingerprint,
            noop_no_clobber_hook,
        )?;
    }
    if path_is_regular(&final_path)? {
        maybe_replace_restore_destination_for_test(&destination)?;
        verify_regular_fingerprint_within_root(
            source_root,
            &destination,
            &pending.expected_fingerprint,
        )?;
        verify_regular_fingerprint_within_root(
            &roots.recycle,
            &final_path,
            &pending.expected_fingerprint,
        )?;
        remove_verified(&roots.recycle, &final_path, &pending.expected_fingerprint)?;
    }
    if path_is_regular(&staging)? {
        remove_verified(&roots.recycle, &staging, &pending.expected_fingerprint)?;
    }

    let current = locked
        .state
        .entries
        .get_mut(&identity_key(&entry.identity))
        .context("maintenance entry disappeared during restore reconcile")?;
    current.lifecycle = LifecycleState::Visible;
    current.recycled_at = None;
    current.purged_at = None;
    locked.state.pending = None;
    locked.persist()
}

fn reconcile_purge(
    locked: &mut super::state::LockedState<'_>,
    roots: &MaintenanceRoots,
    entry: &MaintenanceEntry,
    pending: &PendingOperation,
    now: DateTime<Utc>,
) -> Result<()> {
    ensure_recycle_root(&roots.recycle)?;
    let final_path = safe_join_within_root(&roots.recycle, &pending.recycle_relative_path)?;
    let staging = safe_join_within_root(&roots.recycle, &pending.staging_relative_path)?;
    let final_state = inspect_file(&roots.recycle, &final_path, &pending.expected_fingerprint)?;
    let staging_state = inspect_file(&roots.recycle, &staging, &pending.expected_fingerprint)?;
    if let (Some(a), Some(b)) = (&final_state, &staging_state) {
        if a != b {
            anyhow::bail!("pending purge has a different staging and recycle file")
        }
    }
    if final_state.is_some() {
        remove_verified(&roots.recycle, &final_path, &pending.expected_fingerprint)?;
    }
    if staging_state.is_some() {
        remove_verified(&roots.recycle, &staging, &pending.expected_fingerprint)?;
    }
    let current = locked
        .state
        .entries
        .get_mut(&identity_key(&entry.identity))
        .context("maintenance entry disappeared during purge reconcile")?;
    current.lifecycle = LifecycleState::PurgedLocal;
    current.purged_at = Some(now);
    if current.identity.source == SessionSource::Claude {
        current.project_name.clear();
        current.classifier_version = 0;
        current.score = 0;
        current.reason_codes.clear();
        current.hidden_since = None;
        current.recycled_at = None;
        current.keep = false;
        current.explicit_test = false;
    }
    locked.state.pending = None;
    locked.persist()
}

fn inspect_file(root: &Path, path: &Path, expected: &str) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("session maintenance path is a symlink: {}", path.display())
        }
        Ok(metadata) if !metadata.is_file() => {
            anyhow::bail!(
                "session maintenance path is not a regular file: {}",
                path.display()
            )
        }
        Ok(_) => {
            verify_regular_fingerprint_within_root(root, path, expected)?;
            Ok(Some(expected.to_string()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn path_is_regular(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "session maintenance target is a symlink: {}",
                path.display()
            )
        }
        Ok(metadata) if !metadata.is_file() => {
            anyhow::bail!(
                "session maintenance target is not a regular file: {}",
                path.display()
            )
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn verify_fingerprint(path: &Path, expected: &str) -> Result<()> {
    let actual = fingerprint_file(path)?.digest;
    if actual != expected {
        anyhow::bail!("session fingerprint mismatch")
    }
    Ok(())
}

fn verify_regular_fingerprint_within_root(root: &Path, path: &Path, expected: &str) -> Result<()> {
    validate_regular_candidate(root, path)?;
    verify_fingerprint(path, expected)
}

fn remove_verified(root: &Path, path: &Path, expected: &str) -> Result<()> {
    verify_regular_fingerprint_within_root(root, path, expected)?;
    fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))?;
    sync_parent_directory(path.parent().context("removed file has no parent")?)?;
    Ok(())
}

fn move_source_to_recycle(
    source_root: &Path,
    source: &Path,
    recycle_root: &Path,
    staging: &Path,
    final_path: &Path,
    expected: &str,
) -> Result<()> {
    validate_regular_candidate(source_root, source)?;
    verify_fingerprint(source, expected)?;
    prepare_regular_file_destination(
        recycle_root,
        staging.strip_prefix(recycle_root).unwrap_or(staging),
    )?;
    prepare_regular_file_destination(
        recycle_root,
        final_path.strip_prefix(recycle_root).unwrap_or(final_path),
    )?;

    if force_copy_fallback() {
        copy_source_to_final(source_root, source, recycle_root, final_path, expected)?;
        return Ok(());
    }

    move_noclobber(
        source_root,
        source,
        recycle_root,
        staging,
        expected,
        maybe_create_source_to_staging_target_for_test,
    )?;
    if path_is_regular(final_path)? {
        verify_regular_fingerprint_within_root(recycle_root, final_path, expected)?;
        remove_verified(recycle_root, staging, expected)?;
        return Ok(());
    }
    promote_noclobber(
        recycle_root,
        staging,
        recycle_root,
        final_path,
        expected,
        maybe_create_staging_to_final_target_for_test,
    )
}

fn copy_source_to_final(
    source_root: &Path,
    source: &Path,
    recycle_root: &Path,
    final_path: &Path,
    expected: &str,
) -> Result<()> {
    let final_relative = final_path
        .strip_prefix(recycle_root)
        .context("recycle target is outside recycle root")?;
    let final_path = prepare_regular_file_destination(recycle_root, final_relative)?;
    copy_verified_noclobber(
        source,
        source_root,
        recycle_root,
        &final_path,
        expected,
        true,
        maybe_create_copy_final_target_for_test,
    )
}

fn copy_verified_file(
    source: &Path,
    source_root: &Path,
    destination_root: &Path,
    destination_relative: &Path,
    expected: &str,
    before_commit: NoClobberHook,
) -> Result<()> {
    let destination = prepare_regular_file_destination(destination_root, destination_relative)?;
    copy_verified_noclobber(
        source,
        source_root,
        destination_root,
        &destination,
        expected,
        false,
        before_commit,
    )
}

type NoClobberHook = fn(&Path, &Path) -> Result<()>;

fn move_noclobber(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
    expected: &str,
    before_commit: NoClobberHook,
) -> Result<()> {
    validate_regular_candidate(source_root, source)?;
    verify_fingerprint(source, expected)?;
    let relative = destination
        .strip_prefix(destination_root)
        .context("destination is outside trusted root")?;
    let destination = prepare_regular_file_destination(destination_root, relative)?;

    if path_is_regular(&destination)? {
        verify_regular_fingerprint_within_root(destination_root, &destination, expected)?;
        remove_verified(source_root, source, expected)?;
        return Ok(());
    }

    before_commit(source, &destination)?;
    match fs::hard_link(source, &destination) {
        Ok(()) => {
            sync_parent_directory(
                destination
                    .parent()
                    .context("no parent for no-clobber destination")?,
            )?;
            verify_regular_fingerprint_within_root(destination_root, &destination, expected)?;
            remove_verified(source_root, source, expected)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            verify_regular_fingerprint_within_root(destination_root, &destination, expected)?;
            remove_verified(source_root, source, expected)
        }
        Err(error) if is_link_fallback_error(&error) => copy_verified_noclobber(
            source,
            source_root,
            destination_root,
            &destination,
            expected,
            true,
            noop_no_clobber_hook,
        ),
        Err(error) => Err(error).context("failed to create no-clobber hard link"),
    }
}

fn promote_noclobber(
    source_root: &Path,
    staging: &Path,
    destination_root: &Path,
    final_path: &Path,
    expected: &str,
    before_commit: NoClobberHook,
) -> Result<()> {
    move_noclobber(
        source_root,
        staging,
        destination_root,
        final_path,
        expected,
        before_commit,
    )
}

fn copy_verified_noclobber(
    source: &Path,
    source_root: &Path,
    destination_root: &Path,
    destination: &Path,
    expected: &str,
    remove_source: bool,
    before_commit: NoClobberHook,
) -> Result<()> {
    validate_regular_candidate(source_root, source)?;
    verify_fingerprint(source, expected)?;
    let relative = destination
        .strip_prefix(destination_root)
        .context("destination is outside trusted root")?;
    let destination = prepare_regular_file_destination(destination_root, relative)?;

    if path_is_regular(&destination)? {
        verify_regular_fingerprint_within_root(destination_root, &destination, expected)?;
        if remove_source {
            remove_verified(source_root, source, expected)?;
        }
        return Ok(());
    }

    let parent = destination
        .parent()
        .context("no parent for no-clobber destination")?;
    let mut temp = NamedTempFile::new_in(parent)?;
    let mut input = File::open(source)?;
    std::io::copy(&mut input, temp.as_file_mut())?;
    temp.as_file_mut().flush()?;
    temp.as_file().sync_all()?;
    verify_fingerprint(temp.path(), expected)?;
    before_commit(source, &destination)?;

    match temp.persist_noclobber(&destination) {
        Ok(_) => {}
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
            verify_regular_fingerprint_within_root(destination_root, &destination, expected)?;
            if remove_source {
                remove_verified(source_root, source, expected)?;
            }
            return Ok(());
        }
        Err(error) => return Err(error.error.into()),
    }

    sync_parent_directory(parent)?;
    verify_regular_fingerprint_within_root(destination_root, &destination, expected)?;
    if remove_source {
        remove_verified(source_root, source, expected)?;
    }
    Ok(())
}

fn is_link_fallback_error(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::Unsupported || is_cross_device_error(error)
}

fn ensure_recycle_root(root: &Path) -> Result<()> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            anyhow::bail!("recycle root must be a non-symlink directory")
        }
        Ok(_) => validate_directory_root(root).map(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(root)?;
            validate_directory_root(root).map(|_| ())
        }
        Err(error) => Err(error.into()),
    }
}

fn staging_relative_path(final_relative: &Path) -> PathBuf {
    PathBuf::from("staging").join(final_relative)
}

fn validate_pending_binding(entry: &MaintenanceEntry, pending: &PendingOperation) -> Result<()> {
    if pending.identity != entry.identity
        || pending.source_relative_path != entry.original_relative_path
        || pending.recycle_relative_path != recycle_relative_path(entry)
        || pending.staging_relative_path != staging_relative_path(&recycle_relative_path(entry))
    {
        anyhow::bail!("pending journal does not match deterministic session binding")
    }
    Ok(())
}

fn session_component(value: &str) -> String {
    format!("id-{}", blake3::hash(value.as_bytes()).to_hex())
}

fn safe_component(value: &str) -> String {
    if !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && !value.starts_with('\\')
        && value.as_bytes().get(1).is_none_or(|byte| *byte != b':')
    {
        value.to_string()
    } else {
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }
}

fn force_copy_fallback() -> bool {
    #[cfg(debug_assertions)]
    if std::env::var_os("CCS_TEST_MAINTENANCE_FORCE_COPY").is_some() {
        return true;
    }
    #[cfg(test)]
    {
        FORCE_COPY_FALLBACK.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn is_cross_device_error(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(18)
    }
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(17)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    static FORCE_COPY_FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_RESTORE_DESTINATION_REPLACEMENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_RECONCILE_FINAL_REGULAR_REPLACEMENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_RECONCILE_FINAL_SYMLINK_REPLACEMENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_SOURCE_TO_STAGING_TARGET: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static FORCE_STAGING_TO_FINAL_TARGET: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static FORCE_COPY_FINAL_TARGET: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static FORCE_RESTORE_DESTINATION_TARGET: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static FORCE_SOURCE_TO_STAGING_SYMLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_COPY_FINAL_SYMLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_RESTORE_DESTINATION_SYMLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_RESTORE_STAGING_REGULAR_REPLACEMENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_RESTORE_STAGING_SYMLINK_REPLACEMENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn noop_no_clobber_hook(_source: &Path, _destination: &Path) -> Result<()> {
    Ok(())
}

fn create_race_target_for_test(
    source: &Path,
    destination: &Path,
    same_fingerprint: bool,
) -> Result<()> {
    if same_fingerprint {
        fs::copy(source, destination)?;
    } else {
        fs::write(destination, b"different target")?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink_race_target_for_test(destination: &Path) -> Result<()> {
    let target = destination.with_file_name("race-target-outside.jsonl");
    fs::write(&target, b"outside target")?;
    std::os::unix::fs::symlink(target, destination)?;
    Ok(())
}

fn maybe_create_source_to_staging_target_for_test(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(test)]
    {
        #[cfg(unix)]
        if FORCE_SOURCE_TO_STAGING_SYMLINK.with(std::cell::Cell::get) {
            return create_symlink_race_target_for_test(destination);
        }
        if let Some(same) = FORCE_SOURCE_TO_STAGING_TARGET.with(std::cell::Cell::get) {
            create_race_target_for_test(source, destination, same)?;
        }
    }
    let _ = (source, destination);
    Ok(())
}

fn maybe_create_staging_to_final_target_for_test(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(test)]
    if let Some(same) = FORCE_STAGING_TO_FINAL_TARGET.with(std::cell::Cell::get) {
        create_race_target_for_test(source, destination, same)?;
    }
    let _ = (source, destination);
    Ok(())
}

fn maybe_create_copy_final_target_for_test(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(test)]
    {
        #[cfg(unix)]
        if FORCE_COPY_FINAL_SYMLINK.with(std::cell::Cell::get) {
            return create_symlink_race_target_for_test(destination);
        }
        if let Some(same) = FORCE_COPY_FINAL_TARGET.with(std::cell::Cell::get) {
            create_race_target_for_test(source, destination, same)?;
        }
    }
    let _ = (source, destination);
    Ok(())
}

fn maybe_create_restore_destination_target_for_test(
    source: &Path,
    destination: &Path,
) -> Result<()> {
    #[cfg(test)]
    {
        #[cfg(unix)]
        if FORCE_RESTORE_DESTINATION_SYMLINK.with(std::cell::Cell::get) {
            return create_symlink_race_target_for_test(destination);
        }
        if let Some(same) = FORCE_RESTORE_DESTINATION_TARGET.with(std::cell::Cell::get) {
            create_race_target_for_test(source, destination, same)?;
        }
    }
    let _ = (source, destination);
    Ok(())
}

fn maybe_replace_reconcile_final_for_test(path: &Path) -> Result<()> {
    #[cfg(test)]
    {
        if FORCE_RECONCILE_FINAL_REGULAR_REPLACEMENT.with(std::cell::Cell::get) {
            fs::write(path, b"different final")?;
        }
        #[cfg(unix)]
        if FORCE_RECONCILE_FINAL_SYMLINK_REPLACEMENT.with(std::cell::Cell::get) {
            let target = path.with_file_name("reconcile-race-outside.jsonl");
            fs::write(&target, b"outside")?;
            fs::remove_file(path)?;
            std::os::unix::fs::symlink(target, path)?;
        }
    }
    let _ = path;
    Ok(())
}

fn maybe_replace_restore_destination_for_test(path: &Path) -> Result<()> {
    #[cfg(test)]
    if FORCE_RESTORE_DESTINATION_REPLACEMENT.with(std::cell::Cell::get) {
        fs::write(path, b"changed during restore")?;
    }
    let _ = path;
    Ok(())
}

fn maybe_replace_restore_staging_for_test(path: &Path) -> Result<()> {
    #[cfg(test)]
    {
        if FORCE_RESTORE_STAGING_REGULAR_REPLACEMENT.with(std::cell::Cell::get) {
            fs::write(path, b"changed staging")?;
        }
        #[cfg(unix)]
        if FORCE_RESTORE_STAGING_SYMLINK_REPLACEMENT.with(std::cell::Cell::get) {
            let target = path.with_file_name("restore-staging-race-outside.jsonl");
            fs::write(&target, b"outside staging")?;
            fs::remove_file(path)?;
            std::os::unix::fs::symlink(target, path)?;
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_cache::fingerprint_file;
    use crate::session_maintenance::state::{identity_key, MaintenanceState};
    use crate::session_model::SessionIdentity;
    use std::fs;
    use tempfile::{tempdir, TempDir};

    struct RecycleFixture {
        _dir: TempDir,
        pub roots: MaintenanceRoots,
        pub store: StateStore,
        pub entry: MaintenanceEntry,
        pub now: DateTime<Utc>,
        pub source_file: PathBuf,
    }

    impl RecycleFixture {
        fn new(source: SessionSource) -> Self {
            let dir = tempdir().unwrap();
            let recycle = dir.path().join("recycle");
            let roots = MaintenanceRoots {
                claude: dir.path().join("claude"),
                codex: dir.path().join("codex"),
                omp: dir.path().join("omp"),
                recycle,
            };
            for root in [&roots.claude, &roots.codex, &roots.omp, &roots.recycle] {
                fs::create_dir_all(root).unwrap();
            }
            let source_file = roots
                .source_root(source)
                .join("project")
                .join("session.jsonl");
            fs::create_dir_all(source_file.parent().unwrap()).unwrap();
            fs::write(&source_file, b"session contents\n").unwrap();
            let fingerprint = fingerprint_file(&source_file).unwrap().digest;
            let identity = SessionIdentity {
                source,
                session_id: "session-1".to_string(),
            };
            let entry = MaintenanceEntry {
                identity,
                original_relative_path: PathBuf::from("project/session.jsonl"),
                project_name: "project".to_string(),
                fingerprint,
                lifecycle: LifecycleState::Hidden,
                classifier_version: 1,
                score: 100,
                reason_codes: vec![],
                hidden_since: Some(
                    DateTime::parse_from_rfc3339("2026-08-01T00:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                recycled_at: None,
                purged_at: None,
                keep: false,
                explicit_test: true,
            };
            let store = StateStore::from_config_dir(dir.path());
            store
                .update(|state| {
                    state
                        .entries
                        .insert(identity_key(&entry.identity), entry.clone());
                    Ok(())
                })
                .unwrap();
            Self {
                _dir: dir,
                roots,
                store,
                entry,
                now: DateTime::parse_from_rfc3339("2026-08-08T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                source_file,
            }
        }

        fn recycle_file(&self) -> PathBuf {
            self.roots.recycle.join(recycle_relative_path(&self.entry))
        }

        fn load_entry(&self) -> MaintenanceEntry {
            let state: MaintenanceState = self.store.load().unwrap();
            state
                .entries
                .get(&identity_key(&self.entry.identity))
                .unwrap()
                .clone()
        }

        fn set_pending_recycle(&self) {
            let final_relative = recycle_relative_path(&self.entry);
            self.store
                .update(|state| {
                    state.pending = Some(PendingOperation {
                        identity: self.entry.identity.clone(),
                        operation: PendingOperationKind::Recycle,
                        source_relative_path: self.entry.original_relative_path.clone(),
                        staging_relative_path: staging_relative_path(&final_relative),
                        recycle_relative_path: final_relative,
                        expected_fingerprint: self.entry.fingerprint.clone(),
                    });
                    Ok(())
                })
                .unwrap();
        }

        fn staging_file(&self) -> PathBuf {
            self.roots
                .recycle
                .join(staging_relative_path(&recycle_relative_path(&self.entry)))
        }

        fn set_pending_restore(&self) {
            let final_relative = recycle_relative_path(&self.entry);
            self.store
                .update(|state| {
                    state.pending = Some(PendingOperation {
                        identity: self.entry.identity.clone(),
                        operation: PendingOperationKind::Restore,
                        source_relative_path: self.entry.original_relative_path.clone(),
                        staging_relative_path: staging_relative_path(&final_relative),
                        recycle_relative_path: final_relative,
                        expected_fingerprint: self.entry.fingerprint.clone(),
                    });
                    Ok(())
                })
                .unwrap();
        }
    }

    #[test]
    fn recycle_moves_verified_file_and_records_recycled_state() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        assert!(!fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
    }

    #[test]
    fn recycled_idempotence_removes_matching_reappeared_source() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        fs::write(&fixture.source_file, b"session contents\n").unwrap();

        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        assert!(!fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);

        // A second idempotent call with the source absent remains successful.
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        assert!(!fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
    }

    #[test]
    fn recycled_idempotence_rejects_different_reappeared_source() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        fs::write(&fixture.source_file, b"different source\n").unwrap();
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        assert!(result.is_err());
        assert_eq!(
            fs::read(&fixture.source_file).unwrap(),
            b"different source\n"
        );
        assert!(fixture.recycle_file().exists());
        let state_after = fixture.store.load().unwrap();
        assert!(state_after.pending.is_none());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
        assert_eq!(fixture.load_entry().fingerprint, fixture.entry.fingerprint);
    }

    #[test]
    #[cfg(unix)]
    fn recycled_idempotence_rejects_symlink_reappeared_source() {
        let fixture = RecycleFixture::new(SessionSource::Omp);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        let outside = fixture
            .source_file
            .with_file_name("reappeared-outside.jsonl");
        fs::write(&outside, b"outside source\n").unwrap();
        std::os::unix::fs::symlink(&outside, &fixture.source_file).unwrap();
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        assert!(result.is_err());
        assert!(fixture.source_file.is_symlink());
        assert_eq!(fs::read(&outside).unwrap(), b"outside source\n");
        assert!(fixture.recycle_file().exists());
        let state_after = fixture.store.load().unwrap();
        assert!(state_after.pending.is_none());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
        assert_eq!(fixture.load_entry().fingerprint, fixture.entry.fingerprint);
    }

    #[test]
    #[cfg(unix)]
    fn recycle_rejects_symlink_without_removing_target() {
        let fixture = RecycleFixture::new(SessionSource::Omp);
        let outside = fixture.source_file.with_file_name("outside.jsonl");
        fs::write(&outside, b"outside").unwrap();
        fs::remove_file(&fixture.source_file).unwrap();
        std::os::unix::fs::symlink(&outside, &fixture.source_file).unwrap();
        assert!(
            recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).is_err()
        );
        assert!(outside.exists());
    }

    #[test]
    fn pending_with_missing_source_and_existing_target_finalizes_recycled() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        let final_path = fixture.recycle_file();
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, &final_path).unwrap();
        fs::remove_file(&fixture.source_file).unwrap();
        fixture.set_pending_recycle();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
    }

    #[test]
    fn forced_copy_fallback_preserves_fingerprint_and_removes_source() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        FORCE_COPY_FALLBACK.with(|flag| flag.set(true));
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        FORCE_COPY_FALLBACK.with(|flag| flag.set(false));
        result.unwrap();
        assert!(!fixture.source_file.exists());
        assert_eq!(
            fingerprint_file(&fixture.recycle_file()).unwrap().digest,
            fixture.entry.fingerprint
        );
    }

    #[test]
    fn reconcile_source_only_finishes_recycle() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        fixture.set_pending_recycle();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert!(!fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
    }

    #[test]
    fn reconcile_staging_only_promotes_before_finishing_recycle() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::rename(&fixture.source_file, fixture.staging_file()).unwrap();
        fixture.set_pending_recycle();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert!(!fixture.staging_file().exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
    }

    #[test]
    fn reconcile_source_and_staging_promotes_and_removes_both_duplicates() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        fixture.set_pending_recycle();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert!(!fixture.source_file.exists());
        assert!(!fixture.staging_file().exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
    }

    #[test]
    fn reconcile_revalidates_promoted_regular_final_before_removing_source() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        fixture.set_pending_recycle();
        FORCE_RECONCILE_FINAL_REGULAR_REPLACEMENT.with(|flag| flag.set(true));
        let result = reconcile_pending(&fixture.store, &fixture.roots, fixture.now);
        FORCE_RECONCILE_FINAL_REGULAR_REPLACEMENT.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert!(!fixture.staging_file().exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(
            fs::read(fixture.recycle_file()).unwrap(),
            b"different final"
        );
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_revalidates_promoted_symlink_final_before_removing_source() {
        let fixture = RecycleFixture::new(SessionSource::Omp);
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        fixture.set_pending_recycle();
        FORCE_RECONCILE_FINAL_SYMLINK_REPLACEMENT.with(|flag| flag.set(true));
        let result = reconcile_pending(&fixture.store, &fixture.roots, fixture.now);
        FORCE_RECONCILE_FINAL_SYMLINK_REPLACEMENT.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert!(!fixture.staging_file().exists());
        assert!(fixture.recycle_file().is_symlink());
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn recycle_session_id_components_are_always_distinct_digests() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        let ids = ["abc", "ABC", "abc.", "\u{00e9}", "e\u{301}"];
        let paths: Vec<_> = ids
            .iter()
            .map(|session_id| {
                let mut entry = fixture.entry.clone();
                entry.identity.session_id = (*session_id).to_string();
                recycle_relative_path(&entry)
            })
            .collect();
        for path in &paths {
            let component = path
                .components()
                .nth(1)
                .unwrap()
                .as_os_str()
                .to_string_lossy();
            assert!(component.starts_with("id-"));
            assert_eq!(component.len(), 67);
        }
        for (index, path) in paths.iter().enumerate() {
            assert!(paths[index + 1..].iter().all(|other| other != path));
        }
        let mut same = fixture.entry.clone();
        same.identity.session_id = "abc".to_string();
        assert_eq!(recycle_relative_path(&same), paths[0]);
    }

    #[test]
    fn recycle_source_to_staging_race_different_never_clobbers_target() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        FORCE_SOURCE_TO_STAGING_TARGET.with(|mode| mode.set(Some(false)));
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        FORCE_SOURCE_TO_STAGING_TARGET.with(|mode| mode.set(None));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.staging_file().exists());
        assert_eq!(
            fs::read(fixture.staging_file()).unwrap(),
            b"different target"
        );
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn recycle_source_to_staging_race_same_fingerprint_is_idempotent() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        FORCE_SOURCE_TO_STAGING_TARGET.with(|mode| mode.set(Some(true)));
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        FORCE_SOURCE_TO_STAGING_TARGET.with(|mode| mode.set(None));
        result.unwrap();
        assert!(!fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
        assert!(fixture.store.load().unwrap().pending.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn recycle_source_to_staging_race_symlink_never_removes_source() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        FORCE_SOURCE_TO_STAGING_SYMLINK.with(|flag| flag.set(true));
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        FORCE_SOURCE_TO_STAGING_SYMLINK.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.staging_file().is_symlink());
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn reconcile_staging_to_final_race_different_never_clobbers_target() {
        let fixture = RecycleFixture::new(SessionSource::Omp);
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        fixture.set_pending_recycle();
        FORCE_STAGING_TO_FINAL_TARGET.with(|mode| mode.set(Some(false)));
        let result = reconcile_pending(&fixture.store, &fixture.roots, fixture.now);
        FORCE_STAGING_TO_FINAL_TARGET.with(|mode| mode.set(None));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.staging_file().exists());
        assert_eq!(
            fs::read(fixture.recycle_file()).unwrap(),
            b"different target"
        );
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn reconcile_staging_to_final_race_same_fingerprint_is_idempotent() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        fixture.set_pending_recycle();
        FORCE_STAGING_TO_FINAL_TARGET.with(|mode| mode.set(Some(true)));
        let result = reconcile_pending(&fixture.store, &fixture.roots, fixture.now);
        FORCE_STAGING_TO_FINAL_TARGET.with(|mode| mode.set(None));
        result.unwrap();
        assert!(!fixture.source_file.exists());
        assert!(!fixture.staging_file().exists());
        assert!(fixture.recycle_file().exists());
        assert!(fixture.store.load().unwrap().pending.is_none());
    }

    #[test]
    fn copy_final_race_different_never_clobbers_target() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        FORCE_COPY_FALLBACK.with(|flag| flag.set(true));
        FORCE_COPY_FINAL_TARGET.with(|mode| mode.set(Some(false)));
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        FORCE_COPY_FINAL_TARGET.with(|mode| mode.set(None));
        FORCE_COPY_FALLBACK.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert_eq!(
            fs::read(fixture.recycle_file()).unwrap(),
            b"different target"
        );
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn copy_final_race_same_fingerprint_is_idempotent() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        FORCE_COPY_FALLBACK.with(|flag| flag.set(true));
        FORCE_COPY_FINAL_TARGET.with(|mode| mode.set(Some(true)));
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        FORCE_COPY_FINAL_TARGET.with(|mode| mode.set(None));
        FORCE_COPY_FALLBACK.with(|flag| flag.set(false));
        result.unwrap();
        assert!(!fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
        assert!(fixture.store.load().unwrap().pending.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn copy_final_race_symlink_never_removes_source() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        FORCE_COPY_FALLBACK.with(|flag| flag.set(true));
        FORCE_COPY_FINAL_SYMLINK.with(|flag| flag.set(true));
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        FORCE_COPY_FINAL_SYMLINK.with(|flag| flag.set(false));
        FORCE_COPY_FALLBACK.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.recycle_file().is_symlink());
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn restore_destination_race_different_never_clobbers_target() {
        let fixture = RecycleFixture::new(SessionSource::Omp);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        FORCE_RESTORE_DESTINATION_TARGET.with(|mode| mode.set(Some(false)));
        let result = restore_session(
            &fixture.store,
            &fixture.roots,
            &fixture.load_entry(),
            fixture.now,
        );
        FORCE_RESTORE_DESTINATION_TARGET.with(|mode| mode.set(None));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert_eq!(fs::read(&fixture.source_file).unwrap(), b"different target");
        assert!(fixture.recycle_file().exists());
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn restore_destination_race_same_fingerprint_is_idempotent() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        FORCE_RESTORE_DESTINATION_TARGET.with(|mode| mode.set(Some(true)));
        let result = restore_session(
            &fixture.store,
            &fixture.roots,
            &fixture.load_entry(),
            fixture.now,
        );
        FORCE_RESTORE_DESTINATION_TARGET.with(|mode| mode.set(None));
        result.unwrap();
        assert!(fixture.source_file.exists());
        assert!(!fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Visible);
    }

    #[test]
    #[cfg(unix)]
    fn restore_destination_race_symlink_never_removes_recycle_source() {
        let fixture = RecycleFixture::new(SessionSource::Omp);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        FORCE_RESTORE_DESTINATION_SYMLINK.with(|flag| flag.set(true));
        let result = restore_session(
            &fixture.store,
            &fixture.roots,
            &fixture.load_entry(),
            fixture.now,
        );
        FORCE_RESTORE_DESTINATION_SYMLINK.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert!(fixture.source_file.is_symlink());
        assert!(fixture.recycle_file().exists());
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn recycle_cleans_matching_existing_staging_after_final() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        fs::create_dir_all(fixture.recycle_file().parent().unwrap()).unwrap();
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.recycle_file()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        assert!(!fixture.source_file.exists());
        assert!(!fixture.staging_file().exists());
        assert!(fixture.recycle_file().exists());
    }

    #[test]
    fn reconcile_staging_and_final_same_keeps_final_and_removes_staging() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        fs::create_dir_all(fixture.recycle_file().parent().unwrap()).unwrap();
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.recycle_file()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        fs::remove_file(&fixture.source_file).unwrap();
        fixture.set_pending_recycle();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert!(!fixture.staging_file().exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
    }

    #[test]
    fn reconcile_staging_and_final_different_keeps_both_and_pending() {
        let fixture = RecycleFixture::new(SessionSource::Omp);
        fs::create_dir_all(fixture.recycle_file().parent().unwrap()).unwrap();
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        fs::write(fixture.recycle_file(), b"different").unwrap();
        fs::remove_file(&fixture.source_file).unwrap();
        fixture.set_pending_recycle();
        assert!(reconcile_pending(&fixture.store, &fixture.roots, fixture.now).is_err());
        assert!(fixture.staging_file().exists());
        assert!(fixture.recycle_file().exists());
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn reconcile_three_files_with_different_final_keeps_all_and_pending() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        fs::create_dir_all(fixture.recycle_file().parent().unwrap()).unwrap();
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        fs::write(fixture.recycle_file(), b"different").unwrap();
        fixture.set_pending_recycle();
        assert!(reconcile_pending(&fixture.store, &fixture.roots, fixture.now).is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.staging_file().exists());
        assert!(fixture.recycle_file().exists());
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn reconcile_source_and_final_same_removes_duplicate_source() {
        let fixture = RecycleFixture::new(SessionSource::Omp);
        fs::create_dir_all(fixture.recycle_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.recycle_file()).unwrap();
        fixture.set_pending_recycle();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert!(!fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
    }

    #[test]
    fn reconcile_source_staging_and_final_same_cleans_duplicates() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        fs::create_dir_all(fixture.recycle_file().parent().unwrap()).unwrap();
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.recycle_file()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        fixture.set_pending_recycle();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert!(!fixture.source_file.exists());
        assert!(!fixture.staging_file().exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
    }

    #[test]
    fn reconcile_source_and_final_different_keeps_both_and_pending() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        fs::create_dir_all(fixture.recycle_file().parent().unwrap()).unwrap();
        fs::write(fixture.recycle_file(), b"different").unwrap();
        fixture.set_pending_recycle();
        assert!(reconcile_pending(&fixture.store, &fixture.roots, fixture.now).is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Hidden);
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn reconcile_all_missing_fails_safe_without_marking_recycled() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        fs::remove_file(&fixture.source_file).unwrap();
        fixture.set_pending_recycle();
        assert!(reconcile_pending(&fixture.store, &fixture.roots, fixture.now).is_err());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Hidden);
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn unsafe_session_id_uses_digest_component() {
        let mut entry = RecycleFixture::new(SessionSource::Claude).entry;
        entry.identity.session_id = "../outside\\session".to_string();
        let relative = recycle_relative_path(&entry);
        assert_eq!(relative.components().count(), 3);
        assert!(!relative.to_string_lossy().contains("outside"));
    }

    #[test]
    fn restore_refuses_different_destination_and_keeps_both_files() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        fs::create_dir_all(fixture.source_file.parent().unwrap()).unwrap();
        fs::write(&fixture.source_file, b"different").unwrap();
        let restored = restore_session(
            &fixture.store,
            &fixture.roots,
            &fixture.load_entry(),
            fixture.now,
        );
        assert!(restored.is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
    }

    #[test]
    fn restore_copies_back_and_marks_visible() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        let recycled = fixture.load_entry();
        restore_session(&fixture.store, &fixture.roots, &recycled, fixture.now).unwrap();
        assert!(fixture.source_file.exists());
        assert!(!fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Visible);
        assert!(fixture.store.load().unwrap().pending.is_none());
    }

    #[test]
    fn reconcile_restore_source_and_final_same_removes_final() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        let recycled = fixture.load_entry();
        let final_path = fixture.recycle_file();
        fs::create_dir_all(fixture.source_file.parent().unwrap()).unwrap();
        fs::copy(&final_path, &fixture.source_file).unwrap();
        let final_relative = recycle_relative_path(&recycled);
        fixture
            .store
            .update(|state| {
                state.pending = Some(PendingOperation {
                    identity: recycled.identity.clone(),
                    operation: PendingOperationKind::Restore,
                    source_relative_path: recycled.original_relative_path.clone(),
                    staging_relative_path: staging_relative_path(&final_relative),
                    recycle_relative_path: final_relative,
                    expected_fingerprint: recycled.fingerprint.clone(),
                });
                Ok(())
            })
            .unwrap();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert!(fixture.source_file.exists());
        assert!(!fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Visible);
    }

    #[test]
    fn reconcile_restore_source_and_staging_without_final_cleans_staging_and_is_idempotent() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        fs::copy(fixture.recycle_file(), &fixture.source_file).unwrap();
        fs::remove_file(fixture.recycle_file()).unwrap();
        fixture.set_pending_restore();
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert!(fixture.source_file.exists());
        assert!(!fixture.staging_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Visible);
        assert!(fixture.store.load().unwrap().pending.is_none());
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert!(fixture.source_file.exists());
        assert!(!fixture.staging_file().exists());
    }

    #[test]
    fn reconcile_restore_source_and_staging_replacement_keeps_pending() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        fs::copy(fixture.recycle_file(), &fixture.source_file).unwrap();
        fs::remove_file(fixture.recycle_file()).unwrap();
        fixture.set_pending_restore();
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        FORCE_RESTORE_STAGING_REGULAR_REPLACEMENT.with(|flag| flag.set(true));
        let result = reconcile_pending(&fixture.store, &fixture.roots, fixture.now);
        FORCE_RESTORE_STAGING_REGULAR_REPLACEMENT.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert_eq!(
            fs::read(fixture.staging_file()).unwrap(),
            b"changed staging"
        );
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    #[cfg(unix)]
    fn reconcile_restore_source_and_staging_symlink_keeps_pending() {
        let fixture = RecycleFixture::new(SessionSource::Omp);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        fs::copy(fixture.recycle_file(), &fixture.source_file).unwrap();
        fs::remove_file(fixture.recycle_file()).unwrap();
        fixture.set_pending_restore();
        fs::create_dir_all(fixture.staging_file().parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, fixture.staging_file()).unwrap();
        FORCE_RESTORE_STAGING_SYMLINK_REPLACEMENT.with(|flag| flag.set(true));
        let result = reconcile_pending(&fixture.store, &fixture.roots, fixture.now);
        FORCE_RESTORE_STAGING_SYMLINK_REPLACEMENT.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.staging_file().is_symlink());
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn reconcile_restore_revalidates_destination_before_removing_recycle_file() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        fixture.set_pending_restore();
        FORCE_RESTORE_DESTINATION_REPLACEMENT.with(|flag| flag.set(true));
        let result = reconcile_pending(&fixture.store, &fixture.roots, fixture.now);
        FORCE_RESTORE_DESTINATION_REPLACEMENT.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert_eq!(
            fs::read(&fixture.source_file).unwrap(),
            b"changed during restore"
        );
        assert!(fixture.recycle_file().exists());
        assert!(fixture.store.load().unwrap().pending.is_some());
    }

    #[test]
    fn reconcile_purge_missing_target_finalizes_audit_state() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        let recycled = fixture.load_entry();
        fs::remove_file(fixture.recycle_file()).unwrap();
        let final_relative = recycle_relative_path(&recycled);
        fixture
            .store
            .update(|state| {
                state.pending = Some(PendingOperation {
                    identity: recycled.identity.clone(),
                    operation: PendingOperationKind::Purge,
                    source_relative_path: recycled.original_relative_path.clone(),
                    staging_relative_path: staging_relative_path(&final_relative),
                    recycle_relative_path: final_relative,
                    expected_fingerprint: recycled.fingerprint.clone(),
                });
                Ok(())
            })
            .unwrap();
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::PurgedLocal);
        assert!(fixture.load_entry().purged_at.is_some());
    }

    #[test]
    fn purge_removes_only_verified_recycle_file_and_keeps_audit_entry() {
        let fixture = RecycleFixture::new(SessionSource::Omp);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        let entry = fixture.load_entry();
        purge_session(&fixture.store, &fixture.roots, &entry, fixture.now).unwrap();
        assert!(!fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::PurgedLocal);
        assert!(fixture.load_entry().purged_at.is_some());
    }

    #[test]
    fn restore_rechecks_destination_before_removing_recycle_file() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        FORCE_RESTORE_DESTINATION_REPLACEMENT.with(|flag| flag.set(true));
        let result = restore_session(
            &fixture.store,
            &fixture.roots,
            &fixture.load_entry(),
            fixture.now,
        );
        FORCE_RESTORE_DESTINATION_REPLACEMENT.with(|flag| flag.set(false));
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
    }

    #[test]
    fn reconcile_rejects_wrong_journal_binding_without_modifying_files() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        let final_path = fixture.recycle_file();
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::copy(&fixture.source_file, &final_path).unwrap();
        let final_relative = recycle_relative_path(&fixture.entry);
        fixture
            .store
            .update(|state| {
                state.pending = Some(PendingOperation {
                    identity: fixture.entry.identity.clone(),
                    operation: PendingOperationKind::Recycle,
                    source_relative_path: PathBuf::from("other/session.jsonl"),
                    staging_relative_path: staging_relative_path(&final_relative),
                    recycle_relative_path: final_relative,
                    expected_fingerprint: fixture.entry.fingerprint.clone(),
                });
                Ok(())
            })
            .unwrap();
        assert!(reconcile_pending(&fixture.store, &fixture.roots, fixture.now).is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Hidden);
    }

    #[test]
    fn codex_purge_retains_audit_fields() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        let entry = fixture.load_entry();
        purge_session(&fixture.store, &fixture.roots, &entry, fixture.now).unwrap();
        let purged = fixture.load_entry();
        assert_eq!(purged.lifecycle, LifecycleState::PurgedLocal);
        assert_eq!(purged.project_name, "project");
        assert_eq!(purged.classifier_version, 1);
        assert_eq!(purged.score, 100);
        assert!(purged.hidden_since.is_some());
        assert!(purged.explicit_test);
    }

    #[test]
    fn claude_purge_keeps_only_minimal_suppression_fields() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now).unwrap();
        let entry = fixture.load_entry();
        purge_session(&fixture.store, &fixture.roots, &entry, fixture.now).unwrap();
        let purged = fixture.load_entry();
        assert_eq!(purged.lifecycle, LifecycleState::PurgedLocal);
        assert_eq!(purged.identity, fixture.entry.identity);
        assert_eq!(
            purged.original_relative_path,
            fixture.entry.original_relative_path
        );
        assert_eq!(purged.fingerprint, fixture.entry.fingerprint);
        assert!(purged.purged_at.is_some());
        assert!(purged.project_name.is_empty());
        assert!(purged.reason_codes.is_empty());
        assert_eq!(purged.score, 0);
        assert!(purged.hidden_since.is_none());
        assert!(purged.recycled_at.is_none());
    }

    #[test]
    fn repeated_recycle_restore_and_purge_are_idempotent() {
        let recycle_fixture = RecycleFixture::new(SessionSource::Codex);
        recycle_session(
            &recycle_fixture.store,
            &recycle_fixture.roots,
            &recycle_fixture.entry,
            recycle_fixture.now,
        )
        .unwrap();
        recycle_session(
            &recycle_fixture.store,
            &recycle_fixture.roots,
            &recycle_fixture.load_entry(),
            recycle_fixture.now,
        )
        .unwrap();

        let restore_entry = recycle_fixture.load_entry();
        restore_session(
            &recycle_fixture.store,
            &recycle_fixture.roots,
            &restore_entry,
            recycle_fixture.now,
        )
        .unwrap();
        restore_session(
            &recycle_fixture.store,
            &recycle_fixture.roots,
            &recycle_fixture.load_entry(),
            recycle_fixture.now,
        )
        .unwrap();

        let purge_fixture = RecycleFixture::new(SessionSource::Omp);
        recycle_session(
            &purge_fixture.store,
            &purge_fixture.roots,
            &purge_fixture.entry,
            purge_fixture.now,
        )
        .unwrap();
        let purge_entry = purge_fixture.load_entry();
        purge_session(
            &purge_fixture.store,
            &purge_fixture.roots,
            &purge_entry,
            purge_fixture.now,
        )
        .unwrap();
        purge_session(
            &purge_fixture.store,
            &purge_fixture.roots,
            &purge_fixture.load_entry(),
            purge_fixture.now,
        )
        .unwrap();
    }

    #[test]
    fn pending_persist_failure_does_not_touch_source() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        crate::atomic_file::test_fail_persist_on_call(1);
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        crate::atomic_file::test_clear_persist_failures();
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert!(!fixture.recycle_file().exists());
    }

    #[test]
    fn pending_parent_sync_failure_leaves_source_and_reconcilable_journal() {
        let fixture = RecycleFixture::new(SessionSource::Claude);
        crate::atomic_file::test_force_parent_sync_failure(true);
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        crate::atomic_file::test_force_parent_sync_failure(false);
        assert!(result.is_err());
        assert!(fixture.source_file.exists());
        assert!(fixture.store.load().unwrap().pending.is_some());
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert!(!fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
    }

    #[test]
    fn final_state_persist_failure_leaves_reconcilable_pending_operation() {
        let fixture = RecycleFixture::new(SessionSource::Codex);
        crate::atomic_file::test_fail_persist_on_call(2);
        let result = recycle_session(&fixture.store, &fixture.roots, &fixture.entry, fixture.now);
        crate::atomic_file::test_clear_persist_failures();
        assert!(result.is_err());
        assert!(!fixture.source_file.exists());
        assert!(fixture.recycle_file().exists());
        assert!(fixture.store.load().unwrap().pending.is_some());
        reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
        assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
    }
}
