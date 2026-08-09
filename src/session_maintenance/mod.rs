//! Session maintenance domain orchestration.

pub(crate) mod classifier;
#[allow(dead_code)]
pub(crate) mod recycle;
#[allow(dead_code)]
pub(crate) mod state;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::filter::SessionMaintenanceSettings;
use crate::path_security::{
    safe_join_within_root, safe_relative_path_within_root, validate_directory_root,
    validate_regular_candidate,
};
use crate::session_cache::fingerprint_file;
use crate::session_model::{SessionIdentity, SessionSource, SessionSourceFilter, SessionSummary};

use self::classifier::{classify, ClassifierPolicy, MaintenanceCandidate};
use self::recycle::{
    purge_session, reconcile_pending, recycle_relative_path, recycle_session, MaintenanceRoots,
};
use self::state::{
    identity_key, reconcile_fingerprint, LifecycleState, MaintenanceEntry, StateStore,
};

/// Controls whether maintenance may persist state or move files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceMode {
    Disabled,
    #[allow(dead_code)]
    DryRun,
    Apply,
}

/// Clock dependency used to make maintenance decisions deterministic in tests.
pub(crate) trait MaintenanceClock {
    fn now(&self) -> DateTime<Utc>;
}

/// Production clock for maintenance runs.
pub(crate) struct SystemMaintenanceClock;

impl MaintenanceClock for SystemMaintenanceClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Inputs discovered by the session scanner and trusted by the orchestrator.
pub(crate) struct MaintenanceInput<'a> {
    pub summaries: &'a [SessionSummary],
    pub completed_sources: &'a HashSet<SessionSource>,
    pub roots: &'a MaintenanceRoots,
    pub config_dir: &'a Path,
    pub settings: &'a SessionMaintenanceSettings,
    pub clock: &'a dyn MaintenanceClock,
}

/// Aggregate result of one maintenance run.
#[derive(Debug, Default, Clone)]
pub struct MaintenanceReport {
    pub candidates: usize,
    pub hidden: usize,
    pub recycled: usize,
    pub purged: usize,
    pub restored_visible: usize,
    pub file_actions: usize,
    pub remaining_actions: usize,
    pub warnings: usize,
    pub(crate) visibility: VisibilityIndex,
}

/// Lifecycle states to overlay on session listings.
#[derive(Debug, Default, Clone)]
pub(crate) struct VisibilityIndex {
    pub(crate) states: HashMap<SessionIdentity, LifecycleState>,
}

/// Construct a classifier candidate from a validated scanner summary.
pub(crate) fn candidate_from_summary(
    summary: &SessionSummary,
    roots: &MaintenanceRoots,
    existing: Option<&MaintenanceEntry>,
) -> Result<MaintenanceCandidate> {
    let identity = summary.identity()?;
    let source_root = roots.source_root(identity.source);
    let original_relative_path = safe_relative_path_within_root(source_root, &summary.file_path)
        .context("session file is not a safe source candidate")?;
    let fingerprint = fingerprint_file(&summary.file_path)?;
    let source_label = identity.source.as_str().to_string();
    let parse_timestamp = |value: &Option<String>| -> Result<Option<DateTime<Utc>>> {
        value
            .as_deref()
            .map(|timestamp| {
                DateTime::parse_from_rfc3339(timestamp)
                    .map(|parsed| parsed.with_timezone(&Utc))
                    .with_context(|| format!("invalid session timestamp for {source_label}"))
            })
            .transpose()
    };

    Ok(MaintenanceCandidate {
        identity,
        original_relative_path,
        project_name: summary.project_name.clone(),
        project_dir: summary.project_dir.clone(),
        cwd: summary.cwd.as_deref().map(PathBuf::from),
        // Set by the caller once the whole candidate set is known.
        repeated_title_burst: false,
        title: summary.title.clone(),
        has_custom_title: summary.has_custom_title,
        user_message_count: summary.user_message_count,
        message_count: summary.message_count,
        first_activity: parse_timestamp(&summary.first_timestamp)?,
        last_activity: parse_timestamp(&summary.last_activity)?,
        size: summary.file_size,
        fingerprint,
        explicit_test: existing.is_some_and(|entry| entry.explicit_test),
        keep: existing.is_some_and(|entry| entry.keep),
    })
}

fn visibility_from_state(state: &state::MaintenanceState) -> VisibilityIndex {
    VisibilityIndex {
        states: state
            .entries
            .values()
            .map(|entry| (entry.identity.clone(), entry.lifecycle))
            .collect(),
    }
}

/// Return the persisted maintenance entry for a source-qualified identity.
pub(crate) fn maintenance_state_for<'a>(
    state: &'a state::MaintenanceState,
    identity: &SessionIdentity,
) -> Option<&'a MaintenanceEntry> {
    state.entries.get(&identity_key(identity))
}

/// Pull-side outcome for a locally recycled or purged Claude revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressionDecision {
    /// No applicable suppression record exists.
    NotSuppressed,
    /// The remote bytes are the same locally suppressed revision.
    SkipSameRevision,
    /// The remote bytes are a new revision and should be restored.
    RestoreNewRevision,
}

/// Decide whether a remote session is suppressed by maintenance state.
///
/// Only Claude sessions in the Recycled or PurgedLocal lifecycle participate. Callers that
/// cannot load state must use `NotSuppressed` so remote data is restored rather than lost.
pub(crate) fn suppression_for_remote(
    state: &state::MaintenanceState,
    identity: &SessionIdentity,
    fingerprint: &str,
) -> SuppressionDecision {
    if identity.source != SessionSource::Claude {
        return SuppressionDecision::NotSuppressed;
    }
    let Some(entry) = maintenance_state_for(state, identity) else {
        return SuppressionDecision::NotSuppressed;
    };
    if !matches!(
        entry.lifecycle,
        LifecycleState::Recycled | LifecycleState::PurgedLocal
    ) {
        return SuppressionDecision::NotSuppressed;
    }
    if entry.fingerprint == fingerprint {
        SuppressionDecision::SkipSameRevision
    } else {
        SuppressionDecision::RestoreNewRevision
    }
}

