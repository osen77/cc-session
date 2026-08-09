# 删除放行窗口（`ccs unlock-delete`）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 `ccs unlock-delete` 命令，开启一个限时窗口（默认 15 分钟），窗口期内所有 push（含 Stop hook 自动 push）自动把「本地已删除、云端仍存在」的 session 同步删除到云端；到期自动恢复保护。

**Architecture:** 新增单一职责模块 `src/sync/delete_unlock.rs` 管理一个"到期时间戳文件"（`config_dir/delete-unlock.json`）。过期为被动判定，无后台进程。`push_history` 在唯一的 `missing_in_repo` 分支消费该状态，因此所有 push 路径自动生效、无需改动任何调用点签名。CLI 层新增 `handlers/unlock_delete.rs`。

**Tech Stack:** Rust 2021、clap 4.5（derive）、serde/serde_json、chrono（本地时区显示）、colored、anyhow；测试用 tempfile + serial_test。

## Global Constraints

- **测试隔离铁律**：禁止读写真实配置目录；用 `CLAUDE_CODE_SYNC_CONFIG_DIR`（常量 `crate::config::CONFIG_DIR_ENV`）覆盖配置目录；所有操作环境变量的测试标 `#[serial]`（来自 `serial_test`），并在测试内自行 `env::remove_var` 清理。
- **fail-safe 语义**：`is_active()` / push 消费遇任何读取或解析错误，一律回退到"保护模式"（不删云端）。宁可放行失败，绝不误删。
- **不写 tombstone**：窗口触发的删除是物理同步（等价 `--prune`），不写入 `.ccs/deletions.json`。
- **默认时长 15 分钟**；`--minutes >= 1`，否则报错。
- **时区**：面向用户的到期时刻用 `chrono::Local`（跟随系统，即 Asia/Shanghai），与 `src/logger.rs` 一致。
- **中文文案**：所有面向用户的新增输出用简体中文。

## 文件结构

- Create: `src/sync/delete_unlock.rs` — 窗口状态管理（unlock/disable/status/is_active + 纯函数 `remaining_at`）
- Create: `src/handlers/unlock_delete.rs` — CLI handler `handle_unlock_delete`
- Modify: `src/config.rs` — 新增 `delete_unlock_path()`
- Modify: `src/sync/mod.rs` — `pub mod delete_unlock;`
- Modify: `src/sync/push.rs` — 新增纯函数 `decide_missing_action` + 改 `missing_in_repo` 分支消费窗口状态
- Modify: `src/handlers/mod.rs` — 导出新 handler
- Modify: `src/main.rs` — 新增 `Commands::UnlockDelete` 定义与分发
- Modify: `CLAUDE.md`、`docs/user-guide.md`、`local/notes.md` — 文档

---

### Task 1: `delete_unlock` 模块 + 配置路径 helper

**Files:**
- Modify: `src/config.rs`（在 `user_data_path` 后新增方法；测试模块内新增用例）
- Create: `src/sync/delete_unlock.rs`
- Modify: `src/sync/mod.rs:9`（模块声明区）

**Interfaces:**
- Consumes: `crate::config::ConfigManager::{config_dir, ensure_config_dir}`、常量 `crate::config::CONFIG_DIR_ENV`
- Produces:
  - `ConfigManager::delete_unlock_path() -> anyhow::Result<PathBuf>`
  - `crate::sync::delete_unlock::unlock(minutes: u64) -> anyhow::Result<u64>`（返回到期 unix 秒）
  - `crate::sync::delete_unlock::disable() -> anyhow::Result<()>`
  - `crate::sync::delete_unlock::status() -> anyhow::Result<Option<u64>>`（剩余秒；过期/无文件为 `None`）
  - `crate::sync::delete_unlock::is_active() -> bool`

- [ ] **Step 1: 在 `config.rs` 新增路径 helper**

在 `src/config.rs` 的 `user_data_path`（约 94-97 行）之后新增：

```rust
    /// Get the delete-unlock window state file path (delete-unlock.json)
    pub fn delete_unlock_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("delete-unlock.json"))
    }
```

