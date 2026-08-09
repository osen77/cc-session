# 删除放行窗口（`ccs unlock-delete`）设计

- 日期：2026-07-03
- 状态：已批准，待实现
- 相关模块：`src/sync/push.rs`、`src/sync/delete_unlock.rs`（新增）、`src/config.rs`、`src/main.rs`

## 背景

现有的删除保护机制（见 `local/notes.md` 2026-06-20「删除语义重构」）中，`ccs push` 发现「本地缺失但云端仍存在」的 session 时，默认进入**保护模式**：拒绝把删除同步到云端，仅告警，并提示用 `ccs session restore` 恢复或 `ccs push --prune` 强删。`--prune` 是一次性手动逃生舱。

### 痛点

用户有时通过文件夹操作、`rm` 命令或外部服务来管理 session 文件。这些删除是**有意的**，但：

1. 保护模式会把它们当误删拦下，同步不上去；
2. 开启 `automate` 后，Stop hook 自动触发的 `push` 不带 `--prune`，外部删除**永远**同步不到云端；
3. 永久关闭保护不可接受（真误删会污染云端），每次手动加 `--prune` 又易忘。

### 目标

提供一个**限时放行窗口**：开启后一段时间内（默认 15 分钟），所有 push 路径自动放行「本地缺失」的删除同步到云端；到期自动恢复保护。零常驻开销。

## 需求与语义决策

- **放行程度（A）**：窗口期内 push 对「本地缺失、云端存在」的文件自动执行 prune（同步删除到云端），语义等价于自动加 `--prune`。**不写 tombstone**（物理同步，而非登记在案的意图删除），与现有 `--prune` 一致。
- **命令形态（A）**：新增顶层命令 `ccs unlock-delete`。
- **开启行为（A）**：只开窗口，不自动 push。窗口是"放行状态"，之后任意 push（手动或 hook）在窗口内均放行。
- **默认时长**：15 分钟。
- **安全可见性**：窗口生效期间，凡真的同步删除了云端文件的 push，都打印醒目行提示，避免"窗口忘关、悄悄删一堆"。

### 明确排除（YAGNI）

- 按项目/按 session 粒度的放行；
- 配置文件里的持久开关；
- 后台自动关闭进程/守护线程。

## 架构

### 新增模块 `src/sync/delete_unlock.rs`

职责单一：管理一个"限时放行"状态。

**存储**：`config_dir/delete-unlock.json`，复用 `ConfigManager::config_dir()`。`config.rs` 新增：

```rust
pub fn delete_unlock_path() -> Result<PathBuf> {
    Ok(Self::config_dir()?.join("delete-unlock.json"))
}
```

文件内容极简（Unix 秒，绝对时间戳，时区无关）：

```json
{ "expires_at": 1751520000 }
```

**对外 API**：

| 函数 | 语义 |
|------|------|
| `unlock(minutes: u64) -> Result<u64>` | 写入 `now + minutes*60`，返回到期戳。覆盖式写入——重复开启即续期 |
| `disable() -> Result<()>` | 删除状态文件（幂等：文件不存在也视作成功） |
| `status() -> Result<Option<u64>>` | 返回剩余秒数；已过期或无文件返回 `None` |
| `is_active() -> bool` | push 消费入口，**fail-safe**：任何读取/解析错误一律返回 `false` |

**纯逻辑分离**（可脱离文件 IO 单测）：

```rust
/// 返回剩余秒数；now >= expires_at 返回 None
fn remaining_at(expires_at: u64, now: u64) -> Option<u64>
```

**时间来源**：`SystemTime::now().duration_since(UNIX_EPOCH)`，取秒。

**fail-safe 设计理由**：状态文件损坏/解析失败时，`is_active()` 返回 `false`，push 回退到保护模式。宁可"没放行成功"也绝不"因文件坏了误删云端"。

### 过期机制：被动，无后台进程

不起定时器、不留守护进程。窗口就是一个"到期时间戳文件"：

- push 时 `is_active()` 判断 `now < expires_at`；
- `--status` 展示剩余分钟；
- 到期后文件仍在，但 `is_active()` 自然返回 `false` → 保护恢复。

符合"到期自动关闭"，零常驻开销。

### push.rs 改动点（唯一）

