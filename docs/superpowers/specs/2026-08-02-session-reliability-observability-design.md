# Session 可靠性与可观测性基础设计

- 日期：2026-08-02
- 状态：已批准，待实施
- 目标：提升 `ccs session` 的性能、可靠性、可观测性与 Agent 使用效率
- 首轮范围：数据安全边界、来源一致性、真实文件日志、扫描诊断、关键回归测试
- 后续迭代：cache 与扫描性能、稳定 JSON API、`session doctor`、来源抽象与搜索索引

## 背景

`ccs session` 已从 Claude Code 单来源会话管理扩展为 Claude Code、Codex、OMP 三来源查询，但核心实现仍保留多个单来源假设：

1. 来源能力散落在字符串判断中，交互入口可能允许对只读来源执行 Rename/Delete；
2. 顶层与子命令重复定义 `--source`，调用位置不同可能得到不同结果；
3. `show` 只以裸 `session_id` 查找，跨来源同 ID 时可能选错会话；
4. 普通 `log::warn!` / `log::debug!` 仅输出到 stderr，宣称存在的文件日志实际上只记录初始化消息；
5. WalkDir、metadata、单文件 parse 等错误大量被跳过，用户最终只看到不完整结果或 `No sessions found`；
6. session cache、交互重复扫描和全文搜索存在明确性能问题，但缺少统一指标，难以判断真实瓶颈；
7. list/show/search 的 JSON 能力和 schema 不一致，Agent 需要解析人类文本或承担不受控的大输出。

本设计采用“基础设施优先、敏捷迭代”路径：先建立安全边界和可观测数据，再根据数据推进性能与架构优化，不在首轮直接重写整个 session 子系统。

## 成功标准

首轮完成后应满足：

1. Codex/OMP 不再通过任何 session 入口被误删或写入不兼容的 Rename 记录；
2. `--source` 在顶层和子命令位置具有一致、可测试的语义；
3. 跨来源同 `session_id` 不再静默选取第一个结果；
4. 普通 `log` 记录确实写入平台日志文件，日志轮转和初始化失败可见；
5. session 扫描可以报告 seen/parsed/skipped/malformed/I/O/cache hit/miss 等统计；
6. 用户在扫描降级时能看到简短警告和诊断 ID，JSON 消费者能获得结构化 diagnostics；
7. 日志不记录消息正文、标题、tool input/output、token 或完整私人路径；
8. 新增行为有隔离临时目录的单元或集成测试，不读取真实用户 session/config；
9. 不改变正常成功路径的核心展示结果，新增诊断默认低噪音。

## 明确排除

首轮不做：

- 一次性拆分完整的 `src/handlers/session.rs`；
- 立即引入 SQLite FTS 或其他全文搜索引擎；
- 将整个项目迁移到 `tracing`；
- 自动上传遥测或日志；
- 记录会话正文以辅助排错；
- 直接重写 Claude/Codex/OMP parser；
- 默认把单个损坏 session 升级为整个命令失败。

## 路径选择

### 方案 A：基础设施优先（采用）

先修安全边界和来源身份，再建立真实文件日志与扫描报告。之后基于 cache 命中率、解析字节数和阶段耗时选择性能优化。

优点：每轮可验证、回归面可控、能为后续性能工作提供证据。缺点：首轮会同时触及 CLI、logger、session scan 和测试，但每个改动都能独立提交与回滚。

### 方案 B：补丁式快速修复（不采用）

逐项修复 Delete、`--source`、JSON limit 和 cache prune，只增加零散 warn 日志。

优点：首批代码最少。缺点：错误仍无法汇总，新增来源继续依赖散落分派，性能优化仍靠猜测。

### 方案 C：架构重构优先（不采用）

先建立完整 `SessionProvider` trait、统一 DTO、统一 cache 和 memory 接口，再迁移三来源。

优点：长期结构最清晰。缺点：第一轮范围过大，难以敏捷验证，可能同时改变过多用户行为。

## 总体架构

```text
CLI parse
  → SessionSource 解析
  → SessionIdentity / SourceCapabilities
  → scan_all_session_summaries_with_report
      → source roots
      → WalkDir / metadata / filter
      → SessionIndexCache
      → source parser
      → SessionSummary
      → ScanDiagnostics
  → list / show / search / projects / overview
  → text output + stderr warning
    或 JSON payload + diagnostics

所有 log::Record
  → stderr sink（受 RUST_LOG / --debug 控制）
  → platform file sink（受隐私与轮转规则控制）
```

