use crate::atomic_file::{persist_json_atomic, FileLock};
use crate::filter::SessionMaintenanceSettings;
use crate::session_maintenance::classifier::{Classification, ClassificationDecision, ReasonCode};
use crate::session_model::{claude_session_id_from_path, SessionIdentity, SessionSource};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
#[cfg(debug_assertions)]
use std::time::{Duration as StdDuration, Instant};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LifecycleState {
    Visible,
    Hidden,
    Recycled,
    PurgedLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleTransition {
    NoChange,
    Hide,
    Recycle,
    PurgeLocal,
    RestoreVisible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingOperationKind {
    Recycle,
    Restore,
    Purge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MaintenanceEntry {
    pub identity: SessionIdentity,
    pub original_relative_path: PathBuf,
    pub project_name: String,
    pub fingerprint: String,
    pub lifecycle: LifecycleState,
    pub classifier_version: u32,
    pub score: u16,
    pub reason_codes: Vec<ReasonCode>,
    pub hidden_since: Option<DateTime<Utc>>,
    pub recycled_at: Option<DateTime<Utc>>,
    pub purged_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub keep: bool,
    #[serde(default)]
    pub explicit_test: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingOperation {
    pub identity: SessionIdentity,
    pub operation: PendingOperationKind,
    pub source_relative_path: PathBuf,
    pub staging_relative_path: PathBuf,
    pub recycle_relative_path: PathBuf,
    pub expected_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MaintenanceState {
    pub version: u32,
    pub entries: HashMap<String, MaintenanceEntry>,
    pub pending: Option<PendingOperation>,
}

impl Default for MaintenanceState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            entries: HashMap::new(),
            pending: None,
        }
    }
}

impl MaintenanceState {
    /// Return whether a missing sync-repository path belongs to a locally suppressed Claude session.
    pub(crate) fn is_suppressed_missing_session(&self, relative: &Path) -> bool {
        let Some(session_id) = claude_session_id_from_path(relative) else {
            return false;
        };
        let identity = SessionIdentity {
            source: SessionSource::Claude,
            session_id,
        };
        self.entries
            .get(&identity_key(&identity))
            .is_some_and(|entry| {
                entry.identity.source == SessionSource::Claude
                    && matches!(
                        entry.lifecycle,
                        LifecycleState::Recycled | LifecycleState::PurgedLocal
                    )
            })
    }

    /// Remove a suppression only when the exact observed entry is still current.
    pub(crate) fn clear_suppression_if_matches(
        &mut self,
        identity: &SessionIdentity,
        expected_fingerprint: &str,
        expected_lifecycle: LifecycleState,
    ) -> bool {
        if identity.source != SessionSource::Claude {
            return false;
        }
        let key = identity_key(identity);
        let matches = self.entries.get(&key).is_some_and(|entry| {
            entry.identity == *identity
                && entry.identity.source == SessionSource::Claude
                && entry.fingerprint == expected_fingerprint
                && entry.lifecycle == expected_lifecycle
                && matches!(
                    entry.lifecycle,
                    LifecycleState::Recycled | LifecycleState::PurgedLocal
                )
        });
        if matches {
            self.entries.remove(&key);
        }
        matches
    }
}

pub(crate) fn identity_key(identity: &SessionIdentity) -> String {
    format!("{}:{}", identity.source.as_str(), identity.session_id)
}

pub(crate) fn reconcile_fingerprint(
    entry: &MaintenanceEntry,
    fingerprint: &str,
) -> LifecycleTransition {
    if entry.fingerprint == fingerprint {
        LifecycleTransition::NoChange
    } else {
        LifecycleTransition::RestoreVisible
    }
}

pub(crate) fn next_lifecycle(
    entry: Option<&MaintenanceEntry>,
    decision: &ClassificationDecision,
    now: DateTime<Utc>,
    settings: &SessionMaintenanceSettings,
) -> LifecycleTransition {
    let is_candidate = decision.classification == Classification::TestCandidate;
    let Some(entry) = entry else {
        return if is_candidate {
            LifecycleTransition::Hide
        } else {
            LifecycleTransition::NoChange
        };
    };

    if !is_candidate {
        return LifecycleTransition::NoChange;
    }

    match entry.lifecycle {
        LifecycleState::Visible => LifecycleTransition::Hide,
        LifecycleState::PurgedLocal => LifecycleTransition::NoChange,
        LifecycleState::Hidden => {
            if elapsed_at_least(entry.hidden_since, now, settings.recycle_after_days) {
                LifecycleTransition::Recycle
            } else {
                LifecycleTransition::NoChange
            }
        }
        LifecycleState::Recycled => {
            if elapsed_at_least(entry.hidden_since, now, settings.purge_after_days) {
                LifecycleTransition::PurgeLocal
            } else {
                LifecycleTransition::NoChange
            }
        }
    }
}

fn elapsed_at_least(start: Option<DateTime<Utc>>, now: DateTime<Utc>, days: u64) -> bool {
    let Some(start) = start else {
        return false;
    };
    let max_days = i64::MAX as u64 / 86_400;
    let threshold = Duration::days(days.min(max_days) as i64);
    now.signed_duration_since(start) >= threshold
}

fn validate_state(state: &MaintenanceState) -> Result<()> {
    if state.version != STATE_VERSION {
        anyhow::bail!(
            "unsupported maintenance state version {} (expected {})",
            state.version,
            STATE_VERSION
        );
    }

    for (key, entry) in &state.entries {
        let expected_key = identity_key(&entry.identity);
        if key != &expected_key {
            anyhow::bail!(
                "maintenance entry key {key:?} does not match identity key {expected_key:?}"
            );
        }
        validate_relative_path("original_relative_path", &entry.original_relative_path)?;
    }

    if let Some(pending) = &state.pending {
        let key = identity_key(&pending.identity);
        let Some(entry) = state.entries.get(&key) else {
            anyhow::bail!("pending operation has no matching entry {key:?}");
        };
        let lifecycle_matches = match pending.operation {
            PendingOperationKind::Recycle => entry.lifecycle == LifecycleState::Hidden,
            PendingOperationKind::Restore | PendingOperationKind::Purge => {
                entry.lifecycle == LifecycleState::Recycled
            }
        };
        if !lifecycle_matches {
            anyhow::bail!(
                "pending operation {:?} is incompatible with lifecycle {:?}",
                pending.operation,
                entry.lifecycle
            );
        }
        if pending.expected_fingerprint != entry.fingerprint {
            anyhow::bail!("pending fingerprint does not match entry fingerprint for {key:?}");
        }
        validate_relative_path("source_relative_path", &pending.source_relative_path)?;
        validate_relative_path("staging_relative_path", &pending.staging_relative_path)?;
        validate_relative_path("recycle_relative_path", &pending.recycle_relative_path)?;
    }
    Ok(())
}

fn validate_relative_path(name: &str, path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        anyhow::bail!("relative path {name} must be non-empty and relative");
    }

    let raw = path.to_string_lossy();
    if raw.starts_with('\\') || raw.as_bytes().get(1).is_some_and(|byte| *byte == b':') {
        anyhow::bail!("relative path {name} must be relative");
    }
    for segment in raw.split(['/', '\\']) {
        if segment == "." || segment == ".." {
            anyhow::bail!("relative path {name} cannot contain '.' or '..' components");
        }
    }
    for component in path.components() {
        if matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::RootDir
        ) {
            anyhow::bail!("relative path {name} contains an unsafe path component");
        }
        #[cfg(windows)]
        if matches!(component, Component::Prefix(_)) {
            anyhow::bail!("pending {name} contains a path prefix");
        }
    }
    Ok(())
}

