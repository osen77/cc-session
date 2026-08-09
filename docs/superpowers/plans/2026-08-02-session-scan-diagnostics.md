# Session Scan Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 Claude/Codex/OMP 会话扫描中的静默跳过转换为可统计、可脱敏、可关联 invocation ID、可供文本用户和 JSON Agent 消费的诊断结果。

**Architecture:** 新增独立 `session_diagnostics` 模块承载诊断类型、稳定 path hash 和文本/JSON公共表示；扫描器返回 `SessionScanResult`，原 `Vec<SessionSummary>` wrapper 保留兼容。`SessionRoots` 与显式 config dir 注入使三来源扫描可用 tempfile 集成测试；cache 增加 status/result API但不改变失效、prune 或 schema 行为。

**Tech Stack:** Rust 2021、serde、serde_json、uuid、walkdir、现有 DualLogger/SessionIndexCache、tempfile、serial_test。

## Global Constraints

- 不 commit、不 push。
- 不实现 `session doctor`、Hook 日志统一、cache source-aware retention、mtime/fingerprint、原子 cache save、list `--json`、搜索索引或 streaming parser。
- 保持 `CACHE_VERSION`、`CachedEntry` schema 和现有 JSON业务字段；JSON 只新增 `schema_version` 与 `diagnostics`。
- 原 `scan_all_session_summaries(...) -> Result<Vec<SessionSummary>>` 保留；新代码使用 `_with_report`。
- 根目录不存在且来源尚未使用不是 warning；目录存在但不可读、WalkDir entry 错误、metadata/read/parse/cache 错误必须记录。
- 单文件 data error 不阻断其他文件；根目录/config dir 无法解析可返回 fatal `Err`。
- diagnostics 不包含完整路径、session title、消息正文或 tool 内容。
- path 使用稳定 FNV-1a 64-bit hash，格式 `p-<16 lowercase hex>`；该 hash 只用于关联，不作为安全加密声明。
- warning error text 必须移除原始文件路径并复用日志脱敏规则。
- warnings 最多保留 100 条；额外 warning 只增加 `suppressed_warnings`。
- `diagnostic_id` 复用当前 DualLogger invocation ID；logger 未初始化的单元测试使用新的 `I-XXXXXXXX`。
- 文本正常路径保持静默；仅 `diagnostics.degraded()` 时向 stderr 输出一条聚合 warning。
- JSON 输出始终包含 diagnostics，便于 Agent 判断扫描是否完整。
- 测试不得读写真实配置或真实 session roots。

---

## File Structure

- Create: `src/session_diagnostics.rs`
  - 诊断类型、计数、warning cap、稳定 path hash、error 脱敏、文本聚合。
- Modify: `src/lib.rs`
  - 导出 `pub mod session_diagnostics`。
- Modify: `src/logger.rs`
  - 保存并暴露当前 invocation ID；将脱敏 helper 提升为 `pub(crate)`。
- Modify: `src/session_cache.rs`
  - 增加 `load_with_status` / `save_with_result`，保留旧 wrappers。
- Modify: `src/handlers/session.rs`
  - 新增 `SessionRoots`、`SessionScanResult` 扫描入口。
  - 三来源 scanner 注入 roots/config/diagnostics 并记录计数和 warning。
  - list/search/show/projects/overview 消费 report。
- Test: `src/session_diagnostics.rs` tests
- Test: `src/session_cache.rs` tests
- Test: `src/handlers/session.rs` tests
- Create: `tests/session_scan_diagnostics_tests.rs`
  - tempfile 三来源 scanner/cache/损坏文件集成测试。
- Modify: `README.md`、`docs/user-guide.md`、`local/notes.md`

---

### Task 1: 诊断类型、稳定 hash 与 invocation 关联

**Files:**
- Create: `src/session_diagnostics.rs`
- Modify: `src/lib.rs`
- Modify: `src/logger.rs`
- Test: `src/session_diagnostics.rs`
- Test: `src/logger.rs`

**Interfaces:**
- Produces:
  - `pub const SCAN_DIAGNOSTICS_SCHEMA_VERSION: u32 = 1`
  - `pub const MAX_SCAN_WARNINGS: usize = 100`
  - `pub enum ScanWarningCategory { Io, Data, Cache }`
  - `pub struct ScanWarning`
  - `pub struct ScanDiagnostics`
  - `pub fn stable_path_hash(path: &Path) -> String`
  - `pub fn current_invocation_id() -> Option<&'static str>` in logger