## 组件设计

### 1. 强类型来源和能力边界

继续复用现有 `SessionSourceFilter`，新增单一来源枚举：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum SessionSource {
    Claude,
    Codex,
    Omp,
}
```

新增能力：

```rust
pub struct SourceCapabilities {
    pub can_open: bool,
    pub can_rename: bool,
    pub can_delete: bool,
    pub participates_in_sync: bool,
}

impl SessionSource {
    pub fn capabilities(self) -> SourceCapabilities;
    pub fn as_str(self) -> &'static str;
    pub fn label(self) -> &'static str;
}
```

首轮能力矩阵：

| 来源 | 查看 | 打开 | 重命名 | 删除 | 同步 |
|---|---:|---:|---:|---:|---:|
| Claude | 是 | 是 | 是 | 是 | 是 |
| Codex | 是 | 否 | 否 | 否 | 否 |
| OMP | 是 | 是 | 否 | 否 | 否 |

所有交互菜单和非交互 mutation handler 必须消费能力表，不再自行判断字符串。

### 2. 会话身份

新增：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionIdentity {
    pub source: SessionSource,
    pub session_id: String,
}
```

规则：

- 内部索引以 `(source, session_id)` 为主键；
- `show` 指定单一 source 时直接查找；
- `source=all` 且裸 ID 只有一个候选时保持兼容；
- 有多个候选时返回明确错误并列出 `source + project + id`；
- Rename/Delete 只允许 Claude，非 Claude 返回可理解的只读来源错误。

### 3. `--source` 语义

顶层 global `--source` 与子命令重复定义会造成默认值覆盖。首轮选择以下兼容策略：

- 顶层保留 global `--source`；
- 子命令不再重复声明 source；
- `ccs session --source codex list` 与 `ccs session list --source codex` 均由 clap global 参数解析为同一字段；
- 增加 CLI parser 测试覆盖参数前后位置；
- Rename/Delete 即使收到非 Claude source，也必须由能力层拒绝，而不是静默忽略。

### 4. 扫描报告

新增：

```rust
pub struct SessionScanResult {
    pub summaries: Vec<SessionSummary>,
    pub diagnostics: ScanDiagnostics,
}

#[derive(Default, Serialize)]
pub struct ScanDiagnostics {
    pub files_seen: usize,
    pub files_parsed: usize,
    pub files_skipped: usize,
    pub malformed_files: usize,
    pub io_errors: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub bytes_considered: u64,
    pub elapsed_ms: u64,
    pub warnings: Vec<ScanWarning>,
}
```

`ScanWarning` 只包含诊断所需的脱敏字段：source、operation、error kind、path hash、可选行号，不保存会话内容。

为降低首轮回归面，保留兼容 wrapper：

```rust
fn scan_all_session_summaries(...) -> Result<Vec<SessionSummary>> {
    Ok(scan_all_session_summaries_with_report(...)?.summaries)
}
```

新 CLI 输出逐步改用 `_with_report`。内部稳定调用可暂时继续使用 wrapper。

### 5. 降级与失败语义

错误分级：

| 类别 | 示例 | 命令行为 |
|---|---|---|
| User input | 空搜索词、非法 duration、冲突 flags | 返回错误，退出非零 |
| Per-file data | malformed JSON、尾部半行、未知记录 | 记录 warning，跳过/部分解析，命令可继续 |
| System I/O | 根目录不可读、metadata 失败、权限错误 | 计入 diagnostics；根目录级失败可返回错误 |
| Cache advisory | cache 损坏、保存失败 | 回退全量扫描，记录 warning，不阻断结果 |
| Internal/config integrity | logger 初始化失败、损坏 settings 将被覆盖 | 不得静默默认；提供上下文错误或显式降级 |

文本输出规则：

- 无 warning：保持当前正常输出；
- 有 warning：结果输出后向 stderr 打一条聚合提示；
- 详细路径和错误写入文件日志；
- 聚合提示包含 invocation/diagnostic ID。

示例：

```text
⚠ 会话扫描不完整：5 个文件格式异常，3 个文件无法读取。
  诊断 ID：S-8F21A4；运行 `ccs session doctor --verbose` 查看详情。
```

首轮可以先提示查看日志；`session doctor` 在后续迭代落地后替换提示命令。

JSON 输出规则：

- 保持主业务字段；
- 新增 `schema_version`；
- 新增 `diagnostics`；
- diagnostics 明确 `degraded` 和各类计数；
- 不把警告混入 stdout 之外的非 JSON 文本。