const MAX_RECYCLED_QUERY_WARNINGS: usize = 16;

fn recycled_query_warning(count: &mut usize) {
    if *count < MAX_RECYCLED_QUERY_WARNINGS {
        log::warn!(
            target: crate::logger::SCAN_DIAGNOSTICS_TARGET,
            "recycled session entry skipped during query"
        );
    } else if *count == MAX_RECYCLED_QUERY_WARNINGS {
        log::warn!(
            target: crate::logger::SCAN_DIAGNOSTICS_TARGET,
            "additional recycled session entries skipped during query"
        );
    }
    *count = count.saturating_add(1);
}

/// Load parseable recycled sessions from trusted recycle final files.
///
/// Only `Recycled` state entries are considered. Every final path is rebuilt below the
/// trusted recycle root and must be a regular non-symlink file with the recorded fingerprint.
/// A bad entry is isolated to a bounded warning and does not hide other valid recycled sessions.
pub(crate) fn load_recycled_summaries(
    roots: &MaintenanceRoots,
    state: &state::MaintenanceState,
    source_filter: SessionSourceFilter,
) -> Result<Vec<SessionSummary>> {
    match fs::symlink_metadata(&roots.recycle) {
        Ok(_) => validate_directory_root(&roots.recycle)
            .context("validate session recycle root for query")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("inspect session recycle root for query"),
    };

    let mut entries: Vec<&MaintenanceEntry> = state
        .entries
        .values()
        .filter(|entry| {
            entry.lifecycle == LifecycleState::Recycled
                && source_filter.includes(entry.identity.source)
        })
        .collect();
    entries.sort_by(|left, right| identity_key(&left.identity).cmp(&identity_key(&right.identity)));

    let mut warnings = 0usize;
    let mut summaries = Vec::new();
    for entry in entries {
        let result = (|| -> Result<SessionSummary> {
            let final_relative = recycle_relative_path(entry);
            let final_path = safe_join_within_root(&roots.recycle, &final_relative)?;
            validate_regular_candidate(&roots.recycle, &final_path)?;
            let fingerprint = fingerprint_file(&final_path)?;
            if fingerprint.digest != entry.fingerprint {
                anyhow::bail!("recycled file fingerprint mismatch")
            }

            let mut summary = match entry.identity.source {
                SessionSource::Claude => {
                    let session = crate::parser::ConversationSession::from_file(&final_path)?;
                    let project_dir = match entry.original_relative_path.parent() {
                        Some(parent) if !parent.as_os_str().is_empty() => {
                            safe_join_within_root(&roots.claude, parent)?
                        }
                        _ => {
                            validate_directory_root(&roots.claude)?;
                            roots.claude.clone()
                        }
                    };
                    SessionSummary::from_session(&session, &entry.project_name, &project_dir)
                }
                SessionSource::Codex => {
                    let session = crate::codex::CodexSession::from_file(&final_path)?;
                    let title = session.title(None);
                    SessionSummary::from_codex_session(&session, &entry.project_name, title)
                }
                SessionSource::Omp => {
                    let session = crate::omp::OmpSession::from_file(&final_path)?;
                    SessionSummary::from_omp_session(&session, &entry.project_name)
                }
            };
            if !summary.is_valid() {
                anyhow::bail!("recycled session summary is not semantically valid")
            }
            summary.source = entry.identity.source.as_str().to_string();
            summary.session_id = entry.identity.session_id.clone();
            summary.project_name = entry.project_name.clone();
            summary.file_path = final_path;
            Ok(summary)
        })();

        match result {
            Ok(summary) => summaries.push(summary),
            Err(_) => recycled_query_warning(&mut warnings),
        }
    }

    Ok(summaries)
}

fn maintenance_policy(settings: &SessionMaintenanceSettings) -> Option<ClassifierPolicy> {
    (settings.classifier == "conservative").then(|| {
        // Temporary roots belong to the orchestration environment, not the classifier's
        // policy defaults, so callers and tests can control this boundary explicitly.
        ClassifierPolicy::with_temporary_roots(
            settings.hide_after_hours,
            vec![
                std::env::temp_dir(),
                PathBuf::from("/tmp"),
                PathBuf::from("/private/tmp"),
                PathBuf::from(r"C:\Temp"),
                PathBuf::from(r"C:\Windows\Temp"),
            ],
        )
    })
}

