# ccs session 测试会话自动维护设计

日期：2026-08-08

## 1. 背景

`ccs session` 目前会聚合 Claude Code、Codex 和 OMP 三种来源。近期持续开发、评审、TDD、子 Agent 验证以及测试夹具会产生较多低价值 session，导致交互列表、项目概览和日常浏览出现噪声。

当前实现存在以下约束：

- Claude Code 支持打开、重命名、删除、恢复和同步。
- Codex 与 OMP 当前主要是只读来源；OMP 仅额外支持打开。
- Claude Code 的显式删除通过 tombstone 传播到其他设备。
- 普通查询依赖 session index cache、扫描诊断和来源完整性保护。
- `src/handlers/session.rs` 已同时承担扫描、cache、诊断、CLI、交互界面和 mutation，不适合继续内嵌自动维护逻辑。

本设计增加一个本地、可恢复、可解释的 session 自动维护系统。它不把启发式分类直接转换为跨设备删除意图。

## 2. 目标

1. 自动识别三类噪声：
   - 开发、评审、TDD 或子 Agent 实跑产生的测试型真实会话；
   - synthetic fixture 会话；
   - 消息少、持续时间短、标题明确为测试用途的低价值会话。
2. 覆盖 Claude Code、Codex 和 OMP 三种来源。
3. 使用保守、多条件、可解释规则，宁可漏判，不可轻易误判。
4. 生命周期默认采用：
   - 最后活动满 24 小时后才可隐藏；
   - 首次隐藏满 7 天后才可回收；
   - 首次隐藏满 30 天后才可本地清除。
5. 自动隐藏、回收和清除必须可观察、可恢复，并且在扫描异常时 fail-safe。
6. 保持 `ccs session search` 的跨会话检索价值；隐藏和回收内容在本地清除前仍可搜索。
7. 自动维护不得直接生成 Claude Code tombstone，不得把单机启发式判断自动扩散为跨设备删除。

## 3. 非目标

1. 不使用 LLM 对会话内容做不可解释分类。
2. 不改变现有显式 `ccs session delete` 的语义。
3. 不在本阶段把 Codex、OMP 扩展为通用 rename/delete 来源。
4. 不安装 launchd、Task Scheduler 或 cron 后台任务。
5. 不重构整个 session 子系统；只拆出本功能需要的领域模块和共享类型。
6. 不在本阶段修复既有 tombstone registry 使用裸 session ID 的架构问题；自动维护不得复用该 registry。

## 4. 核心原则

- **分类与执行分离**：分类器只输出判断，不能修改文件。
- **本地维护与同步删除分离**：自动维护状态不等于 tombstone。
- **来源限定身份**：所有内部身份使用 `(source, session_id)`。
- **重新验证后执行**：扫描结果只能作为候选，文件操作前必须重新验证。
- **异常时保留**：无法证明可以安全处理时，一律保留原文件。
- **可解释**：每个候选都必须提供稳定 `reason_codes`。
- **可撤销优先**：先隐藏，再回收，最后才本地清除。

## 5. 架构

### 5.1 新模块

新增：

```text
src/session_maintenance/
├── mod.rs
├── classifier.rs
├── state.rs
└── recycle.rs
```

职责如下：

- `mod.rs`
  - 暴露维护服务、配置和公共类型；
  - 编排分类、状态迁移、查询过滤和恢复。
- `classifier.rs`
  - 接收统一的 `MaintenanceCandidate`；
  - 以纯函数输出 `Keep` 或 `Candidate`；
  - 计算分值和 `reason_codes`。
- `state.rs`
  - 持久化生命周期、白名单、显式 marker、分类版本和 pending journal；
  - 使用独立 lock file；
  - 使用同目录临时文件、flush、`sync_all` 和 atomic replace。
- `recycle.rs`
  - 执行回收、恢复和本地清除；
  - 验证路径、symlink、mtime、size 和 fingerprint；
  - 处理同文件系统 rename 与跨文件系统安全复制。