### 6. 真实文件日志

继续使用 `log` facade，避免首轮迁移全项目。替换当前“env_logger stderr + 手工 `log_to_file`”的割裂实现，使所有 `log::Record` 同时进入：

1. stderr sink：按 `RUST_LOG` 或 `--debug` 过滤；
2. file sink：写入 `ConfigManager::log_file_path()`。

初始化顺序调整为：

```text
Cli::parse
→ 解析 --debug / --log-file
→ ensure config dir
→ rotate
→ init logger
→ 启动其他后台任务
```

初始化失败规则：

- stderr 可用但文件不可写：命令继续，立即打印一次清晰警告；
- 两个 sink 都不可用或 logger 全局注册失败：返回带上下文错误；
- 不再对 `rotate_log_if_needed()` / `init_logger()` 无条件 `.ok()`。

轮转首轮采用启动时检查：10 MiB × 当前文件 + 3 代备份。轮转失败不删除现有日志，并向 stderr 报告。

### 7. 日志结构与隐私

首轮文件日志保持人类可读单行，字段固定，后续可无损迁移 JSONL：

```text
2026-08-02T10:21:32Z WARN invocation=I-7A14 diagnostic=S-8F21A4 command=session.list source=codex operation=parse category=data path_hash=12ac8e elapsed_ms=18 error="invalid JSON at line 104"
```

禁止记录：

- user/assistant message 正文；
- session title；
- tool input/output；
- token、credential、带认证信息的 URL；
-完整 home 路径；
-原始 hook stdin。

允许记录：

- source；
- command 和 operation；
- path/session ID hash；
-文件大小、行号、计数、耗时；
-错误类型和经过清理的错误摘要。

Unix 下新建日志文件应尽量限制为当前用户可读写。平台差异以现有 config_dir 为准。

### 8. Hook 日志衔接

首轮不重构全部 hook，但要消除明显割裂：

- hook debug 路径改用 `ConfigManager`，不再硬编码 macOS 路径；
- hook 子进程 stderr 不再全部丢弃，捕获有限长度内容并写统一日志；
-检查 child exit status，而非只检查 spawn 是否成功；
- hook 日志复用轮转和脱敏规则；
- malformed hook input 记录数据类 warning，但不记录原始输入。

### 9. 性能指标

首轮只采集，不立刻引入外部遥测：

- total scan ms；
- source discovery ms；
- metadata/stat ms；
- cache load/save ms；
- cache hit/miss；
- parse ms 和 parsed bytes；
- skipped/malformed/I/O count；
- search full-load ms。

指标只写本地日志或 diagnostics，不上传。后续性能迭代用真实数据选择 streaming parser、snapshot 或全文索引的优先级。

## 后续性能迭代

### 迭代 2A：Cache 正确性

1. `retain_existing` 按已扫描 source 分区，避免单来源命令清除其他来源 cache；
2. mtime 提升到纳秒级或增加轻量 fingerprint；
3. cache 临时文件 + flush + 原子 rename；
4. cache schema version 与 summary/parser 版本关联；
5. Codex history title 建立独立 fingerprint/cache dependency；
6. canonical path 失败时保留原 key 并记录诊断。

### 迭代 2B：扫描与交互性能

1. 交互模式首次扫描生成 `SessionSnapshot`，菜单循环不再全库解析；
2. mutation 后只刷新受影响文件/项目；
3. grouping 使用 HashMap，排序增加稳定 tie-break；
4.新增 streaming summary parser，只读取摘要字段，不保留完整 message `Value`；
5.超大 JSONL 增加分层策略和诊断，不静默突破所有保护。

### 迭代 3：Agent 使用效率

1. `session list --json --limit --offset`；
2. list/show/search/overview 共享 versioned DTO；
3. JSON 提供 `total`、`returned`、`truncated`、`diagnostics`；
4. `search --json` 严格应用 limit；
5. `--match all|any` 明确多词语义；
6. `session doctor --json` 提供环境和数据健康检查；
7.根据 benchmark 决定是否引入 SQLite FTS 增量搜索索引。

### 迭代 4：来源抽象

当能力矩阵、统一 DTO 和扫描报告稳定后，再引入：

