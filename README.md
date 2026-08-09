# cc-session

[![Release](https://github.com/osen77/cc-session/actions/workflows/release-new.yml/badge.svg)](https://github.com/osen77/cc-session/actions/workflows/release-new.yml)

一个用于同步 Claude Code 对话历史的 Rust CLI 工具，支持跨设备备份和自动同步。

![Demo](image1.png)

## 功能特性

- **自动同步** - 启动时自动拉取，退出时自动推送，无需手动操作
- **多设备同步** - 在不同电脑间保持对话历史一致
- **配置同步** - 同步 settings.json、CLAUDE.md 等配置文件，支持跨平台适配
- **跨 Agent 查询** - `ccs session` 可同时查询 Claude Code、Codex 与 OMP 历史会话
- **智能合并** - 自动合并非冲突的对话变更
- **交互式配置** - 首次运行向导引导完成所有配置
- **自动更新** - 启动时检查新版本，支持一键更新

## 快速开始

### 安装

**一键安装（推荐）：**

```bash
# macOS Apple Silicon (M1/M2/M3/M4)
curl -fsSL https://github.com/osen77/cc-session/releases/latest/download/ccs-macos-aarch64.tar.gz | tar xz && sudo mv ccs /usr/local/bin/

# macOS Intel
curl -fsSL https://github.com/osen77/cc-session/releases/latest/download/ccs-macos-x86_64.tar.gz | tar xz && sudo mv ccs /usr/local/bin/

# Linux x86_64
curl -fsSL https://github.com/osen77/cc-session/releases/latest/download/ccs-linux-x86_64.tar.gz | tar xz && sudo mv ccs /usr/local/bin/
```

**其他安装方式：**

```bash
# 安装脚本（自动检测平台）
curl -fsSL https://raw.githubusercontent.com/osen77/cc-session/master/install.sh | bash

# Windows PowerShell
irm https://raw.githubusercontent.com/osen77/cc-session/master/install.ps1 | iex

# 从源码编译
git clone https://github.com/osen77/cc-session && cd cc-session && cargo install --path .
```

### 更新

```bash
# 自动更新
ccs update

# 或使用安装命令重新下载覆盖（适用于旧版本无 update 命令的情况）
curl -fsSL https://github.com/osen77/cc-session/releases/latest/download/ccs-macos-aarch64.tar.gz | tar xz && sudo mv ccs $(which ccs)
```

### 配置

```bash
ccs setup
```

向导会引导你完成所有配置，包括：
1. 选择同步模式（多设备 / 单设备）
2. 配置远程仓库（已有仓库或自动创建）
3. 设置过滤选项（排除附件、旧对话）
4. 配置自动同步（推荐）
5. 配置跨设备配置同步

### 使用

配置完成后，使用 `claude-sync` 启动 Claude Code 即可自动同步：

```bash
claude-sync
```

### 卸载

```bash
ccs uninstall
```

## 日志排查

`ccs` 默认把日志写到 stderr，并同时写入平台配置目录中的日志文件。需要临时提高可见性或收集一次运行时，可使用：

```bash
# 临时开启 console 和 file 的 DEBUG 日志
ccs --debug session list

# 将本次运行的日志写入指定文件
ccs --log-file ./ccs-debug.log session search "keyword"

# 只在 console 显示错误；file 仍按默认级别记录
RUST_LOG=error ccs session list
```

默认日志文件路径：

- macOS：`~/Library/Application Support/claude-code-sync/claude-code-sync.log`
- Linux：`~/.config/claude-code-sync/claude-code-sync.log`（设置 `XDG_CONFIG_HOME` 时位于 `$XDG_CONFIG_HOME/claude-code-sync/claude-code-sync.log`）
- Windows：`%APPDATA%\\claude-code-sync\\claude-code-sync.log`

每次 ccs 初始化文件 logger 时都会检查日志大小；超过 10 MiB 会在下一次 ccs 调用打开日志时轮转，最多保留 3 代备份。`RUST_LOG=off` 只关闭 console 输出，不会关闭 file sink。日志包含每次运行的 `invocation=I-...`，便于关联同一次运行。系统会自动脱敏常见 token/password/secret/API key、Authorization、URL userinfo，以及完整的 `file://` URI path（包括 hosted URI path 中合法的逗号、引号、`]`、`)` 等字符），不主动记录会话正文；建议不要把其他未识别的敏感内容放进 CLI 参数或日志消息。每次 file write/flush 都重新获取同一 per-log lock，并以 no-follow 方式打开 current；rotation staging 从已打开的 no-follow handle 复制，并在 live rename 前复核 inode/metadata，因此长生命周期进程不会持有轮转前的旧 inode。轮转失败会回滚；若 rollback 自身失败，transaction 目录不会被自动删除，并返回可见的安全错误，便于恢复旧日志。

该锁是 `ccs` 进程之间的 advisory lock，只约束同样取得 `.lock` 文件锁的协作方。系统 `logrotate`、用户脚本或其他直接对日志路径执行 rename/copy/append 的程序若不遵守该协议，无法承诺与 ccs rotation 原子协调；不要把“外部 rotation 测试通过”理解为所有外部 rotator 都受保护。

## 文档

📚 **[用户指南](docs/user-guide.md)** - 完整的安装配置、多设备同步、常用命令和故障排查

📚 **[开发者指南](CLAUDE.md)** - 项目架构、开发规范和贡献指南

## 常用命令

| 命令 | 说明 |
|------|------|
| `ccs` | 进入交互式会话选择（等价于 `ccs session`） |
| `ccs setup` | 交互式配置向导 |
| `ccs sync` | 双向同步 |
| `ccs automate` | 配置自动同步 |
| `ccs status` | 查看同步状态 |
| `ccs session` | 会话管理（含误删保护与恢复） |
| `ccs session search <关键词>` | 跨 Claude Code / Codex / OMP 搜索历史会话 |
| `ccs session overview --since 7d` | 查看最近项目会话概览 |
| `ccs session restore` | 恢复本地回收或同步仓库中的会话 |
| `ccs session maintain` | 配置、预演或执行测试会话维护 |
| `ccs config-sync push` | 推送配置到远程 |
| `ccs config-sync apply <device>` | 应用其他设备配置 |
| `ccs update` | 更新到最新版本 |
| `ccs uninstall` | 卸载并清理所有数据 |

更多命令请参阅 [用户指南](docs/user-guide.md)。

## 会话查询

`ccs session` 默认会同时查询三个来源：

- `CC` - Claude Code，会话文件来自 `~/.claude/projects/`
- `CX` - Codex，会话文件来自 `~/.codex/sessions/`
- `OM` - OMP，会话文件来自 `~/.omp/agent/sessions/`

来源能力矩阵：

| 来源 | 查询 | 打开 | 重命名 | 显式删除 | 本地维护 | 参与同步 |
|------|------|------|--------|----------|----------|----------|
| Claude Code | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Codex | ✅ | ❌ | ❌ | ❌ | ✅ | ❌ |
| OMP | ✅ | ✅ | ❌ | ❌ | ✅ | ❌ |

Codex 和 OMP 的普通 rename/delete 仍是只读能力；“本地维护”只允许保守地隐藏、回收和恢复测试会话，不授予通用删除能力，也不让它们参与同步。

常用示例：

```bash
# 开启维护（默认关闭），查看状态或先预演
ccs session maintain --enable
ccs session maintain --status
ccs session maintain --dry-run

# 默认列表隐藏已维护会话；需要时显式查看
ccs session list --include-hidden

# search 默认包含 hidden/recycled；只搜索 active
ccs session search "关键词" --active-only

# 查看判定原因、保护会话、恢复本地回收副本
ccs session explain <session-id> --json
ccs session keep <session-id>
ccs session restore <session-id>

# 只查询 Codex；查看 OMP 详情
ccs session --source codex list
ccs session --source omp show <session-id>
```

维护默认关闭；启用后由 `list`、`projects`、`overview` 和交互界面等 session 流程惰性推进文件动作，不安装后台 daemon；`search`/`show` 只读取并展示现有可见性。保守分类器的默认生命周期是：最后活动满 24 小时后才允许隐藏，首次隐藏满 7 天后回收到本机 maintenance store，首次隐藏满 30 天后清除本地回收副本；每次最多执行 50 个文件动作。自定义标题、`keep` 和较长会话会受到硬保护。

`list`、`overview` 和交互列表默认隐藏 `hidden`/`recycled`，使用 `--include-hidden` 查看；`search` 默认包含它们，使用 `--active-only` 排除。`purged_local` 只表示本机副本已清除，不代表远端永久删除。

自动维护不会生成 tombstone。Claude Code 对相同远端 revision 使用本地 suppression 防止 pull 复活；远端内容变化时会恢复新 revision。跨设备永久删除仍只能由显式 `ccs session delete` 或手动 `ccs push --prune` 发起。

`--source` 支持 `all`、`claude`、`codex`、`omp`，默认是 `all`。当多个来源存在相同 session ID 时，命令不会静默选择，而会要求使用 `--source` 消歧。`--since` 支持 `30m`、`24h`、`7d`、`2w`。

### 会话扫描诊断

正常扫描保持安静，不会因为扫描结果完整而额外输出提示。如果某些文件损坏、无权读取，或 session index cache 读写失败，`ccs` 会尽量保留其余结果，并在文本命令的 stderr 输出**一条聚合的 degraded warning**；逐条扫描诊断只写入文件日志，不会污染业务输出。Claude projects 根目录的 `read_dir` 失败也按 best-effort 处理：该来源记为 I/O degraded，但不会阻断 Codex/OMP 扫描或已经取得的结果。warning 中只包含稳定的 `path_hash` 和受控摘要，不包含完整路径、会话正文、原始错误内容或 cache 原始路径/错误；cache 诊断细节仅写入文件日志。

`session search` 把参数按空白拆分成多个关键词做 AND 匹配（`search A B` 与 `search "A B"` 等价，`--phrase` 保留整串），并按关键词邻近度加权排序——原文写作「AB」「A和B」的会话会上浮，JSON 中对应 `proximity` 与 `keyword_gap` 字段。所有按 ID 定位的会话命令都接受 4 个字符以上的 ID 前缀。带 `--json` 的命令失败时错误同样以 JSON 写入 stdout（退出码 1），不会留下空输出。

`session overview --json`、`session search --json` 和 `session show --json` 的业务 JSON 顶层会增加 `schema_version: 1` 与 `diagnostics`。其中 `diagnostics.diagnostic_id` 是本次扫描的关联 ID；它与同一次运行文件日志中的 `invocation=I-...` 使用同一个值，可据此在日志中定位详细诊断。`diagnostics.degraded` 始终是由计数器计算出的明确布尔值：clean 为 `false`，存在坏文件、I/O、cache 或被抑制 warning 时为 `true`。`diagnostics` 同时包含扫描计数、cache 命中/未命中、阶段耗时和受限 warning 摘要，字段语义见[用户指南的诊断字段表](docs/user-guide.md#扫描诊断与-json-输出)。

部分损坏的 JSONL 文件会保留其中可解析的 session summary，但记录一次 data warning 且不会写入 session index cache；已有 clean entry 也会在 partial 或 parser error 时 eviction，因此下一次扫描仍会重新解析并继续显示 degraded。当前 cache version 为 4；v4 新增自定义标题保护位，避免 warm cache 丢失 maintenance 的 custom-title 硬保护。命中要求 size、mtime 与流式 BLAKE3 内容 fingerprint 同时一致；fingerprint 的耗时和读取字节数会在 JSON diagnostics 的 `fingerprint_ms`、`fingerprinted_bytes` 中单独统计，I/O 失败只跳过该候选。Claude、Codex、OMP 的 source root 若存在但不是目录，也会被安全跳过并保留其他来源结果；legacy discovery 遇到 WalkDir error 会记录受控 I/O warning 并继续扫描。DualLogger 的 console/file 两个 sink 统一脱敏 home 外 Unix/Windows 绝对路径（含盘符正斜杠、反斜杠和 UNC）、任意 host 的 `file://` URI path、支持 escaped quote/控制字符/换行的完整引号凭据；文件 logger 初始化失败时只输出固定安全 warning，不泄露日志路径或底层错误。日志轮转在锁内以 staging/transaction rollback 完成，symlink 的 current、lock 或 generation 会被拒绝；sink 的 write/flush/poison 失败只发一次安全 stderr fallback 并保留运行状态。diagnostics warning 达到 detail cap 后只写一次固定 suppressed 记录，JSON 的 `suppressed_warnings` 继续准确计数。

当前明确不提供 `session list --json`，也不提供 `session doctor`；不要把这两个命令当作诊断接口。

### Session index cache 一致性边界

Session index cache 是可重建的 advisory 加速层，不是会话历史的权威存储。扫描结果按来源（Claude/Codex/OMP）分别维护 retention：只对本次**完整完成**且被选择的来源做保留/清理，未选择来源永远不会因 source filter 被误删。根目录缺失、不是目录、遍历或文件元数据/fingerprint 不完整时，该来源进入 incomplete fail-safe，旧 cache entry 保留。

对已完成来源，只有重新确认文件为 `NotFound` 才允许 prune；权限错误、读失败、文件在扫描期间变化或 fingerprint 不稳定都会保留旧 entry。Delta merge 会在锁内重新读取 latest cache，并复核文件状态，避免并发 writer 的 lost update 或旧扫描结果覆盖新内容。cache 写入使用同目录临时文件和 atomic replace；Windows 也遵循同一持久化路径，不以替换 cache 文件本身作为锁，锁文件独立存在。

cache 读写失败、非法 JSON 或版本不匹配不会丢弃已扫描的业务结果；无法安全合并时宁可跳过 cache 更新。需要可靠备份时请依赖同步仓库中的 session JSONL，不要把 cache 文件当作数据备份。

### AI Agent 系统提示词参考

在 `CLAUDE.md` 等系统提示词中添加以下内容，让 Agent 自主检索历史对话：

```markdown
## 跨会话上下文检索

需要回忆历史或查找其他项目实现时，用 `ccs session` 检索 Claude Code / Codex / OMP 历史（各参数详见 `--help`）。
典型流程：`overview` 速览全貌 → `search` 找 session_id → `show --around/--tail` 钻取上下文。

ccs session overview --json --since 7d                          # 速览最近 7 天项目动态
ccs session search <关键词1> <关键词2> -p <项目名> --json      # 多词 AND 搜索特定项目的实现
ccs session show <前8位ID> --around "<关键词>" -n 5 --json     # 钻取匹配位置上下文（ID 可写前缀）
```

## 工作原理

Claude Code 将对话历史存储在 `~/.claude/projects/` 目录下的 JSONL 文件中。

Codex 和 OMP 历史会话分别从 `~/.codex/sessions/` 与 `~/.omp/agent/sessions/` 读取，用于 `session list/search/show/overview`。它们的普通 rename/delete 能力仍只读（OMP 可打开原始会话），但可参与本机 test-session maintenance；这类本地回收不写 tombstone，也不参与同步。同步和通用写操作仍只针对 Claude Code 历史。

`ccs` 的工作流程：
1. 发现本地 Claude Code 历史中的所有对话文件
2. 复制到 Git 仓库并推送到远程
3. 拉取时，合并远程变更到本地历史
4. 冲突时保留两个版本，生成冲突报告

## 自动同步流程

```
启动时: claude-sync → 自动 pull → 启动 Claude Code
使用中: 检测新项目 → 自动 pull 该项目历史
每轮对话结束: Stop Hook → 自动 push
```

## 配置同步

除了对话历史，还支持跨设备同步 Claude Code 配置：

```bash
# 推送当前配置
ccs config-sync push

# 查看可用设备
ccs config-sync list

# 应用其他设备配置
ccs config-sync apply MacBook-Pro
```

**同步内容**：
- `settings.json` - 权限、模型配置（自动过滤 hooks）
- `CLAUDE.md` - 用户全局指令（支持平台标签过滤）
- `installed_skills.json` - 已安装的 skills 列表

**平台标签**：CLAUDE.md 支持平台特定内容，跨平台应用时自动过滤

```markdown
<!-- platform:macos -->
macOS 专用配置
<!-- end-platform -->
```

详见 [用户指南 - 配置同步](docs/user-guide.md#配置同步)。

## 安全考虑

- 对话历史可能包含敏感信息
- 建议使用私有 Git 仓库
- 推荐使用 SSH 密钥或访问令牌进行认证

## 相关资源

- **中文仓库**: https://github.com/osen77/cc-session
- **上游项目**: https://github.com/perfectra1n/claude-code-sync
- **问题追踪**: https://github.com/osen77/cc-session/issues

## 贡献

欢迎贡献！请 Fork 仓库，创建功能分支，提交 Pull Request。

---

*最后更新: 2026-03-26*
