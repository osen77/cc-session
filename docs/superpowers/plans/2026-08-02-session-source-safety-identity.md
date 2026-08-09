# Session 来源安全与身份 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Claude/Codex/OMP 会话来源具有统一、可测试的身份与能力边界，阻止非 Claude 破坏性操作，并消除 `--source` 位置差异和跨来源同 ID 静默选错。

**Architecture:** 保留 `SessionSummary.source: String` 和现有 cache/JSON schema，先在 `handlers::session` 边界引入强类型 `SessionSource`、`SourceCapabilities`、`SessionIdentity`。CLI 只保留一个 global `--source`，所有查询和 mutation handler 使用同一来源过滤器；纯 resolver 负责唯一候选、未找到和歧义错误。

**Tech Stack:** Rust 2021、clap derive、anyhow、serde、现有模块内 `#[cfg(test)]` 测试。

## Global Constraints

- 不提交、不 push；用户明确要求当前实施过程不要执行 git commit/push。
- 不改变 `SessionSummary.source: String`、`CachedEntry.source: String` 和现有 JSON `source` 值，避免首轮触发 cache/schema 迁移。
- Claude 可查看、打开、重命名、删除和同步；Codex 只读；OMP 可查看/打开但不可重命名、删除或同步。
- 裸 session ID 在 `source=all` 下只有一个候选时保持兼容；多个候选必须报歧义错误，不得静默选第一个。
- 所有测试不得读写真实用户配置目录或真实 session 数据。
- 环境变量相关测试必须使用 `CLAUDE_CODE_SYNC_CONFIG_DIR` 并标记 `#[serial]`；本 Slice 的纯函数测试不需要环境变量。
- 公共 API 添加 `///` 文档注释；代码风格和注释密度匹配现有 Rust 代码。
- 每个任务先写失败测试，再写最小实现，再跑局部测试。

---

## File Structure

### 修改文件

- `src/handlers/session.rs`
  - 定义强类型来源、能力矩阵和会话身份。
  - 提供 source 解析、动作策略、mutation guard 和 source-aware resolver。
  - 统一交互/非交互 Rename/Delete 安全边界。
  - 修复 `handle_session_show` 的跨来源同 ID 歧义。
- `src/main.rs`
  - 删除 `SessionAction` 内重复的 source 字段。
  - 统一使用 `Commands::Session.source`。
  - 新增 clap parser 单元测试。
- `src/session_cache.rs`
  - 仅补 source 字符串 roundtrip 回归断言，确保 schema 不变。
- `README.md`
  - 明确三来源能力矩阵和 global `--source` 用法。
- `docs/user-guide.md`
  - 同步查询/破坏性操作规则和歧义提示。
- `local/notes.md`
  - 按项目格式记录问题、根因、解决方案、影响范围和预防措施。

### 不新增文件

本 Slice 不拆分 `session.rs`，不新增 provider 模块，不改变 cache version。来源抽象待后续稳定后再拆。

---

### Task 1: 强类型来源、能力矩阵与身份

**Files:**
- Modify: `src/handlers/session.rs:29-70`
- Modify: `src/handlers/session.rs:90-181`
- Test: `src/handlers/session.rs:3768-3964`
- Test: `src/session_cache.rs:243-312`

**Interfaces:**
- Consumes: 现有 `SessionSourceFilter`、`SessionSummary.source: String`。
- Produces:
  - `pub enum SessionSource { Claude, Codex, Omp }`
  - `pub struct SourceCapabilities`
  - `pub struct SessionIdentity`
  - `SessionSource::{as_str,label,capabilities}`
  - `SessionSourceFilter::includes(SessionSource)`
  - `SessionSummary::{source_kind,identity}`

- [ ] **Step 1: 在 session tests 中写能力矩阵失败测试**

在 `src/handlers/session.rs` 的现有 `#[cfg(test)] mod tests` 中增加：