- [ ] **Step 1: 写稳定 hash 失败测试**

```rust
#[test]
fn stable_hash_is_deterministic_and_hides_path() {
    let path = Path::new("/Users/example/private/project/session.jsonl");
    let first = stable_path_hash(path);
    let second = stable_path_hash(path);
    assert_eq!(first, second);
    assert!(first.starts_with("p-"));
    assert_eq!(first.len(), 18);
    assert!(!first.contains("example"));
    assert_ne!(first, stable_path_hash(Path::new("/Users/example/other.jsonl")));
}
```

- [ ] **Step 2: 写 diagnostics 计数、cap 和序列化失败测试**

```rust
#[test]
fn diagnostics_caps_warning_details_and_counts_suppressed() {
    let mut diagnostics = ScanDiagnostics::with_id("I-TEST0001");
    for index in 0..105 {
        diagnostics.record_warning(
            Some("claude"),
            "parse",
            ScanWarningCategory::Data,
            Some(Path::new(&format!("/private/{index}.jsonl"))),
            "invalid JSON in /private/file.jsonl token=secret",
        );
    }

    assert_eq!(diagnostics.warnings.len(), 100);
    assert_eq!(diagnostics.suppressed_warnings, 5);
    assert!(diagnostics.degraded());
    let json = serde_json::to_value(&diagnostics).unwrap();
    assert_eq!(json["diagnostic_id"], "I-TEST0001");
    assert_eq!(json["suppressed_warnings"], 5);
    let text = serde_json::to_string(&diagnostics).unwrap();
    assert!(!text.contains("/private/"));
    assert!(!text.contains("token=secret"));
}
```

- [ ] **Step 3: 写聚合摘要失败测试**

```rust
#[test]
fn summary_line_is_compact_and_path_free() {
    let mut diagnostics = ScanDiagnostics::with_id("I-TEST0002");
    diagnostics.malformed_files = 2;
    diagnostics.io_errors = 3;
    diagnostics.cache_errors = 1;
    assert_eq!(
        diagnostics.summary_line(),
        "Session scan incomplete: 2 malformed, 3 I/O, 1 cache error. Diagnostic ID: I-TEST0002"
    );
}
```

- [ ] **Step 4: 运行 RED**

```bash
cargo test session_diagnostics::tests -- --nocapture
```

Expected: module/types 不存在。

- [ ] **Step 5: 实现 FNV-1a hash**

```rust
pub fn stable_path_hash(path: &Path) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("p-{hash:016x}")
}
```

- [ ] **Step 6: 实现诊断类型**

```rust
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanWarningCategory {
    Io,
    Data,
    Cache,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScanWarning {
    pub source: Option<String>,
    pub operation: String,
    pub category: ScanWarningCategory,
    pub path_hash: Option<String>,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanDiagnostics {
    pub schema_version: u32,
    pub diagnostic_id: String,
    pub files_seen: usize,
    pub files_parsed: usize,
    pub files_skipped: usize,
    pub malformed_files: usize,
    pub io_errors: usize,
    pub cache_errors: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub bytes_considered: u64,
    pub elapsed_ms: u64,
    pub warnings: Vec<ScanWarning>,
    pub suppressed_warnings: usize,
}
```

`with_id` 初始化全部计数为 0。`new()` 使用：

```rust
let id = crate::logger::current_invocation_id()
    .map(str::to_string)
    .unwrap_or_else(|| format!("I-{}", &Uuid::new_v4().simple().to_string()[..8].to_uppercase()));
```

避免临时字符串切片生命周期错误：先保存 UUID string 再 slice。

`record_warning`：

- category 对应增加 `io_errors`/`malformed_files`/`cache_errors`；
- path 转 hash；
- error 中先替换传入 path 为 `<path>`；
- 调用 `crate::logger::sanitize_log_message(..., dirs::home_dir().as_deref())`；
- cap 100；
- 使用 `log::warn!` 记录 source/operation/category/path_hash/sanitized error，不记录原路径。

- [ ] **Step 7: 暴露 logger invocation ID**

`src/logger.rs` 增加：

```rust
static CURRENT_INVOCATION_ID: OnceLock<String> = OnceLock::new();

pub fn current_invocation_id() -> Option<&'static str> {
    CURRENT_INVOCATION_ID.get().map(String::as_str)
}
```