- [ ] **Step 2: 在 `config.rs` 测试内断言新路径**

在 `test_config_paths`（约 132 行）末尾 `log` 断言之后追加：

```rust
        let unlock = ConfigManager::delete_unlock_path().unwrap();
        assert!(unlock.to_string_lossy().contains("delete-unlock.json"));
```

- [ ] **Step 3: 运行确认路径断言通过**

Run: `cargo test --lib config::tests::test_config_paths -- --nocapture`
Expected: PASS

- [ ] **Step 4: 创建 `src/sync/delete_unlock.rs`（先写会失败编译的骨架 + 测试）**

写入完整实现（含纯函数与 fail-safe）：

```rust
//! Time-boxed "delete unlock" window.
//!
//! When active, `ccs push` treats locally-missing sessions as intentional
//! deletions and prunes them from the sync repo (same as `--prune`, but with
//! NO tombstone). The window expires passively — there is no background
//! process; every consumer re-checks the stored expiry timestamp.

use crate::config::ConfigManager;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize, Deserialize)]
struct UnlockState {
    /// Absolute expiry in unix seconds (timezone-independent).
    expires_at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Remaining seconds until expiry, or `None` if already expired.
/// Pure function — no IO — so expiry logic is unit-testable in isolation.
fn remaining_at(expires_at: u64, now: u64) -> Option<u64> {
    if now < expires_at {
        Some(expires_at - now)
    } else {
        None
    }
}

fn state_path() -> Result<PathBuf> {
    ConfigManager::delete_unlock_path()
}

/// Open (or extend) the window for `minutes`. Overwrites any existing state,
/// so calling again simply renews the deadline. Returns the expiry unix ts.
pub fn unlock(minutes: u64) -> Result<u64> {
    let expires_at = now_secs() + minutes * 60;
    ConfigManager::ensure_config_dir()?;
    let path = state_path()?;
    let json = serde_json::to_string(&UnlockState { expires_at })?;
    std::fs::write(&path, json)
        .with_context(|| format!("Failed to write delete-unlock state: {}", path.display()))?;
    Ok(expires_at)
}

/// Close the window. Idempotent: a missing file is treated as success.
pub fn disable() -> Result<()> {
    let path = state_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to remove delete-unlock state: {}", path.display()))?;
    }
    Ok(())
}

/// Remaining seconds if the window is active, else `None` (expired/absent).
pub fn status() -> Result<Option<u64>> {
    let path = state_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)?;
    let state: UnlockState = serde_json::from_str(&content)?;
    Ok(remaining_at(state.expires_at, now_secs()))
}

/// Fail-safe active check for push consumption. ANY error (missing/corrupt/
/// unreadable state) resolves to `false` so push falls back to protection.
pub fn is_active() -> bool {
    matches!(status(), Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_DIR_ENV;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    fn with_temp_config(f: impl FnOnce()) {
        let tmp = TempDir::new().unwrap();
        env::set_var(CONFIG_DIR_ENV, tmp.path());
        f();
        env::remove_var(CONFIG_DIR_ENV);
    }

    #[test]
    fn test_remaining_at_active() {
        assert_eq!(remaining_at(100, 40), Some(60));
    }

    #[test]
    fn test_remaining_at_expired() {
        assert_eq!(remaining_at(100, 100), None);
        assert_eq!(remaining_at(100, 150), None);
    }

    #[test]
    #[serial]
    fn test_unlock_then_status_roundtrip() {
        with_temp_config(|| {
            unlock(15).unwrap();
            let remaining = status().unwrap().expect("window should be active");
            // 15 minutes = 900s; allow a little slack for test execution.
            assert!(remaining > 890 && remaining <= 900, "remaining={remaining}");
            assert!(is_active());
        });
    }

    #[test]
    #[serial]
    fn test_disable_clears_window() {
        with_temp_config(|| {
            unlock(15).unwrap();
            disable().unwrap();
            assert_eq!(status().unwrap(), None);
            assert!(!is_active());
        });
    }

    #[test]
    #[serial]
    fn test_absent_file_is_inactive() {
        with_temp_config(|| {
            assert_eq!(status().unwrap(), None);
            assert!(!is_active());
        });
    }

    #[test]
    #[serial]
    fn test_corrupt_file_is_failsafe_inactive() {
        with_temp_config(|| {
            ConfigManager::ensure_config_dir().unwrap();
            std::fs::write(ConfigManager::delete_unlock_path().unwrap(), "not json {{").unwrap();
            // status() surfaces the parse error, but is_active() must be fail-safe.
            assert!(status().is_err());
            assert!(!is_active());
        });
    }
}
```

