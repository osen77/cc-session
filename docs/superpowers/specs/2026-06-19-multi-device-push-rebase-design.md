# 多设备 push 并发自愈机制设计

- **日期**: 2026-06-19
- **状态**: Draft (待用户审阅)
- **作者**: Claude Code (brainstorming)
- **相关模块**: `src/sync/push.rs`, `src/scm/git.rs`, `src/scm/mod.rs`, `src/sync/state.rs`, `src/conflict.rs`, `src/merge.rs`, `src/handlers/hooks.rs`

## 1. 背景与问题

多台设备同时使用 `ccs push` 时,远程仓库出现分叉,后续 push 全部卡住,无法提交到远程。

### 根因(已通过源码查证)

1. **push 失败但报成功** (`src/sync/push.rs:632-643`)
   - 现有流程是 `stage_all → commit → push` 直调。
   - push 被 remote 拒绝(non-fast-forward)时,仅 `log::warn!("Failed to push: {}", e)`,UI 仍显示 "Push complete!"。
   - 用户被误导以为成功,实际本地已领先远程一个 commit,历史分叉。

2. **零并发感知与零分叉检测**
   - 无任何锁/互斥机制。
   - `src/sync/state.rs` 的 `SyncState` 不存 `last_synced_commit` 指针,无法检测分叉,也不会自动恢复。

### 现有可复用能力(不需要重写)

- `src/merge.rs`: 消息级合并,按 UUID 去重 + 时间戳选新,支持构建统一消息树。
- `src/conflict.rs::resolve_keep_both()`: 不可合并时保留两份(远程文件重命名为 `<session>-conflict-<timestamp>.jsonl`)。
- `src/scm/git.rs::current_commit_hash()`: 已封装 `rev-parse HEAD`,可作为指针比对基础。

## 2. 设计目标

- **消除静默分叉**:push 失败必须被正确感知与处理,不再误导用户。
- **自动化自愈**:并发 push 分叉时自动 `pull --rebase` 修正,保持线性历史,无需用户介入。
- **不丢数据**:不可合并时 fallback 到 keep-both,会话内容不丢失。
- **复用现有合并能力**:不重写 merge.rs / conflict.rs。
- **减少出错率**:对存量已分叉仓库主动修正;对卡死的 rebase 状态自动清理。
- **范围受控**:自愈仅对 git 仓库生效,hg 维持现状(用户场景为 git)。

### 非目标 (YAGNI)

- ❌ 不做远程文件锁 / lease 串行协调(强一致方案,过度设计)。
- ❌ 不做 git hook 自动恢复(跨平台维护负担重,可观测性差)。
- ❌ 不主动 `force push`(rebase 本地未推送 commit 已足够,force 有改写远程风险)。
- ❌ 不扩展 hg 的 rebase 自愈。

## 3. 架构与边界

### 核心思路

新增单一编排函数 `sync::push::push_with_rebase()`,封装 "commit → push → 失败时 pull --rebase → 重试 → fallback" 闭环。所有 push 入口(手动 `ccs push`、Stop hook、wrapper)收敛到此,消除散落的 `commit → push` 直调。

### 分层

```
handlers (push CLI / hooks / wrapper)
        │  全部调用
        ▼
sync::push::push_with_rebase()   ← 新增编排层(状态机)
        │  依赖
        ▼
scm::Git  (扩展 fetch / rebase / push_classified)  ← 基础设施层
        │  依赖
        ▼
merge.rs / conflict.rs (现有,复用)  ← 业务逻辑层
```

### 模块职责边界

| 模块 | 职责 | 不做什么 |
|------|------|---------|
| `scm/git.rs` | 纯 git 命令封装 + 错误分类 | 不做重试编排,不碰业务 |
| `sync/push.rs::push_with_rebase` | 重试循环 / rebase 触发 / fallback 决策 | 不实现合并算法 |
| `merge.rs` / `conflict.rs` | 消息级合并 / keep-both(现有) | 不感知 git 操作 |
| `sync/state.rs` | `last_synced_commit` 指针存取 | 不做分叉判断 |

### SCM 兼容性

`Scm` trait 被 git 和 hg 共同实现。rebase 是 git 概念。处理方式:

- trait 新增方法提供**默认实现**(返回 `Err`/不支持),git override,hg 保持默认。
- 推送自愈能力**仅对 git 仓库生效**;hg 仓库 fallback 到现有行为(commit + 普通 push + warn)。

## 4. 组件与数据流

### 组件 1: SCM 层扩展

#### 新增错误分类(`src/scm/mod.rs` 或 `src/scm/git.rs`)

```rust
pub enum PushError {
    NonFastForward,           // 远程有新提交,需 rebase
    AuthFailure(String),      // 认证 / 权限
    Network(String),          // 网络
    Other(String),            // 其他
}

pub enum RebaseOutcome {
    Clean,                       // 无冲突,rebase 成功
    Conflict(Vec<PathBuf>),      // 有冲突的文件路径列表
}
```

`PushError::NonFastForward` 通过 stderr 含 `non-fast-forward` / `! [rejected]` / `fetch first` 判定。

#### trait 新增方法(默认实现,不强迫 hg)

```rust
fn push_classified(&self, remote: &str, branch: &str) -> Result<(), PushError>;
fn fetch(&self, remote: &str) -> Result<()>;
fn rebase(&self, upstream: &str) -> Result<RebaseOutcome>;
fn is_rebase_in_progress(&self) -> Result<bool>;
fn rebase_abort(&self) -> Result<()>;
fn rebase_continue(&self) -> Result<RebaseOutcome>;  // add 后继续 rebase
```

git.rs 实现要点:

- `push_classified`: 执行 `git push`,依据 stderr 分类返回 `PushError`。
- `fetch`: `git fetch <remote>`。
- `rebase`: `git rebase <upstream>`,检测 exit code / stderr 区分 `Clean` / `Conflict`;冲突时不自动 abort,留给编排层决策。
- `is_rebase_in_progress`: 检查 `.git/rebase-merge` 或 `.git/rebase-apply` 目录存在。
- `rebase_abort` / `rebase_continue`: 对应 `git rebase --abort` / `--continue`。

### 组件 2: 编排层 `push_with_rebase`

替换 `src/sync/push.rs:632-643` 的 `commit → push` 直调段。状态机伪代码:

```
push_with_rebase(repo, state, ...):
  0. if repo.is_rebase_in_progress()?:
        log::warn!("检测到未完成的 rebase,自动 abort");
        repo.rebase_abort()?;
  1. if !repo.has_changes()? : return PushResult::NothingToPush
     repo.stage_all()?;
     repo.commit(message)?;
  2. base = current_commit_hash()
  3. for attempt in 1..=3:
       match repo.push_classified(origin, branch):
         Ok =>
           state.last_synced_commit = current_commit_hash();
           return PushResult::Clean
         Err(NonFastForward) =>
           repo.fetch(origin)?;
           match repo.rebase("origin/<branch>"):
             Clean => continue loop            // 重试 push
             Conflict(files) =>
               // try_merge_session_files: 复用 merge.rs 现有消息级合并能力
               //    (在编排层内组织对冲突文件的调用,非独立新模块)
               match try_merge_session_files(files):
                 Ok =>
                   repo.stage_all()?;
                   repo.rebase_continue()?;
                   continue loop              // 重试 push
                 Err =>
                   repo.rebase_abort()?;
                   resolve_keep_both(...);    // conflict.rs 现有逻辑
                   return PushResult::Degraded { conflicts: files }
         Err(AuthFailure|Network|Other) =>
           return Err(立即放弃,非分叉问题)
  4. return Err("3 次重试耗尽,远程竞争激烈,请稍后手动 ccs push")
```

#### PushResult 枚举

```rust
pub enum PushResult {
    Clean,                              // 正常推送
    Degraded { conflicts: Vec<PathBuf> }, // rebase 冲突已 keep-both(非失败)
    NothingToPush,                      // 无变更
}
```

`Degraded` 不是 `Err`:数据未丢失,仅降级为本地 + 冲突文件共存,Stop hook 不应因此报错打扰用户。

### 组件 3: 状态指针 `last_synced_commit`

```rust
pub struct SyncState {
    pub sync_repo_path: PathBuf,
    pub has_remote: bool,
    #[serde(default)]
    pub is_cloned_repo: bool,
    #[serde(default)]                            // 新增,向后兼容旧 state.json
    pub last_synced_commit: Option<String>,      // 上次成功 push 后的本地 HEAD
}
```