### 5.2 统一候选结构

三种来源的 parser 保持不变，在扫描摘要之上构造：

```rust
struct MaintenanceCandidate {
    identity: SessionIdentity,
    file_path: PathBuf,
    project_name: String,
    title: String,
    has_custom_title: bool,
    user_message_count: usize,
    message_count: usize,
    first_activity: Option<SystemTime>,
    last_activity: Option<SystemTime>,
    size: u64,
    fingerprint: FileFingerprint,
}
```

分类器不直接理解 Claude、Codex、OMP 的 JSONL 格式。

### 5.3 持久化位置

```text
<config_dir>/
├── session-maintenance.json
├── session-maintenance.lock
└── session-recycle/
    ├── claude/<session-id>/<fingerprint>.jsonl
    ├── codex/<session-id>/<fingerprint>.jsonl
    └── omp/<session-id>/<fingerprint>.jsonl
```

state 只保存来源根目录内的相对原路径，不保存可跨机器误用的绝对路径。恢复时重新发现当前来源根目录，再经过 containment 校验拼接目标路径。回收文件以 source、session ID 和 fingerprint 分层，避免同名文件覆盖。

配置写入现有 `config.toml`：

```toml
[session_maintenance]
enabled = false
classifier = "conservative"
hide_after_hours = 24
recycle_after_days = 7
purge_after_days = 30
max_actions_per_run = 50
```

默认关闭。用户必须显式执行 `ccs session maintain --enable`，避免升级后读命令突然改变历史文件。

## 6. 分类规则

### 6.1 安全门槛

以下任一条件成立时，本轮不能自动隐藏、回收或清除：

- 最后活动不足 24 小时；
- 文件在扫描后发生 size、mtime 或 fingerprint 变化；
- 会话有用户自定义标题；
- 会话已加入 `keep` 白名单；
- 来源扫描为 incomplete 或 degraded；
- 原文件是 symlink；
- 路径不在经验证的来源根目录内；
- 同一来源存在重复 session ID，无法唯一定位；
- state、锁、journal 或回收目录存在无法解释的不一致。

### 6.2 确定性命中

确定性信号可直接达到阈值，但仍受 24 小时冷却期保护：

- 用户或测试框架通过 `mark-test` 写入显式 marker；
- session ID 匹配保留的 fixture 格式，例如 `cc-task4`、`cx-cache-task4`；
- fixture ID 与 `/tmp/task*-project` 等 fixture cwd 共同出现；
- ID、cwd 和会话结构共同匹配测试夹具协议。

“文件损坏”本身不能作为测试会话证据。

### 6.3 组合评分

普通历史会话需要多个信号共同达到默认阈值 `70`：

| 信号 | 分值 |
|---|---:|
| 精确测试标题，如 `测试`、`test`、`hello`、`试一下` | 35 |
| 自动验证型标题，如 `fixture`、`smoke test`、`test brief` | 25 |
| 用户消息不超过 2 条 | 20 |
| 总消息不超过 6 条 | 10 |
| 首尾活动不超过 15 分钟 | 15 |
| cwd 位于系统临时目录 | 20 |
| 非正常 UUID 且匹配保留测试前缀 | 60 |

标题关键词本身不能达到阈值。

反向保护：

- 总消息超过 20：不自动分类；
- 持续时间超过 2 小时：不自动分类；
- 有自定义标题：不自动分类；
- 已恢复或显式 `keep`：不自动分类，除非用户解除白名单。

分类结果示例：

```json
{
  "classification": "test_candidate",
  "score": 70,
  "reason_codes": [
    "exact_test_title",
    "few_user_messages",
    "short_duration"
  ]
}
```

state 保存 `classifier_version`。规则升级后，旧结果必须重新评估，不能直接沿用。

## 7. 生命周期