- [ ] **Step 5: 在 `sync/mod.rs` 声明模块**

在 `src/sync/mod.rs:9`（`pub mod tombstone;` 附近）新增一行：

```rust
pub mod delete_unlock;
```

- [ ] **Step 6: 运行 delete_unlock 单测，确认全部通过**

Run: `cargo test --lib sync::delete_unlock -- --nocapture`
Expected: PASS（6 个用例：2 纯函数 + 4 IO/fail-safe）

- [ ] **Step 7: Commit**

```bash
git add src/config.rs src/sync/delete_unlock.rs src/sync/mod.rs
git commit -m "feat(delete-unlock): 窗口状态模块与配置路径"
```

---

### Task 2: push.rs 消费窗口状态

**Files:**
- Modify: `src/sync/push.rs`（新增纯函数 `decide_missing_action` + 改 `missing_in_repo` 分支约 615-697；测试模块 1031+ 追加用例）

**Interfaces:**
- Consumes: `crate::sync::delete_unlock::status()`（Task 1）
- Produces: `decide_missing_action(prune: bool, unlock_remaining: Option<u64>) -> MissingAction`（模块内 `pub(crate)` 供测试）

- [ ] **Step 1: 写失败测试（先加 enum + 函数签名占位，让测试可编译）**

在 `src/sync/push.rs` 顶层（`collect_missing_repo_sessions` 之前，约 173 行）新增：

```rust
/// How to handle sessions present in the sync repo but missing locally.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MissingAction {
    /// Keep them in the repo (accidental-loss protection).
    Protect,
    /// User passed `--prune`: physical sync, "Pruned N" wording.
    PruneManual,
    /// Delete-unlock window active: prune + 🔓 wording. Carries remaining minutes.
    PruneUnlock(u64),
}

/// Decide the action for locally-missing sessions.
/// Explicit `--prune` always wins over the window (and keeps the plain wording).
pub(crate) fn decide_missing_action(prune: bool, unlock_remaining: Option<u64>) -> MissingAction {
    if prune {
        MissingAction::PruneManual
    } else if let Some(secs) = unlock_remaining {
        MissingAction::PruneUnlock(secs / 60)
    } else {
        MissingAction::Protect
    }
}
```

在 `src/sync/push.rs` 测试模块（约 1031 `mod tests`）内追加：

```rust
    #[test]
    fn test_decide_missing_action_protect() {
        assert_eq!(decide_missing_action(false, None), MissingAction::Protect);
    }

    #[test]
    fn test_decide_missing_action_manual_prune_wins_over_window() {
        assert_eq!(decide_missing_action(true, None), MissingAction::PruneManual);
        assert_eq!(decide_missing_action(true, Some(600)), MissingAction::PruneManual);
    }

    #[test]
    fn test_decide_missing_action_window_prune_reports_minutes() {
        assert_eq!(decide_missing_action(false, Some(600)), MissingAction::PruneUnlock(10));
        assert_eq!(decide_missing_action(false, Some(59)), MissingAction::PruneUnlock(0));
    }
```

- [ ] **Step 2: 运行确认新单测通过（函数已随测试一并加入，应直接 PASS）**

Run: `cargo test --lib sync::push::tests::test_decide_missing_action -- --nocapture`
Expected: PASS（3 个用例）

- [ ] **Step 3: 改接 `missing_in_repo` 分支**

在 `src/sync/push.rs` 约 654 行、`if missing_in_repo.is_empty()` 之前，先读取窗口剩余：

