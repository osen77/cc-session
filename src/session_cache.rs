//! Session index cache — avoids re-parsing JSONL files on every `ccs session list` run.
//!
//! The cache is stored at `{config_dir}/session_index.json` and keyed by canonical
//! file path. Scanner lookups are valid only when `file_size`, `mtime_secs`, and
//! `content_fingerprint` all match the measured file state. The legacy `lookup` and
//! `insert` wrappers intentionally preserve their old size+mtime-only, no-I/O behavior;
//! scanner code should use the fingerprint-aware APIs instead.

use crate::atomic_file::{persist_json_atomic, FileLock};
use crate::path_security::canonical_utf8_key;
use crate::session_diagnostics::{error_kind_from_error, ChangedDuringRead, ScanWarningErrorKind};
use crate::session_model::SessionSummary;
use anyhow::{anyhow, Context, Result};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const CACHE_VERSION: u32 = 5;
const KNOWN_CACHE_SOURCES: [&str; 3] = ["claude", "codex", "omp"];
const LEGACY_CONTENT_FINGERPRINT: &str = "";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    pub digest: String,
    pub bytes: u64,
}

/// Stream a file through BLAKE3 without loading its complete contents in memory.
pub fn fingerprint_file(path: &Path) -> Result<FileFingerprint> {
    #[cfg(test)]
    if TEST_FINGERPRINT_ERROR_PATH.with(|value| value.borrow().as_deref() == Some(path)) {
        return Err(anyhow!(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "test fingerprint read failed",
        )));
    }

    let mut file = File::open(path).with_context(|| {
        format!(
            "failed to open session file for fingerprint: {}",
            path.display()
        )
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes = 0_u64;
    loop {
        let read = file.read(&mut buffer).with_context(|| {
            format!(
                "failed to read session file for fingerprint: {}",
                path.display()
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.saturating_add(read as u64);
    }
    Ok(FileFingerprint {
        digest: hasher.finalize().to_hex().to_string(),
        bytes,
    })
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionIndexCache {
    pub version: u32,
    /// key = canonical file path as UTF-8 string; non-UTF-8 paths are not cached
    pub entries: HashMap<String, CachedEntry>,
}

/// Result of loading the advisory session index cache.
#[derive(Debug)]
pub struct CacheLoadStatus {
    /// The loaded cache, or an empty cache when loading failed.
    pub cache: SessionIndexCache,
    /// A diagnostic warning for non-missing load failures.
    pub warning: Option<String>,
    /// Whether the warning describes a routine rebuild rather than a defect.
    ///
    /// A version mismatch is the expected path after an upgrade that changed
    /// [`CACHE_VERSION`]: the index is rebuilt in full from the sources and nothing
    /// is lost, so it must not be reported as a scan degradation. Invalid or
    /// unreadable cache files stay defects — they can mean corruption or a
    /// permission problem the user needs to know about.
    pub routine_rebuild: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedEntry {
    pub file_size: u64,
    pub mtime_secs: i64,
    #[serde(default)]
    pub content_fingerprint: String,
    pub source: String,
    pub session_id: String,
    pub title: String,
    pub project_name: String,
    pub project_dir: String,
    #[serde(default)]
    pub cwd: Option<String>,
    pub message_count: usize,
    pub user_message_count: usize,
    pub assistant_message_count: usize,
    pub first_timestamp: Option<String>,
    pub last_activity: Option<String>,
    #[serde(default)]
    pub has_custom_title: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheFileState {
    pub file_size: u64,
    pub mtime_secs: i64,
    pub content_fingerprint: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheUpsert {
    pub key: String,
    pub expected: CacheFileState,
    pub entry: CachedEntry,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheRemoval {
    pub key: String,
    pub expected: CacheFileState,
}

#[allow(dead_code)]
#[derive(Debug, Default)]
pub(crate) struct CacheDelta {
    pub upserts: Vec<CacheUpsert>,
    pub removals: Vec<CacheRemoval>,
}

#[derive(Debug, Default)]
pub(crate) struct CacheMergeReport {
    pub revalidation_issues: Vec<CacheRevalidationIssue>,
}

#[derive(Debug)]
pub(crate) struct CacheRevalidationIssue {
    pub key: String,
    pub error_kind: ScanWarningErrorKind,
    pub detail: String,
}

#[allow(dead_code)]
#[derive(Debug, Default)]
pub(crate) struct CacheRetention {
    pub seen_by_source: HashMap<String, HashSet<String>>,
    pub completed_sources: HashSet<String>,
}

enum CacheLoadKind {
    Missing,
    Loaded(SessionIndexCache),
    Invalid,
    VersionMismatch(SessionIndexCache),
    ReadFailed,
}

/// Migrate known-source legacy raw keys to canonical identities without
/// discarding entries that cannot be proven safe to migrate.
fn canonicalize_known_source_entries(cache: &mut SessionIndexCache) {
    let mut retained = HashMap::new();
    let mut candidates = Vec::new();

    for (key, entry) in std::mem::take(&mut cache.entries) {
        if !KNOWN_CACHE_SOURCES.contains(&entry.source.as_str()) {
            retained.insert(key, entry);
            continue;
        }

        let path = Path::new(&key);
        let existing_regular = matches!(
            std::fs::symlink_metadata(path),
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file()
        );
        let Some(canonical) = existing_regular.then(|| canonical_utf8_key(path)).flatten() else {
            // Fail-safe: preserve known entries when the old path cannot be
            // canonicalized or is no longer a regular non-symlink file.
            retained.insert(key, entry);
            continue;
        };
        let priority = usize::from(key != canonical);
        candidates.push((entry.source.clone(), canonical, priority, key, entry));
    }

    candidates.sort_by(|left, right| {
        (&left.0, &left.1, left.2, &left.3).cmp(&(&right.0, &right.1, right.2, &right.3))
    });
    let mut selected = HashSet::new();
    for (source, canonical, _priority, raw_key, entry) in candidates {
        if !selected.insert((source.clone(), canonical.clone())) {
            continue;
        }
        if retained.contains_key(&canonical) {
            // A future/unknown entry may already use this string key. Keep
            // the migrated entry under its raw key rather than overwriting it.
            retained.insert(raw_key, entry);
        } else {
            retained.insert(canonical, entry);
        }
    }
    cache.entries = retained;
}

impl CachedEntry {
    #[allow(dead_code)]
    pub(crate) fn file_state(&self) -> CacheFileState {
        CacheFileState {
            file_size: self.file_size,
            mtime_secs: self.mtime_secs,
            content_fingerprint: self.content_fingerprint.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl SessionIndexCache {
    /// Create an empty cache with the current version.
    fn empty() -> Self {
        SessionIndexCache {
            version: CACHE_VERSION,
            entries: HashMap::new(),
        }
    }

    /// Load the cache from `{config_dir}/session_index.json` with diagnostics.
    ///
    /// A missing cache is the normal cold-start case and has no warning. Other
    /// read, parse, and version errors return an empty cache with a warning;
    /// see [`CacheLoadStatus::routine_rebuild`] for which of those are defects.
    pub fn load_with_status(config_dir: &Path) -> CacheLoadStatus {
        match load_unlocked_with_kind(config_dir) {
            CacheLoadKind::Missing => {
                debug!("Session cache not found");
                CacheLoadStatus {
                    cache: Self::empty(),
                    warning: None,
                    routine_rebuild: false,
                }
            }
            CacheLoadKind::Loaded(mut cache) => {
                canonicalize_known_source_entries(&mut cache);
                debug!("Loaded session cache with {} entries", cache.entries.len());
                CacheLoadStatus {
                    cache,
                    warning: None,
                    routine_rebuild: false,
                }
            }
            CacheLoadKind::Invalid => CacheLoadStatus {
                cache: Self::empty(),
                warning: Some("cache data invalid".to_string()),
                routine_rebuild: false,
            },
            CacheLoadKind::VersionMismatch(_) => CacheLoadStatus {
                cache: Self::empty(),
                warning: Some("cache version mismatch".to_string()),
                routine_rebuild: true,
            },
            CacheLoadKind::ReadFailed => CacheLoadStatus {
                cache: Self::empty(),
                warning: Some("cache read failed".to_string()),
                routine_rebuild: false,
            },
        }
    }

    /// Load the cache from `{config_dir}/session_index.json`.
    ///
    /// Returns an empty cache on any error (missing file, parse failure,
    /// version mismatch). Never panics.
    ///
    /// Kept as a compatibility wrapper for downstream callers; the scanner uses
    /// `load_with_status` so it can surface degraded scans.
    #[allow(dead_code)]
    pub fn load(config_dir: &Path) -> Self {
        Self::load_with_status(config_dir).cache
    }

    /// Save the cache to `{config_dir}/session_index.json`, returning errors.
    ///
    /// Creates `config_dir` if it does not exist. The cache format and retention
    /// behavior are unchanged; this method only makes failures observable.
    pub fn save_with_result(&self, config_dir: &Path) -> Result<()> {
        let lock_path = cache_lock_path(config_dir);
        let _lock = FileLock::acquire(&lock_path).with_context(|| {
            format!(
                "Failed to acquire session cache lock {}",
                lock_path.display()
            )
        })?;
        let target = cache_path(config_dir);
        persist_json_atomic(&target, self).with_context(|| {
            format!(
                "Failed to atomically replace session cache {}",
                target.display()
            )
        })?;

        debug!("Saved session cache ({} entries)", self.entries.len());
        Ok(())
    }

    /// Save the cache to `{config_dir}/session_index.json`.
    ///
    /// Creates `config_dir` if it does not exist. Logs warnings on error
    /// but does not propagate them — the cache is advisory.
    ///
    /// Kept as a compatibility wrapper for downstream callers; the scanner uses
    /// `save_with_result` so it can surface degraded scans.
    #[allow(dead_code)]
    pub fn save(&self, config_dir: &Path) {
        if self.save_with_result(config_dir).is_err() {
            warn!("Session cache save failed; continuing without cache");
        }
    }

    /// Return a cached summary after checking only the legacy size and mtime metadata.
    ///
    /// This compatibility wrapper intentionally performs no file I/O and does not check
    /// `content_fingerprint`. Scanner code should use [`Self::lookup_with_fingerprint`].
    #[allow(dead_code)]
    pub fn lookup(
        &self,
        key: &str,
        file_path: &Path,
        file_size: u64,
        mtime_secs: i64,
    ) -> Option<SessionSummary> {
        let entry = self.entries.get(key)?;
        if entry.file_size != file_size || entry.mtime_secs != mtime_secs {
            return None;
        }

        Some(SessionSummary {
            source: entry.source.clone(),
            session_id: entry.session_id.clone(),
            title: entry.title.clone(),
            project_name: entry.project_name.clone(),
            project_dir: PathBuf::from(&entry.project_dir),
            cwd: entry.cwd.clone(),
            file_path: file_path.to_path_buf(),
            message_count: entry.message_count,
            user_message_count: entry.user_message_count,
            assistant_message_count: entry.assistant_message_count,
            first_timestamp: entry.first_timestamp.clone(),
            last_activity: entry.last_activity.clone(),
            file_size,
            has_custom_title: entry.has_custom_title,
        })
    }

    pub fn lookup_with_fingerprint(
        &self,
        key: &str,
        file_path: &Path,
        file_size: u64,
        mtime_secs: i64,
        content_fingerprint: &str,
    ) -> Option<SessionSummary> {
        let entry = self.entries.get(key)?;

        if entry.file_size != file_size
            || entry.mtime_secs != mtime_secs
            || entry.content_fingerprint != content_fingerprint
        {
            return None;
        }

        Some(SessionSummary {
            source: entry.source.clone(),
            session_id: entry.session_id.clone(),
            title: entry.title.clone(),
            project_name: entry.project_name.clone(),
            project_dir: PathBuf::from(&entry.project_dir),
            cwd: entry.cwd.clone(),
            file_path: file_path.to_path_buf(),
            message_count: entry.message_count,
            user_message_count: entry.user_message_count,
            assistant_message_count: entry.assistant_message_count,
            first_timestamp: entry.first_timestamp.clone(),
            last_activity: entry.last_activity.clone(),
            file_size,
            has_custom_title: entry.has_custom_title,
        })
    }

    /// Insert or update a cache entry using the legacy size+mtime-only contract.
    ///
    /// This compatibility wrapper performs no file I/O and stores an empty fingerprint
    /// sentinel. Scanner code should use [`Self::insert_with_fingerprint`].
    #[allow(dead_code)]
    pub fn insert(
        &mut self,
        key: String,
        file_size: u64,
        mtime_secs: i64,
        summary: &SessionSummary,
    ) {
        self.insert_with_fingerprint(
            key,
            file_size,
            mtime_secs,
            LEGACY_CONTENT_FINGERPRINT.to_string(),
            summary,
        );
    }

    pub fn insert_with_fingerprint(
        &mut self,
        key: String,
        file_size: u64,
        mtime_secs: i64,
        content_fingerprint: String,
        summary: &SessionSummary,
    ) {
        self.entries.insert(
            key,
            CachedEntry {
                file_size,
                mtime_secs,
                content_fingerprint,
                source: summary.source.clone(),
                session_id: summary.session_id.clone(),
                title: summary.title.clone(),
                project_name: summary.project_name.clone(),
                project_dir: summary.project_dir.to_string_lossy().to_string(),
                cwd: summary.cwd.clone(),
                message_count: summary.message_count,
                user_message_count: summary.user_message_count,
                assistant_message_count: summary.assistant_message_count,
                first_timestamp: summary.first_timestamp.clone(),
                last_activity: summary.last_activity.clone(),
                has_custom_title: summary.has_custom_title,
            },
        );
    }

    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }

    /// Remove entries missing from a completed source scan when deletion is confirmed.
    #[allow(dead_code)]
    pub(crate) fn retain_existing_by_source(
        &mut self,
        retention: &CacheRetention,
        confirmed_missing: &HashSet<String>,
    ) {
        let before = self.entries.len();
        self.entries.retain(|key, entry| {
            if !KNOWN_CACHE_SOURCES.contains(&entry.source.as_str()) {
                return true;
            }
            let Some(seen) = retention.seen_by_source.get(&entry.source) else {
                return true;
            };
            let should_remove = retention.completed_sources.contains(&entry.source)
                && !seen.contains(key)
                && confirmed_missing.contains(key);
            !should_remove
        });
        let removed = before - self.entries.len();
        if removed > 0 {
            debug!("Pruned {removed} stale entries from completed source scans");
        }
    }

    /// Remove all entries whose keys are **not** present in `seen_paths`.
    ///
    /// Legacy whole-cache prune. New scanner code should use
    /// [`Self::retain_existing_by_source`] to avoid pruning unselected sources.
    #[allow(dead_code)] // Deprecated compatibility API; active scanner uses source-aware merge.
    pub fn retain_existing(&mut self, seen_paths: &HashSet<String>) {
        let before = self.entries.len();
        self.entries.retain(|k, _| seen_paths.contains(k));
        let removed = before - self.entries.len();
        if removed > 0 {
            debug!("Pruned {removed} stale entries from session cache");
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn load_unlocked_with_kind(config_dir: &Path) -> CacheLoadKind {
    let path = cache_path(config_dir);
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return CacheLoadKind::Missing;
        }
        Err(_) => return CacheLoadKind::ReadFailed,
    };

    let cache: SessionIndexCache = match serde_json::from_slice(&data) {
        Ok(cache) => cache,
        Err(_) => return CacheLoadKind::Invalid,
    };
    if cache.version != CACHE_VERSION {
        CacheLoadKind::VersionMismatch(cache)
    } else {
        CacheLoadKind::Loaded(cache)
    }
}

#[cfg(test)]
thread_local! {
    static TEST_FINGERPRINT_ERROR_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static TEST_REVALIDATION_ERROR_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_test_fingerprint_error_path(path: Option<PathBuf>) {
    TEST_FINGERPRINT_ERROR_PATH.with(|value| *value.borrow_mut() = path);
}

#[cfg(test)]
pub(crate) fn set_test_revalidation_error_path(path: Option<PathBuf>) {
    TEST_REVALIDATION_ERROR_PATH.with(|value| *value.borrow_mut() = path);
}

#[allow(dead_code)]
fn current_file_state(path: &Path) -> Result<Option<CacheFileState>> {
    #[cfg(test)]
    if TEST_REVALIDATION_ERROR_PATH.with(|value| value.borrow().as_deref() == Some(path)) {
        return Err(anyhow!(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "test revalidation read failed",
        )));
    }
    current_file_state_with_hook(path, |_| {})
}

#[allow(dead_code)]
fn current_file_state_with_hook<F>(
    path: &Path,
    after_fingerprint: F,
) -> Result<Option<CacheFileState>>
where
    F: FnOnce(&Path),
{
    let before = match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => metadata,
        Ok(_) => {
            return Err(anyhow!(
                "session cache object is not a trusted regular file"
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow!(error)),
    };
    let before_mtime =
        mtime_secs(&before).ok_or_else(|| anyhow!("session file modification time unavailable"))?;
    let first_fingerprint = fingerprint_file(path)?;
    after_fingerprint(path);

    let after = match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => metadata,
        Ok(_) => {
            return Err(anyhow!(
                "session cache object is not a trusted regular file"
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow!(error)),
    };
    let after_mtime =
        mtime_secs(&after).ok_or_else(|| anyhow!("session file modification time unavailable"))?;
    if before.len() != after.len() || before_mtime != after_mtime {
        return Err(anyhow!(ChangedDuringRead));
    }

    let second_fingerprint = fingerprint_file(path)?;
    if first_fingerprint != second_fingerprint || second_fingerprint.bytes != after.len() {
        return Err(anyhow!(ChangedDuringRead));
    }
    Ok(Some(CacheFileState {
        file_size: after.len(),
        mtime_secs: after_mtime,
        content_fingerprint: second_fingerprint.digest,
    }))
}

#[allow(dead_code)]
fn all_known_sources_completed(retention: &CacheRetention) -> bool {
    KNOWN_CACHE_SOURCES
        .iter()
        .all(|source| retention.completed_sources.contains(*source))
}

#[allow(dead_code)]
fn validate_retention(retention: &CacheRetention) -> Result<()> {
    for source in &retention.completed_sources {
        if !retention.seen_by_source.contains_key(source) {
            return Err(anyhow!(
                "session cache merge skipped: completed source missing seen set"
            ));
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn merge_scan_with_result(
    config_dir: &Path,
    delta: &CacheDelta,
    retention: &CacheRetention,
) -> Result<()> {
    merge_scan_with_report(config_dir, delta, retention).map(|_| ())
}

#[allow(dead_code)]
pub(crate) fn merge_scan_with_report(
    config_dir: &Path,
    delta: &CacheDelta,
    retention: &CacheRetention,
) -> Result<CacheMergeReport> {
    let _lock = FileLock::acquire(&cache_lock_path(config_dir))?;
    validate_retention(retention)?;
    let mut report = CacheMergeReport::default();
    let mut cache = match load_unlocked_with_kind(config_dir) {
        CacheLoadKind::Missing => SessionIndexCache::empty(),
        CacheLoadKind::Loaded(cache) => cache,
        CacheLoadKind::Invalid => {
            if all_known_sources_completed(retention) {
                SessionIndexCache::empty()
            } else {
                return Err(anyhow!(
                    "session cache merge skipped: invalid cache requires complete source scans"
                ));
            }
        }
        CacheLoadKind::VersionMismatch(mut cache) => {
            if all_known_sources_completed(retention) {
                cache.version = CACHE_VERSION;
                cache
            } else {
                return Err(anyhow!(
                    "session cache merge skipped: version mismatch requires complete source scans"
                ));
            }
        }
        CacheLoadKind::ReadFailed => {
            return Err(anyhow!("session cache merge skipped: cache read failed"));
        }
    };
    canonicalize_known_source_entries(&mut cache);

    for upsert in &delta.upserts {
        let path = Path::new(&upsert.key);
        match current_file_state(path) {
            Ok(Some(state)) if state == upsert.expected => {
                cache
                    .entries
                    .insert(upsert.key.clone(), upsert.entry.clone());
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(error) => report_revalidation_issue(&mut report, &upsert.key, error),
        }
    }

    for removal in &delta.removals {
        let path = Path::new(&removal.key);
        match current_file_state(path) {
            Ok(None) => cache.remove(&removal.key),
            Ok(Some(state)) if state == removal.expected => cache.remove(&removal.key),
            Ok(Some(_)) => {}
            Err(error) => report_revalidation_issue(&mut report, &removal.key, error),
        }
    }

    let stale_keys: Vec<String> = cache
        .entries
        .iter()
        .filter_map(|(key, entry)| {
            let seen = retention.seen_by_source.get(&entry.source)?;
            if retention.completed_sources.contains(&entry.source) && !seen.contains(key) {
                Some(key.clone())
            } else {
                None
            }
        })
        .collect();
    let mut confirmed_missing = HashSet::new();
    for key in stale_keys {
        match current_file_state(Path::new(&key)) {
            Ok(None) => {
                confirmed_missing.insert(key);
            }
            Ok(Some(_)) => {}
            Err(error) => report_revalidation_issue(&mut report, &key, error),
        }
    }
    cache.retain_existing_by_source(retention, &confirmed_missing);

    persist_json_atomic(&cache_path(config_dir), &cache)?;
    Ok(report)
}

fn report_revalidation_issue(report: &mut CacheMergeReport, key: &str, error: anyhow::Error) {
    report.revalidation_issues.push(CacheRevalidationIssue {
        key: key.to_string(),
        error_kind: error_kind_from_error(&error),
        detail: format!("{error:#}"),
    });
}

/// Extract the last-modified time from file metadata as seconds since UNIX epoch.
///
/// Kept for compatibility with callers that used the pre-diagnostics scanner.
#[allow(dead_code)]
pub fn mtime_secs(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

fn cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join("session_index.json")
}

fn cache_lock_path(config_dir: &Path) -> PathBuf {
    config_dir.join("session_index.json.lock")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_summary(file_path: &Path, project_dir: &Path) -> SessionSummary {
        SessionSummary {
            source: "claude".to_string(),
            session_id: "test-session-id".to_string(),
            title: "Test session title".to_string(),
            project_name: "my-project".to_string(),
            project_dir: project_dir.to_path_buf(),
            cwd: Some("/Users/example/my-project".to_string()),
            file_path: file_path.to_path_buf(),
            message_count: 10,
            user_message_count: 5,
            assistant_message_count: 5,
            first_timestamp: Some("2024-01-01T00:00:00Z".to_string()),
            last_activity: Some("2024-01-02T00:00:00Z".to_string()),
            file_size: 1234,
            has_custom_title: true,
        }
    }

    fn path_key(p: &Path) -> String {
        canonical_utf8_key(p).unwrap_or_else(|| p.to_string_lossy().to_string())
    }

    #[test]
    fn test_cold_cache_returns_none() {
        let dir = TempDir::new().unwrap();
        let cache = SessionIndexCache::load(dir.path());
        let fake_path = dir.path().join("fake.jsonl");
        assert!(cache
            .lookup(&path_key(&fake_path), &fake_path, 100, 999)
            .is_none());
    }

    #[test]
    fn test_insert_and_lookup() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("session.jsonl");
        std::fs::write(&file_path, b"session content").unwrap();
        let project_dir = dir.path().join("project");
        let key = path_key(&file_path);

        let summary = make_summary(&file_path, &project_dir);
        let file_size = 1234_u64;
        let mtime = 1700000000_i64;

        let mut cache = SessionIndexCache::empty();
        cache.insert(key.clone(), file_size, mtime, &summary);

        // Matching size + mtime → Some
        let result = cache.lookup(&key, &file_path, file_size, mtime);
        assert!(result.is_some());
        let s = result.unwrap();
        assert_eq!(s.session_id, "test-session-id");
        assert_eq!(s.file_size, file_size);
        assert!(s.has_custom_title);

        // Different size → None
        assert!(cache
            .lookup(&key, &file_path, file_size + 1, mtime)
            .is_none());

        // Different mtime → None
        assert!(cache
            .lookup(&key, &file_path, file_size, mtime + 1)
            .is_none());
    }

    #[test]
    fn legacy_lookup_and_insert_support_synthetic_key_without_io() {
        let dir = TempDir::new().unwrap();
        let synthetic_path = dir.path().join("missing-session.jsonl");
        let project_dir = dir.path().join("project");
        let key = path_key(&synthetic_path);
        let summary = make_summary(&synthetic_path, &project_dir);
        let mut cache = SessionIndexCache::empty();

        cache.insert(key.clone(), 1234, 1700000000, &summary);

        let result = cache.lookup(&key, &synthetic_path, 1234, 1700000000);
        assert_eq!(
            result.map(|session| session.session_id),
            Some("test-session-id".to_string())
        );
        assert_eq!(cache.entries.get(&key).unwrap().content_fingerprint, "");
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let config_dir = dir.path().join("config");
        let file_path = dir.path().join("session.jsonl");
        std::fs::write(&file_path, b"session content").unwrap();
        let project_dir = dir.path().join("project");
        let key = path_key(&file_path);

        let summary = make_summary(&file_path, &project_dir);
        let file_size = 4096_u64;
        let mtime = 1700000001_i64;

        let mut cache = SessionIndexCache::empty();
        cache.insert(key.clone(), file_size, mtime, &summary);
        cache.save(&config_dir);

        let loaded = SessionIndexCache::load(&config_dir);
        assert_eq!(loaded.version, CACHE_VERSION);
        assert!(!loaded.entries.is_empty());
        assert_eq!(loaded.entries.len(), 1);
        let cached = loaded.entries.values().next().unwrap();
        assert_eq!(cached.source, "claude");
        assert!(cached.has_custom_title);

        let result = loaded.lookup(&key, &file_path, file_size, mtime);
        assert!(result.is_some());
        let s = result.unwrap();
        assert_eq!(s.session_id, "test-session-id");
        assert_eq!(s.project_name, "my-project");
        assert_eq!(s.message_count, 10);
        assert_eq!(s.first_timestamp, Some("2024-01-01T00:00:00Z".to_string()));
    }

    #[test]
    fn known_source_raw_key_migrates_to_canonical_and_deduplicates() {
        let temp = tempfile::tempdir().unwrap();
        let config_dir = temp.path().join("config");
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let file_path = project_dir.join("session.jsonl");
        std::fs::write(&file_path, b"session content").unwrap();
        let raw_path = temp.path().join("project/../project/session.jsonl");
        let canonical = canonical_utf8_key(&file_path).unwrap();
        let summary = make_summary(&file_path, &project_dir);
        let fingerprint = fingerprint_file(&file_path).unwrap();
        let metadata = std::fs::metadata(&file_path).unwrap();
        let mtime = mtime_secs(&metadata).unwrap();

        let mut cache = SessionIndexCache::empty();
        cache.insert_with_fingerprint(
            raw_path.to_string_lossy().to_string(),
            metadata.len(),
            mtime,
            fingerprint.digest.clone(),
            &summary,
        );
        cache.insert_with_fingerprint(
            canonical.clone(),
            metadata.len(),
            mtime,
            fingerprint.digest,
            &summary,
        );
        cache.save(&config_dir);

        let loaded = SessionIndexCache::load(&config_dir);
        assert_eq!(loaded.entries.len(), 1);
        assert!(loaded.entries.contains_key(&canonical));
    }

    #[test]
    fn test_corrupt_cache_returns_empty() {
        let dir = TempDir::new().unwrap();
        let cache_file = cache_path(dir.path());
        std::fs::write(&cache_file, b"not valid json !!!").unwrap();

        let cache = SessionIndexCache::load(dir.path());
        assert!(cache.entries.is_empty());
        assert_eq!(cache.version, CACHE_VERSION);
    }

    #[test]
    fn load_with_status_distinguishes_missing_and_corrupt_cache() {
        let temp = tempfile::tempdir().unwrap();
        let missing = SessionIndexCache::load_with_status(temp.path());
        assert!(missing.warning.is_none());

        std::fs::write(temp.path().join("session_index.json"), b"not-json").unwrap();
        let corrupt = SessionIndexCache::load_with_status(temp.path());
        assert_eq!(corrupt.warning.as_deref(), Some("cache data invalid"));
        assert!(corrupt.cache.entries.is_empty());
    }

    #[test]
    fn load_with_status_reports_exact_read_failure_warning_for_cache_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("session_index.json")).unwrap();

        let status = SessionIndexCache::load_with_status(temp.path());

        assert_eq!(status.warning.as_deref(), Some("cache read failed"));
        assert!(status.cache.entries.is_empty());
    }

    #[test]
    fn load_with_status_reports_exact_version_mismatch_warning() {
        let temp = tempfile::tempdir().unwrap();
        let cache_file = cache_path(temp.path());
        let bad_version = serde_json::json!({
            "version": 999,
            "entries": {}
        });
        std::fs::write(&cache_file, serde_json::to_vec(&bad_version).unwrap()).unwrap();

        let status = SessionIndexCache::load_with_status(temp.path());

        assert_eq!(status.warning.as_deref(), Some("cache version mismatch"));
        assert!(status.cache.entries.is_empty());
    }

    #[test]
    fn only_a_version_mismatch_counts_as_a_routine_rebuild() {
        let temp = tempfile::tempdir().unwrap();
        let cache_file = cache_path(temp.path());

        assert!(!SessionIndexCache::load_with_status(temp.path()).routine_rebuild);

        std::fs::write(
            &cache_file,
            serde_json::to_vec(&serde_json::json!({"version": 999, "entries": {}})).unwrap(),
        )
        .unwrap();
        assert!(SessionIndexCache::load_with_status(temp.path()).routine_rebuild);

        std::fs::write(&cache_file, b"not-json").unwrap();
        assert!(!SessionIndexCache::load_with_status(temp.path()).routine_rebuild);

        std::fs::remove_file(&cache_file).unwrap();
        std::fs::create_dir(&cache_file).unwrap();
        assert!(!SessionIndexCache::load_with_status(temp.path()).routine_rebuild);
    }

    #[test]
    fn save_with_result_reports_config_path_errors() {
        let temp = tempfile::tempdir().unwrap();
        let parent_file = temp.path().join("config-parent");
        std::fs::write(&parent_file, b"not a directory").unwrap();
        let config_dir = parent_file.join("nested");

        let cache = SessionIndexCache::load(temp.path());
        let error = cache.save_with_result(&config_dir).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(config_dir.to_string_lossy().as_ref()));
    }

    #[test]
    fn test_version_mismatch_returns_empty() {
        let dir = TempDir::new().unwrap();
        let cache_file = cache_path(dir.path());
        let bad_version = serde_json::json!({
            "version": 999,
            "entries": {}
        });
        std::fs::write(&cache_file, serde_json::to_vec(&bad_version).unwrap()).unwrap();

        let cache = SessionIndexCache::load(dir.path());
        assert!(cache.entries.is_empty());
        assert_eq!(cache.version, CACHE_VERSION);
    }

    #[test]
    fn lookup_requires_content_fingerprint_even_when_metadata_matches() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("session.jsonl");
        std::fs::write(&file_path, b"valid content").unwrap();
        let project_dir = dir.path().join("project");
        let key = path_key(&file_path);
        let summary = make_summary(&file_path, &project_dir);

        let mut cache = SessionIndexCache::empty();
        cache.insert_with_fingerprint(
            key.clone(),
            12,
            1700000000,
            "fingerprint-valid".to_string(),
            &summary,
        );

        assert!(cache
            .lookup_with_fingerprint(&key, &file_path, 12, 1700000000, "fingerprint-valid")
            .is_some());
        assert!(cache
            .lookup_with_fingerprint(&key, &file_path, 12, 1700000000, "fingerprint-corrupt")
            .is_none());
    }

    #[test]
    fn remove_evicts_existing_entry_before_retain_existing() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("session.jsonl");
        let project_dir = dir.path().join("project");
        let key = path_key(&file_path);
        let summary = make_summary(&file_path, &project_dir);

        let mut cache = SessionIndexCache::empty();
        cache.insert_with_fingerprint(
            key.clone(),
            12,
            1700000000,
            "fingerprint".to_string(),
            &summary,
        );
        cache.remove(&key);
        let mut seen = HashSet::new();
        seen.insert(key.clone());
        cache.retain_existing(&seen);

        assert!(!cache.entries.contains_key(&key));
    }

    #[test]
    fn test_retain_existing_prunes() {
        let dir = TempDir::new().unwrap();
        let path_a = dir.path().join("a.jsonl");
        let path_b = dir.path().join("b.jsonl");
        std::fs::write(&path_a, b"a").unwrap();
        std::fs::write(&path_b, b"b").unwrap();
        let project_dir = dir.path().join("proj");
        let key_a = path_key(&path_a);
        let key_b = path_key(&path_b);

        let summary_a = make_summary(&path_a, &project_dir);
        let summary_b = make_summary(&path_b, &project_dir);

        let mut cache = SessionIndexCache::empty();
        cache.insert(key_a.clone(), 100, 111, &summary_a);
        cache.insert(key_b.clone(), 200, 222, &summary_b);
        assert_eq!(cache.entries.len(), 2);

        // Retain only path_a
        let mut seen = HashSet::new();
        seen.insert(key_a.clone());
        cache.retain_existing(&seen);

        assert_eq!(cache.entries.len(), 1);
        assert!(cache.lookup(&key_a, &path_a, 100, 111).is_some());
        assert!(cache.lookup(&key_b, &path_b, 200, 222).is_none());
    }

    #[test]
    fn source_aware_retention_prunes_only_completed_source() {
        let mut cache = test_cache_with_sources(&["claude-old", "codex-old"]);
        let claude_key = "claude-old".to_string();
        let codex_key = "codex-old".to_string();
        let retention = retention_with_seen_and_completed("claude", &[], true);
        let confirmed_missing = HashSet::from([claude_key.clone(), codex_key.clone()]);

        cache.retain_existing_by_source(&retention, &confirmed_missing);

        assert!(!cache.entries.contains_key(&claude_key));
        assert!(cache.entries.contains_key(&codex_key));
    }

    #[test]
    fn source_aware_retention_preserves_unselected_sources() {
        let mut cache = test_cache_with_sources(&["claude-old", "codex-old"]);
        let retention = retention_with_seen_and_completed("claude", &[], true);
        let confirmed_missing = HashSet::from(["claude-old".to_string(), "codex-old".to_string()]);

        cache.retain_existing_by_source(&retention, &confirmed_missing);

        assert!(!cache.entries.contains_key("claude-old"));
        assert!(cache.entries.contains_key("codex-old"));
    }

    #[test]
    fn source_aware_retention_preserves_incomplete_source() {
        let mut cache = test_cache_with_sources(&["claude-old"]);
        let retention = retention_with_seen_and_completed("claude", &[], false);
        let confirmed_missing = HashSet::from(["claude-old".to_string()]);

        cache.retain_existing_by_source(&retention, &confirmed_missing);

        assert!(cache.entries.contains_key("claude-old"));
    }

    #[test]
    fn source_aware_retention_preserves_seen_key_even_if_confirmed_missing() {
        let mut cache = test_cache_with_sources(&["claude-seen"]);
        let retention = retention_with_seen_and_completed("claude", &["claude-seen"], true);
        let confirmed_missing = HashSet::from(["claude-seen".to_string()]);

        cache.retain_existing_by_source(&retention, &confirmed_missing);

        assert!(cache.entries.contains_key("claude-seen"));
    }

    #[test]
    fn source_aware_retention_preserves_unconfirmed_missing_key() {
        let mut cache = test_cache_with_sources(&["claude-unconfirmed"]);
        let retention = retention_with_seen_and_completed("claude", &[], true);
        let confirmed_missing = HashSet::new();

        cache.retain_existing_by_source(&retention, &confirmed_missing);

        assert!(cache.entries.contains_key("claude-unconfirmed"));
    }

    #[test]
    fn source_aware_retention_preserves_unknown_source_even_when_marked_complete() {
        let mut cache = test_cache_with_sources(&["future-old"]);
        let retention = CacheRetention {
            seen_by_source: HashMap::from([("future".to_string(), HashSet::new())]),
            completed_sources: HashSet::from(["future".to_string()]),
        };
        let confirmed_missing = HashSet::from(["future-old".to_string()]);

        cache.retain_existing_by_source(&retention, &confirmed_missing);

        assert!(cache.entries.contains_key("future-old"));
    }

    #[test]
    fn source_aware_retention_prunes_completed_omp_source() {
        let mut cache = test_cache_with_sources(&["omp-old"]);
        let retention = retention_with_seen_and_completed("omp", &[], true);
        let confirmed_missing = HashSet::from(["omp-old".to_string()]);

        cache.retain_existing_by_source(&retention, &confirmed_missing);

        assert!(!cache.entries.contains_key("omp-old"));
    }

    #[test]
    fn source_aware_retention_preserves_unknown_source() {
        let mut cache = test_cache_with_sources(&["mystery-old"]);
        let retention = CacheRetention {
            seen_by_source: HashMap::new(),
            completed_sources: HashSet::from(["mystery".to_string()]),
        };
        let confirmed_missing = HashSet::from(["mystery-old".to_string()]);

        cache.retain_existing_by_source(&retention, &confirmed_missing);

        assert!(cache.entries.contains_key("mystery-old"));
    }

    #[test]
    fn mtime_secs_returns_metadata_unix_seconds() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("session.jsonl");
        std::fs::write(&file_path, b"session").unwrap();
        let metadata = std::fs::metadata(&file_path).unwrap();
        let expected = metadata
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        assert_eq!(mtime_secs(&metadata), Some(expected));
    }

    #[test]
    fn delta_removal_and_upsert_preserve_expected_state() {
        let expected = CacheFileState {
            file_size: 42,
            mtime_secs: 1_700_000_000,
            content_fingerprint: "fingerprint".to_string(),
        };
        let entry = CachedEntry {
            file_size: expected.file_size,
            mtime_secs: expected.mtime_secs,
            content_fingerprint: expected.content_fingerprint.clone(),
            source: "claude".to_string(),
            session_id: "session-id".to_string(),
            title: "title".to_string(),
            project_name: "project".to_string(),
            project_dir: "/tmp/project".to_string(),
            cwd: Some("/tmp/project".to_string()),
            message_count: 1,
            user_message_count: 1,
            assistant_message_count: 0,
            first_timestamp: None,
            last_activity: None,
            has_custom_title: false,
        };
        let delta = CacheDelta {
            upserts: vec![CacheUpsert {
                key: "upsert-key".to_string(),
                expected: entry.file_state(),
                entry,
            }],
            removals: vec![CacheRemoval {
                key: "remove-key".to_string(),
                expected: expected.clone(),
            }],
        };

        assert_eq!(delta.upserts[0].expected, expected);
        assert_eq!(delta.upserts[0].entry.file_state(), expected);
        assert_eq!(delta.removals[0].expected, expected);
    }

    #[test]
    fn atomic_save_roundtrip_leaves_valid_json() {
        let temp = TempDir::new().unwrap();
        let cache = SessionIndexCache::empty();

        cache.save_with_result(temp.path()).unwrap();

        let bytes = std::fs::read(cache_path(temp.path())).unwrap();
        let loaded: SessionIndexCache = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.version, CACHE_VERSION);
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn atomic_save_failure_does_not_truncate_target() {
        let temp = TempDir::new().unwrap();
        let target = cache_path(temp.path());
        std::fs::create_dir(&target).unwrap();
        let marker = target.join("marker");
        std::fs::write(&marker, b"keep").unwrap();

        let error = SessionIndexCache::empty()
            .save_with_result(temp.path())
            .unwrap_err();

        assert!(!error.to_string().is_empty());
        assert!(target.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn merge_applies_disjoint_deltas_to_latest_cache() {
        let temp = TempDir::new().unwrap();
        let existing_path = temp.path().join("existing.jsonl");
        let added_path = temp.path().join("added.jsonl");
        std::fs::write(&existing_path, b"existing").unwrap();
        std::fs::write(&added_path, b"added").unwrap();

        let mut initial = SessionIndexCache::empty();
        let existing_state = file_state_for_test(&existing_path);
        initial.entries.insert(
            path_key(&existing_path),
            cached_entry_for_test(&existing_path, "claude", existing_state.clone(), "existing"),
        );
        initial.save_with_result(temp.path()).unwrap();

        let added_state = file_state_for_test(&added_path);
        let delta = CacheDelta {
            upserts: vec![CacheUpsert {
                key: path_key(&added_path),
                expected: added_state.clone(),
                entry: cached_entry_for_test(&added_path, "codex", added_state, "added"),
            }],
            removals: Vec::new(),
        };

        merge_scan_with_result(temp.path(), &delta, &CacheRetention::default()).unwrap();

        let merged = SessionIndexCache::load(temp.path());
        assert!(merged.entries.contains_key(&path_key(&existing_path)));
        assert_eq!(
            merged.entries.get(&path_key(&added_path)).unwrap().source,
            "codex"
        );
    }

    #[test]
    fn merge_skips_stale_upsert_after_file_changes() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, b"old").unwrap();
        let expected = file_state_for_test(&path);

        let mut initial = SessionIndexCache::empty();
        initial.entries.insert(
            path_key(&path),
            cached_entry_for_test(&path, "claude", expected.clone(), "old"),
        );
        initial.save_with_result(temp.path()).unwrap();
        std::fs::write(&path, b"new").unwrap();

        let delta = CacheDelta {
            upserts: vec![CacheUpsert {
                key: path_key(&path),
                expected,
                entry: cached_entry_for_test(&path, "claude", file_state_for_test(&path), "new"),
            }],
            removals: Vec::new(),
        };

        let report =
            merge_scan_with_report(temp.path(), &delta, &CacheRetention::default()).unwrap();
        assert!(report.revalidation_issues.is_empty());

        let merged = SessionIndexCache::load(temp.path());
        assert_eq!(merged.entries.get(&path_key(&path)).unwrap().title, "old");
    }

    #[test]
    fn merge_skips_stale_removal_after_file_changes() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, b"old").unwrap();
        let expected = file_state_for_test(&path);

        let mut initial = SessionIndexCache::empty();
        initial.entries.insert(
            path_key(&path),
            cached_entry_for_test(&path, "claude", expected.clone(), "old"),
        );
        initial.save_with_result(temp.path()).unwrap();
        std::fs::write(&path, b"new").unwrap();

        let delta = CacheDelta {
            upserts: Vec::new(),
            removals: vec![CacheRemoval {
                key: path_key(&path),
                expected,
            }],
        };

        let report =
            merge_scan_with_report(temp.path(), &delta, &CacheRetention::default()).unwrap();
        assert!(report.revalidation_issues.is_empty());

        assert!(SessionIndexCache::load(temp.path())
            .entries
            .contains_key(&path_key(&path)));
    }

    #[test]
    fn merge_report_exposes_revalidation_error_without_removing_entry() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, b"session").unwrap();
        let key = path_key(&path);
        let expected = file_state_for_test(&path);

        let mut initial = SessionIndexCache::empty();
        initial.entries.insert(
            key.clone(),
            cached_entry_for_test(&path, "claude", expected.clone(), "old"),
        );
        initial.save_with_result(temp.path()).unwrap();

        let delta = CacheDelta {
            upserts: Vec::new(),
            removals: vec![CacheRemoval {
                key: key.clone(),
                expected,
            }],
        };
        set_test_revalidation_error_path(Some(PathBuf::from(&key)));
        let report =
            merge_scan_with_report(temp.path(), &delta, &CacheRetention::default()).unwrap();
        set_test_revalidation_error_path(None);

        assert_eq!(report.revalidation_issues.len(), 1);
        assert_eq!(report.revalidation_issues[0].key, key);
        assert_eq!(
            report.revalidation_issues[0].error_kind,
            crate::session_diagnostics::ScanWarningErrorKind::PermissionDenied
        );
        assert!(SessionIndexCache::load(temp.path())
            .entries
            .contains_key(&key));
    }

    #[test]
    fn invalid_cache_rebuild_requires_complete_sources() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, b"session").unwrap();
        let invalid = cache_path(temp.path());
        std::fs::write(&invalid, b"not-json").unwrap();
        let state = file_state_for_test(&path);
        let delta = CacheDelta {
            upserts: vec![CacheUpsert {
                key: path_key(&path),
                expected: state.clone(),
                entry: cached_entry_for_test(&path, "claude", state, "rebuilt"),
            }],
            removals: Vec::new(),
        };

        let incomplete = CacheRetention {
            seen_by_source: HashMap::new(),
            completed_sources: HashSet::from(["claude".to_string()]),
        };
        assert!(merge_scan_with_result(temp.path(), &delta, &incomplete).is_err());
        assert_eq!(std::fs::read(&invalid).unwrap(), b"not-json");

        let complete = CacheRetention {
            seen_by_source: HashMap::from([
                ("claude".to_string(), HashSet::new()),
                ("codex".to_string(), HashSet::new()),
                ("omp".to_string(), HashSet::new()),
            ]),
            completed_sources: HashSet::from([
                "claude".to_string(),
                "codex".to_string(),
                "omp".to_string(),
            ]),
        };
        merge_scan_with_result(temp.path(), &delta, &complete).unwrap();
        assert_eq!(
            SessionIndexCache::load(temp.path())
                .entries
                .get(&path_key(&path))
                .unwrap()
                .title,
            "rebuilt"
        );
    }

    #[test]
    fn version_mismatch_rebuild_preserves_unknown_entries_and_upgrades_version() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, b"session").unwrap();

        let mut old_cache = test_cache_with_sources(&["future-entry"]);
        old_cache.version = CACHE_VERSION - 1;
        std::fs::write(
            cache_path(temp.path()),
            serde_json::to_vec(&old_cache).unwrap(),
        )
        .unwrap();

        let cold = SessionIndexCache::load_with_status(temp.path());
        assert!(cold.cache.entries.is_empty());
        assert_eq!(cold.warning.as_deref(), Some("cache version mismatch"));

        let state = file_state_for_test(&path);
        let delta = CacheDelta {
            upserts: vec![CacheUpsert {
                key: path_key(&path),
                expected: state.clone(),
                entry: cached_entry_for_test(&path, "claude", state, "rebuilt"),
            }],
            removals: Vec::new(),
        };
        let retention = CacheRetention {
            seen_by_source: HashMap::from([
                ("claude".to_string(), HashSet::new()),
                ("codex".to_string(), HashSet::new()),
                ("omp".to_string(), HashSet::new()),
            ]),
            completed_sources: HashSet::from([
                "claude".to_string(),
                "codex".to_string(),
                "omp".to_string(),
            ]),
        };

        merge_scan_with_result(temp.path(), &delta, &retention).unwrap();

        let merged = SessionIndexCache::load(temp.path());
        assert_eq!(merged.version, CACHE_VERSION);
        assert!(merged.entries.contains_key("future-entry"));
        assert_eq!(
            merged.entries.get(&path_key(&path)).unwrap().title,
            "rebuilt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn revalidation_untrusted_symlink_is_reported_and_entry_retained() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, b"original").unwrap();
        let state = file_state_for_test(&path);
        let key = path_key(&path);
        let mut cache = SessionIndexCache::empty();
        cache.entries.insert(
            key.clone(),
            cached_entry_for_test(&path, "claude", state, "keep me"),
        );
        cache.save_with_result(temp.path()).unwrap();

        let outside = temp.path().join("outside.jsonl");
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();

        let retention = CacheRetention {
            seen_by_source: HashMap::from([(String::from("claude"), HashSet::new())]),
            completed_sources: HashSet::from([String::from("claude")]),
        };
        let report =
            merge_scan_with_report(temp.path(), &CacheDelta::default(), &retention).unwrap();

        assert_eq!(report.revalidation_issues.len(), 1);
        assert_eq!(
            report.revalidation_issues[0].error_kind,
            ScanWarningErrorKind::Unknown
        );
        assert!(SessionIndexCache::load(temp.path())
            .entries
            .contains_key(&key));
    }

    #[test]
    fn revalidation_non_regular_object_is_reported_and_entry_retained() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, b"original").unwrap();
        let state = file_state_for_test(&path);
        let key = path_key(&path);
        let mut cache = SessionIndexCache::empty();
        cache.entries.insert(
            key.clone(),
            cached_entry_for_test(&path, "claude", state, "keep me"),
        );
        cache.save_with_result(temp.path()).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        let retention = CacheRetention {
            seen_by_source: HashMap::from([(String::from("claude"), HashSet::new())]),
            completed_sources: HashSet::from([String::from("claude")]),
        };
        let report =
            merge_scan_with_report(temp.path(), &CacheDelta::default(), &retention).unwrap();

        assert_eq!(report.revalidation_issues.len(), 1);
        assert_eq!(
            report.revalidation_issues[0].error_kind,
            ScanWarningErrorKind::Unknown
        );
        assert!(SessionIndexCache::load(temp.path())
            .entries
            .contains_key(&key));
    }

    #[test]
    fn current_file_state_rejects_same_metadata_replacement_after_fingerprint() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, b"old").unwrap();
        let original_modified = std::fs::metadata(&path).unwrap().modified().unwrap();

        let result = current_file_state_with_hook(&path, |path| {
            std::fs::write(path, b"new").unwrap();
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(original_modified))
                .unwrap();
        });

        assert!(result.is_err());
    }

    #[test]
    fn completed_source_without_seen_map_rejects_merge_and_preserves_target() {
        let temp = TempDir::new().unwrap();
        let mut initial = SessionIndexCache::empty();
        initial.entries.insert(
            "existing-entry".to_string(),
            test_cache_with_sources(&["existing-entry"]).entries["existing-entry"].clone(),
        );
        initial.save_with_result(temp.path()).unwrap();
        let before = std::fs::read(cache_path(temp.path())).unwrap();

        let retention = CacheRetention {
            seen_by_source: HashMap::new(),
            completed_sources: HashSet::from(["claude".to_string()]),
        };
        let error =
            merge_scan_with_result(temp.path(), &CacheDelta::default(), &retention).unwrap_err();

        assert!(error.to_string().contains("missing seen set"));
        assert_eq!(std::fs::read(cache_path(temp.path())).unwrap(), before);
    }

    #[test]
    fn read_failed_cache_is_never_overwritten() {
        let temp = TempDir::new().unwrap();
        let target = cache_path(temp.path());
        std::fs::create_dir(&target).unwrap();
        let marker = target.join("marker");
        std::fs::write(&marker, b"keep").unwrap();

        let error = merge_scan_with_result(
            temp.path(),
            &CacheDelta::default(),
            &CacheRetention::default(),
        )
        .unwrap_err();

        assert!(!error.to_string().is_empty());
        assert!(target.is_dir());
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    fn file_state_for_test(path: &Path) -> CacheFileState {
        let metadata = std::fs::metadata(path).unwrap();
        let fingerprint = fingerprint_file(path).unwrap();
        CacheFileState {
            file_size: metadata.len(),
            mtime_secs: mtime_secs(&metadata).unwrap(),
            content_fingerprint: fingerprint.digest,
        }
    }

    fn cached_entry_for_test(
        path: &Path,
        source: &str,
        state: CacheFileState,
        title: &str,
    ) -> CachedEntry {
        let mut summary = make_summary(path, path.parent().unwrap());
        summary.source = source.to_string();
        summary.title = title.to_string();
        CachedEntry {
            file_size: state.file_size,
            mtime_secs: state.mtime_secs,
            content_fingerprint: state.content_fingerprint,
            source: summary.source,
            session_id: summary.session_id,
            title: summary.title,
            project_name: summary.project_name,
            project_dir: summary.project_dir.to_string_lossy().to_string(),
            cwd: summary.cwd.clone(),
            message_count: summary.message_count,
            user_message_count: summary.user_message_count,
            assistant_message_count: summary.assistant_message_count,
            first_timestamp: summary.first_timestamp,
            last_activity: summary.last_activity,
            has_custom_title: summary.has_custom_title,
        }
    }

    fn test_cache_with_sources(keys_and_sources: &[&str]) -> SessionIndexCache {
        let mut cache = SessionIndexCache::empty();
        for key_and_source in keys_and_sources {
            let source = key_and_source
                .split_once('-')
                .map(|(source, _)| source)
                .unwrap_or(key_and_source)
                .to_string();
            cache.entries.insert(
                (*key_and_source).to_string(),
                CachedEntry {
                    file_size: 1,
                    mtime_secs: 1,
                    content_fingerprint: "fingerprint".to_string(),
                    source,
                    session_id: (*key_and_source).to_string(),
                    title: String::new(),
                    project_name: String::new(),
                    project_dir: String::new(),
                    cwd: None,
                    message_count: 0,
                    user_message_count: 0,
                    assistant_message_count: 0,
                    first_timestamp: None,
                    last_activity: None,
                    has_custom_title: false,
                },
            );
        }
        cache
    }

    fn retention_with_seen_and_completed(
        source: &str,
        seen: &[&str],
        completed: bool,
    ) -> CacheRetention {
        CacheRetention {
            seen_by_source: HashMap::from([(
                source.to_string(),
                seen.iter().map(|key| (*key).to_string()).collect(),
            )]),
            completed_sources: if completed {
                HashSet::from([source.to_string()])
            } else {
                HashSet::new()
            },
        }
    }
}