/// Reconcile pending filesystem work, classify completed source summaries, and optionally apply
/// lifecycle transitions. Invalid completed-batch input fails safe with a warning-only report; an
/// unreadable state store is returned to the caller so visibility cannot silently widen.
pub(crate) fn run_maintenance(
    input: MaintenanceInput<'_>,
    mode: MaintenanceMode,
) -> Result<MaintenanceReport> {
    let store = StateStore::from_config_dir(input.config_dir);
    let mut report = MaintenanceReport::default();
    let mut state = store.load().context("load session maintenance state")?;
    if mode == MaintenanceMode::Disabled {
        report.visibility = visibility_from_state(&state);
        return Ok(report);
    }
    report.visibility = visibility_from_state(&state);
    let Some(policy) = maintenance_policy(input.settings) else {
        report.warnings = 1;
        return Ok(report);
    };

    // Validate the entire completed batch before any pending journal or file mutation. A single
    // malformed identity, duplicate, unsafe path, timestamp, or fingerprint aborts the batch.
    let mut grouped: HashMap<SessionIdentity, Vec<&SessionSummary>> = HashMap::new();
    let mut validation_failed = false;
    for summary in input.summaries {
        let Ok(identity) = summary.identity() else {
            validation_failed = true;
            continue;
        };
        if input.completed_sources.contains(&identity.source) {
            grouped.entry(identity).or_default().push(summary);
        }
    }

    let mut candidates = Vec::new();
    for (identity, summaries) in grouped {
        if summaries.len() != 1 {
            validation_failed = true;
            continue;
        }
        let summary = summaries[0];
        let key = identity_key(&identity);
        let existing = state.entries.get(&key);
        match candidate_from_summary(summary, input.roots, existing) {
            Ok(candidate) => candidates.push((candidate, existing.cloned())),
            Err(_) => validation_failed = true,
        }
    }
    if validation_failed {
        report.warnings += 1;
        return Ok(report);
    }
    candidates.sort_by(|left, right| {
        identity_key(&left.0.identity).cmp(&identity_key(&right.0.identity))
    });
    report.candidates = candidates.len();

    // Repeat detection needs the whole candidate set, so it runs once here and the
    // per-session classifier stays a pure function of its candidate.
    let burst_inputs: Vec<(String, Option<DateTime<Utc>>)> = candidates
        .iter()
        .map(|(candidate, _)| (candidate.title.clone(), candidate.first_activity))
        .collect();
    for (burst, (candidate, _)) in classifier::repeated_title_bursts(&burst_inputs)
        .into_iter()
        .zip(candidates.iter_mut())
    {
        candidate.repeated_title_burst = burst;
    }

    // A pending transaction is recoverable only when its source scan completed. DryRun must
    // remain entirely read-only, so it intentionally skips reconciliation.
    if mode == MaintenanceMode::Apply {
        if let Some(pending) = state.pending.as_ref() {
            if input.completed_sources.contains(&pending.identity.source) {
                if let Err(error) = reconcile_pending(&store, input.roots, input.clock.now()) {
                    report.warnings += 1;
                    state = store
                        .load()
                        .context("reload maintenance state after reconciliation failure")?;
                    report.visibility = visibility_from_state(&state);
                    let _ = error;
                    return Ok(report);
                }
                state = store
                    .load()
                    .context("reload maintenance state after reconciliation")?;
                report.visibility = visibility_from_state(&state);
            }
        }
        if prune_purged_audits(&store, input.clock.now(), input.completed_sources).is_err() {
            report.warnings += 1;
            state = store
                .load()
                .context("reload maintenance state after audit pruning failure")?;
            report.visibility = visibility_from_state(&state);
            return Ok(report);
        }
        state = store
            .load()
            .context("reload maintenance state after audit pruning")?;
        report.visibility = visibility_from_state(&state);
    }

    let mutation_result: Result<()> = (|| {
        let mut planned_file_actions = 0usize;
        for (candidate, existing) in candidates {
            let key = identity_key(&candidate.identity);
            let decision = classify(&candidate, &policy, input.clock.now());
            let entry = existing;

            if let Some(current) = entry.as_ref() {
                if reconcile_fingerprint(current, &candidate.fingerprint.digest)
                    == state::LifecycleTransition::RestoreVisible
                {
                    report.restored_visible += 1;
                    if mode == MaintenanceMode::Apply {
                        let fingerprint = candidate.fingerprint.digest.clone();
                        store.update(|saved| {
                            if let Some(current) = saved.entries.get_mut(&key) {
                                current.lifecycle = LifecycleState::Visible;
                                current.fingerprint = fingerprint;
                                current.hidden_since = None;
                                current.recycled_at = None;
                                current.purged_at = None;
                            }
                            Ok(())
                        })?;
                    }
                    continue;
                }
            }

            if mode == MaintenanceMode::Apply {
                if let Some(current) = entry.as_ref() {
                    if current.classifier_version != classifier::CLASSIFIER_VERSION {
                        update_entry_metadata(
                            &store,
                            &key,
                            current,
                            &candidate,
                            &decision,
                            input.clock.now(),
                        )?;
                    }
                }
            }
            let transition =
                state::next_lifecycle(entry.as_ref(), &decision, input.clock.now(), input.settings);
            match (entry.as_ref(), transition) {
                (None, state::LifecycleTransition::Hide) => {
                    report.hidden += 1;
                    if mode == MaintenanceMode::Apply {
                        let new_entry = MaintenanceEntry {
                            identity: candidate.identity.clone(),
                            original_relative_path: candidate.original_relative_path.clone(),
                            project_name: candidate.project_name.clone(),
                            fingerprint: candidate.fingerprint.digest.clone(),
                            lifecycle: LifecycleState::Hidden,
                            classifier_version: classifier::CLASSIFIER_VERSION,
                            score: decision.score,
                            reason_codes: decision.reasons.clone(),
                            hidden_since: Some(input.clock.now()),
                            recycled_at: None,
                            purged_at: None,
                            keep: candidate.keep,
                            explicit_test: candidate.explicit_test,
                        };
                        store.update(|saved| {
                            saved.entries.insert(key.clone(), new_entry);
                            Ok(())
                        })?;
                    }
                }
                (Some(current), state::LifecycleTransition::Hide) => {
                    report.hidden += 1;
                    if mode == MaintenanceMode::Apply {
                        update_entry_metadata(
                            &store,
                            &key,
                            current,
                            &candidate,
                            &decision,
                            input.clock.now(),
                        )?;
                    }
                }
                (Some(current), state::LifecycleTransition::Recycle) => {
                    if planned_file_actions >= input.settings.max_actions_per_run {
                        report.remaining_actions += 1;
                        continue;
                    }
                    if mode == MaintenanceMode::Apply {
                        recycle_session(&store, input.roots, current, input.clock.now())?;
                    }
                    planned_file_actions += 1;
                    report.file_actions += 1;
                    report.recycled += 1;
                }
                (Some(current), state::LifecycleTransition::PurgeLocal) => {
                    if planned_file_actions >= input.settings.max_actions_per_run {
                        report.remaining_actions += 1;
                        continue;
                    }
                    if mode == MaintenanceMode::Apply {
                        purge_session(&store, input.roots, current, input.clock.now())?;
                    }
                    planned_file_actions += 1;
                    report.file_actions += 1;
                    report.purged += 1;
                }
                _ => {}
            }
        }

        if mode == MaintenanceMode::Apply {
            state = store.load()?;
            report.visibility = visibility_from_state(&state);
        }
        Ok(())
    })();

    if let Err(_error) = mutation_result {
        report.warnings += 1;
        state = store
            .load()
            .context("reload maintenance state after maintenance failure")?;
        report.visibility = visibility_from_state(&state);
    }
    Ok(report)
}