```rust
#[test]
fn test_session_source_capabilities() {
    assert_eq!(
        SessionSource::Claude.capabilities(),
        SourceCapabilities {
            can_open: true,
            can_rename: true,
            can_delete: true,
            participates_in_sync: true,
        }
    );
    assert_eq!(
        SessionSource::Codex.capabilities(),
        SourceCapabilities {
            can_open: false,
            can_rename: false,
            can_delete: false,
            participates_in_sync: false,
        }
    );
    assert_eq!(
        SessionSource::Omp.capabilities(),
        SourceCapabilities {
            can_open: true,
            can_rename: false,
            can_delete: false,
            participates_in_sync: false,
        }
    );
}
```

- [ ] **Step 2: 写来源字符串、filter 和 identity 失败测试**

```rust
#[test]
fn test_session_source_strings_and_labels() {
    assert_eq!(SessionSource::Claude.as_str(), "claude");
    assert_eq!(SessionSource::Codex.as_str(), "codex");
    assert_eq!(SessionSource::Omp.as_str(), "omp");
    assert_eq!(SessionSource::Claude.label(), "CC");
    assert_eq!(SessionSource::Codex.label(), "CX");
    assert_eq!(SessionSource::Omp.label(), "OM");
}

#[test]
fn test_session_source_filter_includes_source() {
    assert!(SessionSourceFilter::All.includes(SessionSource::Claude));
    assert!(SessionSourceFilter::All.includes(SessionSource::Codex));
    assert!(SessionSourceFilter::All.includes(SessionSource::Omp));
    assert!(SessionSourceFilter::Claude.includes(SessionSource::Claude));
    assert!(!SessionSourceFilter::Claude.includes(SessionSource::Codex));
    assert!(SessionSourceFilter::Codex.includes(SessionSource::Codex));
    assert!(SessionSourceFilter::Omp.includes(SessionSource::Omp));
}

#[test]
fn test_session_identity_includes_source() {
    let claude = SessionIdentity {
        source: SessionSource::Claude,
        session_id: "same-id".to_string(),
    };
    let codex = SessionIdentity {
        source: SessionSource::Codex,
        session_id: "same-id".to_string(),
    };
    assert_ne!(claude, codex);
}
```

再使用现有 `SessionSummary` fixture 增加：

```rust
#[test]
fn test_summary_rejects_unknown_source() {
    let mut summary = make_test_summary("unknown-id", "project", "claude");
    summary.source = "future-source".to_string();
    let error = summary.source_kind().unwrap_err().to_string();
    assert!(error.contains("Unknown session source"));
}
```

如果现有 tests 没有统一 `make_test_summary`，在 tests 模块内新增只构造纯数据的 helper：

```rust
fn make_test_summary(session_id: &str, project_name: &str, source: &str) -> SessionSummary {
    SessionSummary {
        source: source.to_string(),
        session_id: session_id.to_string(),
        title: "Test session".to_string(),
        project_name: project_name.to_string(),
        project_dir: PathBuf::from("/tmp/project"),
        file_path: PathBuf::from(format!("/tmp/{session_id}.jsonl")),
        message_count: 2,
        user_message_count: 1,
        assistant_message_count: 1,
        first_timestamp: Some("2026-08-02T00:00:00Z".to_string()),
        last_activity: Some("2026-08-02T00:01:00Z".to_string()),
        file_size: 100,
    }
}
```

- [ ] **Step 3: 运行局部测试并确认失败**

Run:

```bash
cargo test handlers::session::tests::test_session_source -- --nocapture
```

Expected: FAIL，错误指向 `SessionSource`、`SourceCapabilities` 或相关方法尚未定义。

- [ ] **Step 4: 实现强类型来源和能力矩阵**

在 `SessionSourceFilter` 前增加：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionSource {
    Claude,
    Codex,
    Omp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCapabilities {
    pub can_open: bool,
    pub can_rename: bool,
    pub can_delete: bool,
    pub participates_in_sync: bool,
}

impl SessionSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Omp => "omp",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "CC",
            Self::Codex => "CX",
            Self::Omp => "OM",
        }
    }

    pub fn capabilities(self) -> SourceCapabilities {
        match self {
            Self::Claude => SourceCapabilities {
                can_open: true,
                can_rename: true,
                can_delete: true,
                participates_in_sync: true,
            },
            Self::Codex => SourceCapabilities {
                can_open: false,
                can_rename: false,
                can_delete: false,
                participates_in_sync: false,
            },
            Self::Omp => SourceCapabilities {
                can_open: true,
                can_rename: false,
                can_delete: false,
                participates_in_sync: false,
            },
        }
    }
}

