# Session Test Maintenance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 `ccs session` 增加覆盖 Claude Code、Codex、OMP 的保守测试会话识别，以及可解释、可恢复、不会自动传播远端删除的隐藏→回收→本地清除生命周期。

**Architecture:** 先把 session 领域类型从超大 handler 中抽离，再建立纯分类器、原子 state registry 和安全回收事务。查询命令在摘要层叠加 visibility；search/show 额外扫描回收区；Claude sync 通过 source-qualified fingerprint suppression 避免回收会话被 pull 复活，同时保持显式 `delete`/`push --prune` 的既有语义。

**Tech Stack:** Rust 2021、clap 4、serde/serde_json、toml、chrono、blake3、fs4、tempfile、serial_test、anyhow。

## Global Constraints

- 默认配置必须是 `enabled = false`、`classifier = "conservative"`、`hide_after_hours = 24`、`recycle_after_days = 7`、`purge_after_days = 30`、`max_actions_per_run = 50`。
- 自动维护不得写 Claude tombstone；Codex/OMP 不得参与 ccs 同步。
- 内部身份必须使用 `(source, session_id)`，禁止新增裸 session ID 状态索引。
- 单一标题关键词不能达到分类阈值；默认阈值为 `70`。
- 来源 incomplete/degraded、文件变化、symlink、路径越界、重复同来源 ID、state/journal 不一致时必须 fail-safe 保留。
- 生命周期从首次 `hidden_since` 计算；首次启用最多隐藏，不能立即回收或 purge。
- `search` 默认包含 visible/hidden/recycled；`list/projects/overview/interactive` 默认只包含 visible。
- Claude `purged_local` 仅表示本地副本清除；远端永久删除仍要求显式 `delete` 或手动 `push --prune`。
- 所有测试配置目录必须通过 `CLAUDE_CODE_SYNC_CONFIG_DIR` 隔离；操作环境变量的测试必须使用 `#[serial]`。
- 不新增后台 daemon、launchd、Task Scheduler 或 cron。
- 所有公共 API 必须有 `///` 文档注释。
- 每个实现任务结束时更新测试；功能完成时更新 `README.md`、`docs/user-guide.md`、项目 `CLAUDE.md` 和 `local/notes.md`。

---

## File Structure

### New files

- `src/session_model.rs`：`SessionSource`、`SessionIdentity`、`SessionSourceFilter`、`ProjectSummary`、`SessionSummary` 及三来源摘要构造。
- `src/atomic_file.rs`：可复用的私有 lock file 与 JSON atomic persist，供 cache 和 maintenance state 共用。
- `src/session_maintenance/mod.rs`：维护服务、配置读取、候选构造、visibility overlay、命令所需公共接口。
- `src/session_maintenance/classifier.rs`：纯分类器、分值、保护门槛、reason codes。
- `src/session_maintenance/state.rs`：版本化 registry、生命周期、marker/keep、pending journal、并发 merge。
- `src/session_maintenance/recycle.rs`：安全回收、恢复、purge、跨文件系统 copy fallback、pending 协调。
- `tests/session_maintenance_cli_tests.rs`：真实 CLI、三来源、查询可见性和恢复集成测试。
- `tests/session_maintenance_concurrency_tests.rs`：跨进程 state writer、maintenance/restore 竞态和 atomic reader 测试。

### Modified files

- `src/lib.rs`：注册 `atomic_file`、`session_model`、`session_maintenance`。
- `src/config.rs`：增加 maintenance state、lock、recycle 路径 helper。
- `src/filter.rs`：增加 `[session_maintenance]` 配置结构和默认值。
- `src/parser.rs`：公开 Claude custom-title 判断。
- `src/session_cache.rs`：改用共享 atomic helper；cache 增加 `has_custom_title`，保持 v3 兼容时以默认 false 读取。
- `src/handlers/session.rs`：移除已抽取领域类型；暴露 source completeness；接入 maintenance、visibility、recycled search/show、恢复命令。
- `src/handlers/mod.rs`：re-export 新 handler。
- `src/main.rs`：增加 CLI 参数和 dispatch。
- `src/sync/pull.rs`：过滤相同 fingerprint 的本地抑制 Claude 会话；新修订解除抑制。
- `src/sync/push.rs`：保护/删除放行时排除 maintenance-suppressed missing；显式 `--prune` 仍可删除。
- `README.md`、`docs/user-guide.md`、`CLAUDE.md`、`local/notes.md`：文档和问题记录。

---

### Task 1: Extract Session Domain Model and Source Completeness

**Files:**
- Create: `src/session_model.rs`
- Modify: `src/lib.rs:49-76`
- Modify: `src/parser.rs:319-339`
- Modify: `src/handlers/session.rs:49-445`
- Modify: `src/handlers/session.rs:718-761`
- Modify: `src/session_cache.rs:9-10`
- Test: `src/session_model.rs`
- Test: `src/handlers/session.rs`

**Interfaces:**
- Produces: `SessionSource`, `SourceCapabilities`, `SessionIdentity`, `SessionSourceFilter`, `ProjectSummary`, `SessionSummary` in `crate::session_model`.
- Produces: `ConversationSession::has_custom_title(&self) -> bool`.
- Produces: `SessionScanResult.completed_sources: HashSet<SessionSource>`.
- Consumes: existing `ConversationSession`, `CodexSession`, `OmpSession` APIs.

- [ ] **Step 1: Add failing custom-title and source-completeness tests**

Add to `src/parser.rs` tests:

```rust
#[test]
fn has_custom_title_reports_renamed_sessions_only() {
    let user_entry: ConversationEntry = serde_json::from_str(
        r#"{"type":"user","message":{"role":"user","content":"hello"},"sessionId":"s1"}"#,
    )
    .unwrap();
    let custom_title_entry: ConversationEntry = serde_json::from_str(
        r#"{"type":"custom-title","customTitle":"renamed","sessionId":"s1"}"#,
    )
    .unwrap();

    let plain = ConversationSession {
        session_id: "s1".to_string(),
        entries: vec![user_entry.clone()],
        file_path: "s1.jsonl".to_string(),
    };
    let renamed = ConversationSession {
        session_id: "s1".to_string(),
        entries: vec![user_entry, custom_title_entry],
        file_path: "s1.jsonl".to_string(),
    };

    assert!(!plain.has_custom_title());
    assert!(renamed.has_custom_title());
}
```

Add to `src/handlers/session.rs` tests a scan assertion using the existing injected roots fixture:

```rust
assert_eq!(result.completed_sources.len(), 3);
assert!(result.completed_sources.contains(&SessionSource::Claude));
assert!(result.completed_sources.contains(&SessionSource::Codex));
assert!(result.completed_sources.contains(&SessionSource::Omp));
```

- [ ] **Step 2: Run focused tests and verify RED**

Run:

```bash
cargo test has_custom_title_reports_renamed_sessions_only --lib
cargo test scan_report_counts_three_sources_and_malformed_files --lib
```

Expected: first command fails because `has_custom_title` is missing; second fails because `completed_sources` is missing.

- [ ] **Step 3: Create `src/session_model.rs` and move domain types without behavior changes**

Move the existing definitions and implementations from `src/handlers/session.rs` into `src/session_model.rs`. Use these derives and visibilities:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionSource {
    Claude,
    Codex,
    Omp,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionIdentity {
    pub source: SessionSource,
    pub session_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSourceFilter {
    All,
    Claude,
    Codex,
    Omp,
}
```

Keep `SessionSummary.source: String` for cache/JSON compatibility, add:

```rust
pub has_custom_title: bool,
```

Expose typed helpers:

```rust
impl SessionSummary {
    pub(crate) fn source_kind(&self) -> anyhow::Result<SessionSource> {
        SessionSource::try_from(self.source.as_str())
    }

    pub(crate) fn identity(&self) -> anyhow::Result<SessionIdentity> {
        Ok(SessionIdentity {
            source: self.source_kind()?,
            session_id: self.session_id.clone(),
        })
    }
}
```

Set Claude from `session.has_custom_title()` and Codex/OMP to `false`.

Register the modules created by this task in `src/lib.rs`:

```rust
pub mod session_model;
pub mod session_maintenance;
```

Create an empty compilable `src/session_maintenance/mod.rs` containing only module documentation until Task 3. Task 2 registers `pub(crate) mod atomic_file;` only after `src/atomic_file.rs` exists.

- [ ] **Step 4: Add `ConversationSession::has_custom_title`**

Add beside `title()` in `src/parser.rs`:

```rust
/// Returns whether the session contains a user-created Claude custom title.
pub fn has_custom_title(&self) -> bool {
    self.entries
        .iter()
        .any(|entry| entry.entry_type == "custom-title" && entry.custom_title.is_some())
}
```

- [ ] **Step 5: Expose completed sources from the scan tracker**

Add to `SourceScanTracker`:

```rust
fn completed_sources(&self) -> HashSet<SessionSource> {
    self.started_sources
        .difference(&self.incomplete_sources)
        .filter_map(|source| SessionSource::try_from(source.as_str()).ok())
        .collect()
}
```

Extend `SessionScanResult`:

```rust
pub completed_sources: HashSet<SessionSource>,
```

Construct it before consuming tracker retention:

```rust
let completed_sources = tracker.completed_sources();
let retention = tracker.retention();
```

Return both summaries/diagnostics and `completed_sources` on every scan path.

- [ ] **Step 6: Update cache conversion for `has_custom_title`**

In `CachedEntry` add:

```rust
#[serde(default)]
pub has_custom_title: bool,
```

Copy the field in both `CachedEntry -> SessionSummary` and `SessionSummary -> CachedEntry` conversions. Bump `CACHE_VERSION` from `3` to `4`; otherwise an old cached custom-titled Claude session would deserialize `has_custom_title = false` and lose a hard protection signal. Update cache version-mismatch tests to expect v4 cold reparse.

- [ ] **Step 7: Run focused and full library tests**

Run:

```bash
cargo test has_custom_title_reports_renamed_sessions_only --lib
cargo test handlers::session::tests --lib
cargo test session_cache --lib
```

Expected: all PASS.

- [ ] **Step 8: Commit**

```bash
git add src/lib.rs src/session_model.rs src/session_maintenance/mod.rs src/parser.rs src/handlers/session.rs src/session_cache.rs
git commit -m "refactor(session): extract domain model"
```

---

### Task 2: Shared Atomic Persistence and Maintenance Configuration

**Files:**
- Create: `src/atomic_file.rs`
- Modify: `src/lib.rs`
- Modify: `src/session_cache.rs:214-248,533-567,772-786`
- Modify: `src/config.rs:58-105`
- Modify: `src/filter.rs:181-275`
- Test: `src/atomic_file.rs`
- Test: `src/filter.rs`
- Test: `src/config.rs`

**Interfaces:**
- Produces: `FileLock::acquire(lock_path: &Path) -> Result<FileLock>`.
- Produces: `persist_json_atomic<T: Serialize>(target: &Path, value: &T) -> Result<()>`.
- Produces: `SessionMaintenanceSettings` and config defaults.
- Produces: `ConfigManager::{session_maintenance_path, session_maintenance_lock_path, session_recycle_dir}`.

- [ ] **Step 1: Write failing atomic persistence tests**

Create `src/atomic_file.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Payload {
        value: u32,
    }

    #[test]
    fn persist_json_atomic_replaces_with_complete_json() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("state.json");
        persist_json_atomic(&target, &Payload { value: 1 }).unwrap();
        persist_json_atomic(&target, &Payload { value: 2 }).unwrap();
        let loaded: Payload = serde_json::from_slice(&std::fs::read(target).unwrap()).unwrap();
        assert_eq!(loaded, Payload { value: 2 });
    }

    #[test]
    fn file_lock_serializes_two_writers() {
        let dir = tempdir().unwrap();
        let lock_path = dir.path().join("state.lock");
        let first = FileLock::acquire(&lock_path).unwrap();
        let path = lock_path.clone();
        let waiter = std::thread::spawn(move || FileLock::acquire(&path).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!waiter.is_finished());
        drop(first);
        drop(waiter.join().unwrap());
    }
}
```

- [ ] **Step 2: Run test and verify RED**

Run: `cargo test atomic_file --lib`

Expected: compile failure because `FileLock` and `persist_json_atomic` are not defined.

- [ ] **Step 3: Implement reusable lock and atomic JSON writer**

Register `pub(crate) mod atomic_file;` in `src/lib.rs`, then implement `src/atomic_file.rs`:

```rust
use anyhow::{Context, Result};
use fs4::FileExt;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use tempfile::NamedTempFile;

pub(crate) struct FileLock {
    file: File,
}