`#[serde(default)]` 保证旧 state.json 反序列化不报错(指针为 None,视为"未跟踪")。

#### 指针语义与漂移检测

- **语义**:上次成功 push 后的**本地** HEAD(非远程 HEAD)。
- **预检(诊断,不阻断)**:push 前比对 `last_synced_commit` 与当前 HEAD。若两者非祖先关系,说明历史已分叉(可能曾静默失败),主动走 rebase 路径修正。
- **更新时机**:仅 push 成功后写入。

### 数据流

#### 路径 A: 无分叉(99% 场景)

```
commit → push 成功 → 更新 last_synced_commit = HEAD → PushResult::Clean
```

#### 路径 B: 并发分叉(两台同时 push)

```
设备 X push 成功(远程 = C1)
设备 Y commit(C1') → push 拒(non-fast-forward)
  → fetch → rebase origin/branch
    → C1' 重放到 C1 之上
    → 同会话文件冲突?
        否 → push 成功(线性,无 merge commit)→ 更新指针 → Clean
        是 → merge.rs 消息级合并
               → 合并成功 → push 成功 → Clean
               → 仍冲突 → abort + keep-both → Degraded
```

## 5. 错误处理与边界

### 错误分类与处置矩阵

| 场景 | 触发 | 处置 | 用户感知 |
|------|------|------|---------|
| 正常推送 | push Ok | 更新指针 | `✓ Pushed` |
| 远程领先(可 rebase) | NonFastForward + rebase Clean | 重试 push | `↻ Rebased and pushed`(--quiet 下静默) |
| rebase 后文件冲突,可合并 | Conflict + merge.rs 成功 | add + rebase --continue + 重试 | `↻ Merged and pushed` |
| rebase 后文件冲突,不可合并 | Conflict + merge 失败 | abort + keep-both | `⚠ Degraded: N conflicts kept as separate files`(仅手动模式提示) |
| 认证 / 网络失败 | AuthFailure / Network | 立即放弃,不重试 | `✗ Push failed: <原因>`(手动模式);hook 记日志不打扰 |
| 3 次重试耗尽 | 多设备连续抢推 | 放弃 | `✗ Remote busy after 3 retries, retry later` |
| 无变更 | has_changes=false | 跳过 | `Nothing to push` |

### 关键边界处理

1. **遗留 rebase 状态自动 abort**:编排层入口前检测 `.git/rebase-merge` / `.git/rebase-apply`,若存在则 `rebase_abort`。防止仓库卡死在 rebase 中途导致后续所有 git 操作失败。Stop hook 场景无法交互,自动 abort 是更安全的选择。

2. **空提交保护**:编排层先判 `has_changes()`,无变更直接返回 `NothingToPush`,不调用 commit,避免 git 报 `nothing to commit`。

3. **last_synced_commit 漂移检测**:push 前若指针与当前 HEAD 非祖先关系,主动触发 rebase 修正。用于修复存量已分叉仓库,符合"越自动化越好、减少出错率"目标。

4. **Stop hook 静默契约**:`handle_stop()` 调用 `ccs push --quiet`,绝不因同步问题打断用户对话。
   - `--quiet` 模式下 `Degraded` / 重试均不打印,只写 `hook-debug.log`。
   - 仅 `AuthFailure` / 重试耗尽这类需用户介入的错误写简短日志。
   - 手动 `ccs push`(非 quiet)才显示完整进度与警告。

5. **hg 仓库降级**:自愈仅 git。hg 仓库调用 `push_with_rebase` 时 `push_classified` 返回 `PushError::Other("不支持")`,编排层 fallback 到现有行为(commit + 普通 push + warn),不退化不崩溃。

6. **与 config_sync 的关系**:`ccs push` 默认 `push_with_config=true` 同步设备配置,配置文件在 commit 阶段同一次提交。rebase 时配置文件在合并范围内,无需特殊处理。

## 6. 测试策略

### 测试原则

单元测试覆盖逻辑判定,集成测试覆盖真实 git 行为。沿用项目 `CLAUDE_CODE_SYNC_CONFIG_DIR` 隔离机制与 `#[serial]` 约定。

### 单元测试(纯逻辑,无 git)