impl TryFrom<&str> for SessionSource {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "omp" => Ok(Self::Omp),
            other => anyhow::bail!("Unknown session source: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionIdentity {
    pub source: SessionSource,
    pub session_id: String,
}
```

- [ ] **Step 5: 扩展 filter 和 summary 边界方法**

在 `SessionSourceFilter` impl 中增加：

```rust
fn includes(self, source: SessionSource) -> bool {
    matches!(
        (self, source),
        (Self::All, _)
            | (Self::Claude, SessionSource::Claude)
            | (Self::Codex, SessionSource::Codex)
            | (Self::Omp, SessionSource::Omp)
    )
}
```

在 `impl SessionSummary` 中增加：

```rust
fn source_kind(&self) -> Result<SessionSource> {
    SessionSource::try_from(self.source.as_str())
}

fn identity(&self) -> Result<SessionIdentity> {
    Ok(SessionIdentity {
        source: self.source_kind()?,
        session_id: self.session_id.clone(),
    })
}
```

将现有 `source_label(&str)` 改为内部调用 `SessionSource::try_from(source)?.label()`；若该函数当前必须返回 `&str` 且不能传播错误，改为：

```rust
fn source_label(source: &str) -> &'static str {
    SessionSource::try_from(source)
        .map(SessionSource::label)
        .unwrap_or("??")
}
```

未知来源的 mutation/identity 仍必须走 `source_kind()` 并返回错误，不得把 `??` 当作 Claude。

- [ ] **Step 6: 跑来源相关测试**

Run:

```bash
cargo test handlers::session::tests::test_session_source -- --nocapture
cargo test handlers::session::tests::test_session_identity_includes_source -- --nocapture
cargo test handlers::session::tests::test_summary_rejects_unknown_source -- --nocapture
```

Expected: PASS。

- [ ] **Step 7: 补 cache source schema 回归断言**

在 `src/session_cache.rs` 现有 roundtrip 测试加载 cache 后增加：

```rust
assert_eq!(loaded.entries.len(), 1);
let cached = loaded.entries.values().next().unwrap();
assert_eq!(cached.source, "claude");
```

Run:

```bash
cargo test session_cache::tests -- --nocapture
```

Expected: PASS，证明首轮未改变 `session_index.json` 的 source 字段格式。

---

### Task 2: 动作策略和破坏性操作防线

**Files:**
- Modify: `src/handlers/session.rs:322-329`
- Modify: `src/handlers/session.rs:920-970`
- Modify: `src/handlers/session.rs:1298-1339`
- Modify: `src/handlers/session.rs:1625-1704`
- Test: `src/handlers/session.rs:3768-3964`

**Interfaces:**
- Consumes: Task 1 的 `SessionSource::capabilities()`、`SessionSummary::source_kind()`。
- Produces:
  - `fn action_choices_for_source(SessionSource) -> Vec<ActionChoice>`
  - `fn ensure_can_rename(&SessionSummary) -> Result<()>`
  - `fn ensure_can_delete(&SessionSummary) -> Result<()>`

- [ ] **Step 1: 让 ActionChoice 可比较并写动作矩阵失败测试**

将 enum derive 改为：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionChoice {
    OpenInEditor,
    ViewDetails,
    Rename,
    Delete,
    Back,
}
```

在 tests 中增加：

```rust
#[test]
fn test_action_choices_follow_source_capabilities() {
    assert_eq!(
        action_choices_for_source(SessionSource::Claude),
        vec![
            ActionChoice::OpenInEditor,
            ActionChoice::ViewDetails,
            ActionChoice::Rename,
            ActionChoice::Delete,
            ActionChoice::Back,
        ]
    );
    assert_eq!(
        action_choices_for_source(SessionSource::Codex),
        vec![ActionChoice::ViewDetails, ActionChoice::Back]
    );
    assert_eq!(
        action_choices_for_source(SessionSource::Omp),
        vec![
            ActionChoice::OpenInEditor,
            ActionChoice::ViewDetails,
            ActionChoice::Back,
        ]
    );
}
```