```text
visible
  │ 分类命中且最后活动 >= 24h
  ▼
hidden
  │ hidden_since >= 7d，重新验证仍命中
  ▼
recycled
  │ hidden_since >= 30d，重新验证无冲突
  ▼
purged_local
```

时间从首次进入 `hidden` 开始计算。已有旧会话在功能首次启用时最多进入 hidden，不能直接回收或清除。

每次推进状态前必须：

1. 重新加载最新 state；
2. 重新确认来源扫描完整；
3. 重新验证路径、mtime、size 和 fingerprint；
4. 重新运行分类器；
5. 确认没有 keep、restore 或内容更新。

内容变化后，旧分类失效并恢复为 visible。

## 8. 查询与交互语义

### 8.1 默认可见性

- `ccs session`
- `ccs session list`
- `ccs session projects`
- `ccs session overview`

默认只展示 visible。增加 `--include-hidden` 后展示 hidden 和 recycled，并附带状态。

### 8.2 Search

`ccs session search` 默认搜索：

- visible；
- hidden；
- recycled。

结果增加 `visibility` 字段。增加 `--active-only` 可只搜索 visible。

这保证全局“先查再答”的历史检索流程不会因为列表降噪而提前丢失内容。

### 8.3 Show

`ccs session show <id>` 能解析 visible、hidden 和 recycled，并显示：

- 当前 lifecycle state；
- score；
- reason codes；
- 首次隐藏和到期时间。

## 9. CLI 设计

```bash
ccs session maintain --enable
ccs session maintain --disable
ccs session maintain --status
ccs session maintain --dry-run
ccs session maintain --run

ccs session list --include-hidden
ccs session search <keyword> --active-only
ccs session explain <session-id>
ccs session keep <session-id>
ccs session unkeep <session-id>
ccs session mark-test <session-id>
ccs session unmark-test <session-id>
ccs session restore <session-id>
```

### 9.1 `maintain`

- `--enable`：开启惰性自动维护；
- `--disable`：停止自动状态推进，不改变已有状态；
- `--status`：显示配置、候选数、各状态数量、下一到期时间；
- `--dry-run`：扫描和分类，但不写 maintenance state、不移动或删除 session 文件；现有 advisory session cache 的读写行为保持不变；
- `--run`：立即执行一轮维护。

启用后，session 系列命令完成完整扫描后复用同一维护入口。每次最多执行 `max_actions_per_run` 个文件迁移或 purge。达到上限时必须输出剩余数量，不允许静默截断。

visible 变 hidden 只更新 registry，不移动文件。

### 9.2 `explain`

输出分类状态、score、安全门槛、reason codes 和下次状态变化时间。支持文本和 JSON，便于用户与 Agent 排查误判。

### 9.3 `keep` / `unkeep`

`keep` 将 session 加入白名单。如果当前为 hidden，则恢复 visible；如果当前在回收区，则先恢复文件，再写入 keep。`unkeep` 只解除白名单，不立即隐藏或移动文件；后续维护轮次必须重新分类并重新开始生命周期。

### 9.4 `mark-test` / `unmark-test`

`mark-test` 写入显式测试 marker。marker 仍受 24 小时冷却期保护，且该命令不直接移动或删除文件。`unmark-test` 只移除显式 marker；如果仍满足保守组合规则，后续仍可能被分类，用户需要 `keep` 才能明确阻止自动维护。

### 9.5 `restore`

解析顺序：

1. maintenance state 为 hidden：恢复 visible；
2. 本地回收区存在副本：恢复到原来源目录；
3. Claude Code 本地无副本：沿用现有同步仓库恢复；
4. Codex/OMP 本地无副本：报告不可恢复。

恢复成功后默认写入 keep，避免下一轮再次分类。

## 10. 三来源写操作边界

### Claude Code

- 自动隐藏、回收、purge 都是本机维护；
- 不自动写 tombstone；
- 显式 `delete` 保持现有跨设备 tombstone 语义；
- purge 后同步仓库可能仍保留备份。

### Codex