`init_logger_with_options` 在 `log::set_boxed_logger` 成功后设置：

```rust
let _ = CURRENT_INVOCATION_ID.set(invocation_id.clone());
```

`sanitize_log_message` 改为 `pub(crate)`，不改变行为。

测试：global logger 未初始化时 `ScanDiagnostics::new()` 仍生成 `I-XXXXXXXX`；已存在 options 的纯测试不要求注册全局 logger。

- [ ] **Step 8: 运行 tests**

```bash
cargo test session_diagnostics::tests logger::tests -- --nocapture
```

若 cargo 不接受两个 filter，分别运行两个命令。Expected: PASS。

---

### Task 2: Cache status API 与可注入 SessionRoots

**Files:**
- Modify: `src/session_cache.rs`
- Modify: `src/handlers/session.rs`
- Test: `src/session_cache.rs`
- Test: `src/handlers/session.rs`

**Interfaces:**
- Consumes: Task 1 `ScanDiagnostics`。
- Produces:
  - `pub struct CacheLoadStatus`
  - `SessionIndexCache::load_with_status`
  - `SessionIndexCache::save_with_result`
  - `pub(crate) struct SessionRoots`
  - `SessionRoots::discover()`

- [ ] **Step 1: 写 cache status 失败测试**

```rust
#[test]
fn load_with_status_distinguishes_missing_and_corrupt_cache() {
    let temp = tempfile::tempdir().unwrap();
    let missing = SessionIndexCache::load_with_status(temp.path());
    assert!(missing.warning.is_none());

    std::fs::write(temp.path().join("session_index.json"), b"not-json").unwrap();
    let corrupt = SessionIndexCache::load_with_status(temp.path());
    assert!(corrupt.warning.as_deref().unwrap().contains("corrupt"));
    assert!(corrupt.cache.entries.is_empty());
}
```

- [ ] **Step 2: 写 save error 失败测试**

在 Unix 下把 config path 指向普通文件的子路径，调用 `save_with_result` 必须 Err 且带 path；Windows 用已存在普通文件作为 parent 的跨平台场景。

- [ ] **Step 3: 实现兼容 cache API**

```rust
pub struct CacheLoadStatus {
    pub cache: SessionIndexCache,
    pub warning: Option<String>,
}
```

`load_with_status`：

- NotFound => empty cache, warning None；
- 其他 read error => empty + warning；
- JSON corrupt/version mismatch => empty + warning；
- success => cache + None。

兼容：

```rust
pub fn load(config_dir: &Path) -> Self {
    Self::load_with_status(config_dir).cache
}

pub fn save_with_result(&self, config_dir: &Path) -> Result<()>;

pub fn save(&self, config_dir: &Path) {
    if let Err(error) = self.save_with_result(config_dir) {
        warn!("{error:#}");
    }
}
```

不改变 JSON、version、retain 行为。

- [ ] **Step 4: 写 SessionRoots 路径测试**

```rust
#[test]
fn session_roots_can_be_injected() {
    let temp = tempfile::tempdir().unwrap();
    let roots = SessionRoots {
        claude_projects: temp.path().join("claude"),
        codex_sessions: temp.path().join("codex/sessions"),
        codex_history: temp.path().join("codex/history.jsonl"),
        omp_sessions: temp.path().join("omp/sessions"),
    };
    assert!(roots.claude_projects.ends_with("claude"));
}
```

- [ ] **Step 5: 实现 SessionRoots**

放在 `session.rs` 扫描入口附近：

```rust
#[derive(Debug, Clone)]
pub(crate) struct SessionRoots {
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
    pub codex_history: PathBuf,
    pub omp_sessions: PathBuf,
}

impl SessionRoots {
    fn discover() -> Result<Self> {
        Ok(Self {
            claude_projects: claude_projects_dir()?,
            codex_sessions: codex_sessions_dir()?,
            codex_history: codex_history_path()?,
            omp_sessions: omp_sessions_dir()?,
        })
    }
}
```

不新增环境变量，不改变生产 root 语义。

- [ ] **Step 6: 运行 tests**

```bash
cargo test session_cache::tests -- --nocapture
cargo test handlers::session::tests::session_roots -- --nocapture
```

Expected: PASS。

---

### Task 3: 三来源 scanner instrumentation 与 tempfile 集成测试