- [ ] **Step 2: 写 mutation guard 失败测试**

```rust
#[test]
fn test_non_claude_sources_are_read_only_for_mutations() {
    let claude = make_test_summary("id", "project", "claude");
    let codex = make_test_summary("id", "project", "codex");
    let omp = make_test_summary("id", "project", "omp");

    assert!(ensure_can_rename(&claude).is_ok());
    assert!(ensure_can_delete(&claude).is_ok());
    assert!(ensure_can_rename(&codex).unwrap_err().to_string().contains("read-only"));
    assert!(ensure_can_delete(&codex).unwrap_err().to_string().contains("read-only"));
    assert!(ensure_can_rename(&omp).unwrap_err().to_string().contains("read-only"));
    assert!(ensure_can_delete(&omp).unwrap_err().to_string().contains("read-only"));
}
```

- [ ] **Step 3: 运行测试确认失败**

Run:

```bash
cargo test handlers::session::tests::test_action_choices_follow_source_capabilities -- --nocapture
cargo test handlers::session::tests::test_non_claude_sources_are_read_only_for_mutations -- --nocapture
```

Expected: FAIL，helper 尚不存在。

- [ ] **Step 4: 实现动作策略和 guard**

在 `ActionChoice` 后增加：

```rust
fn action_choices_for_source(source: SessionSource) -> Vec<ActionChoice> {
    let capabilities = source.capabilities();
    let mut actions = Vec::new();
    if capabilities.can_open {
        actions.push(ActionChoice::OpenInEditor);
    }
    actions.push(ActionChoice::ViewDetails);
    if capabilities.can_rename {
        actions.push(ActionChoice::Rename);
    }
    if capabilities.can_delete {
        actions.push(ActionChoice::Delete);
    }
    actions.push(ActionChoice::Back);
    actions
}

fn ensure_can_rename(session: &SessionSummary) -> Result<()> {
    let source = session.source_kind()?;
    if source.capabilities().can_rename {
        Ok(())
    } else {
        anyhow::bail!("{} sessions are read-only and cannot be renamed", source.label())
    }
}

fn ensure_can_delete(session: &SessionSummary) -> Result<()> {
    let source = session.source_kind()?;
    if source.capabilities().can_delete {
        Ok(())
    } else {
        anyhow::bail!("{} sessions are read-only and cannot be deleted", source.label())
    }
}
```

- [ ] **Step 5: 让交互菜单消费动作策略**

`show_action_menu` 先解析：

```rust
let source = session.source_kind()?;
let actions = action_choices_for_source(source);
```

将 `ActionChoice` 映射为 label：

```rust
let open_label = if source == SessionSource::Omp {
    "Open in OMP"
} else {
    "Open in Claude"
};

let labels: Vec<&str> = actions
    .iter()
    .map(|action| match action {
        ActionChoice::OpenInEditor => open_label,
        ActionChoice::ViewDetails => "View details",
        ActionChoice::Rename => "Rename session",
        ActionChoice::Delete => "Delete session",
        ActionChoice::Back => "Back to session list",
    })
    .collect();
```

Prompt 返回后按 label 的 index 返回 `actions[index]`，取消返回 `Back`。不要继续用字符串 guard 判断 Codex。

- [ ] **Step 6: 在所有 mutation 入口加入防御性 guard**

在 `rename_session_interactive` 第一行加入：

```rust
ensure_can_rename(session)?;
```

在 `delete_session_interactive` 第一行加入：

```rust
ensure_can_delete(session)?;
```

在 `delete_session_with_commit` 删除本地文件之前加入：

```rust
ensure_can_delete(session)?;
```

同步更新该函数文档，删除“Codex session local-only deletion”描述，改为只接受具有 delete capability 的来源。

`remove_session_for_batch` 的调用来源目前由 Claude cleanup 限定；仍在函数开头加入 `ensure_can_delete(session)?`，防止未来入口绕过能力边界。

- [ ] **Step 7: 跑动作与 guard 测试**

Run:

```bash
cargo test handlers::session::tests::test_action_choices_follow_source_capabilities -- --nocapture
cargo test handlers::session::tests::test_non_claude_sources_are_read_only_for_mutations -- --nocapture
```

Expected: PASS。

---

### Task 3: 统一 global `--source` 解析和 dispatch

**Files:**
- Modify: `src/main.rs:33-40`
- Modify: `src/main.rs:347-359`
- Modify: `src/main.rs:494-629`
- Modify: `src/main.rs:1100-1183`
- Test: `src/main.rs:1189` 之后新增 tests 模块

**Interfaces:**
- Consumes: 现有 `SessionSourceArg -> SessionSourceFilter` 转换。
- Produces: `Commands::Session.source` 成为 session 全部子命令唯一来源字段。

- [ ] **Step 1: 写 clap global source 失败测试**

在 `src/main.rs` 末尾增加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn parse_session_source(args: &[&str]) -> SessionSourceArg {
        let cli = Cli::try_parse_from(args).expect("CLI should parse");
        match cli.command {
            Some(Commands::Session { source, .. }) => source,
            _ => panic!("expected session command"),
        }
    }

    #[test]
    fn test_session_source_before_subcommand() {
        assert_eq!(
            parse_session_source(&["ccs", "session", "--source", "codex", "list"]),
            SessionSourceArg::Codex
        );
    }

    #[test]
    fn test_session_source_after_subcommand() {
        assert_eq!(
            parse_session_source(&["ccs", "session", "list", "--source", "codex"]),
            SessionSourceArg::Codex
        );
    }

    #[test]
    fn test_session_source_defaults_to_all() {
        assert_eq!(
            parse_session_source(&["ccs", "session", "list"]),
            SessionSourceArg::All
        );
    }

    #[test]
    fn test_session_source_applies_to_mutations() {
        assert_eq!(
            parse_session_source(&[
                "ccs",
                "session",
                "--source",
                "omp",
                "delete",
                "session-id",
                "--force",
            ]),
            SessionSourceArg::Omp
        );
    }
}
```

为断言补充：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SessionSourceArg { ... }
```

- [ ] **Step 2: 运行 parser tests 并确认至少位置语义测试失败或暴露重复字段**

Run:

```bash
cargo test --bin ccs tests::test_session_source -- --nocapture
```

Expected: 当前实现中 subcommand 自有默认 source 与 outer source 分裂；测试或后续 dispatch 检查无法证明统一来源。

- [ ] **Step 3: 删除 SessionAction 内重复 source 字段**

从以下 variant 删除内部 `source`：

```rust
SessionAction::List
SessionAction::Search
SessionAction::Show
SessionAction::Projects
SessionAction::Overview
```

保留唯一字段：

```rust
Commands::Session {
    action: Option<SessionAction>,
    #[arg(short, long, global = true)]
    project: Option<String>,
    #[arg(short, long, global = true, default_value = "all")]
    source: SessionSourceArg,
}
```

不要删除 `global = true`，它保证 flag 可位于子命令前后。

- [ ] **Step 4: 统一 dispatch 使用 outer source**

在 `Commands::Session { action, project, source }` match 中，所有 handler 均使用 outer `source.into()`：

```rust
handle_session_interactive(project.as_deref(), source.into())?;
handle_session_list(filter, show_ids, source.into())?;
handle_session_search(..., source.into())?;
handle_session_show(..., source.into())?;
handle_session_projects(source.into())?;
handle_session_overview(..., source.into())?;
```

Rename/Delete 改为：

```rust
handle_session_rename(&session_id, &title, source.into())?;
handle_session_delete(&session_id, force, source.into())?;
```

保留 List/Search 的子命令 `project` 优先、global project 回退逻辑：

```rust
let filter = list_project.as_deref().or(project.as_deref());
```

- [ ] **Step 5: 跑 CLI parser tests**

Run:

```bash
cargo test --bin ccs tests::test_session_source -- --nocapture
```

Expected: 4 个测试 PASS。

- [ ] **Step 6: 验证 help 中只显示一个 source 语义**

Run:

```bash
cargo run -- session --help
cargo run -- session list --help
cargo run -- session show --help
```