- 允许 maintenance 模块本地回收和恢复原文件；
- 普通 rename/delete 仍保持拒绝；
- 不参与 ccs 同步，不写 tombstone。

### OMP

- 允许 maintenance 模块本地回收和恢复原文件；
- 普通 rename/delete 仍保持拒绝；
- 保留现有 open 能力；
- 不参与 ccs 同步，不写 tombstone。

文档中的能力矩阵应增加“本地维护”列，避免把 maintenance 误解为通用删除能力。

## 11. Claude 同步抑制

Claude 自动回收后，远端同步仓库仍可能包含相同 session。为防止 pull 反复恢复：

- maintenance state 保存 `(source, session_id, fingerprint)` 本地抑制记录；
- pull 遇到完全相同 fingerprint 时不重新落回活动目录；
- push 不把该缺失解释为自动远端删除，不写 tombstone；
- 远端同 ID 内容 fingerprint 改变时视为新修订，解除抑制并恢复到活动目录；
- 用户显式 restore 时解除抑制。

该抑制逻辑必须使用 source-qualified identity，不能复用当前按裸 session ID 匹配的 tombstone registry。

`purged_local` 对 Claude 的含义是“本地回收副本已清除”，不是“所有设备和远端永久销毁”。跨设备永久删除仍必须由用户显式执行现有 delete 流程。

Claude 的最小抑制记录在 `purged_local` 后继续保留，直到用户 restore、远端同 ID 内容变化，或同步仓库已确认不再存在该会话。Codex/OMP 的 purged audit record 再保留 30 天后可从 state 中裁剪，因为它们没有远端自动复活路径。

## 12. 文件事务与恢复

### 12.1 同文件系统

1. 锁内重新加载最新 state；
2. 重新验证来源、路径和 fingerprint；
3. 写入 `pending_recycle` 并 fsync；
4. 原子移动到回收 staging；
5. 将 staging 持久化为最终回收文件；
6. 写入 `recycled` 状态并 atomic save；
7. 清除 pending。

### 12.2 跨文件系统

rename 不可用时：

1. 安全创建目标临时文件；
2. 流式复制并同时计算 fingerprint；
3. flush + `sync_all`；
4. 校验目标 fingerprint；
5. atomic persist 目标；
6. 重新验证源文件；
7. 最后删除源文件；
8. 更新 state。

任何一步失败都优先保留源文件。

### 12.3 Pending journal 协调

启动时处理未完成事务：

- 源存在、目标不存在：取消 pending，保留源；
- 源不存在、目标存在：补齐 recycled 状态；
- 两者都存在且 fingerprint 相同：保留目标，重新验证后处理重复源；
- 两者都存在但内容不同：停止并报告冲突；
- 两者都不存在：标记 degraded，不能伪造成功。

## 13. 并发与错误处理

- maintenance 使用独立 lock file；
- 扫描可在锁外运行，但状态迁移必须在锁内重新验证；
- state 保存复用 session cache 已验证的 same-dir temp + atomic replace 模式；
- degraded 或 incomplete 来源本轮不推进破坏性状态；
- 单个 session 失败不阻断其他安全候选，但诊断必须准确累计；
- 日志只记录 source、reason code、稳定 path hash 和受控错误类别；
- 不记录完整路径、标题正文、消息内容或底层敏感错误；
- 回收目录和恢复目标拒绝 symlink，并使用现有 path containment helper；
- 状态推进必须幂等，重复运行不能重复移动或丢失文件。

## 14. Cache 集成

- visible/hidden 仍在原来源目录时，继续使用现有 session index cache；
- 默认 list/overview 在摘要层叠加 maintenance visibility，不重复解析文件；
- recycled 文件仅在 search、show 或 `--include-hidden` 时扫描回收根；
- recycled cache entry 仍保留原 source，cache key 必须包含实际文件路径；
- maintenance state 不是会话内容权威存储，原始 JSONL 或回收副本才是权威内容；
- cache 错误不能导致 maintenance 推进状态。