| 测试 | 覆盖点 |
|------|--------|
| `test_classify_push_error_non_fast_forward` | stderr 含 `! [rejected]` → `PushError::NonFastForward` |
| `test_classify_push_error_auth` | stderr 含 `Authentication failed` → `AuthFailure` |
| `test_classify_push_error_network` | 含 `Could not resolve host` → `Network` |
| `test_pushresult_degraded_not_error` | `Degraded` 不被当作 `Err`(Stop hook 契约) |
| `test_state_backward_compat` | 旧 state.json(无 `last_synced_commit`)能正常 load,字段为 None |
| `test_drift_detection_logic` | 指针与 HEAD 非祖先关系 → 触发 rebase 标志(纯逻辑判定) |

### 集成测试(真实 git 仓库,`tests/` 目录)

用临时目录建真实 git repo + bare remote 模拟多设备:

| 测试 | 场景 | 断言 |
|------|------|------|
| `test_concurrent_push_second_rebases` | 两 worktree 抢推,第二个触发 rebase | 远程历史线性,无 merge commit,两方 commit 都在 |
| `test_rebase_clean_success` | 远程领先,push reject,rebase 无冲突 | push 成功,`last_synced_commit` 更新 |
| `test_rebase_conflict_mergeable` | 同会话文件双方改了不同消息,rebase 冲突 | merge.rs 合并成功,单一文件,push 成功 |
| `test_rebase_conflict_unmergeable_fallback` | 同会话文件不可合并 | abort rebase,keep-both 生成冲突文件,`PushResult::Degraded` |
| `test_stale_rebase_state_autofix` | 仓库预先置于 rebase-in-progress 状态 | 编排层自动 abort 后正常推送 |
| `test_auth_failure_no_retry` | 模拟认证失败 | 不重试,立即返回 Err |
| `test_retry_exhausted` | mock remote 持续 reject | 3 次后放弃 |
| `test_hg_fallback_no_crash` | hg 仓库(或 trait 默认实现) | fallback 到旧行为,不 panic |

### 测试隔离(沿用项目约定)

- 全部用 `CLAUDE_CODE_SYNC_CONFIG_DIR` 指向临时目录,**绝不碰真实 `state.json`**(项目历史教训)。
- 涉及环境变量的测试标 `#[serial]`。
- 集成测试的 git repo 用 `tempfile::TempDir`,自动清理。

### 不测的(避免过度)

- ❌ 不测 hg 的 rebase(未实现)。
- ❌ 不测真实网络(用本地 bare repo 模拟 remote)。
- ❌ 不测 3 台以上同时抢推(3 次重试耗尽是兜底,逻辑足够)。

## 7. 实现影响清单

### 新增

- `src/scm/mod.rs`: `PushError` / `RebaseOutcome` 枚举;trait 新增 `push_classified` / `fetch` / `rebase` / `is_rebase_in_progress` / `rebase_abort` / `rebase_continue`(默认实现)。
- `src/scm/git.rs`: 上述方法的 git 实现。
- `src/sync/push.rs`: `PushResult` 枚举 + `push_with_rebase()` 编排函数。
- `src/sync/state.rs`: `SyncState` 新增 `last_synced_commit` 字段 + 读写辅助。

### 修改

- `src/sync/push.rs:632-643`: `commit → push` 直调段替换为 `push_with_rebase()` 调用。
- `src/handlers/hooks.rs::handle_stop()`: push 入口对接新编排层(行为不变,只是底层自愈)。

### 不变

- `src/merge.rs`、`src/conflict.rs`: 复用,不重写。
- hg 实现路径:维持现状。
- `ccs pull` 流程:本设计聚焦 push 自愈,pull 侧不在本次范围。

## 8. 风险与回退

- **风险**:rebase 改写本地未推送 commit 的 hash。已缓解:仅 rebase 本地未推送 commit,不动远程历史,无需 force;遗留 rebase 状态自动 abort 防卡死。
- **回退**:编排层对非 git 仓库或异常情况 fallback 到现有 commit + push 行为,不引入新故障面。`#[serde(default)]` 保证旧 state.json 兼容,可平滑升级/回退版本。

---

*设计基于 brainstorming 流程,待用户审阅后转入 writing-plans 生成实现计划。*