**Files:**
- Modify: `src/handlers/session.rs:629-934`
- Create: `tests/session_scan_diagnostics_tests.rs`
- Test: `src/handlers/session.rs`

**Interfaces:**
- Consumes: `SessionRoots`、cache status API、`ScanDiagnostics`。
- Produces:
  - `pub struct SessionScanResult { pub summaries, pub diagnostics }`
  - `fn scan_all_session_summaries_with_report(...)`
  - `pub(crate) fn scan_all_session_summaries_with_roots(...)`

- [ ] **Step 1: 定义 result 和兼容 wrapper 测试**

```rust
#[derive(Debug)]
pub struct SessionScanResult {
    pub summaries: Vec<SessionSummary>,
    pub diagnostics: ScanDiagnostics,
}
```

测试 `scan_all_session_summaries` 仍返回 Vec；新 `_with_roots` 返回 diagnostics。

- [ ] **Step 2: 创建三来源 fixture**

`tests/session_scan_diagnostics_tests.rs` 创建：

```text
<temp>/claude/project-valid/valid.jsonl
<temp>/claude/project-valid/broken.jsonl
<temp>/codex/sessions/2026/valid.jsonl
<temp>/codex/sessions/2026/broken.jsonl
<temp>/codex/history.jsonl
<temp>/omp/sessions/valid.jsonl
<temp>/omp/sessions/broken.jsonl
<temp>/config
```

最小有效内容：

Claude：

```json
{"type":"user","sessionId":"cc-1","cwd":"/tmp/demo","timestamp":"2026-08-02T00:00:00Z","message":{"role":"user","content":"hello"}}
```

Codex：

```json
{"timestamp":"2026-08-02T00:00:00Z","type":"session_meta","payload":{"id":"cx-1","cwd":"/tmp/demo"}}
{"timestamp":"2026-08-02T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}
```

OMP：

```json
{"type":"session","version":3,"id":"om-1","timestamp":"2026-08-02T00:00:00Z","cwd":"/tmp/demo","title":"OMP"}
{"type":"message","timestamp":"2026-08-02T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
```

broken 内容用非法 UTF-8 字节或不可解析 JSON。若 parser 对单行 malformed 返回空成功 session，使用非法 UTF-8 确保 `from_file` 返回 Err。

- [ ] **Step 3: 写 cold scan 失败测试**

```rust
#[test]
fn scan_report_counts_three_sources_and_malformed_files() {
    // 构造 roots/config
    let report = scan_all_session_summaries_with_roots(
        None,
        SessionSourceFilter::All,
        &roots,
        config.path(),
    )
    .unwrap();

    assert_eq!(report.summaries.len(), 3);
    assert_eq!(report.diagnostics.files_seen, 6);
    assert_eq!(report.diagnostics.files_parsed, 3);
    assert_eq!(report.diagnostics.cache_misses, 6);
    assert_eq!(report.diagnostics.malformed_files, 3);
    assert!(report.diagnostics.bytes_considered > 0);
    assert!(report.diagnostics.elapsed_ms <= 60_000);
    assert!(report.diagnostics.degraded());
}
```

允许 parser 对某类 broken 文件产生不同计数时，fixture 必须调整到确定 Err，不能放宽断言掩盖行为。

- [ ] **Step 4: 写 warm cache 失败测试**

第一次 scan 后第二次 scan：

```rust
assert_eq!(second.diagnostics.cache_hits, 3);
assert_eq!(second.diagnostics.cache_misses, 3); // 三个 broken 不进入 cache
assert_eq!(second.summaries.len(), 3);
```

- [ ] **Step 5: instrumentation scanner 签名**

```rust
fn scan_claude_summaries_cached(
    root: &Path,
    cache: &mut SessionIndexCache,
    seen_paths: &mut HashSet<String>,
    summaries: &mut Vec<SessionSummary>,
    diagnostics: &mut ScanDiagnostics,
    project_filter: Option<&str>,
) -> Result<()>;
```

Codex 额外接收 `sessions_root`、`history_path`；OMP 接收 root。

- [ ] **Step 6: 记录统一计数**

对每个 JSONL：

- 识别为候选后 `files_seen += 1`；
- metadata success 后 `bytes_considered += len`；
- cache hit => `cache_hits += 1`；
- cache miss => `cache_misses += 1`；
- parse success => `files_parsed += 1`；
- parser Err => `record_warning(Data, operation="parse", path, error)`；
- `FilterConfig::should_include=false` 或无效 summary => `files_skipped += 1`。