## 15. 测试策略

### 15.1 分类器单元测试

- 三来源显式 marker；
- 已知 fixture ID/cwd；
- 单一 test 关键词不足以命中；
- 多信号达到阈值；
- 自定义标题、长会话、近期活跃受到保护；
- classifier version 变化触发重新评估；
- keep 优先于启发式分类；
- 损坏文件不能仅凭损坏状态被判为测试。

### 15.2 生命周期测试

使用注入 clock，不真实 sleep：

- 24 小时前不隐藏；
- 首次启用不会把旧候选直接回收；
- hidden 7 天后回收；
- hidden 30 天后本地 purge；
- fingerprint 变化恢复 visible；
- restore 后写入 keep；
- disable 后不推进状态。

### 15.3 文件安全测试

- Unix、Windows 路径；
- symlink 拒绝；
- `..` 路径逃逸拒绝；
- 同文件系统 rename；
- 跨文件系统 copy + fsync + verify；
- pending journal 各协调分支；
- 中途失败不丢源文件；
- 重复执行幂等。

### 15.4 并发测试

- 两个进程同时分类同一 session；
- 一个进程 restore，另一个进程 recycle；
- 扫描后、加锁前文件变化；
- state writer 不丢更新；
- reader 永远只能读取合法完整 JSON；
- action budget 达到上限时正确保留剩余任务并报告。

### 15.5 CLI 和真实数据测试

- `--dry-run` 不写 maintenance state、不移动或删除 session 文件；
- enable/disable/status/run；
- list/projects/overview 默认隐藏；
- `--include-hidden` 展示状态；
- search 默认仍命中 hidden/recycled；
- `--active-only` 排除 hidden/recycled；
- show/explain/keep/unkeep/mark-test/unmark-test/restore；
- 三来源回收与恢复；
- Claude pull 不复活相同 fingerprint；
- Claude 新修订解除抑制；
- Codex/OMP 不产生 tombstone；
- 所有环境变量测试使用 `CLAUDE_CODE_SYNC_CONFIG_DIR` 并标记 `#[serial]`。

## 16. 文档与问题记录

实现时同步更新：

- `README.md`；
- `docs/user-guide.md`；
- 项目 `CLAUDE.md` 的 session 能力矩阵、模块结构和测试策略；
- `local/notes.md`，记录分类误判边界、同步抑制和事务恢复。

## 17. 验收标准

1. 开启维护后，明确 fixture 与多信号测试会话能按时间线自动隐藏、回收和本地清除。
2. 单一 test 关键词、近期活动、长会话、自定义标题不会被自动隐藏。
3. 任一来源扫描 degraded 时，该来源零破坏性状态推进。
4. `ccs session search` 在本地 purge 前仍能搜索 hidden/recycled 内容。
5. 所有分类结果能通过 `explain` 给出稳定 reason codes。
6. restore 能恢复 hidden/recycled，并避免下一轮再次自动分类。
7. 自动维护不会生成 tombstone，也不会传播 Codex/OMP 删除。
8. Claude 相同 fingerprint 不被 pull 复活；新修订能恢复。
9. 崩溃、并发和跨文件系统复制测试证明不会丢失唯一源文件。
10. `cargo test`、目标集成测试、`cargo clippy -- -D warnings` 和 `cargo fmt --check` 全部通过。

## 18. 实施顺序建议

1. 提取 typed maintenance candidate 与纯分类器，先完成规则测试。
2. 实现配置、state、clock 和生命周期纯逻辑。
3. 实现 recycle 事务、journal 和安全测试。
4. 接入三来源扫描摘要和 visibility overlay。
5. 增加 maintain/explain/keep/unkeep/mark-test/unmark-test/restore CLI。
6. 接入 search/show/recycled 扫描。
7. 接入 Claude pull/push 本地抑制。
8. 完成真实 CLI 测试、文档、`local/notes.md` 和全量验证。
