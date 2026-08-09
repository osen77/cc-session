# Session Optimization Backlog

本文件记录 Session Cache Correctness Slice Task 4 之后的优化候选。以下项目均为后续 backlog，不属于 Task 4 的实现范围；优先级可在实际 metrics、用户反馈和跨平台验证后调整。

## P0：先补可观测性与正确性基础

### 1. Hook logging 统一

将 Hook 的 `hook-debug.log` 与 ccs DualLogger 的结构化 invocation、脱敏和轮转能力统一，保留 Hook 调用链的独立 operation/source 字段。优先处理受限 PATH、后台失败和重试链路的可定位性；不得把 session 正文或原始错误泄露到普通输出。

### 2. Codex history dependency fingerprint

为 Codex session cache 命中增加 `~/.codex/history.jsonl` 的依赖 fingerprint（或按 session 的稳定依赖摘要）。Codex 标题可能来自 history 文件，history 变化时不能继续复用陈旧 title。需要先定义粒度和成本，再补命中/失效 metrics。

### 3. Interactive snapshot

为交互式 session 菜单建立一次扫描 snapshot，保证项目列表、会话列表和详情操作使用同一批次的 source-aware identity，避免长时间交互期间文件变化造成 stale selection。涉及 rename/delete 前仍需重新校验文件状态和 root containment。

## P1：稳定机器接口与大规模查询体验

### 4. `session list --json`

提供稳定的机器可消费列表 DTO，复用现有 diagnostics、source filter、分页/排序语义。先明确 schema version、错误/退化契约和与 overview 的字段边界，不直接暴露内部 cache entry。

### 5. `session doctor --json`

提供只读诊断命令，汇总 source roots、cache 状态、最近扫描 metrics、日志关联 ID 和可行动建议。必须保持路径、原始 OS error、session 正文和凭据脱敏；不能把 advisory cache failure 误报成数据丢失。

### 6. Agent JSON 稳定 DTO / 分页

为 Agent 使用场景设计版本化 JSON DTO、稳定 `(source, session_id)` identity、游标或 page token、排序和 bounded result size。避免直接序列化交互 handler 内部结构；明确分页期间的 snapshot/一致性语义。

## P2：性能优化（以 metrics 驱动）

### 7. Streaming summary parser

评估将三类 parser 的 summary 路径改为 streaming/增量解析，减少大 JSONL 文件的内存峰值和重复读取。必须先通过 `parse_ms`、`parsed_bytes`、fingerprint 成本和文件大小分布确认收益，再设计 partial/error 语义兼容方案。

### 8. 基于 metrics 再决定搜索索引

暂不预设引入全文搜索索引。先收集 search full-load 时间、session 数量、文件大小、查询频率、cache hit/miss 和用户可接受延迟；只有 metrics 显示扫描/加载成为主要瓶颈时，再选择增量倒排索引或其他方案，并定义失效、重建和 source-aware retention 规则。

## 范围声明

Task 4 已完成的是 cache 并发、atomic persist、source-aware retention、incomplete fail-safe、confirmed NotFound prune、隔离测试和文档说明。上述优化项目尚未实施，不应被当前版本的 CLI 帮助、诊断 JSON 或用户指南描述为已提供能力。