Expected: `--source <SOURCE>` 作为 global 参数可在 session 子命令前后使用，不再存在两套相互覆盖的字段。

---

### Task 4: Source-aware resolver 和 Show 歧义处理

**Files:**
- Modify: `src/handlers/session.rs:2538-2705`
- Modify: `src/handlers/session.rs:3546-3604`
- Test: `src/handlers/session.rs:3768-3964`

**Interfaces:**
- Consumes: `SessionSourceFilter::includes`、`SessionSummary::source_kind`。
- Produces:
  - `fn resolve_session_by_id<'a>(...) -> Result<&'a SessionSummary>`
  - 新签名 `handle_session_rename(..., source: SessionSourceFilter)`
  - 新签名 `handle_session_delete(..., source: SessionSourceFilter)`

- [ ] **Step 1: 写 resolver 0/1/多候选失败测试**

```rust
#[test]
fn test_resolve_session_by_id_returns_unique_candidate() {
    let sessions = vec![make_test_summary("id", "project", "claude")];
    let found = resolve_session_by_id(&sessions, "id", SessionSourceFilter::All).unwrap();
    assert_eq!(found.source, "claude");
}

#[test]
fn test_resolve_session_by_id_honors_source_filter() {
    let sessions = vec![
        make_test_summary("id", "claude-project", "claude"),
        make_test_summary("id", "codex-project", "codex"),
    ];
    let found = resolve_session_by_id(&sessions, "id", SessionSourceFilter::Codex).unwrap();
    assert_eq!(found.source, "codex");
}

#[test]
fn test_resolve_session_by_id_rejects_ambiguous_all_source() {
    let sessions = vec![
        make_test_summary("id", "claude-project", "claude"),
        make_test_summary("id", "codex-project", "codex"),
    ];
    let error = resolve_session_by_id(&sessions, "id", SessionSourceFilter::All)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Ambiguous session ID"));
    assert!(error.contains("claude"));
    assert!(error.contains("codex"));
    assert!(error.contains("--source"));
}

#[test]
fn test_resolve_session_by_id_reports_not_found() {
    let sessions = vec![make_test_summary("other", "project", "claude")];
    let error = resolve_session_by_id(&sessions, "missing", SessionSourceFilter::All)
        .unwrap_err()
        .to_string();
    assert_eq!(error, "Session not found: missing");
}
```

- [ ] **Step 2: 运行 resolver tests 并确认失败**

Run:

```bash
cargo test handlers::session::tests::test_resolve_session_by_id -- --nocapture
```

Expected: FAIL，resolver 尚不存在。

- [ ] **Step 3: 实现纯 resolver**

在 `handle_session_show` 前增加：

```rust
fn resolve_session_by_id<'a>(
    sessions: &'a [SessionSummary],
    session_id: &str,
    source_filter: SessionSourceFilter,
) -> Result<&'a SessionSummary> {
    let mut candidates = Vec::new();

    for session in sessions.iter().filter(|s| s.session_id == session_id) {
        let source = session.source_kind()?;
        if source_filter.includes(source) {
            candidates.push(session);
        }
    }

    match candidates.as_slice() {
        [] => anyhow::bail!("Session not found: {session_id}"),
        [session] => Ok(*session),
        _ => {
            let details = candidates
                .iter()
                .map(|session| {
                    format!(
                        "  {}  project={}  id={}",
                        session.source, session.project_name, session.session_id
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "Ambiguous session ID '{session_id}'. Specify --source:\n{details}"
            )
        }
    }
}
```

- [ ] **Step 4: 替换 Show 的裸 `.find`**

将：

```rust
if let Some(session) = sessions.iter().find(|s| s.session_id == session_id) {
    // ...
}
anyhow::bail!("Session not found: {}", session_id)
```

改为：

```rust
let session = resolve_session_by_id(&sessions, session_id, source)?;
```

后续展示逻辑直接使用 `session`，删除末尾重复 not-found 分支。保留 around/head/tail/JSON 行为不变。

- [ ] **Step 5: 让非交互 Rename/Delete 使用统一扫描和 resolver**

签名改为：

```rust
pub fn handle_session_rename(
    session_id: &str,
    new_title: &str,
    source: SessionSourceFilter,
) -> Result<()>
```

