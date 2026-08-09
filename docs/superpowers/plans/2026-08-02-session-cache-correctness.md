# Session Cache Correctness Implementation Plan

> **For agentic workers:** Execute task-by-task with fresh implementer and independent review. Steps use checkbox syntax. Do not commit or push.

**Goal:** 让 source-specific session 扫描只更新自己完整覆盖的 cache 分区，并通过跨进程锁、锁内 delta merge 和同目录原子替换避免误删、半截 JSON 与并发 lost update。

**Architecture:** Scanner 在锁外产生 `CacheDelta` 与 `CacheRetention`，记录每个来源的 seen paths 和 coverage completeness。最终在独立 lock file 上获取 `fs4` 独占锁，重新加载最新 cache、重新验证 mutation 对应文件状态、只应用当前扫描 delta，并通过 `tempfile::NamedTempFile` 同目录持久化完整 JSON。

**Tech Stack:** Rust 2021、fs4、tempfile、serde、BLAKE3、现有 ScanDiagnostics。

## Global Constraints

- 不 commit、不 push，不安装、不发布。
- 测试只使用 tempfile HOME/config/session/log，不读写真实用户 cache。
- 保留 `SessionIndexCache::load/save/lookup/insert/retain_existing/mtime_secs` 旧签名。
- `CachedEntry` JSON shape、`source: String`、`CACHE_VERSION = 3` 不变。
- 已知 partial/parser error 继续 eviction 当前 key。
- 未选择或扫描不完整的 source 必须 fail-safe 保留。
- cache merge/lock/persist 失败只增加 diagnostics，不丢业务 summaries。
- Codex history title dependency、search index、provider 重构不在本 Slice。
- `tempfile` 从 dev-dependency 提升为 runtime dependency；不引入其他新 crate。

---

### Task 1: Source-aware retention 与 scan delta 纯模型

**Files:**
- Modify: `src/session_cache.rs`
- Test: `src/session_cache.rs`

**Produces:**

```rust
pub(crate) struct CacheFileState {
    pub file_size: u64,
    pub mtime_secs: i64,
    pub content_fingerprint: String,
}

pub(crate) struct CacheUpsert {
    pub key: String,
    pub expected: CacheFileState,
    pub entry: CachedEntry,
}

pub(crate) struct CacheRemoval {
    pub key: String,
    pub expected: CacheFileState,
}

#[derive(Default)]
pub(crate) struct CacheDelta {
    pub upserts: Vec<CacheUpsert>,
    pub removals: Vec<CacheRemoval>,
}

#[derive(Default)]
pub(crate) struct CacheRetention {
    pub seen_by_source: HashMap<String, HashSet<String>>,
    pub completed_sources: HashSet<String>,
}
```

- [ ] 写 RED tests：
  - `source_aware_retention_prunes_only_completed_source`
  - `source_aware_retention_preserves_unselected_sources`
  - `source_aware_retention_preserves_incomplete_source`
  - `source_aware_retention_preserves_unknown_source`
  - `delta_removal_and_upsert_preserve_expected_state`
- [ ] 运行 `cargo test session_cache::tests::source_aware -- --nocapture`，确认接口缺失失败。
- [ ] 实现上述内部类型、构造 helper、`CachedEntry::file_state()`。
- [ ] 实现：

```rust
pub(crate) fn retain_existing_by_source(
    &mut self,
    retention: &CacheRetention,
    confirmed_missing: &HashSet<String>,
)
```

只删除：entry source 在 completed、key 不在该 source seen、key 在 confirmed_missing。
- [ ] 保留旧 `retain_existing` 实现与签名，并在 rustdoc 标注 legacy whole-cache prune；生产 scanner 后续不再调用。
- [ ] 运行 session_cache focused tests 与 Clippy。

---