`push_history` 的 `missing_in_repo` 处理分支（当前 `src/sync/push.rs` 约 655–697 行），判定条件从 `prune` 改为：

```rust
let unlock_active = delete_unlock::is_active();
if missing_in_repo.is_empty() {
    // 无缺失，无需处理
} else if prune || unlock_active {
    // 执行 prune（删除云端缺失文件，不写 tombstone）
    // ...原有删除循环...
    if unlock_active && !prune {
        // 窗口触发的醒目安全提示
        println!("  🔓 删除放行窗口生效中，已同步删除 {} 个 session（剩余 {} 分钟）", deleted_from_repo, remaining_minutes);
    } else {
        // 原有 --prune 文案
    }
} else {
    // 原有保护模式文案
}
```

- 手动 `--prune`：文案与语义完全不变；
- 窗口触发的 prune：额外打印醒目安全行；
- 两者都不写 tombstone。

因所有 push 路径（手动 + Stop hook 自动，均经 `push_history`）共享该分支，窗口对它们**一并生效**，无需改动任何调用点签名。

### CLI：新增顶层命令 `ccs unlock-delete`

`main.rs` 新增子命令：

```
ccs unlock-delete                 # 开启，默认 15 分钟
ccs unlock-delete --minutes 60    # 自定义时长
ccs unlock-delete --off           # 提前关闭
ccs unlock-delete --status        # 查看剩余时间/是否生效
```

行为：

- `--minutes` 取值 `>= 1`，否则报错提示（0/负无意义）；`--off` 与 `--status` 与 `--minutes` 互斥（`--off`/`--status` 优先）；
- 开启后打印到期时刻（中国时区 Asia/Shanghai）与一句安全提醒；
- `--status`：有窗口时显示剩余分钟与到期时刻；无窗口时提示"当前处于保护状态（删除不会同步到云端）"；
- `--off`：关闭并确认。

## 数据流

```
ccs unlock-delete  ──►  写 delete-unlock.json { expires_at }
        │
（用户 rm 文件 / 外部服务删除）
        │
ccs push / Stop hook push ──► push_history ──► is_active()?
        │                                         │
        │                                    是 ──► prune 缺失 + 醒目提示（不写 tombstone）
        │                                    否 ──► 保护模式（原行为）
        │
     到期后 is_active()=false ──► 保护自动恢复
```

## 错误处理

- `is_active()`：fail-safe，任何错误 → `false`（回退保护）；
- `unlock` / `disable` / `status`：`anyhow::Result`，写入/删除失败带上下文；
- CLI `--minutes 0`：报错，提示 `--off` 用于关闭。

## 测试

遵守项目测试隔离铁律：`CLAUDE_CODE_SYNC_CONFIG_DIR` 覆盖配置目录 + 所有涉及环境变量的测试标 `#[serial]`，用 `setup_test_config_env()` / `cleanup_test_config_env()`。

**`delete_unlock.rs` 内 `#[cfg(test)]`**：

- `remaining_at`：未过期返回正确剩余、已过期返回 `None`、边界（`now == expires_at` → `None`）；
- `unlock` → `status` 往返：写入后剩余分钟落在预期区间；
- `disable` 后 `is_active()==false`、`status()==None`；
- 坏 JSON / 空文件 → `is_active()==false`（fail-safe）；
- 无文件 → `is_active()==false`、`status()==None`。

**push 分支**（对标现有 push 测试结构）：

- 窗口生效（`is_active()==true`）时，`missing_in_repo` 被 prune、云端文件被删且**不写 tombstone**；
- 窗口未生效时，维持保护（原行为回归）。

## 文档

- `CLAUDE.md`：在「Push 保护模式」相关章节补充 `unlock-delete` 说明；
- `docs/user-guide.md`：新增命令用法；
- `local/notes.md`：按项目规范记录本次功能新增（含背景、方案、影响范围）。

## 影响范围

- 新增：`src/sync/delete_unlock.rs`；
- 修改：`src/config.rs`（路径 helper）、`src/sync/push.rs`（1 处分支）、`src/sync/mod.rs`（导出）、`src/main.rs`（子命令 + 分发）；
- 版本：功能新增，建议 minor bump（由 `scripts/release.sh` 决定）。