WalkDir：不得再 `filter_map(|e| e.ok())`。逐项 match，Err 记录 Io warning；能取得 `error.path()` 时传 path。

metadata/read_dir entry/history load Err 记录 Io/Data warning并继续；根 root 的 `read_dir` 本身失败可返回 Err。

cache load/save warning使用 Cache category。`cache.save_with_result` 失败记录 warning，不阻断 summaries。

- [ ] **Step 7: 新扫描入口**

```rust
fn scan_all_session_summaries_with_report(
    project_filter: Option<&str>,
    source: SessionSourceFilter,
) -> Result<SessionScanResult> {
    let roots = SessionRoots::discover()?;
    let config_dir = ConfigManager::config_dir()?;
    scan_all_session_summaries_with_roots(project_filter, source, &roots, &config_dir)
}
```

`_with_roots`：

- `Instant::now()`；
- `ScanDiagnostics::new()`；
- cache load status warning；
- 按 source scanner；
- 保持现有 `retain_existing` 行为，不在此修 cache source prune；
- save status；
- sort summaries；
- `elapsed_ms = start.elapsed().as_millis()`，饱和转 u64；
- 返回 result。

兼容 wrapper：

```rust
fn scan_all_session_summaries(...) -> Result<Vec<SessionSummary>> {
    Ok(scan_all_session_summaries_with_report(...)?.summaries)
}
```

- [ ] **Step 8: Unix unreadable fixture**

`#[cfg(unix)] #[serial]` 创建 permission 000 文件或目录，scan 后 `io_errors >= 1`；恢复权限后 TempDir 才 drop。若测试以 root 用户运行导致权限测试不可靠，测试 metadata error 的稳定替代 helper，不得写真实路径。

- [ ] **Step 9: 运行 scanner tests**

```bash
cargo test --test session_scan_diagnostics_tests -- --nocapture
cargo test handlers::session::tests -- --nocapture
```

Expected: PASS。

---

### Task 4: Handler 文本 warning 与 JSON diagnostics

**Files:**
- Modify: `src/handlers/session.rs` list/projects/overview/show/search
- Test: `src/handlers/session.rs`
- Test: `tests/session_scan_diagnostics_tests.rs`

**Interfaces:**
- Consumes: `scan_all_session_summaries_with_report`。
- Produces:
  - `fn emit_scan_warning(&ScanDiagnostics)`
  - JSON `schema_version: 1` + `diagnostics`

- [ ] **Step 1: 写文本聚合 helper 测试**

为避免捕获 stderr，纯 helper：

```rust
fn scan_warning_message(diagnostics: &ScanDiagnostics) -> Option<String> {
    diagnostics.degraded().then(|| diagnostics.summary_line())
}
```

测试 clean=None，degraded=Some 且无 path。

- [ ] **Step 2: 实现 stderr 输出**

```rust
fn emit_scan_warning(diagnostics: &ScanDiagnostics) {
    if let Some(message) = scan_warning_message(diagnostics) {
        eprintln!("WARNING: {message}. Run with --debug and inspect the ccs log for details.");
    }
}
```

不要提示尚未实现的 `session doctor`。

- [ ] **Step 3: list/projects 使用 report**

替换 scan 调用：

```rust
let report = scan_all_session_summaries_with_report(...)?;
let sessions = report.summaries;
```

所有正常/空结果 return 前调用 `emit_scan_warning(&report.diagnostics)`。为避免 move 后借用，将 diagnostics 先绑定或解构：

```rust
let SessionScanResult { summaries, diagnostics } = report;
```

- [ ] **Step 4: overview JSON 添加字段**

空和非空 JSON 均加入：

```json
{
  "schema_version": 1,
  "diagnostics": { ...existing ScanDiagnostics fields... }
}
```

text 分支末尾/提前返回前 emit warning。

- [ ] **Step 5: show JSON 三条路径添加字段**

- no messages；
- around not found；
-正常消息。

全部加入相同 `schema_version` 和 diagnostics。text 的 interactive details、empty、around not found、normal output 在 return 前 emit warning。

- [ ] **Step 6: search JSON 添加字段**

空结果和正常结果都加入 schema/diagnostics；text 所有 return 前 emit warning。