pub(crate) struct StateStore {
    state_path: PathBuf,
    lock_path: PathBuf,
}

pub(crate) struct LockedState<'a> {
    store: &'a StateStore,
    pub(crate) state: MaintenanceState,
}

#[cfg(debug_assertions)]
pub(crate) fn wait_for_maintenance_test_gate(
    ready_env: &str,
    release_env: &str,
    label: &str,
) -> Result<()> {
    let Some(ready) = std::env::var_os(ready_env) else {
        return Ok(());
    };
    let Some(release) = std::env::var_os(release_env) else {
        anyhow::bail!("{ready_env} requires {release_env}");
    };
    let ready = PathBuf::from(ready);
    let release = PathBuf::from(release);
    fs::write(&ready, b"ready")
        .with_context(|| format!("write {label} ready marker {}", ready.display()))?;

    let deadline = Instant::now() + StdDuration::from_secs(30);
    while !release.exists() {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for {label} release marker {}",
                release.display()
            );
        }
        std::thread::sleep(StdDuration::from_millis(5));
    }
    Ok(())
}

impl LockedState<'_> {
    pub(crate) fn persist(&self) -> Result<()> {
        validate_state(&self.state)?;
        persist_json_atomic(&self.store.state_path, &self.state)
    }
}

impl StateStore {
    pub(crate) fn from_config_dir(config_dir: &Path) -> Self {
        Self {
            state_path: config_dir.join("session-maintenance.json"),
            lock_path: config_dir.join("session-maintenance.lock"),
        }
    }