impl FileLock {
    pub(crate) fn acquire(lock_path: &Path) -> Result<Self> {
        let parent = lock_path.parent().context("lock path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(lock_path)?;
        set_private_permissions(lock_path)?;
        FileExt::lock(&file)?;
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

pub(crate) fn persist_json_atomic<T: Serialize>(target: &Path, value: &T) -> Result<()> {
    let parent = target.parent().context("JSON target has no parent")?;
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec(value)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    set_private_permissions(temp.path())?;
    temp.write_all(&bytes)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(target).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
```

Refactor `session_cache.rs` to use `FileLock` and `persist_json_atomic`; preserve separate `session_index.json.lock` and all existing merge tests.

- [ ] **Step 4: Add maintenance config tests**

Add to `src/filter.rs` tests:

```rust
#[test]
fn maintenance_defaults_are_safe_and_disabled() {
    let config = FilterConfig::default();
    assert!(!config.session_maintenance.enabled);
    assert_eq!(config.session_maintenance.classifier, "conservative");
    assert_eq!(config.session_maintenance.hide_after_hours, 24);
    assert_eq!(config.session_maintenance.recycle_after_days, 7);
    assert_eq!(config.session_maintenance.purge_after_days, 30);
    assert_eq!(config.session_maintenance.max_actions_per_run, 50);
}
```

- [ ] **Step 5: Implement maintenance settings and config paths**

Add to `src/filter.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMaintenanceSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_maintenance_classifier")]
    pub classifier: String,
    #[serde(default = "default_hide_after_hours")]
    pub hide_after_hours: u64,
    #[serde(default = "default_recycle_after_days")]
    pub recycle_after_days: u64,
    #[serde(default = "default_purge_after_days")]
    pub purge_after_days: u64,
    #[serde(default = "default_max_maintenance_actions")]
    pub max_actions_per_run: usize,
}

impl Default for SessionMaintenanceSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            classifier: "conservative".to_string(),
            hide_after_hours: 24,
            recycle_after_days: 7,
            purge_after_days: 30,
            max_actions_per_run: 50,
        }
    }
}
```

Add `pub session_maintenance: SessionMaintenanceSettings` with `#[serde(default)]` to `FilterConfig` and initialize it in `Default`.

Add to `ConfigManager`:

```rust
pub fn session_maintenance_path() -> Result<PathBuf> {
    Ok(Self::config_dir()?.join("session-maintenance.json"))
}

pub fn session_maintenance_lock_path() -> Result<PathBuf> {
    Ok(Self::config_dir()?.join("session-maintenance.lock"))
}

pub fn session_recycle_dir() -> Result<PathBuf> {
    Ok(Self::config_dir()?.join("session-recycle"))
}
```

- [ ] **Step 6: Run tests**

Run:

```bash
cargo test atomic_file --lib
cargo test maintenance_defaults_are_safe_and_disabled --lib
cargo test session_cache --lib
cargo test config::tests --lib
```

Expected: all PASS.

- [ ] **Step 7: Commit**

```bash
git add src/lib.rs src/atomic_file.rs src/session_cache.rs src/filter.rs src/config.rs
git commit -m "refactor: share atomic state persistence"
```

---

### Task 3: Conservative Pure Classifier

**Files:**
- Create: `src/session_maintenance/classifier.rs`
- Modify: `src/session_maintenance/mod.rs`
- Test: `src/session_maintenance/classifier.rs`

**Interfaces:**
- Produces: `MaintenanceCandidate`, `Classification`, `ClassificationDecision`, `ReasonCode`.
- Produces: `classify(candidate: &MaintenanceCandidate, policy: &ClassifierPolicy, now: DateTime<Utc>) -> ClassificationDecision`.
- Consumes: `SessionIdentity`, `FileFingerprint`, maintenance settings.

- [ ] **Step 1: Write classifier tests before implementation**

Use fixed timestamps and construct candidates through a helper. Required tests:

```rust
#[test]
fn exact_test_title_alone_does_not_cross_threshold() {
    let candidate = candidate("test", 5, 3, 60, "550e8400-e29b-41d4-a716-446655440000");
    let decision = classify(&candidate, &policy(), now());
    assert_eq!(decision.classification, Classification::Keep);
    assert!(decision.reasons.contains(&ReasonCode::ExactTestTitle));
}

#[test]
fn multiple_low_value_signals_cross_threshold() {
    let candidate = candidate("test", 2, 1, 5, "550e8400-e29b-41d4-a716-446655440000");
    let decision = classify(&candidate, &policy(), now());
    assert_eq!(decision.classification, Classification::TestCandidate);
    assert_eq!(decision.score, 80);
}

#[test]
fn custom_title_and_recent_activity_are_hard_protections() {
    let mut custom = candidate("test", 2, 1, 5, "cc-task4");
    custom.has_custom_title = true;
    assert_eq!(classify(&custom, &policy(), now()).classification, Classification::Keep);

    let mut recent = candidate("test", 2, 1, 5, "cc-task4");
    recent.last_activity = Some(now() - chrono::Duration::hours(2));
    assert_eq!(classify(&recent, &policy(), now()).classification, Classification::Keep);
}

#[test]
fn explicit_keep_overrides_explicit_test_marker() {
    let mut candidate = candidate("test", 2, 1, 5, "cc-task4");
    candidate.explicit_test = true;
    candidate.keep = true;
    assert_eq!(classify(&candidate, &policy(), now()).classification, Classification::Keep);
}
```

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test session_maintenance::classifier --lib`

Expected: compile failure because classifier types/functions are missing.

- [ ] **Step 3: Implement exact classifier types**

Define:

```rust
pub(crate) const CLASSIFIER_VERSION: u32 = 1;
pub(crate) const DEFAULT_THRESHOLD: u16 = 70;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassifierPolicy {
    pub threshold: u16,
    pub hide_after_hours: u64,
}

impl ClassifierPolicy {
    pub(crate) fn conservative(hide_after_hours: u64) -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            hide_after_hours,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Keep,
    TestCandidate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    ExplicitTestMarker,
    FixtureSessionId,
    FixtureTemporaryCwd,
    ExactTestTitle,
    AutomatedValidationTitle,
    FewUserMessages,
    FewTotalMessages,
    ShortDuration,
    TemporaryCwd,
    RecentActivityProtection,
    CustomTitleProtection,
    LongConversationProtection,
    KeepProtection,
}