```rust
    // Delete-unlock window: when active, treat locally-missing sessions as
    // intentional deletions (same as --prune, no tombstone). Fail-safe: any
    // error resolves to None → protection.
    let unlock_remaining = crate::sync::delete_unlock::status().ok().flatten();
```

将现有 `if missing_in_repo.is_empty() { … } else if prune { … } else { … }`（约 655-697 行）整体替换为：

```rust
    if missing_in_repo.is_empty() {
        // Nothing missing locally — no protection or pruning needed.
    } else {
        match decide_missing_action(prune, unlock_remaining) {
            MissingAction::PruneManual | MissingAction::PruneUnlock(_) => {
                // Physical sync of the deletion. No tombstone is written —
                // prune/window are physical syncs, not intentional-delete
                // registrations.
                for file_path in &missing_in_repo {
                    if let Err(e) = fs::remove_file(file_path) {
                        log::warn!("Failed to prune missing session: {}", e);
                    } else {
                        deleted_from_repo += 1;
                        log::debug!("Pruned missing session: {}", file_path.display());
                    }
                }
                if verbosity != VerbosityLevel::Quiet {
                    match decide_missing_action(prune, unlock_remaining) {
                        MissingAction::PruneUnlock(mins) => {
                            println!(
                                "  {} 删除放行窗口生效中，已同步删除 {} 个 session（剩余 {} 分钟）",
                                "🔓".yellow(),
                                deleted_from_repo,
                                mins
                            );
                        }
                        _ => {
                            println!(
                                "  {} Pruned {} missing sessions from sync repo",
                                "✓".green(),
                                deleted_from_repo
                            );
                        }
                    }
                }
            }
            MissingAction::Protect => {
                // Protection mode: refuse to propagate the local absence. The
                // repo keeps these sessions so they survive as a recoverable
                // backup.
                if verbosity != VerbosityLevel::Quiet {
                    println!(
                        "  {} Detected {} session(s) missing locally but present in sync repo — protected from deletion.",
                        "⚠".yellow(),
                        missing_in_repo.len()
                    );
                    println!(
                        "    {} Use '{}' to recover them, or '{}' to force-delete.",
                        "→".cyan(),
                        format!("{} session restore", BINARY_NAME).cyan(),
                        format!("{} push --prune", BINARY_NAME).cyan()
                    );
                }
                log::info!(
                    "Protected {} missing sessions from deletion (use --prune or unlock-delete to force)",
                    missing_in_repo.len()
                );
            }
        }
    }
```

- [ ] **Step 4: 编译并跑 push 测试，确认无回归**

Run: `cargo test --lib sync::push -- --nocapture`
Expected: PASS（原有 drift/rebase 用例 + 新增 3 个 decide 用例）

- [ ] **Step 5: Commit**

```bash
git add src/sync/push.rs
git commit -m "feat(delete-unlock): push 窗口期放行缺失删除"
```

---

### Task 3: CLI 命令 `ccs unlock-delete`

**Files:**
- Create: `src/handlers/unlock_delete.rs`
- Modify: `src/handlers/mod.rs:19,45`（模块声明区 + 导出区）
- Modify: `src/main.rs`（`enum Commands` 内新增变体 + match 分发）

**Interfaces:**
- Consumes: `crate::sync::delete_unlock::{unlock, disable, status}`（Task 1）
- Produces: `crate::handlers::unlock_delete::handle_unlock_delete(minutes: u64, off: bool, status: bool) -> anyhow::Result<()>`

- [ ] **Step 1: 创建 handler**

写入 `src/handlers/unlock_delete.rs`：

