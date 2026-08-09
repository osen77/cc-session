# 裸 `ccs` 默认进入 Session Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让不带子命令的 `ccs` 等价于 `ccs session`，直接进入交互式会话选择，同时保持 hooks、wrapper 和所有显式子命令行为不变。

**Architecture:** 新增一个纯 CLI 默认分派函数，将缺失子命令解析为 `Commands::Session`。主流程在更新检查和 onboarding 判断前确定实际命令，使裸 `ccs` 与显式 `ccs session` 共享相同的本地命令语义。

**Tech Stack:** Rust 2021、Clap、现有 `Commands`/`SessionAction` CLI 模型、Cargo 测试工具链。

## Global Constraints

- 未初始化同步仓库时，裸 `ccs` 仍进入 session，不触发 onboarding。
- `ccs sync` 继续显式执行双向同步。
- `ccs --help`、`ccs --version`、全局日志参数和所有显式子命令保持不变。
- 裸 `ccs` 不启动网络更新检查。
- hooks、wrapper 和 automate 必须继续使用显式子命令，不修改其运行逻辑。
- 按项目规范更新 `local/notes.md`。

## File Structure

- Modify: `src/main.rs` — 定义默认命令解析函数、调整主流程顺序、增加 CLI 回归测试。
- Modify: `README.md` — 在常用命令中说明裸 `ccs` 的新默认行为。
- Modify: `docs/user-guide.md` — 更新交互模式和命令表。
- Modify: `local/notes.md` — 记录行为变更、影响范围与验证结果。

---

### Task 1: 默认命令分派与回归测试

**Files:**
- Modify: `src/main.rs:696-801`
- Test: `src/main.rs:1268-1305`

**Interfaces:**
- Consumes: `Option<Commands>`、`Commands::Session`、`SessionSourceArg::All`。
- Produces: `fn resolve_command(command: Option<Commands>) -> Commands`，供 `main()` 和单元测试共同使用。

- [ ] **Step 1: 写失败测试**

在 `src/main.rs` 的 `tests` 模块中加入：

```rust
#[test]
fn test_default_command_is_interactive_session() {
    let cli = Cli::try_parse_from(["ccs"]).expect("bare ccs should parse");

    match resolve_command(cli.command) {
        Commands::Session {
            action,
            project,
            source,
            include_hidden,
        } => {
            assert!(action.is_none());
            assert!(project.is_none());
            assert_eq!(source, SessionSourceArg::All);
            assert!(!include_hidden);
        }
        _ => panic!("bare ccs should default to interactive session mode"),
    }
}
```

- [ ] **Step 2: 验证测试先失败**

Run:

```bash
cargo test --bin ccs test_default_command_is_interactive_session
```

Expected: 编译失败，提示找不到 `resolve_command`。

- [ ] **Step 3: 实现默认命令解析**

在 `impl From<SessionSourceArg>` 后、`main()` 前加入：

```rust
fn resolve_command(command: Option<Commands>) -> Commands {
    command.unwrap_or(Commands::Session {
        action: None,
        project: None,
        source: SessionSourceArg::All,
        include_hidden: false,
    })
}
```

在 logger 初始化和 warning 输出后立即解析实际命令：

```rust
let command = resolve_command(cli.command);
```

将更新检查判断改为基于 `command`，而不是 `cli.command`：

```rust
let is_update_command = matches!(command, Commands::Update { .. });
let is_local_command = matches!(
    command,
    Commands::Session { .. }
        | Commands::Config { .. }
        | Commands::Status { .. }
        | Commands::Report { .. }
        | Commands::History { .. }
);
```

删除 `needs_onboarding` 后原有的 `if let Some(cmd) = cli.command { ... } else { Commands::Sync { ... } }` 默认分派块。保留后续所有 `command` 类型判断和 `match command` 分派不变。

- [ ] **Step 4: 验证默认分派与 hooks 显式子命令**

Run:

```bash
cargo test --bin ccs test_default_command_is_interactive_session
cargo test --bin ccs test_global_logger_flags_parse_before_and_after_subcommand
cargo test --bin ccs hook_command_is_quoted_absolute_path
cargo test --bin ccs spawn_ccs_subcommand_returns_result_without_panic
```

Expected: 四个命令全部 PASS；hook command 继续以显式 `hook-stop` 结尾。

- [ ] **Step 5: 提交功能代码**

```bash
git add src/main.rs
git commit -m "feat(cli): default to session interaction"
```

---

### Task 2: 文档、变更记录与完整验证

**Files:**
- Modify: `README.md:117-135`
- Modify: `docs/user-guide.md:390-407`
- Modify: `docs/user-guide.md:568-593`
- Modify: `local/notes.md:1`

**Interfaces:**
- Consumes: Task 1 已实现的裸命令行为。
- Produces: 面向用户的明确命令说明和项目变更记录。

- [ ] **Step 1: 更新 README 和用户指南**

在 README 常用命令表中增加：

```markdown
| `ccs` | 进入交互式会话选择（等价于 `ccs session`） |
```

将用户指南交互模式示例更新为：

```bash
# 最常用：直接进入交互式会话选择
ccs

# 等价的显式写法
ccs session

# 只展示 Codex
ccs session --source codex
```

在用户指南常用命令表中增加：

```markdown
| `ccs` | 进入交互式会话选择（等价于 `ccs session`） |
```

并在表格附近明确说明：双向同步不再是裸命令默认行为，需要显式运行 `ccs sync`。

- [ ] **Step 2: 更新项目问题记录**

在 `local/notes.md` 顶部加入：

```markdown
## 2026-08-09：裸 ccs 默认进入会话交互模式

### 问题描述
- 用户高频使用 `ccs session`，但裸 `ccs` 默认执行同步，增加了日常进入会话选择的输入成本。

### 根本原因
- CLI 在缺失子命令时构造 `Commands::Sync`，且更新检查在默认命令确定前判断空命令，无法获得与显式 session 一致的本地命令语义。

### 解决方案
- 使用纯 `resolve_command()` 将缺失子命令解析为交互式 `Commands::Session`，并在更新检查与 onboarding 判断前确定实际命令。
- 保留 `ccs sync` 的显式同步行为；hooks 和 wrapper 继续使用明确的 pull、push 与 hook 子命令。

### 影响范围
- `src/main.rs`
- `README.md`
- `docs/user-guide.md`

### 预防措施
- 默认命令必须在网络行为和 onboarding 判断前解析；内部自动化调用必须始终携带显式子命令。
```

- [ ] **Step 3: 运行完整验证**

Run:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Expected: 格式检查通过、Clippy 无 warning、全部测试通过。若任一命令失败，先修复并重新运行三条命令，不提交失败状态。

- [ ] **Step 4: 检查最终差异**

Run:

```bash
git diff --check
git status --short
```

Expected: `git diff --check` 无输出；状态仅包含 README、用户指南和 `local/notes.md` 的预期改动。

- [ ] **Step 5: 提交文档和验证记录**

```bash
git add README.md docs/user-guide.md local/notes.md
git commit -m "docs: document default session command"
```