- [ ] **Step 7: JSON contract 测试**

不要依赖 stdout 截获大函数；提取：

```rust
fn attach_scan_diagnostics(
    mut payload: serde_json::Value,
    diagnostics: &ScanDiagnostics,
) -> serde_json::Value
```

测试业务字段保留，新增 `schema_version=1`、diagnostic_id/counters/warnings。

- [ ] **Step 8: 运行 tests**

```bash
cargo test handlers::session::tests::scan_warning -- --nocapture
cargo test handlers::session::tests::attach_scan_diagnostics -- --nocapture
cargo test --test session_scan_diagnostics_tests -- --nocapture
cargo test --bin ccs session -- --nocapture
```

Expected: PASS。

---

### Task 5: 文档、notes 与最终验证

**Files:**
- Modify: `README.md`
- Modify: `docs/user-guide.md`
- Modify: `local/notes.md`
- Verify: all Slice 3 files

- [ ] **Step 1: README 说明降级提示**

必须说明：

- 正常扫描保持安静；
- 有损坏/权限/cache 错误时结果仍尽量输出，同时 stderr 给一条聚合 warning；
- JSON `overview/search/show` 新增 `schema_version` 和 `diagnostics`；
- `diagnostic_id` 与文件日志 invocation ID 关联；
- warning 只含 path hash，不含完整路径/会话内容；
- `list --json` 和 `session doctor` 尚未提供。

- [ ] **Step 2: user guide 增加诊断字段表**

记录字段语义：files_seen、files_parsed、files_skipped、malformed_files、io_errors、cache_errors、cache_hits/misses、bytes_considered、elapsed_ms、suppressed_warnings、degraded 判定。

- [ ] **Step 3: local notes**

新增模板章节，问题为 WalkDir/metadata/parser/cache 错误静默导致“不完整但看似成功”；根因、ScanResult 方案、影响和测试预防完整。

- [ ] **Step 4: 目标测试**

```bash
cargo test session_diagnostics::tests -- --nocapture
cargo test session_cache::tests -- --nocapture
cargo test handlers::session::tests -- --nocapture
cargo test --test session_scan_diagnostics_tests -- --nocapture
```

- [ ] **Step 5: 全量 gate**

```bash
cargo fmt
cargo fmt --check
cargo clippy -- -D warnings
cargo test
git diff --check
```

全部 exit 0。

- [ ] **Step 6: 隔离实跑**

使用 integration fixture 或临时 HOME，不污染真实 roots/config。运行 search/show/overview JSON，使用 `jq` 验证：

```text
.schema_version == 1
.diagnostics.diagnostic_id startswith("I-")
.diagnostics.files_seen >= 1
```

另验证 text degraded stderr 只有一条聚合 warning，stdout 业务结果仍存在。

- [ ] **Step 7: scope 检查**

```bash
git status --short
git diff --stat
git diff -- src/session_diagnostics.rs src/lib.rs src/logger.rs src/session_cache.rs src/handlers/session.rs tests/session_scan_diagnostics_tests.rs README.md docs/user-guide.md local/notes.md
```

不得混入 doctor、hooks、cache retention/mtime、list JSON、搜索索引。

---

## Plan Self-Review

### Spec coverage

- 诊断类型、path hash、warning cap、invocation ID：Task 1。
- cache load/save 状态且兼容旧 API：Task 2。
- SessionRoots 注入和三来源统计：Task 2-3。
- WalkDir/metadata/parse/cache 错误可见：Task 3。
- Vec wrapper 兼容：Task 3。
- 文本聚合 warning 和 JSON diagnostics/schema：Task 4。
- tempfile 三来源与 cold/warm cache：Task 3。
- 文档、notes、全 gate：Task 5。
- doctor、Hook、cache correctness、list JSON、索引明确排除。

### Type consistency

- `ScanDiagnostics` 在独立模块定义，scanner 和 handlers 共享。
- `SessionScanResult` 在 session handler 定义，兼容 wrapper只取 summaries。
- `SessionRoots` 与 config dir 均可注入，不新增生产环境变量。
- cache status API 保留 `load/save` 旧签名。
- JSON 通过 `attach_scan_diagnostics` 统一添加字段，避免多个分支 schema 漂移。

### Placeholder scan

计划无 TBD、TODO、未定义接口或模糊“适当错误处理”。排除项均明确列出。