```rust
//! CLI handler for `ccs unlock-delete`.

use crate::sync::delete_unlock;
use anyhow::Result;
use colored::Colorize;

/// Handle `ccs unlock-delete`.
/// `off` and `status` take priority over opening a window.
pub fn handle_unlock_delete(minutes: u64, off: bool, status: bool) -> Result<()> {
    if off {
        delete_unlock::disable()?;
        println!(
            "{} 删除放行窗口已关闭，恢复保护模式。",
            "✓".green()
        );
        return Ok(());
    }

    if status {
        match delete_unlock::status()? {
            Some(secs) => println!(
                "{} 删除放行窗口生效中，剩余约 {} 分钟。",
                "🔓".yellow(),
                secs / 60
            ),
            None => println!(
                "{} 当前处于保护状态（删除不会同步到云端）。",
                "🔒".green()
            ),
        }
        return Ok(());
    }

    if minutes == 0 {
        anyhow::bail!("时长必须 ≥ 1 分钟；如需关闭请用 `ccs unlock-delete --off`");
    }

    let expires_at = delete_unlock::unlock(minutes)?;
    let expire_local = chrono::DateTime::from_timestamp(expires_at as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "?".to_string());

    println!(
        "{} 已开启删除放行窗口 {} 分钟（到期 {}）。",
        "🔓".yellow(),
        minutes,
        expire_local
    );
    println!(
        "  {} 窗口期内 push（含自动同步）会把本地已删除的 session 同步删除到云端，请谨慎。",
        "⚠".yellow()
    );
    Ok(())
}
```

- [ ] **Step 2: 在 `handlers/mod.rs` 声明并导出**

在 `src/handlers/mod.rs` 模块声明区（约 18 行，`pub mod uninstall;` 附近，保持字母序可放 `undo` 前后）新增：

```rust
pub mod unlock_delete;
```

在导出区（约 45 行）新增：

```rust
pub use unlock_delete::handle_unlock_delete;
```

- [ ] **Step 3: 在 `main.rs` 新增子命令定义**

在 `src/main.rs` 的 `enum Commands`（约 43-…）内，选一处（如 `Restore { … }` 之后）新增变体：

```rust
    /// Temporarily allow push to sync session deletions to the cloud
    UnlockDelete {
        /// Window duration in minutes (default: 15)
        #[arg(long, default_value_t = 15)]
        minutes: u64,

        /// Close the window now
        #[arg(long, conflicts_with = "status")]
        off: bool,

        /// Show remaining time / whether the window is active
        #[arg(long)]
        status: bool,
    },
```

- [ ] **Step 4: 在 `main.rs` 分发**

在主命令 `match command { … }` 内（`Commands::Push { … } => { … }` 分支之后，约 845 行）新增：

```rust
        Commands::UnlockDelete {
            minutes,
            off,
            status,
        } => {
            handle_unlock_delete(minutes, off, status)?;
        }
```

（`handle_unlock_delete` 经 `use handlers::*;`（main.rs:25）自动可见。）

- [ ] **Step 5: 编译**

Run: `cargo build`
Expected: 编译通过，无 warning（如有未使用告警按提示修正）。

- [ ] **Step 6: 真实实跑验证各路径（隔离配置目录，绝不碰真实配置）**

```bash
export CLAUDE_CODE_SYNC_CONFIG_DIR="$(mktemp -d)"
cargo run -q -- unlock-delete --status          # 期望：🔒 当前处于保护状态
cargo run -q -- unlock-delete --minutes 15      # 期望：🔓 已开启…15 分钟（到期 HH:MM:SS）+ ⚠ 提醒
cargo run -q -- unlock-delete --status          # 期望：🔓 生效中，剩余约 14 分钟
cargo run -q -- unlock-delete --off             # 期望：✓ 已关闭，恢复保护模式
cargo run -q -- unlock-delete --status          # 期望：🔒 当前处于保护状态
cargo run -q -- unlock-delete --minutes 0       # 期望：报错“时长必须 ≥ 1 分钟”
cat "$CLAUDE_CODE_SYNC_CONFIG_DIR/delete-unlock.json" 2>/dev/null || echo "(已清理)"
unset CLAUDE_CODE_SYNC_CONFIG_DIR
```

Expected: 各行输出与注释一致；`--minutes 0` 非零退出。

- [ ] **Step 7: Commit**

```bash
git add src/handlers/unlock_delete.rs src/handlers/mod.rs src/main.rs
git commit -m "feat(delete-unlock): 新增 ccs unlock-delete 命令"
```

---

### Task 4: 文档与问题记录

**Files:**
- Modify: `CLAUDE.md`（「Push 保护模式」相关章节）
- Modify: `docs/user-guide.md`
- Modify: `local/notes.md`