实现：

```rust
let sessions = scan_all_session_summaries(None, source)?;
let session = resolve_session_by_id(&sessions, session_id, source)?;
ensure_can_rename(session)?;
rename_session(&session.file_path, session_id, new_title)?;
println!("{} Session renamed successfully!", "SUCCESS:".green().bold());
Ok(())
```

Delete 同理：

```rust
pub fn handle_session_delete(
    session_id: &str,
    force: bool,
    source: SessionSourceFilter,
) -> Result<()>
```

在任何确认提示之前调用：

```rust
let sessions = scan_all_session_summaries(None, source)?;
let session = resolve_session_by_id(&sessions, session_id, source)?;
ensure_can_delete(session)?;
```

然后复用现有确认和 `delete_session_with_commit` 逻辑。删除旧的 `scan_all_projects` + `scan_project_sessions` 双重扫描循环。

- [ ] **Step 6: 明确显式只读来源错误**

验证以下路径：

```text
--source codex rename → resolver 命中 Codex → read-only error
--source omp delete → resolver 命中 OMP → read-only error
source=all 且仅 Codex ID → read-only error
source=all 且 Claude/Codex 同 ID → ambiguous error，要求指定 source
```

错误必须在文件写入和确认提示之前发生。

- [ ] **Step 7: 跑 resolver 和 session 局部测试**

Run:

```bash
cargo test handlers::session::tests::test_resolve_session_by_id -- --nocapture
cargo test handlers::session::tests::test_non_claude_sources_are_read_only_for_mutations -- --nocapture
```

Expected: PASS。

---

### Task 5: 文档和问题记录

**Files:**
- Modify: `README.md:120-165`
- Modify: `docs/user-guide.md:375-430`
- Modify: `local/notes.md`

**Interfaces:**
- Consumes: Task 1-4 的最终 CLI 行为。
- Produces: 用户可发现的来源能力、source 用法和歧义处理说明。

- [ ] **Step 1: 更新 README 三来源能力矩阵**

在 session 命令说明中加入：

```markdown
| 来源 | 查询 | 打开 | 重命名 | 删除 | 参与同步 |
|------|------|------|--------|------|----------|
| Claude Code | ✅ | ✅ | ✅ | ✅ | ✅ |
| Codex | ✅ | ❌ | ❌ | ❌ | ❌ |
| OMP | ✅ | ✅ | ❌ | ❌ | ❌ |
```

说明：

```bash
ccs session --source codex list
ccs session list --source codex   # 等价
ccs session --source omp show <id>
```

跨来源 ID 冲突时必须用 `--source` 消歧。

- [ ] **Step 2: 更新 user guide**

明确：

- 交互模式默认可展示三来源；
- Codex/OMP 为只读来源；
- OMP 可以打开原始会话，但不能通过 ccs Rename/Delete；
- Rename/Delete 只对 Claude 生效；
- `show` 在同 ID 多来源时会列候选并要求 `--source`。

- [ ] **Step 3: 按项目格式记录 local/notes**

新增章节：

```markdown
## 2026-08-02: Session 多来源安全边界与身份修复

### 问题描述
- 交互菜单允许删除 Codex/OMP，会造成外部历史误删。
- OMP Rename 写入 Claude custom-title 记录但 parser 不读取。
- 顶层与子命令重复定义 source，参数位置可能改变查询范围。
- show 只按裸 session ID 查找，跨来源冲突时静默选错。

### 根本原因
- 多来源扩展沿用 String 分派，缺少统一能力矩阵和复合身份。
- mutation handler 与查询 handler 使用不同扫描路径。
- clap global source 与子命令默认 source 重复建模。

### 解决方案
- 引入 SessionSource、SourceCapabilities、SessionIdentity。
- 非 Claude mutation 在菜单、交互 handler、非交互 handler 和底层 delete 入口多层拒绝。
- 所有 session 子命令统一使用 global source。
- 使用 source-aware resolver 处理唯一、未找到和歧义候选。

### 影响范围
- src/handlers/session.rs
- src/main.rs
- src/session_cache.rs tests
- README.md / docs/user-guide.md

### 预防措施
- 新增来源必须先定义 capabilities。
- 内部身份使用 (source, session_id)，不得以裸 ID 作为全局唯一键。
- CLI global 参数不得在子命令重复定义同名默认值。
```