#[derive(Debug, Clone)]
pub struct MaintenanceCandidate {
    pub identity: SessionIdentity,
    pub original_relative_path: PathBuf,
    pub project_name: String,
    pub project_dir: PathBuf,
    pub title: String,
    pub has_custom_title: bool,
    pub user_message_count: usize,
    pub message_count: usize,
    pub first_activity: Option<DateTime<Utc>>,
    pub last_activity: Option<DateTime<Utc>>,
    pub size: u64,
    pub fingerprint: FileFingerprint,
    pub explicit_test: bool,
    pub keep: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationDecision {
    pub classification: Classification,
    pub score: u16,
    pub reasons: Vec<ReasonCode>,
}
```

- [ ] **Step 4: Implement protection-first scoring**

Implement in this order:

1. `keep` returns Keep with `KeepProtection`.
2. `has_custom_title`, recent activity `< hide_after_hours`, message count `> 20`, or duration `> 2h` return Keep with the matching protection reason.
3. Explicit marker scores 100.
4. Fixture ID regex `^(cc|cx|om)(-cache)?-task[0-9]+$` scores 60.
5. Exact lowercased/trimmed title in `测试,test,hello,hi,试一下` scores 35.
6. Title containing `fixture`, `smoke test`, or `test brief` scores 25.
7. User messages `<= 2` scores 20; total messages `<= 6` scores 10; duration `<= 15m` scores 15; temporary cwd scores 20.
8. Return TestCandidate only when score `>= 70`.

Use `regex::Regex` initialized with `std::sync::OnceLock` so the fixture ID regex is compiled once.

- [ ] **Step 5: Run classifier tests and clippy for the module**

Run:

```bash
cargo test session_maintenance::classifier --lib
cargo clippy --lib -- -D warnings
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/session_maintenance/mod.rs src/session_maintenance/classifier.rs
git commit -m "feat(session): add conservative maintenance classifier"
```

---

### Task 4: Versioned Maintenance State and Lifecycle

**Files:**
- Create: `src/session_maintenance/state.rs`
- Modify: `src/session_maintenance/mod.rs`
- Test: `src/session_maintenance/state.rs`

**Interfaces:**
- Produces: `MaintenanceState`, `MaintenanceEntry`, `LifecycleState`, `PendingOperation`, `StateStore`.
- Produces: `StateStore::update<F, T>(&self, update: F) -> Result<T>` for one-save mutations.
- Produces: `StateStore::transaction<F, T>(&self, transaction: F) -> Result<T>` with a `LockedState::persist()` intermediate-save API for journaled file operations.
- Produces: `next_lifecycle(entry, decision, now, settings) -> LifecycleTransition`.

- [ ] **Step 1: Write lifecycle tests with injected time**

Required tests:

```rust
#[test]
fn first_match_only_enters_hidden_even_for_old_session() {
    let transition = next_lifecycle(None, &test_decision(), now(), &settings());
    assert_eq!(transition, LifecycleTransition::Hide);
}

#[test]
fn hidden_entry_recycles_after_seven_days() {
    let entry = hidden_entry(now() - chrono::Duration::days(7));
    assert_eq!(
        next_lifecycle(Some(&entry), &test_decision(), now(), &settings()),
        LifecycleTransition::Recycle
    );
}

#[test]
fn hidden_entry_purges_only_after_thirty_days() {
    let entry = recycled_entry(now() - chrono::Duration::days(30));
    assert_eq!(
        next_lifecycle(Some(&entry), &test_decision(), now(), &settings()),
        LifecycleTransition::PurgeLocal
    );
}

#[test]
fn changed_fingerprint_returns_visible() {
    let mut entry = hidden_entry(now() - chrono::Duration::days(7));
    entry.fingerprint = "old".to_string();
    assert_eq!(
        reconcile_fingerprint(&entry, "new"),
        LifecycleTransition::RestoreVisible
    );
}
```

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test session_maintenance::state --lib`

Expected: compile failure because state types/functions are missing.

- [ ] **Step 3: Implement state schema**

Define exact schema:

```rust
const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Visible,
    Hidden,
    Recycled,
    PurgedLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleTransition {
    NoChange,
    Hide,
    Recycle,
    PurgeLocal,
    RestoreVisible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingOperationKind {
    Recycle,
    Restore,
    Purge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceEntry {
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
pub struct PendingOperation {
    pub identity: SessionIdentity,
    pub operation: PendingOperationKind,
    pub source_relative_path: PathBuf,
    pub staging_relative_path: PathBuf,
    pub recycle_relative_path: PathBuf,
    pub expected_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceState {
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
```

Use key function:

```rust
pub(crate) fn identity_key(identity: &SessionIdentity) -> String {
    format!("{}:{}", identity.source.as_str(), identity.session_id)
}
```

- [ ] **Step 4: Implement lock/reload/atomic-save state store**

Define:

```rust
pub(crate) struct StateStore {
    state_path: PathBuf,
    lock_path: PathBuf,
}

pub(crate) struct LockedState<'a> {
    store: &'a StateStore,
    pub state: MaintenanceState,
}

impl LockedState<'_> {
    pub(crate) fn persist(&self) -> Result<()> {
        persist_json_atomic(&self.store.state_path, &self.state)
    }
}

impl StateStore {
    pub(crate) fn from_config_dir(config_dir: &Path) -> Self;
    pub(crate) fn load(&self) -> Result<MaintenanceState>;
    pub(crate) fn transaction<F, T>(&self, transaction: F) -> Result<T>
    where
        F: FnOnce(&mut LockedState<'_>) -> Result<T>;
    pub(crate) fn update<F, T>(&self, update: F) -> Result<T>
    where
        F: FnOnce(&mut MaintenanceState) -> Result<T>;
}
```

`transaction` acquires `FileLock`, loads latest state inside the lock, and passes `LockedState` to the closure; the caller invokes `persist()` at each durability boundary. `update` is a wrapper that mutates `locked.state` and persists exactly once after the closure succeeds. Missing state returns `MaintenanceState::default()`. Invalid JSON or version mismatch returns Err and performs zero writes.

- [ ] **Step 5: Add state writer merge and invalid-state tests**

Test that two sequential `update` closures preserve both entries, and malformed JSON causes an error while bytes remain unchanged.

Run: `cargo test session_maintenance::state --lib`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/session_maintenance/mod.rs src/session_maintenance/state.rs
git commit -m "feat(session): add maintenance lifecycle state"
```

---

### Task 5: Safe Recycle, Restore, Purge, and Journal Recovery

**Files:**
- Create: `src/session_maintenance/recycle.rs`
- Modify: `src/session_maintenance/state.rs`
- Modify: `src/session_maintenance/mod.rs`
- Test: `src/session_maintenance/recycle.rs`

**Interfaces:**
- Produces: `MaintenanceRoots::source_root(SessionSource) -> &Path`.
- Produces: `recycle_session`, `restore_session`, `purge_session`, `reconcile_pending`.
- Consumes: `StateStore`, `MaintenanceEntry`, path-security helpers, `fingerprint_file`.

- [ ] **Step 1: Write path and journal tests**

Tests must create temp source roots and config root, then assert:

```rust
#[test]
fn recycle_moves_verified_file_and_records_recycled_state() {
    let fixture = RecycleFixture::new(SessionSource::Codex);
    let entry = fixture.hidden_entry();
    recycle_session(&fixture.store, &fixture.roots, &entry, fixture.now).unwrap();
    assert!(!fixture.source_file.exists());
    assert!(fixture.recycle_file(&entry).exists());
    assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
}

#[test]
fn recycle_rejects_symlink_without_removing_target() {
    let fixture = RecycleFixture::with_symlink(SessionSource::Omp);
    assert!(recycle_session(&fixture.store, &fixture.roots, &fixture.hidden_entry(), fixture.now).is_err());
    assert!(fixture.outside_target.exists());
}

#[test]
fn pending_with_missing_source_and_existing_target_finalizes_recycled() {
    let fixture = RecycleFixture::pending_target_only(SessionSource::Claude);
    reconcile_pending(&fixture.store, &fixture.roots, fixture.now).unwrap();
    assert_eq!(fixture.load_entry().lifecycle, LifecycleState::Recycled);
}
```

Also cover source-only, staging-only, final-only, source+final same, source+final different, and source/staging/final all-missing states. A staging-only state must be promoted to final before marking Recycled.

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test session_maintenance::recycle --lib`

Expected: compile failure because recycle functions are missing.

- [ ] **Step 3: Implement roots and deterministic recycle path**

Define:

```rust
pub(crate) struct MaintenanceRoots {
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub omp: PathBuf,
    pub recycle: PathBuf,
}

impl MaintenanceRoots {
    pub(crate) fn source_root(&self, source: SessionSource) -> &Path {
        match source {
            SessionSource::Claude => &self.claude,
            SessionSource::Codex => &self.codex,
            SessionSource::Omp => &self.omp,
        }
    }
}

pub(crate) fn recycle_relative_path(entry: &MaintenanceEntry) -> PathBuf {
    PathBuf::from(entry.identity.source.as_str())
        .join(&entry.identity.session_id)
        .join(format!("{}.jsonl", entry.fingerprint))
}
```

Validate `session_id` as one safe component before using it. If it contains separators or special components, use a BLAKE3 digest of the ID as the directory component and retain the real ID only in state.

- [ ] **Step 4: Implement transactional recycle**

`recycle_session` must perform exactly:

1. `StateStore::transaction` acquires the maintenance lock and loads latest state.
2. Rebuild source from trusted root + `original_relative_path` using `safe_join_within_root`.
3. `validate_regular_candidate` and fingerprint revalidation.
4. Set `pending_recycle` with source/staging/final relative paths and call `locked.persist()` before touching the source file.
5. Attempt `fs::rename(source, staging)` under the recycle root, then `fs::rename(staging, final)`.
6. On cross-device error from the first rename, copy to `NamedTempFile` under target parent, flush, sync, fingerprint, persist as final, revalidate source, then remove source.
7. Set lifecycle/recycled timestamp, clear pending, and call `locked.persist()` again before releasing the lock.

Use a test-only thread-local force flag for copy fallback because CI cannot reliably create two filesystems:

```rust
#[cfg(test)]
thread_local! {
    static FORCE_COPY_FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
```

- [ ] **Step 5: Implement restore and purge**

`restore_session` must refuse overwrite unless the existing destination fingerprint equals the recycled file; differing content returns conflict and leaves both files.

`purge_session` must remove only the validated recycle file. For Claude, retain the minimal `PurgedLocal` suppression entry. For Codex/OMP, retain audit state with `purged_at`; Task 6 prunes it after 30 additional days.

- [ ] **Step 6: Run recycle and path-security tests**

Run:

```bash
cargo test session_maintenance::recycle --lib
cargo test path_security --lib
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/session_maintenance/mod.rs src/session_maintenance/state.rs src/session_maintenance/recycle.rs
git commit -m "feat(session): add safe recycle transactions"
```

---

### Task 6: Maintenance Orchestrator and Visibility Overlay

**Files:**
- Modify: `src/session_maintenance/mod.rs`
- Modify: `src/handlers/session.rs:718-999,3023-3762`
- Test: `src/session_maintenance/mod.rs`
- Test: `src/handlers/session.rs`

**Interfaces:**
- Produces: `MaintenanceMode::{Disabled, DryRun, Apply}`.
- Produces: `MaintenanceReport`, `VisibilityIndex`.
- Produces: `run_maintenance`, `filter_visible`, `candidate_from_summary`.
- Consumes: scan summaries, completed sources, roots, settings, classifier, state, recycle.

- [ ] **Step 1: Write orchestrator tests**

Cover:

```rust
#[test]
fn incomplete_source_never_advances_destructive_state() {
    let fixture = MaintenanceFixture::hidden_for_days(SessionSource::Codex, 8);
    let completed = HashSet::from([SessionSource::Claude, SessionSource::Omp]);
    let report = run_maintenance(fixture.input(completed), MaintenanceMode::Apply).unwrap();
    assert_eq!(report.recycled, 0);
    assert!(fixture.source_file.exists());
}

#[test]
fn old_candidate_first_run_only_becomes_hidden() {
    let fixture = MaintenanceFixture::old_test_candidate(SessionSource::Claude, 120);
    let report = run_maintenance(fixture.input_all_complete(), MaintenanceMode::Apply).unwrap();
    assert_eq!(report.hidden, 1);
    assert_eq!(report.recycled, 0);
    assert!(fixture.source_file.exists());
}

#[test]
fn action_budget_reports_remaining_work() {
    let fixture = MaintenanceFixture::with_recyclable_sessions(3);
    let mut input = fixture.input_all_complete();
    input.settings.max_actions_per_run = 2;
    let report = run_maintenance(input, MaintenanceMode::Apply).unwrap();
    assert_eq!(report.file_actions, 2);
    assert_eq!(report.remaining_actions, 1);
}

#[test]
fn duplicate_source_identity_and_unknown_profile_are_fail_safe() {
    let duplicate = MaintenanceFixture::duplicate_identity(SessionSource::Claude);
    let report = run_maintenance(duplicate.input_all_complete(), MaintenanceMode::Apply).unwrap();
    assert_eq!(report.file_actions, 0);
    assert!(report.warnings > 0);

    let invalid = MaintenanceFixture::with_classifier("unknown");
    let report = run_maintenance(invalid.input_all_complete(), MaintenanceMode::Apply).unwrap();
    assert_eq!(report.file_actions, 0);
    assert!(report.warnings > 0);
}
```

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test session_maintenance::tests --lib`

Expected: compile failure because orchestrator is missing.

- [ ] **Step 3: Implement candidate construction and clock**

Define:

```rust
pub(crate) trait MaintenanceClock {
    fn now(&self) -> DateTime<Utc>;
}

pub(crate) struct SystemMaintenanceClock;

impl MaintenanceClock for SystemMaintenanceClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
```

`candidate_from_summary` must parse RFC3339 timestamps, compute source-relative path with `safe_relative_path_within_root`, fingerprint the source file, and merge existing `keep`/`explicit_test` flags from state.

- [ ] **Step 4: Implement orchestrator report and visibility**

Define:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceMode {
    Disabled,
    DryRun,
    Apply,
}

pub(crate) struct MaintenanceInput<'a> {
    pub summaries: &'a [SessionSummary],
    pub completed_sources: &'a HashSet<SessionSource>,
    pub roots: &'a MaintenanceRoots,
    pub config_dir: &'a Path,
    pub settings: &'a SessionMaintenanceSettings,
    pub clock: &'a dyn MaintenanceClock,
}

#[derive(Debug, Default)]
pub struct MaintenanceReport {
    pub candidates: usize,
    pub hidden: usize,
    pub recycled: usize,
    pub purged: usize,
    pub restored_visible: usize,
    pub file_actions: usize,
    pub remaining_actions: usize,
    pub warnings: usize,
}

#[derive(Debug, Default)]
pub struct VisibilityIndex {
    pub states: HashMap<SessionIdentity, LifecycleState>,
}
```

`DryRun` performs classification and report generation but does not call `StateStore::update` or recycle operations. `Disabled` only loads visibility already present in state and does not classify or advance.

Before classifying, group summaries by `SessionIdentity`; any duplicate identity is excluded from maintenance and increments `warnings`. Reject an unknown `settings.classifier` value by returning a warning report with zero mutations. Re-run classification when `entry.classifier_version != CLASSIFIER_VERSION`. Prune Codex/OMP `PurgedLocal` audit entries only after `purged_at + 30 days`; never prune Claude suppression entries by age alone. Maintenance logs may contain only source label, reason code, score, lifecycle transition and `blake3` path hash—never title, message body or full path.

- [ ] **Step 5: Overlay default visibility in handlers**

Add a helper in `src/handlers/session.rs`:

```rust
fn visible_summaries(
    summaries: Vec<SessionSummary>,
    visibility: &VisibilityIndex,
    include_hidden: bool,
) -> Vec<SessionSummary> {
    if include_hidden {
        return summaries;
    }
    summaries
        .into_iter()
        .filter(|summary| {
            summary
                .identity()
                .ok()
                .and_then(|identity| visibility.states.get(&identity).copied())
                .map(|state| state == LifecycleState::Visible)
                .unwrap_or(true)
        })
        .collect()
}
```

Apply it to interactive/list/projects/overview only. Do not apply it to search/show in this task.

- [ ] **Step 6: Run tests**

Run:

```bash
cargo test session_maintenance --lib
cargo test handlers::session::tests --lib
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/session_maintenance/mod.rs src/handlers/session.rs
git commit -m "feat(session): apply maintenance lifecycle"
```

---

### Task 7: Maintenance CLI, Explain, Keep, and Markers

**Files:**
- Modify: `src/main.rs:357-369,505-614,1100-1177`
- Modify: `src/handlers/session.rs:3239-3762,4911-5064`
- Modify: `src/handlers/mod.rs:37-43`
- Test: `src/main.rs`
- Test: `src/handlers/session.rs`

**Interfaces:**
- Produces handlers: `handle_session_maintain`, `handle_session_explain`, `handle_session_keep`, `handle_session_mark_test`.
- Consumes: maintenance orchestrator/state store.

- [ ] **Step 1: Add failing clap parsing tests**

Add cases to `src/main.rs` tests for:

```text
ccs session maintain --enable
ccs session maintain --disable
ccs session maintain --status
ccs session maintain --dry-run
ccs session maintain --run
ccs session explain abc --json
ccs session keep abc
ccs session unkeep abc
ccs session mark-test abc
ccs session unmark-test abc
ccs session list --include-hidden
ccs session search test --active-only
```

Assert mutually exclusive maintain actions are rejected.

- [ ] **Step 2: Run CLI tests and verify RED**

Run: `cargo test main::tests --bin ccs`

Expected: parsing failures for new subcommands/options.

- [ ] **Step 3: Add exact clap variants**

Extend `SessionAction` with:

```rust
Maintain {
    #[arg(long, conflicts_with_all = ["disable", "status", "dry_run", "run"])]
    enable: bool,
    #[arg(long, conflicts_with_all = ["enable", "status", "dry_run", "run"])]
    disable: bool,
    #[arg(long, conflicts_with_all = ["enable", "disable", "dry_run", "run"])]
    status: bool,
    #[arg(long, conflicts_with_all = ["enable", "disable", "status", "run"])]
    dry_run: bool,
    #[arg(long, conflicts_with_all = ["enable", "disable", "status", "dry_run"])]
    run: bool,
},
Explain {
    session_id: String,
    #[arg(long)]
    json: bool,
},
Keep { session_id: String },
Unkeep { session_id: String },
MarkTest { session_id: String },
UnmarkTest { session_id: String },
```

Add global `--include-hidden` to `Commands::Session` so it works before or after subcommands. Add `active_only: bool` to Search.

- [ ] **Step 4: Implement handlers**

`handle_session_maintain` updates `FilterConfig.session_maintenance.enabled` only for enable/disable, calls dry-run/apply for the corresponding action, and prints counts including `remaining_actions`.

`handle_session_explain` resolves source ambiguity with existing `resolve_session_by_id`, then returns lifecycle, score, reason codes, hidden_since and next transition. JSON output must include existing scan diagnostics.

`handle_session_keep(session_id, keep, source)` and `handle_session_mark_test(session_id, marked, source)` must:

1. perform complete mutation scan;
2. resolve one source-qualified identity;
3. lock/reload latest state;
4. update flag;
5. restore hidden/recycled session when setting keep;
6. avoid immediate hide when unsetting.

- [ ] **Step 5: Re-export and dispatch**

Update `src/handlers/mod.rs` and `src/main.rs` dispatch. Preserve existing action update-check skip behavior at `src/main.rs:1277-1300`; all new local session actions must also skip network update checks.

- [ ] **Step 6: Run CLI and handler tests**

Run:

```bash
cargo test --bin ccs
cargo test handlers::session::tests --lib
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs src/handlers/session.rs src/handlers/mod.rs
git commit -m "feat(session): add maintenance commands"
```

---

### Task 8: Recycled Search, Show, Visibility Output, and Three-Source Restore

**Files:**
- Modify: `src/session_maintenance/mod.rs`
- Modify: `src/handlers/session.rs:3239-3307,3569-3762,3800-3899,4584-4839,4911-5064`
- Modify: `src/main.rs`
- Test: `src/handlers/session.rs`
- Test: `tests/session_maintenance_cli_tests.rs`

**Interfaces:**
- Produces: `load_recycled_summaries`, `maintenance_state_for`.
- Extends: `handle_session_restore_with_source` to local maintenance for all sources before Claude sync fallback.

- [ ] **Step 1: Add recycled summary parser tests**

Create fixtures by source, move them to recycle layout, and assert:

```rust
let summaries = load_recycled_summaries(&roots, &state, SessionSourceFilter::All).unwrap();
assert_eq!(summaries.len(), 3);
assert_eq!(summaries.iter().map(|s| s.source.as_str()).collect::<Vec<_>>(), vec!["claude", "codex", "omp"]);
```

Add search assertion that a keyword found only in a recycled file is returned with `visibility: "recycled"`.

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test recycled --lib`

Expected: failure because recycled scanning is missing.

- [ ] **Step 3: Implement source-dispatched recycled parsing**

For each recycled state entry:

- Claude: `ConversationSession::from_file`, then `SessionSummary::from_session` using stored project name and the original project directory reconstructed from the current Claude root plus `original_relative_path.parent()`.
- Codex: `CodexSession::from_file`, then `SessionSummary::from_codex_session` with stored project name/title fallback；`project_dir` 继续来自回收 JSONL 内的 cwd。
- OMP: `OmpSession::from_file`, then `SessionSummary::from_omp_session` with stored project name；`project_dir` 继续来自回收 JSONL 内的 cwd。

After summary construction, overwrite `file_path` with recycle path and preserve source/session identity from state. Invalid recycled files produce a bounded maintenance warning and do not abort other entries.

- [ ] **Step 4: Integrate query semantics**

- list/projects/overview/interactive: active summaries plus recycled only when `--include-hidden`.
- search: always append recycled unless `--active-only`; do not filter active hidden summaries.
- show: append recycled before `resolve_session_by_id`.
- JSON search/show results: add `visibility` without changing `schema_version = 1`; new field is additive.
- Text output: prefix hidden/recycled rows with `[hidden]` or `[recycled]`.

- [ ] **Step 5: Extend restore order**

Implement exact order:

1. maintenance hidden → Visible + keep;
2. maintenance recycled → `restore_session` + keep;
3. Claude with no maintenance copy → existing sync repo restore;
4. Codex/OMP with no maintenance copy → error `No local recycled copy is available for CX/OM session <id>`.

Remove `ensure_restore_source_supported` from the maintenance paths; retain Claude-only guard only around sync-repo fallback.

- [ ] **Step 6: Add real CLI tests**

In `tests/session_maintenance_cli_tests.rs`, use `env!("CARGO_BIN_EXE_ccs")`, temp HOME/USERPROFILE/source roots, and `CLAUDE_CODE_SYNC_CONFIG_DIR`. Cover text and JSON search/show, source ambiguity, include-hidden, active-only, and restore for all three sources.

Run:

```bash
cargo test --test session_maintenance_cli_tests
cargo test handlers::session::tests --lib
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/session_maintenance/mod.rs src/handlers/session.rs src/main.rs tests/session_maintenance_cli_tests.rs
git commit -m "feat(session): query and restore recycled sessions"
```

---

### Task 9: Claude Pull/Push Suppression Without Automatic Tombstones

**Files:**
- Modify: `src/session_maintenance/mod.rs`
- Modify: `src/session_maintenance/state.rs`
- Modify: `src/session_model.rs`
- Modify: `src/sync/pull.rs:49-123,241-323,674-780`
- Modify: `src/sync/push.rs:180-240,820-929`
- Test: `src/sync/pull.rs`
- Test: `src/sync/push.rs`
- Test: `tests/integration_sync_tests.rs`

**Interfaces:**
- Produces: `suppression_for_remote(identity, fingerprint) -> SuppressionDecision`.
- Produces: `is_suppressed_missing_session(relative: &Path) -> bool`.
- Consumes: maintenance state and remote file fingerprints.

- [ ] **Step 1: Write pull suppression tests**

Add tests proving:

```rust
assert_eq!(
    suppression_for_remote(&state, &identity, "same"),
    SuppressionDecision::SkipSameRevision
);
assert_eq!(
    suppression_for_remote(&state, &identity, "changed"),
    SuppressionDecision::RestoreNewRevision
);
```

Add an integration fixture where a Claude session is recycled locally and unchanged remotely; pull must not recreate it. Change remote bytes under the same ID; pull must restore it and clear suppression.

- [ ] **Step 2: Write push partition tests**

Test three policies:

- Protect: suppressed missing stays remote and does not produce accidental-loss warning count.
- PruneUnlock: suppressed missing stays remote.
- PruneManual: suppressed missing is included and deleted because user explicitly requested `--prune`.

- [ ] **Step 3: Run tests and verify RED**

Run:

```bash
cargo test sync::pull::tests --lib
cargo test sync::push::tests --lib
```

Expected: failures because suppression APIs are missing.

- [ ] **Step 4: Implement typed suppression decisions**

Define:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressionDecision {
    NotSuppressed,
    SkipSameRevision,
    RestoreNewRevision,
}
```

Only Claude entries in `Recycled` or `PurgedLocal` participate. State load failure returns `NotSuppressed` so pull restores data rather than risking loss.

- [ ] **Step 5: Integrate pull before conflict detection and merge**

After remote discovery, fingerprint each remote Claude file that has a matching maintenance entry:

- `SkipSameRevision`: remove from the remote merge input and increment a local suppression count.
- `RestoreNewRevision`: clear maintenance suppression under lock, keep remote session in merge input.
- fingerprint failure: keep remote session in merge input and log a safe warning.

Do not modify tombstone propagation.

- [ ] **Step 6: Partition push missing sessions by explicit policy**

Add a shared Claude filename helper in `src/session_model.rs` and use it from both pull tombstone propagation and push suppression:

```rust
pub(crate) fn claude_session_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    Some(
        name.strip_suffix(".jsonl")?
            .trim_start_matches("session-")
            .to_string(),
    )
}
```

After `collect_missing_repo_sessions`, partition by source-qualified identity rather than relative path layout:

```rust
let (suppressed_missing, ordinary_missing): (Vec<_>, Vec<_>) = missing_in_repo
    .into_iter()
    .partition(|relative| {
        claude_session_id_from_path(relative)
            .map(|session_id| maintenance.is_suppressed_missing_session(&session_id))
            .unwrap_or(false)
    });
```

This works for both encoded local-project layout and `use_project_name_only` sync layout because it depends only on the Claude filename ID.

Use:

- `MissingAction::Protect`: process only ordinary missing; retain suppressed remote files.
- `MissingAction::PruneUnlock`: prune only ordinary missing; retain suppressed remote files.
- `MissingAction::PruneManual`: prune both lists.

State load failure must classify nothing as suppressed, preserving existing protection behavior.

- [ ] **Step 7: Run sync tests**

Run:

```bash
cargo test sync::pull::tests --lib
cargo test sync::push::tests --lib
cargo test --test integration_sync_tests
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/session_maintenance/mod.rs src/session_maintenance/state.rs src/session_model.rs src/sync/pull.rs src/sync/push.rs tests/integration_sync_tests.rs
git commit -m "feat(session): suppress recycled Claude revisions"
```

---

### Task 10: Cross-Process Concurrency and Crash-Safety Integration Tests

**Files:**
- Create: `tests/session_maintenance_concurrency_tests.rs`
- Modify: `src/session_maintenance/state.rs`
- Modify: `src/session_maintenance/recycle.rs`
- Test: `tests/session_maintenance_concurrency_tests.rs`

**Interfaces:**
- Consumes public CLI and test-only debug gates.
- Produces no new production behavior except deterministic test gates under `#[cfg(debug_assertions)]`.

- [ ] **Step 1: Add test-only gates**

Mirror existing cache gate style with environment variables:

```text
CCS_TEST_MAINTENANCE_LOCK_READY
CCS_TEST_MAINTENANCE_LOCK_RELEASE
CCS_TEST_MAINTENANCE_AFTER_PENDING_READY
CCS_TEST_MAINTENANCE_AFTER_PENDING_RELEASE
CCS_TEST_MAINTENANCE_FORCE_COPY
```

Gates compile only under `debug_assertions`, wait at most 30 seconds, and return explicit timeout errors.

- [ ] **Step 2: Write dual-writer state test**

Spawn two `ccs session mark-test` processes against different sessions and the same config dir. Hold the first process after lock acquisition, start the second, release, then assert both source-qualified entries exist in valid JSON.

- [ ] **Step 3: Write restore-vs-recycle race test**

Hold maintenance after pending journal persistence. Start restore for the same session. Release maintenance and assert exactly one valid final state:

- active source exists + keep true; or
- recycle file exists + Recycled state.

The forbidden result is both files missing.

- [ ] **Step 4: Write atomic reader stress test**

While one process repeatedly toggles mark/unmark and keep/unkeep, repeatedly read `session-maintenance.json` and deserialize it. Assert no partial JSON across at least 200 reads.

- [ ] **Step 5: Run concurrency tests repeatedly**

Run:

```bash
cargo test --test session_maintenance_concurrency_tests -- --nocapture
cargo test --test session_maintenance_concurrency_tests -- --nocapture
cargo test --test session_maintenance_concurrency_tests -- --nocapture
```

Expected: all three runs PASS without hangs.

- [ ] **Step 6: Commit**

```bash
git add src/session_maintenance/state.rs src/session_maintenance/recycle.rs tests/session_maintenance_concurrency_tests.rs
git commit -m "test(session): verify maintenance concurrency"
```

---

### Task 11: Documentation, Problem Record, and Capability Matrix

**Files:**
- Modify: `README.md:138-204`
- Modify: `docs/user-guide.md:376-534`
- Modify: `CLAUDE.md`
- Modify: `local/notes.md`

**Interfaces:**
- Documents all commands, defaults, local-only purge semantics, query visibility, and source capability boundaries.

- [ ] **Step 1: Update README command examples**

Document:

```bash
ccs session maintain --enable
ccs session maintain --status
ccs session maintain --dry-run
ccs session list --include-hidden
ccs session search "keyword" --active-only
ccs session explain <session-id> --json
ccs session keep <session-id>
ccs session restore <session-id>
```

State explicitly that maintenance defaults to disabled and automatic maintenance runs lazily during session commands only after enablement.

- [ ] **Step 2: Update source capability matrix**

Use columns:

```text
来源 | 查询 | 打开 | 重命名 | 显式删除 | 本地维护 | 参与同步
Claude Code | ✅ | ✅ | ✅ | ✅ | ✅ | ✅
Codex       | ✅ | ❌ | ❌ | ❌ | ✅ | ❌
OMP         | ✅ | ✅ | ❌ | ❌ | ✅ | ❌
```

Explain that Codex/OMP local maintenance does not grant general delete capability.

- [ ] **Step 3: Document lifecycle and search semantics**

Document exact defaults: 24h hide gate, 7d recycle, 30d local purge, action budget 50, search includes hidden/recycled, list/overview hides them by default. Update session index cache documentation from version 3 to version 4 and explain that v4 adds the custom-title protection bit.

Explain Claude suppression: automatic maintenance creates no tombstone; `purged_local` is local; explicit `delete` or manual `push --prune` remains the only remote-destruction path.

- [ ] **Step 4: Update project architecture/testing instructions**

Add `session_maintenance/` to module structure. Add tests for classifier, lifecycle, recycle journal, suppression and three-source restore. Preserve the requirement to use `CLAUDE_CODE_SYNC_CONFIG_DIR` + `#[serial]`.

- [ ] **Step 5: Add `local/notes.md` entry**

Use the required format with date `2026-08-08`, including:

- 问题描述：测试/fixture/短会话污染默认列表；
- 根本原因：三来源只有查询聚合，没有可解释生命周期；
- 解决方案：保守分类、local registry、journal recycle、sync suppression；
- 影响范围：session/query/sync/config；
- 预防措施：source-qualified identity、degraded fail-safe、explicit tombstone separation。

- [ ] **Step 6: Verify docs contain no stale read-only statement**

Run:

```bash
rg -n "Codex.*只读|OMP.*只读|仅 Claude.*Restore|session maintain|include-hidden|active-only|purged_local" README.md docs/user-guide.md CLAUDE.md local/notes.md
```

Expected: old blanket “Codex/OMP 完全只读” statements are replaced with precise “普通 rename/delete 只读，本地维护可写” wording.

- [ ] **Step 7: Commit**

```bash
git add README.md docs/user-guide.md CLAUDE.md local/notes.md
git commit -m "docs: document session maintenance"
```

---

### Task 12: Full Verification, Simplification Review, and Release Readiness

**Files:**
- Modify only files required by findings from verification.

**Interfaces:**
- Verifies the complete feature against the approved design.

- [ ] **Step 1: Format and check formatting**

Run:

```bash
cargo fmt
cargo fmt --check
```

Expected: PASS.

- [ ] **Step 2: Run focused feature tests**

Run:

```bash
cargo test session_maintenance --lib
cargo test --test session_maintenance_cli_tests
cargo test --test session_maintenance_concurrency_tests
cargo test sync::pull::tests --lib
cargo test sync::push::tests --lib
```

Expected: PASS.

- [ ] **Step 3: Run full test suite**

Run:

```bash
cargo test
```

Expected: PASS with zero failing tests.

- [ ] **Step 4: Run clippy with warnings denied**

Run:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: PASS with zero warnings.

- [ ] **Step 5: Perform real dry-run against isolated copied fixtures**

Create temp copies of representative CC/CX/OM session files, set `HOME`, `USERPROFILE`, and `CLAUDE_CODE_SYNC_CONFIG_DIR` to temp roots, then run:

```bash
ccs session maintain --dry-run
ccs session maintain --enable
ccs session maintain --run
ccs session list --include-hidden
ccs session search "test"
```

Expected: dry-run produces no maintenance state; enabled run hides only qualified old fixtures; search still finds hidden/recycled content.

- [ ] **Step 6: Review diff for reuse and unnecessary complexity**

Check:

```bash
git diff --stat master...HEAD
git diff --check master...HEAD
rg -n "SessionCacheLock|persist_atomic_unlocked|FileLock|persist_json_atomic" src
rg -n "session_id.*HashMap|HashMap.*session_id" src/session_maintenance src/sync
```

Expected: one shared atomic helper; maintenance state indexes use source-qualified keys; no copied cache lock implementation.

- [ ] **Step 7: Verify git status and commit final fixes if any**

Run: `git status --short`

Expected: clean. If verification required code fixes, commit only those verified changes:

```bash
git add -u
git commit -m "fix(session): finalize maintenance safety"
```

- [ ] **Step 8: Record final evidence**

Append final commands and results to the 2026-08-08 `local/notes.md` entry, then commit:

```bash
git add local/notes.md
git commit -m "docs: record session maintenance verification"
```