- [ ] **Step 1: 更新 `CLAUDE.md`**

在描述 Push 保护模式 / `--prune` 的段落（`local/notes.md` 与架构说明中「删除语义」相关处对应的 CLAUDE.md 位置）补充一句：

```markdown
- **临时放行删除**：`ccs unlock-delete` 开启限时窗口（默认 15 分钟），窗口期内所有 push（含 Stop hook 自动同步）会把本地已删除的 session 同步删除到云端，等价于自动 `--prune`（不写 tombstone）；到期被动恢复保护。`--minutes N` 自定义时长，`--off` 提前关闭，`--status` 查看剩余。
```

- [ ] **Step 2: 更新 `docs/user-guide.md`**

在命令示例区新增一节：

```markdown
### 临时放行 session 删除

默认情况下，本地缺失的 session 会被 push 保护（不同步删除到云端）。若你用文件管理器、`rm` 或外部服务有意删除了 session，希望删除同步上云：

​```bash
ccs unlock-delete                 # 开启放行窗口，默认 15 分钟
ccs unlock-delete --minutes 60    # 自定义时长
ccs unlock-delete --status        # 查看剩余时间
ccs unlock-delete --off           # 提前关闭
​```

窗口期内的每次 push（含自动同步）都会把本地已删除的 session 同步删除到云端；到期自动恢复保护，无需手动关闭。
```

- [ ] **Step 3: 记录到 `local/notes.md`**

在文件顶部新增一条（遵循项目记录格式）：

```markdown
## 2026-07-03: 新增删除放行窗口 `ccs unlock-delete`

### 问题描述
- 用户有时用 `rm`/文件管理器/外部服务有意删除 session，但 push 保护模式会拦截，且 Stop hook 自动 push 不带 `--prune`，导致有意删除永远同步不上云。

### 解决方案
- 新增 `src/sync/delete_unlock.rs`：`config_dir/delete-unlock.json` 存到期 unix 时间戳，被动过期、无后台进程。
- `push.rs` 新增纯函数 `decide_missing_action`，`missing_in_repo` 分支消费窗口状态：窗口生效时等价 `--prune`（不写 tombstone），打印醒目 🔓 提示；显式 `--prune` 优先且保留原文案。
- 新增 `ccs unlock-delete [--minutes N|--off|--status]`（默认 15 分钟）。
- `is_active()` fail-safe：状态文件损坏/缺失一律回退保护，绝不误删。

### 影响范围
- 新增 `src/sync/delete_unlock.rs`、`src/handlers/unlock_delete.rs`；改 `config.rs`、`sync/mod.rs`、`sync/push.rs`、`handlers/mod.rs`、`main.rs`。

### 预防措施
- 单测覆盖：`remaining_at` 纯函数、unlock/disable/status 往返、坏文件 fail-safe、`decide_missing_action` 三态；CLI 各路径隔离实跑验证。
```

- [ ] **Step 4: 全量测试 + clippy 收尾**

Run:
```bash
cargo test
cargo clippy -- -D warnings
```
Expected: 全部 PASS，clippy 无告警。

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md docs/user-guide.md local/notes.md
git commit -m "docs(delete-unlock): 用户指南与问题记录"
```

---

## 自审结论

- **Spec 覆盖**：模块/存储/被动过期 → Task 1；push 唯一改动点 + 不写 tombstone + 醒目提示 + 覆盖所有 push 路径 → Task 2；命令 `--minutes/--off/--status` + 默认 15 分钟 + 中国时区 → Task 3；文档三处 → Task 4。无遗漏。
- **占位符**：无 TBD/TODO，所有代码步骤含完整代码。
- **类型一致性**：`delete_unlock::{unlock,disable,status,is_active}`、`ConfigManager::delete_unlock_path`、`decide_missing_action`/`MissingAction`、`handle_unlock_delete(minutes,off,status)` 在定义与消费处签名一致。
- **版本**：改动完成后由 `./scripts/release.sh` 决定 minor bump 与 CHANGELOG（不在任务步骤内硬编码版本号）。