- [ ] **Step 4: 检查文档与 help 一致性**

Run:

```bash
rg -n "Codex|OMP|--source|Rename|Delete|只读" README.md docs/user-guide.md
cargo run -- session --help
```

Expected: 文档覆盖三来源，help 与示例参数位置一致。

---

### Task 6: 全量验证和回归检查

**Files:**
- Verify only: 所有本 Slice 修改文件

**Interfaces:**
- Consumes: Task 1-5 的完整实现。
- Produces: 可进入下一 Slice 的已验证工作区。

- [ ] **Step 1: 格式化代码**

Run:

```bash
cargo fmt
```

Expected: 命令退出 0。

- [ ] **Step 2: 跑目标测试**

Run:

```bash
cargo test handlers::session::tests::test_session_source -- --nocapture
cargo test handlers::session::tests::test_session_identity -- --nocapture
cargo test handlers::session::tests::test_action_choices -- --nocapture
cargo test handlers::session::tests::test_non_claude -- --nocapture
cargo test handlers::session::tests::test_resolve_session_by_id -- --nocapture
cargo test session_cache::tests -- --nocapture
cargo test --bin ccs tests::test_session_source -- --nocapture
```

Expected: 全部 PASS。

- [ ] **Step 3: 跑全量测试**

Run:

```bash
cargo test
```

Expected: 全部 PASS；不得读写真实配置目录。

- [ ] **Step 4: 跑 Clippy**

Run:

```bash
cargo clippy -- -D warnings
```

Expected: 退出 0，无 warning。

- [ ] **Step 5: 检查格式和 diff whitespace**

Run:

```bash
cargo fmt --check
git diff --check
```

Expected: 两个命令均退出 0。

- [ ] **Step 6: 实跑纯读取 CLI**

Run:

```bash
cargo run -- session --source claude list
cargo run -- session list --source codex
cargo run -- session --source omp projects
```

Expected:

- source flag 在子命令前后都生效；
- 查询命令正常运行；
- 不执行 Rename/Delete，不修改真实 session。

- [ ] **Step 7: 用测试 fixture 验证 destructive guard**

不要对真实 session 执行删除。使用单元测试或临时 fixture 验证：

```text
Codex Rename → read-only error
Codex Delete → read-only error
OMP Rename → read-only error
OMP Delete → read-only error
Claude mutation guard → allowed
跨来源同 ID + source=all → ambiguous error
```

Expected: guard 在任何文件写入、删除或确认提示之前返回。

- [ ] **Step 8: 审查 diff 范围**

Run:

```bash
git status --short
git diff --stat
git diff -- src/handlers/session.rs src/main.rs src/session_cache.rs README.md docs/user-guide.md local/notes.md
```

Expected: 只包含本 Slice 设计范围内的改动；不包含 cache version、provider 重构、logger 或 scanner diagnostics 实现。

---

## Plan Self-Review

### Spec coverage

本计划覆盖首轮规格中的 Slice 1：

- 来源强类型与能力矩阵：Task 1；
- 非 Claude mutation 防线：Task 2；
- global source 一致性：Task 3；
- 跨来源身份与 show 歧义：Task 4；
- 文档和问题记录：Task 5；
- 完整验证：Task 6。

真实文件日志、扫描诊断、Hook 诊断统一属于独立可交付子系统，将在本计划通过后分别生成计划，不混入本 Slice。

### Type consistency

- `SessionSource` 是单一来源；`SessionSourceFilter` 是查询过滤器，职责不混用。
- `SessionSummary.source` 保持 String；所有能力和 identity 操作通过 `source_kind()` 显式转换。
- mutation handler 新增 `SessionSourceFilter` 参数，与 main.rs outer global source 一致。
- resolver 返回 `&SessionSummary`，Show/Rename/Delete 复用同一候选语义。

### Placeholder scan

计划不含 TBD、TODO、未定义函数或“稍后实现”占位步骤。后续 Slice 被明确排除并要求单独计划。