fn prune_purged_audits(
    store: &StateStore,
    now: DateTime<Utc>,
    completed_sources: &HashSet<SessionSource>,
) -> Result<()> {
    let cutoff = now - Duration::days(30);
    let state = store.load()?;
    let should_prune = state.entries.values().any(|entry| {
        completed_sources.contains(&entry.identity.source)
            && matches!(
                entry.identity.source,
                SessionSource::Codex | SessionSource::Omp
            )
            && entry.lifecycle == LifecycleState::PurgedLocal
            && entry.purged_at.is_some_and(|purged_at| purged_at <= cutoff)
    });
    if should_prune {
        store.update(|saved| {
            saved.entries.retain(|_, entry| {
                !(completed_sources.contains(&entry.identity.source)
                    && matches!(
                        entry.identity.source,
                        SessionSource::Codex | SessionSource::Omp
                    )
                    && entry.lifecycle == LifecycleState::PurgedLocal
                    && entry.purged_at.is_some_and(|purged_at| purged_at <= cutoff))
            });
            Ok(())
        })?;
    }
    Ok(())
}

fn update_entry_metadata(
    store: &StateStore,
    key: &str,
    current: &MaintenanceEntry,
    candidate: &MaintenanceCandidate,
    decision: &classifier::ClassificationDecision,
    now: DateTime<Utc>,
) -> Result<()> {
    let current = current.clone();
    store.update(|saved| {
        let entry = saved
            .entries
            .get_mut(key)
            .context("maintenance entry disappeared during classification")?;
        if entry.fingerprint != current.fingerprint {
            anyhow::bail!("stale maintenance entry during classification")
        }
        entry.classifier_version = classifier::CLASSIFIER_VERSION;
        entry.score = decision.score;
        entry.reason_codes = decision.reasons.clone();
        entry.project_name = candidate.project_name.clone();
        entry.keep = candidate.keep;
        entry.explicit_test = candidate.explicit_test;
        if entry.hidden_since.is_none() {
            entry.hidden_since = Some(now);
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::SessionMaintenanceSettings;
    use crate::session_model::{
        SessionIdentity, SessionSource, SessionSourceFilter, SessionSummary,
    };
    use chrono::{DateTime, Duration, Utc};
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    struct MaintenanceFixture {
        _temp: TempDir,
        source_file: PathBuf,
        roots: recycle::MaintenanceRoots,
        config_dir: PathBuf,
        now: DateTime<Utc>,
    }

    impl MaintenanceFixture {
        fn new(source: SessionSource, age_days: i64) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let claude = root.join("claude");
            let codex = root.join("codex");
            let omp = root.join("omp");
            let recycle = root.join("recycle");
            let config_dir = root.join("config");
            for dir in [&claude, &codex, &omp, &recycle, &config_dir] {
                fs::create_dir_all(dir).unwrap();
            }
            let source_root = match source {
                SessionSource::Claude => &claude,
                SessionSource::Codex => &codex,
                SessionSource::Omp => &omp,
            };
            let source_file = source_root.join("project").join("session.jsonl");
            fs::create_dir_all(source_file.parent().unwrap()).unwrap();
            fs::write(&source_file, b"session fixture").unwrap();
            Self {
                _temp: temp,
                source_file,
                roots: recycle::MaintenanceRoots {
                    claude,
                    codex,
                    omp,
                    recycle,
                },
                config_dir,
                now: DateTime::parse_from_rfc3339("2026-08-08T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            }
            .with_age(age_days)
        }

        fn with_age(self, age_days: i64) -> Self {
            let age = self.now - Duration::days(age_days);
            let content = format!(
                "{{\"timestamp\":\"{}\"}}\n{{\"timestamp\":\"{}\"}}\n",
                age.to_rfc3339(),
                age.to_rfc3339()
            );
            fs::write(&self.source_file, content).unwrap();
            self
        }

        fn old_test_candidate(source: SessionSource, age_days: i64) -> Self {
            Self::new(source, age_days)
        }

        fn hidden_for_days(source: SessionSource, days: i64) -> Self {
            let fixture = Self::new(source, 120);
            let identity = SessionIdentity {
                source,
                session_id: "cx-task6".to_string(),
            };
            let fingerprint = crate::session_cache::fingerprint_file(&fixture.source_file)
                .unwrap()
                .digest;
            let entry = state::MaintenanceEntry {
                identity: identity.clone(),
                original_relative_path: PathBuf::from("project/session.jsonl"),
                project_name: "project".to_string(),
                fingerprint,
                lifecycle: state::LifecycleState::Hidden,
                classifier_version: classifier::CLASSIFIER_VERSION,
                score: 100,
                reason_codes: vec![classifier::ReasonCode::ExplicitTestMarker],
                hidden_since: Some(fixture.now - Duration::days(days)),
                recycled_at: None,
                purged_at: None,
                keep: false,
                explicit_test: true,
            };
            state::StateStore::from_config_dir(&fixture.config_dir)
                .update(|saved| {
                    saved.entries.insert(state::identity_key(&identity), entry);
                    Ok(())
                })
                .unwrap();
            fixture
        }

        fn with_recyclable_sessions(count: usize) -> Self {
            let fixture = Self::new(SessionSource::Claude, 120);
            let store = state::StateStore::from_config_dir(&fixture.config_dir);
            store
                .update(|saved| {
                    for index in 0..count {
                        let relative = PathBuf::from(format!("project/session-{index}.jsonl"));
                        let path = fixture.roots.claude.join(&relative);
                        fs::write(&path, format!("fixture-{index}")).unwrap();
                        let fingerprint = crate::session_cache::fingerprint_file(&path)
                            .unwrap()
                            .digest;
                        let identity = SessionIdentity {
                            source: SessionSource::Claude,
                            session_id: format!("cc-task6-{index}"),
                        };
                        saved.entries.insert(
                            state::identity_key(&identity),
                            state::MaintenanceEntry {
                                identity,
                                original_relative_path: relative,
                                project_name: "project".to_string(),
                                fingerprint,
                                lifecycle: state::LifecycleState::Hidden,
                                classifier_version: classifier::CLASSIFIER_VERSION,
                                score: 100,
                                reason_codes: vec![classifier::ReasonCode::ExplicitTestMarker],
                                hidden_since: Some(fixture.now - Duration::days(8)),
                                recycled_at: None,
                                purged_at: None,
                                keep: false,
                                explicit_test: true,
                            },
                        );
                    }
                    Ok(())
                })
                .unwrap();
            fixture
        }

        fn summary(
            &self,
            source: SessionSource,
            session_id: &str,
            file_path: &Path,
        ) -> SessionSummary {
            let age = self.now - Duration::days(120);
            SessionSummary {
                source: source.as_str().to_string(),
                session_id: session_id.to_string(),
                title: "test".to_string(),
                project_name: "project".to_string(),
                project_dir: PathBuf::from("/tmp/task-6-project"),
                cwd: Some("/tmp/task-6-project".to_string()),
                file_path: file_path.to_path_buf(),
                message_count: 1,
                user_message_count: 1,
                assistant_message_count: 0,
                first_timestamp: Some((age - Duration::minutes(5)).to_rfc3339()),
                last_activity: Some(age.to_rfc3339()),
                file_size: fs::metadata(file_path).unwrap().len(),
                has_custom_title: false,
            }
        }

        fn run_with(
            &self,
            summaries: Vec<SessionSummary>,
            completed: HashSet<SessionSource>,
            settings: SessionMaintenanceSettings,
            mode: MaintenanceMode,
        ) -> MaintenanceReport {
            run_maintenance(
                MaintenanceInput {
                    summaries: Box::leak(summaries.into_boxed_slice()),
                    completed_sources: Box::leak(Box::new(completed)),
                    roots: &self.roots,
                    config_dir: &self.config_dir,
                    settings: Box::leak(Box::new(settings)),
                    clock: Box::leak(Box::new(FixedClock(self.now))),
                },
                mode,
            )
            .unwrap()
        }
    }

    struct FixedClock(DateTime<Utc>);

    impl MaintenanceClock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    fn all_complete() -> HashSet<SessionSource> {
        HashSet::from([
            SessionSource::Claude,
            SessionSource::Codex,
            SessionSource::Omp,
        ])
    }

    #[test]
    fn incomplete_source_never_advances_destructive_state() {
        let fixture = MaintenanceFixture::hidden_for_days(SessionSource::Codex, 8);
        let completed = HashSet::from([SessionSource::Claude, SessionSource::Omp]);
        let summary = fixture.summary(SessionSource::Codex, "cx-task6", &fixture.source_file);
        let report = fixture.run_with(
            vec![summary],
            completed,
            SessionMaintenanceSettings::default(),
            MaintenanceMode::Apply,
        );
        assert_eq!(report.recycled, 0);
        assert!(fixture.source_file.exists());
    }

    #[test]
    fn old_candidate_first_run_only_becomes_hidden() {
        let fixture = MaintenanceFixture::old_test_candidate(SessionSource::Claude, 120);
        let summary = fixture.summary(SessionSource::Claude, "cc-task6", &fixture.source_file);
        let report = fixture.run_with(
            vec![summary],
            all_complete(),
            SessionMaintenanceSettings::default(),
            MaintenanceMode::Apply,
        );
        assert_eq!(report.hidden, 1);
        assert_eq!(report.recycled, 0);
        assert!(fixture.source_file.exists());
    }

    #[test]
    fn action_budget_reports_remaining_work() {
        let fixture = MaintenanceFixture::with_recyclable_sessions(3);
        let settings = SessionMaintenanceSettings {
            max_actions_per_run: 2,
            ..SessionMaintenanceSettings::default()
        };
        let summaries = (0..3)
            .map(|index| {
                fixture.summary(
                    SessionSource::Claude,
                    &format!("cc-task6-{index}"),
                    &fixture
                        .roots
                        .claude
                        .join(format!("project/session-{index}.jsonl")),
                )
            })
            .collect();
        let report = fixture.run_with(summaries, all_complete(), settings, MaintenanceMode::Apply);
        assert_eq!(report.file_actions, 2);
        assert_eq!(report.remaining_actions, 1);
    }

    #[test]
    fn apply_prunes_only_old_codex_and_omp_purged_audits() {
        let fixture = MaintenanceFixture::new(SessionSource::Codex, 120);
        let store = state::StateStore::from_config_dir(&fixture.config_dir);
        let sources = [
            (
                SessionSource::Codex,
                "exact",
                Some(fixture.now - Duration::days(30)),
            ),
            (
                SessionSource::Omp,
                "just-before",
                Some(fixture.now - Duration::days(30) + Duration::seconds(1)),
            ),
            (
                SessionSource::Codex,
                "future",
                Some(fixture.now + Duration::days(1)),
            ),
            (SessionSource::Omp, "missing", None),
            (
                SessionSource::Claude,
                "claude",
                Some(fixture.now - Duration::days(365)),
            ),
        ];
        store
            .update(|saved| {
                for (source, session_id, purged_at) in sources {
                    let identity = SessionIdentity {
                        source,
                        session_id: session_id.to_string(),
                    };
                    saved.entries.insert(
                        state::identity_key(&identity),
                        state::MaintenanceEntry {
                            identity,
                            original_relative_path: PathBuf::from("project/session.jsonl"),
                            project_name: "project".to_string(),
                            fingerprint: "audit-only".to_string(),
                            lifecycle: state::LifecycleState::PurgedLocal,
                            classifier_version: classifier::CLASSIFIER_VERSION,
                            score: 100,
                            reason_codes: vec![classifier::ReasonCode::ExplicitTestMarker],
                            hidden_since: None,
                            recycled_at: None,
                            purged_at,
                            keep: false,
                            explicit_test: true,
                        },
                    );
                }
                Ok(())
            })
            .unwrap();

        fixture.run_with(
            Vec::new(),
            all_complete(),
            SessionMaintenanceSettings::default(),
            MaintenanceMode::Apply,
        );

        let state = store.load().unwrap();
        assert!(!state.entries.contains_key("codex:exact"));
        assert!(state.entries.contains_key("omp:just-before"));
        assert!(state.entries.contains_key("codex:future"));
        assert!(state.entries.contains_key("omp:missing"));
        assert!(state.entries.contains_key("claude:claude"));
    }

    #[test]
    fn pending_reconcile_waits_for_duplicate_batch_validation() {
        let fixture = MaintenanceFixture::hidden_for_days(SessionSource::Codex, 8);
        let identity = SessionIdentity {
            source: SessionSource::Codex,
            session_id: "cx-task6".to_string(),
        };
        let fingerprint = crate::session_cache::fingerprint_file(&fixture.source_file)
            .unwrap()
            .digest;
        state::StateStore::from_config_dir(&fixture.config_dir)
            .update(|saved| {
                let entry = saved.entries.get("codex:cx-task6").unwrap();
                saved.pending = Some(state::PendingOperation {
                    identity,
                    operation: state::PendingOperationKind::Recycle,
                    source_relative_path: PathBuf::from("project/session.jsonl"),
                    staging_relative_path: PathBuf::from(".staging/cx-task6"),
                    recycle_relative_path: recycle::recycle_relative_path(entry),
                    expected_fingerprint: fingerprint,
                });
                Ok(())
            })
            .unwrap();
        let summary = fixture.summary(SessionSource::Codex, "cx-task6", &fixture.source_file);
        let report = fixture.run_with(
            vec![summary.clone(), summary],
            HashSet::from([SessionSource::Codex]),
            SessionMaintenanceSettings::default(),
            MaintenanceMode::Apply,
        );
        assert!(report.warnings > 0);
        assert!(fixture.source_file.exists());
        assert!(state::StateStore::from_config_dir(&fixture.config_dir)
            .load()
            .unwrap()
            .pending
            .is_some());
    }

    #[test]
    fn maintenance_error_preserves_visibility_after_prior_file_action() {
        let fixture = MaintenanceFixture::with_recyclable_sessions(2);
        let store = state::StateStore::from_config_dir(&fixture.config_dir);
        let second = store
            .load()
            .unwrap()
            .entries
            .get("claude:cc-task6-1")
            .cloned()
            .unwrap();
        let conflict = fixture
            .roots
            .recycle
            .join(recycle::recycle_relative_path(&second));
        fs::create_dir_all(conflict.parent().unwrap()).unwrap();
        fs::write(&conflict, b"conflicting destination").unwrap();
        let summaries = (0..2)
            .map(|index| {
                fixture.summary(
                    SessionSource::Claude,
                    &format!("cc-task6-{index}"),
                    &fixture
                        .roots
                        .claude
                        .join(format!("project/session-{index}.jsonl")),
                )
            })
            .collect();
        let report = fixture.run_with(
            summaries,
            all_complete(),
            SessionMaintenanceSettings::default(),
            MaintenanceMode::Apply,
        );
        assert!(report.warnings > 0);
        let state = store.load().unwrap();
        assert_eq!(
            state.entries["claude:cc-task6-0"].lifecycle,
            LifecycleState::Recycled
        );
        assert_eq!(
            state.entries["claude:cc-task6-1"].lifecycle,
            LifecycleState::Hidden
        );
    }

    #[test]
    fn duplicate_source_identity_and_unknown_profile_are_fail_safe() {
        let duplicate = MaintenanceFixture::old_test_candidate(SessionSource::Claude, 120);
        let summary = duplicate.summary(SessionSource::Claude, "cc-task6", &duplicate.source_file);
        let report = duplicate.run_with(
            vec![summary.clone(), summary],
            all_complete(),
            SessionMaintenanceSettings::default(),
            MaintenanceMode::Apply,
        );
        assert_eq!(report.file_actions, 0);
        assert!(report.warnings > 0);

        let invalid = MaintenanceFixture::old_test_candidate(SessionSource::Claude, 120);
        let settings = SessionMaintenanceSettings {
            classifier: "unknown".to_string(),
            ..SessionMaintenanceSettings::default()
        };
        let report = invalid.run_with(
            vec![invalid.summary(SessionSource::Claude, "cc-task6", &invalid.source_file)],
            all_complete(),
            settings,
            MaintenanceMode::Apply,
        );
        assert_eq!(report.file_actions, 0);
        assert!(report.warnings > 0);
    }

    #[test]
    fn dry_run_does_not_write_state_or_recycle_files() {
        let fixture = MaintenanceFixture::old_test_candidate(SessionSource::Claude, 120);
        let state_path = fixture.config_dir.join("session-maintenance.json");
        let before = fs::read(&state_path).ok();
        let report = fixture.run_with(
            vec![fixture.summary(SessionSource::Claude, "cc-task6", &fixture.source_file)],
            all_complete(),
            SessionMaintenanceSettings::default(),
            MaintenanceMode::DryRun,
        );
        assert_eq!(report.hidden, 1);
        assert_eq!(fs::read(&state_path).ok(), before);
        assert!(fixture.source_file.exists());
    }

    #[test]
    fn disabled_mode_loads_existing_visibility_without_side_effects() {
        let fixture = MaintenanceFixture::new(SessionSource::Claude, 120);
        let store = state::StateStore::from_config_dir(&fixture.config_dir);
        let entries = [
            ("hidden", LifecycleState::Hidden),
            ("recycled", LifecycleState::Recycled),
            ("purged", LifecycleState::PurgedLocal),
        ];
        store
            .update(|saved| {
                for (session_id, lifecycle) in entries {
                    let identity = SessionIdentity {
                        source: SessionSource::Claude,
                        session_id: format!("cc-{session_id}"),
                    };
                    saved.entries.insert(
                        state::identity_key(&identity),
                        state::MaintenanceEntry {
                            identity,
                            original_relative_path: PathBuf::from("project/session.jsonl"),
                            project_name: "project".to_string(),
                            fingerprint: "fixture-fingerprint".to_string(),
                            lifecycle,
                            classifier_version: classifier::CLASSIFIER_VERSION,
                            score: 100,
                            reason_codes: vec![classifier::ReasonCode::ExplicitTestMarker],
                            hidden_since: Some(fixture.now - Duration::days(8)),
                            recycled_at: (lifecycle == LifecycleState::Recycled)
                                .then_some(fixture.now - Duration::days(1)),
                            purged_at: (lifecycle == LifecycleState::PurgedLocal)
                                .then_some(fixture.now - Duration::days(1)),
                            keep: false,
                            explicit_test: true,
                        },
                    );
                }
                Ok(())
            })
            .unwrap();
        let state_path = fixture.config_dir.join("session-maintenance.json");
        let before = fs::read(&state_path).unwrap();

        let report = fixture.run_with(
            Vec::new(),
            HashSet::new(),
            SessionMaintenanceSettings::default(),
            MaintenanceMode::Disabled,
        );

        assert_eq!(report.candidates, 0);
        assert_eq!(report.hidden, 0);
        assert_eq!(report.recycled, 0);
        assert_eq!(report.purged, 0);
        assert_eq!(report.file_actions, 0);
        assert_eq!(report.visibility.states.len(), 3);
        assert_eq!(
            report.visibility.states[&SessionIdentity {
                source: SessionSource::Claude,
                session_id: "cc-hidden".to_string(),
            }],
            LifecycleState::Hidden
        );
        assert_eq!(
            report.visibility.states[&SessionIdentity {
                source: SessionSource::Claude,
                session_id: "cc-recycled".to_string(),
            }],
            LifecycleState::Recycled
        );
        assert_eq!(
            report.visibility.states[&SessionIdentity {
                source: SessionSource::Claude,
                session_id: "cc-purged".to_string(),
            }],
            LifecycleState::PurgedLocal
        );
        assert_eq!(fs::read(&state_path).unwrap(), before);
        assert!(fixture.source_file.exists());
    }

    #[test]
    fn disabled_mode_rejects_invalid_state() {
        let fixture = MaintenanceFixture::old_test_candidate(SessionSource::Claude, 120);
        fs::write(
            fixture.config_dir.join("session-maintenance.json"),
            b"not valid maintenance state",
        )
        .unwrap();
        let error = run_maintenance(
            MaintenanceInput {
                summaries: &[],
                completed_sources: &HashSet::new(),
                roots: &fixture.roots,
                config_dir: &fixture.config_dir,
                settings: &SessionMaintenanceSettings::default(),
                clock: &FixedClock(fixture.now),
            },
            MaintenanceMode::Disabled,
        )
        .unwrap_err();
        assert!(error.to_string().contains("load session maintenance state"));
        assert!(fixture.source_file.exists());
    }

    #[test]
    fn invalid_state_is_fail_safe() {
        let fixture = MaintenanceFixture::old_test_candidate(SessionSource::Claude, 120);
        fs::write(
            fixture.config_dir.join("session-maintenance.json"),
            b"{\"version\":999,\"entries\":{},\"pending\":null}",
        )
        .unwrap();
        let error = run_maintenance(
            MaintenanceInput {
                summaries: Box::leak(
                    vec![fixture.summary(SessionSource::Claude, "cc-task6", &fixture.source_file)]
                        .into_boxed_slice(),
                ),
                completed_sources: Box::leak(Box::new(all_complete())),
                roots: &fixture.roots,
                config_dir: &fixture.config_dir,
                settings: Box::leak(Box::new(SessionMaintenanceSettings::default())),
                clock: Box::leak(Box::new(FixedClock(fixture.now))),
            },
            MaintenanceMode::Apply,
        )
        .unwrap_err();
        assert!(error.to_string().contains("load session maintenance state"));
        assert!(fixture.source_file.exists());
    }

    #[test]
    fn load_recycled_summaries_skips_semantically_invalid_entries_for_all_sources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let roots = recycle::MaintenanceRoots {
            claude: root.join("claude"),
            codex: root.join("codex"),
            omp: root.join("omp"),
            recycle: root.join("recycle"),
        };
        let config = root.join("config");
        for path in [
            &roots.claude,
            &roots.codex,
            &roots.omp,
            &roots.recycle,
            &config,
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let store = state::StateStore::from_config_dir(&config);
        let fixtures = [
            (
                SessionSource::Claude,
                "cc-invalid",
                PathBuf::from("cc-invalid.jsonl"),
                r#"{"type":"user","sessionId":"cc-invalid","cwd":"/tmp/project","timestamp":"2026-08-08T12:00:00Z","message":{"role":"user","content":[]}}
"#,
            ),
            (
                SessionSource::Codex,
                "cx-invalid",
                PathBuf::from("cx-invalid.jsonl"),
                r#"{"type":"session_meta","payload":{"id":"cx-invalid","cwd":"/tmp/project"},"timestamp":"2026-08-08T12:00:00Z"}
"#,
            ),
            (
                SessionSource::Omp,
                "om-invalid",
                PathBuf::from("om-invalid.jsonl"),
                r#"{"type":"session","id":"om-invalid","cwd":"/tmp/project","timestamp":"2026-08-08T12:00:00Z"}
"#,
            ),
        ];
        for (source, session_id, relative, content) in fixtures {
            let source_file = roots.source_root(source).join(&relative);
            fs::write(&source_file, content).unwrap();
            let fingerprint = fingerprint_file(&source_file).unwrap().digest;
            let identity = SessionIdentity {
                source,
                session_id: session_id.to_string(),
            };
            let entry = state::MaintenanceEntry {
                identity: identity.clone(),
                original_relative_path: relative,
                project_name: "project".to_string(),
                fingerprint,
                lifecycle: LifecycleState::Recycled,
                classifier_version: classifier::CLASSIFIER_VERSION,
                score: 100,
                reason_codes: vec![],
                hidden_since: None,
                recycled_at: Some(Utc::now()),
                purged_at: None,
                keep: false,
                explicit_test: true,
            };
            let final_path = roots.recycle.join(recycle::recycle_relative_path(&entry));
            fs::create_dir_all(final_path.parent().unwrap()).unwrap();
            fs::rename(source_file, final_path).unwrap();
            store
                .update(|state| {
                    state.entries.insert(identity_key(&identity), entry);
                    Ok(())
                })
                .unwrap();
        }

        let state = store.load().unwrap();
        let summaries = load_recycled_summaries(&roots, &state, SessionSourceFilter::All).unwrap();
        assert!(summaries.is_empty());
    }

    #[test]
    fn load_recycled_summaries_parses_all_sources_from_trusted_final_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let roots = recycle::MaintenanceRoots {
            claude: root.join("claude"),
            codex: root.join("codex"),
            omp: root.join("omp"),
            recycle: root.join("recycle"),
        };
        for path in [&roots.claude, &roots.codex, &roots.omp, &roots.recycle] {
            fs::create_dir_all(path).unwrap();
        }

        let fixtures = [
            (
                SessionSource::Claude,
                "cc-recycled",
                PathBuf::from("claude-project/cc-recycled.jsonl"),
                r#"{"type":"user","sessionId":"cc-recycled","cwd":"/workspace/claude-project","timestamp":"2026-08-08T12:00:00Z","message":{"role":"user","content":"claude recycled keyword"}}
"#,
            ),
            (
                SessionSource::Codex,
                "cx-recycled",
                PathBuf::from("codex-project/cx-recycled.jsonl"),
                r#"{"type":"session_meta","payload":{"id":"cx-recycled","cwd":"/workspace/codex-project"},"timestamp":"2026-08-08T12:00:00Z"}
{"type":"response_item","payload":{"role":"user","content":[{"text":"codex recycled keyword"}]},"timestamp":"2026-08-08T12:00:01Z"}
"#,
            ),
            (
                SessionSource::Omp,
                "om-recycled",
                PathBuf::from("omp-project/om-recycled.jsonl"),
                r#"{"type":"session","id":"om-recycled","cwd":"/workspace/omp-project","timestamp":"2026-08-08T12:00:00Z"}
{"type":"message","timestamp":"2026-08-08T12:00:01Z","message":{"role":"user","content":[{"type":"text","text":"omp recycled keyword"}]}}
"#,
            ),
        ];
        let store = state::StateStore::from_config_dir(&root.join("config"));
        fs::create_dir_all(root.join("config")).unwrap();
        for (source, session_id, relative, content) in fixtures {
            let staging_source = roots.source_root(source).join(&relative);
            fs::create_dir_all(staging_source.parent().unwrap()).unwrap();
            fs::write(&staging_source, content).unwrap();
            let fingerprint = crate::session_cache::fingerprint_file(&staging_source)
                .unwrap()
                .digest;
            let identity = SessionIdentity {
                source,
                session_id: session_id.to_string(),
            };
            let entry = state::MaintenanceEntry {
                identity,
                original_relative_path: relative,
                project_name: format!("{}-project", source.as_str()),
                fingerprint,
                lifecycle: LifecycleState::Recycled,
                classifier_version: classifier::CLASSIFIER_VERSION,
                score: 100,
                reason_codes: vec![],
                hidden_since: None,
                recycled_at: Some(Utc::now()),
                purged_at: None,
                keep: false,
                explicit_test: true,
            };
            let final_path = roots.recycle.join(recycle::recycle_relative_path(&entry));
            fs::create_dir_all(final_path.parent().unwrap()).unwrap();
            fs::rename(staging_source, final_path).unwrap();
            store
                .update(|state| {
                    state.entries.insert(identity_key(&entry.identity), entry);
                    Ok(())
                })
                .unwrap();
        }

        let state = store.load().unwrap();
        let summaries = load_recycled_summaries(&roots, &state, SessionSourceFilter::All).unwrap();
        assert_eq!(summaries.len(), 3);
        assert_eq!(
            summaries
                .iter()
                .map(|s| s.source.as_str())
                .collect::<Vec<_>>(),
            vec!["claude", "codex", "omp"]
        );
        assert!(summaries
            .iter()
            .all(|summary| summary.file_path.starts_with(&roots.recycle)));
        assert!(summaries
            .iter()
            .any(|summary| summary.title.contains("recycled keyword")));
    }
}