### Task 2: Locked atomic persist 与锁内 merge

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/session_cache.rs`
- Test: `src/session_cache.rs`

**Produces:**

```rust
pub(crate) fn merge_scan_with_result(
    config_dir: &Path,
    delta: &CacheDelta,
    retention: &CacheRetention,
) -> Result<()>;
```

- [ ] 将 `tempfile = "3.24.0"` 移至 runtime dependencies，dev 不重复声明。
- [ ] 写 RED tests：
  - `atomic_save_roundtrip_leaves_valid_json`
  - `atomic_save_failure_does_not_truncate_target`
  - `merge_applies_disjoint_deltas_to_latest_cache`
  - `merge_skips_stale_upsert_after_file_changes`
  - `merge_skips_stale_removal_after_file_changes`
  - `invalid_cache_rebuild_requires_complete_sources`
  - `read_failed_cache_is_never_overwritten`
- [ ] 实现 `SessionCacheLock`：
  - lock path `session_index.json.lock`
  - `OpenOptions(create/read/write)`
  - Unix 0600
  - `fs4::FileExt::lock`
  - Drop unlock
- [ ] 实现 `persist_atomic_unlocked`：
  - create dir
  - `NamedTempFile::new_in(config_dir)`
  - serialize/write_all/flush/sync_all
  - Unix temp 0600
  - `persist(target)` 原子覆盖
  - 错误上下文不得进入普通 logger；Result 由 scanner安全摘要处理
- [ ] `save_with_result` 改为 lock + atomic persist；旧 `save` 仍固定安全 warning。
- [ ] 实现锁内 `merge_scan_with_result`：
  1. 获取 lock
  2. load latest cache及内部 load kind
  3. invalid/version mismatch 仅在三来源全部 completed时允许重建；read failure不覆盖
  4. 对 delta mutation 重新 stat + fingerprint
  5. current state等于 expected才 upsert/remove；NotFound removal允许删除；其他错误跳过
  6. completed source 的 unseen entry仅在 re-stat明确 NotFound时 prune
  7. atomic persist latest
- [ ] Missing cache允许从 delta创建；future/unknown source始终保留。
- [ ] 运行 cache focused、atomic/merge tests、all-target Clippy。

---

### Task 3: Scanner coverage tracker 与生产接线

**Files:**
- Modify: `src/handlers/session.rs`
- Modify: `src/session_diagnostics.rs`（仅 operation allowlist/可选 merge timing）
- Test: `src/handlers/session.rs`

**Produces:**

```rust
struct SourceScanTracker {
    seen_by_source: HashMap<String, HashSet<String>>,
    started_sources: HashSet<String>,
    incomplete_sources: HashSet<String>,
}
```

- [ ] 写 RED scanner tests：
  - All scan建立三来源cache后，Claude-only scan不删除Codex/OMP
  - 完整Claude root删除一个文件，只prune该Claude entry
  - selected root missing/regular-file/read_dir/WalkDir/metadata/fingerprint error保留旧source entries
  - partial/parser error删除已知bad key，但不影响未选择source
  - concurrent/latest cache中的无关entry在merge后保留
- [ ] `SourceScanTracker::begin/seen/mark_incomplete/retention`：completed = started - incomplete。
- [ ] 每个选中 source begin；未选中不参与 retention。
- [ ] missing root、non-directory、root read_dir、WalkDir entry、entry/file metadata、candidate metadata/fingerprint I/O均 mark incomplete。
- [ ] partial/parser error不标整个source incomplete，但记录 `CacheRemoval(expected)`。
- [ ] clean parser/cache-hit更新 seen；clean miss记录 `CacheUpsert(expected, entry)`。
- [ ] 删除生产尾部 `cache.retain_existing(&seen_paths)` + local整包 save，改为：

```rust
merge_scan_with_result(config_dir, &delta, &tracker.retention())
```

失败用安全 Cache warning（operation `merge` 或 `save`），summaries照常返回。
- [ ] cache merge时间计入 `cache_save_ms` 或新增 additive `cache_merge_ms`；文档保持准确。
- [ ] 运行 session handler focused tests、CLI diagnostics regression。

---

### Task 4: 并发、原子性、文档与最终 gate

**Files:**
- Create: `tests/session_cache_concurrency_tests.rs`
- Modify: `tests/session_scan_diagnostics_tests.rs`
- Modify: `README.md`
- Modify: `docs/user-guide.md`
- Modify: `local/notes.md`

- [ ] 增加隔离 CLI source retention test：
  - 同一临时 HOME/config建 CC/CX/OM
  - All scan建立cache
  - Claude-only、Codex-only依次及并发执行
  - 最终cache同时保留所有未删除来源entry
- [ ] 增加跨进程并发测试：两个 `ccs` 子进程对不同source写同一config；使用marker/barrier而非固定sleep作为唯一同步；最终JSON可解析且两边entry都在。
- [ ] 增加 atomic reader stress：writer重复atomic save，reader循环读取；每次只能解析旧或新完整JSON，0 parse error。
- [ ] 增加 lock blocking test：独立进程/child持有lock，writer在释放前不能完成，释放后成功。
- [ ] 更新文档：source-aware retention、incomplete fail-safe、confirmed NotFound prune、lock/delta merge、atomic replace、Windows persist、cache advisory边界。
- [ ] 更新 notes 按模板记录旧全局 prune/lost update/半截JSON根因与预防。
- [ ] 运行：

```bash
cargo test session_cache::tests -- --nocapture
cargo test handlers::session::tests -- --nocapture
cargo test --test session_scan_diagnostics_tests -- --nocapture
cargo test --test session_cache_concurrency_tests -- --nocapture
cargo fmt --all
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
git diff --check
```

- [ ] 生成 `.superpowers/sdd/cache-correctness-final-report.md`，记录RED/GREEN、并发/原子证据、命令exit、修改文件、concerns。
- [ ] 独立整体审查必须达到 0 Critical/Important；Minor全部修复或明确进入下一Slice。