```rust
trait SessionProvider {
    fn source(&self) -> SessionSource;
    fn capabilities(&self) -> SourceCapabilities;
    fn roots(&self) -> Result<Vec<PathBuf>>;
    fn scan_summaries(&self, ctx: &ScanContext) -> Result<SessionScanResult>;
    fn load_messages(&self, path: &Path) -> Result<Vec<DisplayMessage>>;
    fn memory_roots(&self, project: &str) -> Vec<PathBuf>;
}
```

迁移顺序：Codex → OMP → Claude。Claude mutation/sync 逻辑最复杂，最后迁移降低风险。

## 测试设计

### 单元测试

- `SessionSource::capabilities` 完整矩阵；
- `SessionIdentity` 跨来源同 ID 不相等；
- ScanDiagnostics 计数和 degraded 判定；
-路径/session ID hash 稳定且不泄露原值；
-logger level/filter/格式；
-轮转保留 3 代且失败不删除当前日志；
- source-aware cache retain（后续迭代）；
-纳秒 mtime/fingerprint（后续迭代）。

### CLI 解析测试

覆盖：

```text
ccs session --source codex list
ccs session list --source codex
ccs session --source omp show <id>
```

前两个结果必须一致。Rename/Delete 的非 Claude source 必须明确失败。

### 集成测试

为 scanner 增加可注入 roots，不通过真实 home 测试：

```rust
pub struct SessionRoots {
    pub claude_projects: PathBuf,
    pub codex_sessions: PathBuf,
    pub codex_history: PathBuf,
    pub omp_sessions: PathBuf,
}
```

生产环境由 `dirs::home_dir()` 构造，测试由 tempfile 构造。所有涉及环境变量的测试使用 `CLAUDE_CODE_SYNC_CONFIG_DIR` 并标记 `#[serial]`。

关键 fixture：

- CC/CX/OM 各一个有效 session；
-跨来源相同 session ID；
- malformed JSON；
-尾部半行；
-非法 UTF-8；
-不可读文件/目录（按平台 cfg）；
-日志目录不可写；
- hook child 非零退出；
-日志轮转；
- Windows/Unix 混合路径。

### 验证命令

每个逻辑单元完成后运行最小相关测试，首轮结束运行：

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
```

再使用隔离 fixture 实跑：

```bash
ccs session list
ccs session --source claude list
ccs session --source codex list
ccs session search "test" --json
ccs session show <duplicate-id>
```

不得在测试中读写真实用户配置或 session 数据。

## 兼容性与迁移

- 正常文本输出尽量保持不变，只在扫描降级时向 stderr 增加聚合提示；
- JSON 新增字段，不删除已有字段；
-非 Claude Rename/Delete 从当前不一致行为收紧为明确拒绝，属于安全修复；
-裸 ID 唯一时继续可用，冲突时从静默选错改为明确报错；
- logger 路径继续使用现有 `ConfigManager::log_file_path()`；
- cache 改动必须升级 `CACHE_VERSION`；
-不引入远程遥测，不改变同步仓库格式。

## 敏捷交付顺序

### Slice 1：来源安全与身份

- 强类型 `SessionSource`、能力矩阵；
-禁止非 Claude mutation；
-修复 `--source` 继承；
-跨来源 ID 冲突错误；
-回归测试。

### Slice 2：真实文件日志

- logger 双 sink；
-初始化/轮转错误不再静默；
-日志脱敏与 invocation ID；
-logger/rotation 测试。

### Slice 3：扫描诊断

- `SessionScanResult` / `ScanDiagnostics`；
-WalkDir、metadata、parse、cache 错误计数；
-文本聚合警告与 JSON diagnostics；
-三来源 fixture 测试。

### Slice 4：Hook 诊断统一

-统一路径、捕获有限 stderr、检查 exit status；
- hook 测试；
-更新 README、user guide、CLAUDE.md 和 `local/notes.md`。

每个 Slice 独立实现、测试、评审。只有前一 Slice 通过验证后才进入下一 Slice。

## 完成定义

首轮完成需要同时满足：

- 四个 Slice 全部通过相关测试；
- `cargo test`、`cargo clippy -- -D warnings`、`cargo fmt --check` 通过；
-不存在非 Claude mutation 入口；
-普通 warn/debug 能按级别进入真实文件日志；
-至少 list/search/show 能获得扫描 diagnostics；
-日志和 diagnostics 不泄露会话内容与完整私人路径；
-文档与 CLI help 一致；
-改动和根因记录到 `local/notes.md`。

首轮后根据本地阶段耗时、cache 命中率、解析字节数和用户实跑结果决定迭代 2 的优先级，而不是预设必须立即引入全文索引或大规模重构。