    pub(crate) fn load(&self) -> Result<MaintenanceState> {
        self.load_unlocked()
    }

    pub(crate) fn transaction<F, T>(&self, transaction: F) -> Result<T>
    where
        F: FnOnce(&mut LockedState<'_>) -> Result<T>,
    {
        let _lock = FileLock::acquire(&self.lock_path)
            .with_context(|| format!("failed to lock {}", self.lock_path.display()))?;
        #[cfg(debug_assertions)]
        wait_for_maintenance_test_gate(
            "CCS_TEST_MAINTENANCE_LOCK_READY",
            "CCS_TEST_MAINTENANCE_LOCK_RELEASE",
            "maintenance lock",
        )?;
        let state = self.load_unlocked()?;
        let mut locked = LockedState { store: self, state };
        transaction(&mut locked)
    }

    pub(crate) fn update<F, T>(&self, update: F) -> Result<T>
    where
        F: FnOnce(&mut MaintenanceState) -> Result<T>,
    {
        self.transaction(|locked| {
            let result = update(&mut locked.state)?;
            locked.persist()?;
            Ok(result)
        })
    }

    fn load_unlocked(&self) -> Result<MaintenanceState> {
        let bytes = match fs::read(&self.state_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(MaintenanceState::default())
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", self.state_path.display()))
            }
        };

        let state: MaintenanceState = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid maintenance state {}", self.state_path.display()))?;
        validate_state(&state)
            .with_context(|| format!("invalid maintenance state {}", self.state_path.display()))?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::SessionMaintenanceSettings;
    use crate::session_maintenance::classifier::{
        Classification, ClassificationDecision, ReasonCode, CLASSIFIER_VERSION,
    };
    use crate::session_model::{SessionIdentity, SessionSource};
    use chrono::{DateTime, Duration, Utc};
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-08T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn settings() -> SessionMaintenanceSettings {
        SessionMaintenanceSettings::default()
    }

    fn test_decision() -> ClassificationDecision {
        ClassificationDecision {
            classification: Classification::TestCandidate,
            score: 100,
            reasons: vec![ReasonCode::ExplicitTestMarker],
        }
    }

    fn identity() -> SessionIdentity {
        SessionIdentity {
            source: SessionSource::Claude,
            session_id: "session-1".to_string(),
        }
    }

    fn hidden_entry(at: DateTime<Utc>) -> MaintenanceEntry {
        MaintenanceEntry {
            identity: identity(),
            original_relative_path: PathBuf::from("project/session.jsonl"),
            project_name: "project".to_string(),
            fingerprint: "fingerprint".to_string(),
            lifecycle: LifecycleState::Hidden,
            classifier_version: CLASSIFIER_VERSION,
            score: 100,
            reason_codes: vec![ReasonCode::ExplicitTestMarker],
            hidden_since: Some(at),
            recycled_at: None,
            purged_at: None,
            keep: false,
            explicit_test: true,
        }
    }

    fn recycled_entry(at: DateTime<Utc>) -> MaintenanceEntry {
        let mut entry = hidden_entry(now());
        entry.lifecycle = LifecycleState::Recycled;
        entry.recycled_at = Some(at);
        entry
    }

    fn pending(identity: SessionIdentity, operation: PendingOperationKind) -> PendingOperation {
        PendingOperation {
            identity,
            operation,
            source_relative_path: PathBuf::from("project/session.jsonl"),
            staging_relative_path: PathBuf::from("staging/session.jsonl"),
            recycle_relative_path: PathBuf::from("recycle/session.jsonl"),
            expected_fingerprint: "fingerprint".to_string(),
        }
    }

    fn write_state(dir: &std::path::Path, state: &MaintenanceState) -> Vec<u8> {
        let path = dir.join("session-maintenance.json");
        persist_json_atomic(&path, state).unwrap();
        std::fs::read(path).unwrap()
    }

    fn assert_invalid_state_is_rejected(state: MaintenanceState) {
        let dir = tempdir().unwrap();
        let original = write_state(dir.path(), &state);
        let store = StateStore::from_config_dir(dir.path());
        assert!(store.load().is_err());
        assert_eq!(
            std::fs::read(dir.path().join("session-maintenance.json")).unwrap(),
            original
        );
    }

    #[test]
    fn first_match_only_enters_hidden_even_for_old_session() {
        let transition = next_lifecycle(None, &test_decision(), now(), &settings());
        assert_eq!(transition, LifecycleTransition::Hide);
    }

    #[test]
    fn visible_candidate_restarts_lifecycle_with_hide() {
        let mut entry = hidden_entry(now());
        entry.lifecycle = LifecycleState::Visible;
        assert_eq!(
            next_lifecycle(Some(&entry), &test_decision(), now(), &settings()),
            LifecycleTransition::Hide
        );
    }

    #[test]
    fn hidden_entry_recycles_at_seven_days_but_not_before() {
        let exact = hidden_entry(now() - Duration::days(7));
        assert_eq!(
            next_lifecycle(Some(&exact), &test_decision(), now(), &settings()),
            LifecycleTransition::Recycle
        );

        let just_before = hidden_entry(now() - Duration::days(7) + Duration::seconds(1));
        assert_eq!(
            next_lifecycle(Some(&just_before), &test_decision(), now(), &settings()),
            LifecycleTransition::NoChange
        );
    }

    #[test]
    fn purge_uses_first_hidden_time_at_thirty_days_not_recycle_time() {
        let mut exact = recycled_entry(now() - Duration::seconds(1));
        exact.hidden_since = Some(now() - Duration::days(30));
        assert_eq!(
            next_lifecycle(Some(&exact), &test_decision(), now(), &settings()),
            LifecycleTransition::PurgeLocal
        );

        let mut just_before = recycled_entry(now() - Duration::seconds(1));
        just_before.hidden_since = Some(now() - Duration::days(30) + Duration::seconds(1));
        assert_eq!(
            next_lifecycle(Some(&just_before), &test_decision(), now(), &settings()),
            LifecycleTransition::NoChange
        );
    }

    #[test]
    fn identity_key_keeps_sources_and_special_ids_distinct() {
        let claude = SessionIdentity {
            source: SessionSource::Claude,
            session_id: "same:id/中文".to_string(),
        };
        let codex = SessionIdentity {
            source: SessionSource::Codex,
            session_id: claude.session_id.clone(),
        };
        assert_eq!(identity_key(&claude), "claude:same:id/中文");
        assert_ne!(identity_key(&claude), identity_key(&codex));
    }

    #[test]
    fn changed_fingerprint_returns_visible() {
        let mut entry = hidden_entry(now() - Duration::days(7));
        entry.fingerprint = "old".to_string();
        assert_eq!(
            reconcile_fingerprint(&entry, "new"),
            LifecycleTransition::RestoreVisible
        );
    }

    #[test]
    fn suppression_clear_is_compare_and_swap() {
        let entry = recycled_entry(now());
        let mut state = MaintenanceState::default();
        state
            .entries
            .insert(identity_key(&entry.identity), entry.clone());

        let mut changed_fingerprint = entry.clone();
        changed_fingerprint.fingerprint = "newer".to_string();
        state
            .entries
            .insert(identity_key(&entry.identity), changed_fingerprint);
        assert!(!state.clear_suppression_if_matches(
            &entry.identity,
            "fingerprint",
            LifecycleState::Recycled,
        ));
        assert!(state.entries.contains_key(&identity_key(&entry.identity)));

        let mut changed_lifecycle = entry.clone();
        changed_lifecycle.lifecycle = LifecycleState::Visible;
        state
            .entries
            .insert(identity_key(&entry.identity), changed_lifecycle);
        assert!(!state.clear_suppression_if_matches(
            &entry.identity,
            "fingerprint",
            LifecycleState::Recycled,
        ));
        assert!(state.entries.contains_key(&identity_key(&entry.identity)));

        state
            .entries
            .insert(identity_key(&entry.identity), entry.clone());
        assert!(state.clear_suppression_if_matches(
            &entry.identity,
            "fingerprint",
            LifecycleState::Recycled,
        ));
        assert!(!state.entries.contains_key(&identity_key(&entry.identity)));
    }

    #[test]
    fn unchanged_hidden_entry_waits_before_recycling() {
        let entry = hidden_entry(now() - Duration::days(6));
        assert_eq!(
            next_lifecycle(Some(&entry), &test_decision(), now(), &settings()),
            LifecycleTransition::NoChange
        );
    }

    #[test]
    fn hidden_entry_stays_unchanged_when_classifier_protects_it() {
        let entry = hidden_entry(now() - Duration::days(6));
        let mut decision = test_decision();
        decision.classification = Classification::Keep;
        assert_eq!(
            next_lifecycle(Some(&entry), &decision, now(), &settings()),
            LifecycleTransition::NoChange
        );
    }

    #[test]
    fn first_match_keep_does_not_create_hidden_entry() {
        let mut decision = test_decision();
        decision.classification = Classification::Keep;
        assert_eq!(
            next_lifecycle(None, &decision, now(), &settings()),
            LifecycleTransition::NoChange
        );
    }

    #[test]
    fn state_rejects_map_key_that_does_not_match_identity() {
        let entry = hidden_entry(now());
        let mut state = MaintenanceState::default();
        state.entries.insert("wrong-key".to_string(), entry);
        assert_invalid_state_is_rejected(state);
    }

    #[test]
    fn state_rejects_pending_identity_without_entry() {
        let entry = hidden_entry(now());
        let mut state = MaintenanceState::default();
        state.entries.insert(identity_key(&entry.identity), entry);
        state.pending = Some(pending(
            SessionIdentity {
                source: SessionSource::Claude,
                session_id: "missing".to_string(),
            },
            PendingOperationKind::Recycle,
        ));
        assert_invalid_state_is_rejected(state);
    }

    #[test]
    fn state_rejects_pending_fingerprint_mismatch() {
        let entry = hidden_entry(now());
        let mut state = MaintenanceState::default();
        state
            .entries
            .insert(identity_key(&entry.identity), entry.clone());
        let mut operation = pending(entry.identity, PendingOperationKind::Recycle);
        operation.expected_fingerprint = "different".to_string();
        state.pending = Some(operation);
        assert_invalid_state_is_rejected(state);
    }

    #[test]
    fn state_rejects_dangerous_entry_paths() {
        let entry = hidden_entry(now());
        for path in [
            PathBuf::from("/absolute/session.jsonl"),
            PathBuf::from("./project/session.jsonl"),
            PathBuf::from("project/../session.jsonl"),
            PathBuf::from(r"C:drive/session.jsonl"),
        ] {
            let mut invalid = MaintenanceState::default();
            let mut invalid_entry = entry.clone();
            invalid_entry.original_relative_path = path;
            invalid
                .entries
                .insert(identity_key(&invalid_entry.identity), invalid_entry);
            assert_invalid_state_is_rejected(invalid);
        }
    }

    #[test]
    fn state_rejects_pending_operation_that_does_not_match_lifecycle() {
        let entry = hidden_entry(now());
        let mut state = MaintenanceState::default();
        state
            .entries
            .insert(identity_key(&entry.identity), entry.clone());
        for operation in [PendingOperationKind::Restore, PendingOperationKind::Purge] {
            let mut invalid = state.clone();
            invalid.pending = Some(pending(entry.identity.clone(), operation));
            assert_invalid_state_is_rejected(invalid);
        }

        let mut recycled = entry;
        recycled.lifecycle = LifecycleState::Recycled;
        let mut invalid = MaintenanceState::default();
        invalid
            .entries
            .insert(identity_key(&recycled.identity), recycled.clone());
        invalid.pending = Some(pending(recycled.identity, PendingOperationKind::Recycle));
        assert_invalid_state_is_rejected(invalid);
    }

    #[test]
    fn state_rejects_absolute_dot_and_dotdot_pending_paths() {
        let entry = hidden_entry(now());
        let mut base = MaintenanceState::default();
        base.entries
            .insert(identity_key(&entry.identity), entry.clone());
        for (field, path) in [
            ("source", PathBuf::from("/absolute/session.jsonl")),
            ("staging", PathBuf::from("./staging/session.jsonl")),
            ("recycle", PathBuf::from("recycle/../session.jsonl")),
            ("source", PathBuf::from(r"C:drive/session.jsonl")),
        ] {
            let mut invalid = base.clone();
            let mut operation = pending(entry.identity.clone(), PendingOperationKind::Recycle);
            match field {
                "source" => operation.source_relative_path = path,
                "staging" => operation.staging_relative_path = path,
                "recycle" => operation.recycle_relative_path = path,
                _ => unreachable!(),
            }
            invalid.pending = Some(operation);
            assert_invalid_state_is_rejected(invalid);
        }
    }

    #[test]
    fn invalid_version_cannot_be_persisted_by_update() {
        let dir = tempdir().unwrap();
        let store = StateStore::from_config_dir(dir.path());
        assert!(store
            .update(|state| {
                state.version = STATE_VERSION + 1;
                Ok(())
            })
            .is_err());
        assert!(!dir.path().join("session-maintenance.json").exists());
    }

    #[test]
    fn locked_persist_rejects_invalid_state_without_writing() {
        let dir = tempdir().unwrap();
        let store = StateStore::from_config_dir(dir.path());
        let entry = hidden_entry(now());
        let result = store.transaction(|locked| {
            locked
                .state
                .entries
                .insert("wrong-key".to_string(), entry.clone());
            locked.persist()
        });
        assert!(result.is_err());
        assert!(!dir.path().join("session-maintenance.json").exists());
    }

    #[test]
    fn locked_persist_rejects_pending_fingerprint_without_rewriting() {
        let dir = tempdir().unwrap();
        let store = StateStore::from_config_dir(dir.path());
        let entry = hidden_entry(now());
        store
            .update(|state| {
                state
                    .entries
                    .insert(identity_key(&entry.identity), entry.clone());
                Ok(())
            })
            .unwrap();
        let path = dir.path().join("session-maintenance.json");
        let original = std::fs::read(&path).unwrap();

        let result = store.transaction(|locked| {
            let mut operation = pending(entry.identity.clone(), PendingOperationKind::Recycle);
            operation.expected_fingerprint = "different".to_string();
            locked.state.pending = Some(operation);
            locked.persist()
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn locked_persist_rejects_dangerous_entry_path_without_writing() {
        let dir = tempdir().unwrap();
        let store = StateStore::from_config_dir(dir.path());
        let mut entry = hidden_entry(now());
        entry.original_relative_path = PathBuf::from("../outside/session.jsonl");
        let result = store.transaction(|locked| {
            locked
                .state
                .entries
                .insert(identity_key(&entry.identity), entry.clone());
            locked.persist()
        });
        assert!(result.is_err());
        assert!(!dir.path().join("session-maintenance.json").exists());
    }

    #[test]
    fn missing_state_loads_default_without_creating_a_file() {
        let dir = tempdir().unwrap();
        let store = StateStore::from_config_dir(dir.path());

        assert_eq!(store.load().unwrap().version, STATE_VERSION);
        assert!(!dir.path().join("session-maintenance.json").exists());
    }

    #[test]
    fn failed_update_does_not_persist_mutations() {
        let dir = tempdir().unwrap();
        let store = StateStore::from_config_dir(dir.path());
        let entry = hidden_entry(now());

        assert!(store
            .update(|state| -> anyhow::Result<()> {
                state
                    .entries
                    .insert(identity_key(&entry.identity), entry.clone());
                anyhow::bail!("abort update")
            })
            .is_err());
        assert!(!dir.path().join("session-maintenance.json").exists());
    }

    #[test]
    fn update_merges_sequential_state_changes() {
        let dir = tempdir().unwrap();
        let store = StateStore::from_config_dir(dir.path());
        let first = hidden_entry(now());
        let mut second = first.clone();
        second.identity.session_id = "session-2".to_string();

        store
            .update(|state| {
                state
                    .entries
                    .insert(identity_key(&first.identity), first.clone());
                Ok(())
            })
            .unwrap();
        store
            .update(|state| {
                state
                    .entries
                    .insert(identity_key(&second.identity), second.clone());
                Ok(())
            })
            .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.entries.len(), 2);
        assert!(loaded.entries.contains_key(&identity_key(&first.identity)));
        assert!(loaded.entries.contains_key(&identity_key(&second.identity)));
    }

    #[test]
    fn transaction_can_persist_multiple_durability_boundaries() {
        let dir = tempdir().unwrap();
        let store = StateStore::from_config_dir(dir.path());
        let first = hidden_entry(now());
        let mut second = first.clone();
        second.identity.session_id = "session-2".to_string();

        let state_path = dir.path().join("session-maintenance.json");
        let first_phase = store
            .transaction(|locked| {
                locked
                    .state
                    .entries
                    .insert(identity_key(&first.identity), first.clone());
                locked.persist()?;
                let persisted = std::fs::read(&state_path)?;
                locked
                    .state
                    .entries
                    .insert(identity_key(&second.identity), second.clone());
                locked.persist()?;
                Ok(persisted)
            })
            .unwrap();

        let first_state: MaintenanceState = serde_json::from_slice(&first_phase).unwrap();
        assert_eq!(first_state.entries.len(), 1);
        assert!(first_state
            .entries
            .contains_key(&identity_key(&first.identity)));
        assert_eq!(store.load().unwrap().entries.len(), 2);
    }

    #[test]
    fn malformed_state_is_rejected_without_writing() {
        let dir = tempdir().unwrap();
        let store = StateStore::from_config_dir(dir.path());
        let path = dir.path().join("session-maintenance.json");
        let original = b"{not-json";
        std::fs::write(&path, original).unwrap();

        assert!(store.update(|_| Ok(())).is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn unsupported_state_version_is_rejected_without_writing() {
        let dir = tempdir().unwrap();
        let store = StateStore::from_config_dir(dir.path());
        let path = dir.path().join("session-maintenance.json");
        let original = br#"{"version":99,"entries":{},"pending":null}"#;
        std::fs::write(&path, original).unwrap();

        assert!(store.transaction(|_| Ok(())).is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }
}
