# Claude Code Sync 用户指南

本文档包含 `ccs` 的安装配置、多设备同步和常用示例。

---

## 目录

- [安装与更新](#安装与更新)
- [多设备同步配置](#多设备同步配置)
- [日常使用](#日常使用)
- [自动同步（推荐）](#自动同步推荐)
- [配置同步](#配置同步)
- [会话管理](#会话管理)
- [常用命令示例](#常用命令示例)
- [高级配置](#高级配置)
- [日志排查](#日志排查)
- [故障排查](#故障排查)
- [卸载](#卸载)

---

## 安装与更新

### 一键安装（推荐）

直接下载最新版预编译二进制，无需额外依赖：

```bash
# macOS Apple Silicon (M1/M2/M3/M4)
curl -fsSL https://github.com/osen77/cc-session/releases/latest/download/ccs-macos-aarch64.tar.gz | tar xz && sudo mv ccs /usr/local/bin/

# macOS Intel
curl -fsSL https://github.com/osen77/cc-session/releases/latest/download/ccs-macos-x86_64.tar.gz | tar xz && sudo mv ccs /usr/local/bin/

# Linux x86_64
curl -fsSL https://github.com/osen77/cc-session/releases/latest/download/ccs-linux-x86_64.tar.gz | tar xz && sudo mv ccs /usr/local/bin/
```

> **不确定你的 Mac 芯片？** 运行 `uname -m`，输出 `arm64` 是 Apple Silicon，`x86_64` 是 Intel。

### 安装脚本

自动检测平台并安装：

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/osen77/cc-session/master/install.sh | bash

# Windows PowerShell
irm https://raw.githubusercontent.com/osen77/cc-session/master/install.ps1 | iex
```

### 从源码安装

```bash
git clone https://github.com/osen77/cc-session
cd cc-session
cargo install --path .
```

### 更新

```bash
# 方式一：内置更新命令
ccs update

# 方式二：重新下载覆盖（适用于旧版本无 update 命令的情况）
curl -fsSL https://github.com/osen77/cc-session/releases/latest/download/ccs-macos-aarch64.tar.gz | tar xz && sudo mv ccs $(which ccs)
```

> 将 URL 中的 `ccs-macos-aarch64` 替换为你的平台：`ccs-macos-x86_64`（Intel Mac）、`ccs-linux-x86_64`（Linux）。

---

## 多设备同步配置

### 前置条件

- 已创建 GitHub 私有仓库（如 `claude-code-history`）
- 已在所有设备上安装 `ccs`
- 已配置 Git 认证（推荐使用 `gh auth login`）

### 设备 A（首次设置）

```bash
# 运行配置向导
ccs setup
```

向导会引导你：
1. 选择同步模式（多设备/单设备）
2. 输入或创建远程仓库
3. 设置本地备份目录
4. 设置过滤选项（排除附件、旧对话）
5. 可选执行首次同步
6. 配置自动同步（推荐）- 启动时自动拉取，退出时自动推送
7. 配置跨设备配置同步

### 设备 B（加入同步）

```bash
# 运行配置向导，选择已有仓库
ccs setup
```

或手动初始化：

```bash
ccs init \
  --local ~/claude-history-backup \
  --remote https://github.com/YOUR_USERNAME/claude-code-history.git \
  --clone
```

### 验证配置

```bash
# 确认显示 "Use project name only: Yes"
ccs config --show

# 查看状态
ccs status
```

---

## 日常使用

### 推荐：sync 命令

```bash
# 开始/结束工作时执行
ccs sync
```

`sync` 命令会自动：
1. 拉取远程更新 (pull)
2. 合并本地变更
3. 推送到远程 (push)

### 分步操作

```bash
# 仅拉取
ccs pull

# 仅推送
ccs push -m "Update from Mac"

# 强制推送并修剪远程已在本地手动物理删除的历史（逃生舱机制，通常在误删保护触发时使用）
ccs push --prune -m "Force prune missing sessions"
```

### 切换设备工作流

**在设备 A 结束工作：**
```bash
ccs push -m "Windows session"
```

**在设备 B 开始工作：**
```bash
ccs pull
```

---

## 自动同步（推荐）

自动同步可以免去手动执行 `push`/`pull` 的麻烦。

### 配置方式

**方式一：通过 setup 向导（新用户推荐）**

```bash
ccs setup
```

向导最后会询问是否配置自动同步，选择"是"即可一键完成所有配置。

**方式二：单独配置（已完成 setup 的用户）**

```bash
ccs automate
```

此命令会：
1. 安装 Claude Code Hooks（退出时自动推送）
2. 创建启动包装脚本（启动时自动拉取）

### 使用方式

配置完成后，使用 `claude-sync` 替代 `claude` 启动 Claude Code：

```bash
# 使用包装脚本启动（推荐）
claude-sync

# 或添加别名到 shell 配置文件（~/.bashrc 或 ~/.zshrc）
alias claude='claude-sync'
```

### 自动同步流程

```
启动时: claude-sync → 自动 pull → 启动 Claude Code
使用中: 检测新项目 → 自动 pull 该项目历史
每轮对话结束: Stop Hook → 自动 push
```

### 管理命令

```bash
# 查看自动同步状态
ccs automate --status

# 卸载自动同步
ccs automate --uninstall

# 单独管理 hooks
ccs hooks install    # 安装 hooks
ccs hooks uninstall  # 卸载 hooks
ccs hooks show       # 查看状态

# 单独管理包装脚本
ccs wrapper install    # 创建 claude-sync
ccs wrapper uninstall  # 删除 claude-sync
ccs wrapper show       # 查看状态
```

### Hooks 说明

| Hook | 触发时机 | 功能 |
|------|----------|------|
| `SessionStart` | Claude Code 首次启动时 | 拉取最新历史（三重条件检测） |
| `Stop` | 每轮对话完成后 | 推送对话历史 |
| `UserPromptSubmit` | 每次发送消息时 | 检测新项目并拉取远程历史 |

> **SessionStart 三重条件检测**：只有同时满足以下条件才会执行 pull：
> 1. 进程数 = 1（没有其他 Claude 实例）
> 2. source = "startup"（不是 resume/compact）
> 3. 5分钟内未触发过（防抖保护）
>
> 这确保了 `/new`、新窗口、对话压缩等场景不会重复拉取。详见 [Hooks 避坑指南](claude-code-hooks-guide.md)。

### 调试

如果自动同步未生效，可先查看下方的通用日志排查章节。Hook 目前仍有独立的 `hook-debug.log` 调试输出，**Hook debug 尚未统一到 ccs 的 DualLogger**，这是后续工作；这里的文件不是 `claude-code-sync.log`。

```bash
# macOS
cat ~/Library/Application\ Support/claude-code-sync/hook-debug.log

# Linux
cat ~/.config/claude-code-sync/hook-debug.log
```

---

## 配置同步

除了对话历史，`ccs` 还支持同步 Claude Code 配置文件，让你在多个设备间保持一致的使用体验。

### 同步内容

| 文件 | 默认同步 | 说明 |
|------|---------|------|
| `settings.json` | ✅ | 权限、模型配置（自动过滤 hooks 字段） |
| `CLAUDE.md` | ✅ | 用户全局指令（支持平台标签） |
| `installed_skills.json` | ✅ | 已安装 skills 列表 |
| `hooks/` | ❌ | 默认不同步（路径兼容问题） |

### 基本命令

```bash
# 推送当前设备配置到远程
ccs config-sync push

# 查看远程所有设备配置
ccs config-sync list

# 应用其他设备的配置
ccs config-sync apply MacBook-Pro

# 查看配置同步状态
ccs config-sync status
```

### 平台标签

CLAUDE.md 中可能包含平台特定内容。使用平台标签标记后，跨平台应用时会自动过滤。

**标签格式：**

```markdown
# 通用内容（所有平台共享）

## 通用规范
- 代码规范...

<!-- platform:macos -->
## macOS 环境
- 使用 fnm 管理 node 版本
- Homebrew 路径: /opt/homebrew/bin
<!-- end-platform -->

<!-- platform:windows -->
## Windows 环境
- 使用 nvm-windows 管理 node 版本
- 路径分隔符使用反斜杠
<!-- end-platform -->

<!-- platform:linux -->
## Linux 环境
- 使用 nvm 管理 node 版本
<!-- end-platform -->
```

**支持的标签：**

| 标签 | 别名 | 平台 |
|------|------|------|
| `macos` | `mac`, `darwin` | macOS |
| `windows` | `win` | Windows |
| `linux` | - | Linux |

### 应用配置示例

**场景：** 在 Windows 上应用来自 Mac 的配置

```bash
# 查看可用设备
ccs config-sync list
# 输出: MacBook-Pro, Windows-PC

# 应用 Mac 配置
ccs config-sync apply MacBook-Pro
```

**结果：**
- `settings.json` 完整应用（hooks 字段自动过滤）
- `CLAUDE.md` 保留通用内容 + 保留本地 Windows 平台块
- macOS 平台块内容被过滤

### 设备名称

配置按设备名存储在仓库的 `_configs/<device>/` 目录下。

设备名获取优先级：
- **macOS**: 系统偏好设置中的「电脑名称」
- **Windows**: COMPUTERNAME 环境变量
- **Linux**: /etc/hostname

如果名称包含中文或特殊字符，会自动替换为 `-`。

### 目录结构

```
sync-repo/
├── _configs/                    # 配置同步目录
│   ├── MacBook-Pro/
│   │   ├── settings.json
│   │   ├── CLAUDE.md
│   │   └── installed_skills.json
│   └── Windows-PC/
│       └── ...
│
└── projects/                    # 对话历史目录
    └── ...
```

---

## 会话管理

`ccs` 提供交互式会话管理功能，默认可展示 Claude Code、Codex 和 OMP 三个来源的会话。Claude Code 支持完整管理和同步；Codex/OMP 的普通 rename/delete 能力只读，但三种来源都可参与本机 test-session maintenance。

### 来源能力矩阵

| 来源 | 查询 | 打开 | 重命名 | 显式删除 | 本地维护 | 参与同步 |
|------|------|------|--------|----------|----------|----------|
| Claude Code | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Codex | ✅ | ❌ | ❌ | ❌ | ✅ | ❌ |
| OMP | ✅ | ✅ | ❌ | ❌ | ✅ | ❌ |

Codex 和 OMP 不参与同步，也不能通过普通 Rename/Delete 删除会话；本地维护只允许隐藏、回收和恢复被保守分类为测试噪声的副本，不授予通用删除能力。交互菜单会按来源能力隐藏不适用的操作。

### 交互模式（推荐）

```bash
# 最常用：直接进入交互式会话选择，默认展示三个来源
ccs

# 等价的显式写法
ccs session

# 只展示 Codex
ccs session --source codex
```

双向同步不再是裸命令的默认行为，需要显式运行 `ccs sync`。

**在项目目录中运行：**
- 自动识别当前目录对应的项目
- 直接显示该项目的会话列表

**在非项目目录中运行：**
- 显示所有项目列表供选择

**菜单按键（v0.5.8+）：**

| 按键 | 作用 |
|------|------|
| `↑` `↓` | 移动光标（循环），`PageUp`/`PageDown` 翻页，`Home`/`End` 跳转 |
| `→` 或 `Enter` | 进入选中项（下一级） |
| `←` | 返回上一级（项目列表 ← 会话列表 ← 操作菜单） |
| 直接打字 | 过滤当前列表（大小写不敏感），`Backspace` 删除 |
| `Esc` / `Ctrl-C` | 取消：顶层菜单退出程序，操作菜单返回上一级 |
- 选择项目后进入该项目的会话列表

**导航操作：**
- 选择会话 → 进入按来源过滤的操作菜单
- Claude Code 可选择详情、打开、重命名或删除
- OMP 可选择详情或打开原始 OMP 会话
- Codex 只能查看详情
- 选择「← 切换到其他项目」→ 返回项目列表
- 选择「✕ 退出」→ 退出程序
- 操作完成后可返回上一级继续操作

### 非交互模式

```bash
# 列出所有来源的会话
ccs session list

# 只列出 Codex 会话
ccs session --source codex list
ccs session list --source codex   # 等价

# 显示完整会话 ID
ccs session list --show-ids

# 查看会话详情（session-id 可以只写前缀，最少 4 个字符）
ccs session show <session-id>
ccs session show ec41747d

# 查看 OMP 会话详情
ccs session --source omp show <session-id>

# 搜索：多个关键词按空白拆分后 AND 匹配
ccs session search 验证脚本 打包脚本
ccs session search "验证脚本 打包脚本"    # 等价，引号内也会拆分

# 把整个查询当作一个短语，不拆分
ccs session search "验证脚本 打包脚本" --phrase

# 重命名会话（仅 Claude Code）
ccs session rename <session-id> "新的标题"

# 删除会话（仅 Claude Code，需确认）
ccs session delete <session-id>

# 强制删除（跳过确认）
ccs session delete <session-id> --force

# 恢复本地 maintenance 回收副本；Claude 无本地副本时可回退同步仓库
ccs session restore <session-id>

# 维护默认关闭：开启、查看、预演
ccs session maintain --enable
ccs session maintain --status
ccs session maintain --dry-run

# 查看隐藏/回收会话，或只搜索 active
ccs session list --include-hidden
ccs session search "关键词" --active-only

# 查看分类原因、永久保护当前会话
ccs session explain <session-id> --json
ccs session keep <session-id>
```

`--source` 支持 `all`、`claude`、`codex`、`omp`，默认是 `all`。当同一个 session ID 出现在多个来源时，`show`、maintenance marker 和 restore 会要求使用 `--source` 消歧；`rename` 和 `delete` 也不会静默选择错误来源。

**Session ID 前缀。** 所有按 ID 定位的命令（`show`、`rename`、`delete`、`explain`、`restore`、`keep` 等）都接受 ID 前缀，最少 4 个字符，可以直接粘贴 `list`/`search` 输出里的短 ID。完整 ID 精确匹配优先于前缀匹配，因此一个恰好是另一个 ID 前缀的完整 ID 不会被判为歧义；前缀命中多个会话时会列出候选要求收窄。

**搜索关键词拆分。** `search` 把所有参数按空白拆分成多个关键词做 AND 匹配，因此 `search A B` 与 `search "A B"` 等价——引号不会让整串变成一个关键词。确实要匹配含空格的整串时加 `--phrase`。

**邻近度排序。** 多关键词命中时，同一条消息内关键词挨得越近，排序越靠前：搜「A B」而原文写作「AB」或「A和B」的会话会显著上浮。这是加权而非硬性置顶，时间新旧与命中次数仍然参与打分。文本输出用 ⚡ 标记完全相邻的命中，JSON 输出对应 `proximity`（0–1，越大越近）与 `keyword_gap`（关键词之间的字符间隔，`0` 表示相邻，`null` 表示 OR 部分命中）。

### 测试会话维护

Session maintenance 默认关闭。执行 `ccs session maintain --enable` 后，`list`、`projects`、`overview` 和交互界面等路径会惰性推进文件动作，不安装后台进程；`search`/`show` 只读取并展示现有维护状态。默认采用保守分类器，宁可漏判也不根据单一弱信号误伤：

1. 会话最后活动满 **24 小时**后，满足分类阈值才进入 `hidden`；
2. 从首次隐藏起满 **7 天**后移动到本机 recycle store，进入 `recycled`；
3. 从首次隐藏起满 **30 天**后清除本机回收副本，进入 `purged_local`；
4. 每次运行最多执行 **50** 个文件动作，剩余工作留到后续惰性运行。

自定义标题、显式 `keep`、消息较多或持续时间较长的会话会受到硬保护。可用 `explain` 查看 score/reason codes，用 `mark-test`/`unmark-test` 添加或移除显式测试标记；`unkeep` 和 `unmark-test` 不会在同一命令中立即隐藏会话。

可见性规则：

- `list`、`projects`、`overview` 和交互列表默认只显示 active；加 `--include-hidden` 才包含 hidden/recycled；
- `search` 默认包含 active、hidden 和 recycled；加 `--active-only` 排除后两者；
- `show`、`explain` 和 `restore` 使用 `(source, session_id)` 消歧；JSON 的 `visibility` 字段为增量字段，`schema_version` 仍为 `1`。

`purged_local` 只表示本机 maintenance 副本已清除。自动 hide/recycle/purge 不生成 tombstone：Claude 同 fingerprint 的远端副本通过本地 suppression 避免 pull 反复复活，远端内容变化则作为新 revision 恢复。跨设备永久删除仍只能由显式 `ccs session delete` 或手动 `ccs push --prune` 发起；限时 `unlock-delete` 自动路径也不会删除 maintenance-suppressed 会话。

### 扫描诊断与 JSON 输出

正常扫描保持安静。只有扫描发现损坏文件、I/O/权限错误、cache 错误，或 warning 超过保留上限时，才判定为 degraded：文本命令仍尽量输出已有结果，并在 stderr 输出一条聚合 warning；详细的逐条诊断写入文件日志。JSON 命令不输出这条文本 warning，而是把完整的计数和受限诊断放进业务 JSON。Claude、Codex、OMP 的 source root 存在但不是目录时，都会按 best-effort 记录 I/O degraded 并跳过该来源，仍继续保留其他来源结果，不会让整个多来源查询失败。

`session overview --json`、`session search --json` 和 `session show --json` 的顶层字段如下：

- `schema_version`：JSON 输出契约版本，当前为 `1`。
- `diagnostics`：本次扫描的诊断对象；其中也带有 `schema_version: 1`。
- `diagnostics.diagnostic_id`：本次扫描关联 ID，格式为 `I-XXXXXXXX`。它与同一次进程文件日志中的 `invocation=I-XXXXXXXX` 相同，可用这个值关联 JSON 结果与日志 invocation。

带 `--json` 的命令（`explain`、`search`、`show`、`overview`）失败时，错误也以 JSON 形式写到 stdout 并以退出码 1 结束，管道消费方不会拿到空输入：

```json
{"schema_version":1,"error":{"message":"Session not found: 00000000","chain":["Session not found: 00000000"]}}
```

| 字段 | 语义 |
|------|------|
| `files_seen` | 发现的 `.jsonl` 候选文件数；在过滤前计数。 |
| `files_parsed` | parser 成功解析的候选文件数；后续被项目过滤或判定无有效摘要仍算已解析。 |
| `files_skipped` | 因过滤、项目不匹配或摘要无有效消息/标题而跳过的文件数。 |
| `malformed_files` | parser/data 错误的文件数。 |
| `io_errors` | WalkDir、目录项、metadata、mtime 或历史文件读取等 I/O 错误数；单项错误会继续扫描。 |
| `cache_errors` | cache 加载或保存错误数；cache 失败不会丢弃已解析的会话。 |
| `cache_hits` / `cache_misses` | 候选文件命中或未命中 cache 的次数；命中同时要求 size、mtime 和流式 BLAKE3 内容 fingerprint 全部一致。 |
| `bytes_considered` | 成功取得 metadata 的候选文件字节数。 |
| `elapsed_ms` | 扫描、排序及 cache 保存总耗时（毫秒）。 |
| `source_discovery_ms` | 生产环境发现 Claude/Codex/OMP 根目录耗时。 |
| `metadata_ms` | 候选文件 metadata 与 mtime 获取累计耗时。 |
| `fingerprint_ms` | 对候选文件执行内容 fingerprint 的累计耗时；不计入 parser 时间。 |
| `fingerprinted_bytes` | fingerprint 实际读取的字节数；cache hit 也会计入。 |
| `cache_load_ms` / `cache_save_ms` | session index cache 读写耗时。 |
| `parse_ms` | 三类 parser 实际调用累计耗时。 |
| `search_load_ms` | `session search` 的 memory/session full-load 与搜索阶段耗时；非 search 为 0。 |
| `claude_scan_ms` / `codex_scan_ms` / `omp_scan_ms` | 各来源扫描阶段耗时。 |
| `parsed_bytes` | 实际调用 parser 的候选文件字节数；cache hit 不计。 |
| `suppressed_warnings` | 超过 warning 保留上限、只计数而未放入 JSON `warnings` 的条数。 |
| `degraded` | 始终输出的 computed boolean；`malformed_files`、`io_errors`、`cache_errors` 或 `suppressed_warnings` 任一大于 0 时为 `true`，否则为 `false`。 |

部分 malformed JSONL 文件仍会保留有效行对应的 summary，但会记录一次 data/parse warning，并且不会写入 session index cache；如果已有 clean cache entry，也会在 partial 或 parser error 分支中删除，避免后续 warm scan 错误命中。当前 cache version 为 4，旧版本会失效；v4 增加 `has_custom_title` 保护位，确保 warm cache 也不会绕过 maintenance 的自定义标题硬保护；每个候选文件的命中还必须同时满足 size、mtime 和流式 BLAKE3 内容 fingerprint，fingerprint 读取耗时与字节数分别记录在 `fingerprint_ms`、`fingerprinted_bytes`，不混入 parser metrics。fingerprint I/O 失败时只跳过该候选并记录受控 I/O warning。warning 及日志只保留受控类别、操作、稳定 `path_hash` 和安全摘要，不包含完整路径、会话内容、原始错误文本或凭据。cache 的缺失是正常冷启动；不可读、非法 JSON 或版本不匹配只产生 `cache_errors` 与受控的 `cache read failed`、`cache data invalid` 或 `cache version mismatch` 摘要。cache 原始路径和底层错误不会进入 stderr 或文件日志，详细诊断仍只通过安全摘要和 `path_hash` 表达。Claude/Codex/OMP 的 legacy discovery 遇到 WalkDir error 会记录受控 I/O warning 并继续扫描其他 entry，不再静默丢弃错误。DualLogger 的 console/file sink 统一脱敏 home 外 Unix/Windows（含盘符正斜杠、反斜杠和 UNC）绝对路径、任意 host 的 `file://` URI path，以及支持 escaped quote/控制字符/换行的完整引号凭据；文件 logger 初始化失败时 stderr 只输出固定安全 warning，不泄露日志路径或底层错误。轮转在锁内使用 staging/transaction rollback，拒绝 current、lock、generation 的 symlink；sink write/flush/poison 失败只发一次安全 stderr fallback，并在内部保留失败计数。warning detail 达到 `MAX_SCAN_WARNINGS` 后不再逐条写 file log，只写一次固定的 suppressed 记录，但 JSON `suppressed_warnings` 仍按实际条数计数。`session list --json` 与 `session doctor` 尚未提供；当前应使用 overview/search/show 的 JSON `diagnostics` 和日志 invocation 做排查。

#### Cache retention 与并发边界

Session index cache 只是可重建的 advisory 加速层，不是会话历史的权威存储。Retention 按 source 分区：只有本次选择且完整完成扫描的 Claude、Codex 或 OMP 来源才会参与清理，source-filter 未选择的来源不会被 prune。来源根目录缺失、变成 regular file、目录遍历失败、metadata/mtime/fingerprint 无法确认时，扫描标记为 incomplete 并 fail-safe 保留该来源旧 entries。

完整来源的旧 entry 只有在 merge 阶段重新确认文件为 `NotFound` 时才会删除；文件仍存在但内容变化、权限错误、读取失败或 fingerprint 不稳定时一律保留。扫描先产生按来源的 delta，merge 在独立 lock file 的阻塞锁内重新读取 latest cache，再复核文件状态并合并，避免两个进程互相覆盖更新。写入先写同一目录的临时文件、flush/sync 后 atomic replace；Windows 也使用相同的 atomic persist 流程，锁文件与 JSON target 分离，避免替换 target inode 后失去互斥。

如果 cache 读取、解析、版本校验或持久化失败，`ccs` 仍返回已扫描的业务结果，并尽量跳过不安全的 cache 更新；不要把 `session_index.json` 当作备份或同步数据，可靠数据仍以 session JSONL 和同步仓库为准。

> **提示：误删保护与跨设备同步**
> `ccs` 为 Claude Code 会话历史启用了**误删保护 (Deletion Protection)**。当你使用 `ccs session delete` 或在交互式菜单里删除会话时，它会生成一个标准的意图记录（Tombstone），该记录会随着 `push` 同步至远端，从而让其他设备在 `pull` 时也同步删除该会话。
> 如果你没有通过 `ccs` 命令而是意外在本地终端使用了 `rm` 或者清空了目录，下次 `push` 时程序会**拦截**这一操作（以防止远程备份也被误删）。它会保留云端副本，并提示你使用 `ccs session restore` 找回丢失的会话。如果你确实想连带云端一起强制物理销毁，可以通过 `ccs push --prune` 逃生舱绕过保护。

### 会话标题

会话标题取自第一条真实的用户消息。以下内容会被自动过滤：
- IDE 自动发送的 `<ide_opened_file>` 标签
- IDE 自动发送的 `<ide_selection>` 标签
- 系统预热消息 `Warmup`

### 示例输出

```
📂 检测到当前项目: my-project
找到 5 个会话

> 1. 帮我实现用户认证功能...          12条消息  今天
  2. 修复登录页面的样式问题...         8条消息  昨天
  3. 重构数据库连接池...              25条消息  3天前
  ─────────────────────────────────────────────────
  ← 切换到其他项目
  ✕ 退出
```

---

## 临时放行 session 删除

默认情况下，本地缺失的 session 会被 push 保护（不同步删除到云端）。若你用文件管理器、`rm` 或外部服务有意删除了 session，希望删除同步上云：

```bash
ccs unlock-delete                 # 开启放行窗口，默认 15 分钟
ccs unlock-delete --minutes 60    # 自定义时长
ccs unlock-delete --status        # 查看剩余时间
ccs unlock-delete --off           # 提前关闭
```

窗口期内的每次 push（含自动同步）都会把本地已删除的 session 同步删除到云端；到期自动恢复保护，无需手动关闭。

---

## 常用命令示例

### 基本操作

| 命令 | 说明 |
|------|------|
| `ccs` | 进入交互式会话选择（等价于 `ccs session`） |
| `ccs setup` | 交互式配置向导 |
| `ccs sync` | 双向同步 |
| `ccs pull` | 拉取远程更新 |
| `ccs push` | 推送本地更新 |
| `ccs status` | 查看同步状态 |
| `ccs automate` | 配置自动同步 |
| `ccs session` | 交互式会话管理 |
| `ccs session list` | 列出所有会话 |
| `ccs session show <id>` | 查看会话详情 |
| `ccs session rename <id> <title>` | 重命名会话 |
| `ccs session delete <id>` | 删除会话 |
| `ccs session restore <id>` | 恢复本地回收或同步仓库中的会话 |
| `ccs session maintain` | 开启、关闭、查看、预演或执行测试会话维护 |
| `ccs session explain <id>` | 查看 lifecycle、score、reason codes 与下一次转换 |
| `ccs session keep <id>` | 恢复并保护会话，避免自动维护 |
| `ccs config-sync push` | 推送配置到远程 |
| `ccs config-sync list` | 列出远程设备配置 |
| `ccs config-sync apply <device>` | 应用其他设备配置 |
| `ccs config-sync status` | 查看配置同步状态 |
| `ccs hooks show` | 查看 hooks 状态 |
| `ccs wrapper show` | 查看包装脚本状态 |
| `ccs update` | 更新到最新版本 |
| `ccs uninstall` | 卸载并清理所有数据 |

### 配置管理

```bash
# 查看当前配置
ccs config --show

# 只同步最近 30 天的对话
ccs config --exclude-older-than 30

# 排除特定项目
ccs config --exclude-projects "*test*,*temp*"

# 只同步特定项目
ccs config --include-projects "*work*,*important*"
```

### 状态检查

```bash
# 基本状态
ccs status

# 显示文件列表
ccs status --show-files

# 查看冲突
ccs status --show-conflicts
```

### 冲突报告

```bash
# 生成 Markdown 报告
ccs report --format markdown

# 生成 JSON 报告并保存
ccs report --format json --output conflicts.json
```

---

## 高级配置

### Git LFS（大文件）

```bash
# 启用 LFS
ccs config --enable-lfs true

# 自定义 LFS 模式
ccs config --enable-lfs true --lfs-patterns "*.jsonl,*.png"
```

### 自定义同步目录

```bash
# 更改存储子目录（默认 "projects"）
ccs config --sync-subdirectory "claude-conversations"
```

### 自动化备份

**macOS/Linux crontab：**
```bash
# 每天晚上 11 点同步
0 23 * * * ~/.local/bin/ccs sync
```

**非交互式初始化：**

创建 `~/.claude-code-sync-init.toml`：
```toml
repo_path = "~/claude-history-sync"
remote_url = "git@github.com:user/claude-history.git"
clone = true
use_project_name_only = true
```

运行：
```bash
ccs init --config ~/.claude-code-sync-init.toml
```

### 命令别名

**Bash/Zsh：**
```bash
alias ccs='ccs'
alias ccs-sync='ccs sync'
```

**PowerShell：**
```powershell
Set-Alias ccs ccs
```

---

## 日志排查

### 先看哪里

正常在终端运行 `ccs` 时，先查看 stderr；它会显示当前 console 日志级别下的记录。后台任务、自动同步或难以复现的问题，再查看平台日志文件：

- macOS：`~/Library/Application Support/claude-code-sync/claude-code-sync.log`
- Linux：`~/.config/claude-code-sync/claude-code-sync.log`（设置 `XDG_CONFIG_HOME` 时位于 `$XDG_CONFIG_HOME/claude-code-sync/claude-code-sync.log`）
- Windows：`%APPDATA%\\claude-code-sync\\claude-code-sync.log`

每次 ccs 初始化文件 logger 时都会检查日志大小；超过 10 MiB 会在下一次 ccs 调用打开日志时轮转，最多保留 3 代备份。也可以用 `--log-file` 把一次运行隔离到指定路径，避免和其他运行混在一起。

### 临时提高日志级别

```bash
# 临时开启 DEBUG，适合定位一次运行
ccs --debug session list

# 收集一次隔离日志
ccs --log-file ./ccs-debug.log session search "keyword"

# 只显示错误；文件日志仍继续写入
RUST_LOG=error ccs session list

# RUST_LOG=off 只关闭 console，不关闭 file sink
RUST_LOG=off ccs session list
```

`--debug` 适合短时间排查，不建议长期运行。每次进程会生成一个 `invocation=I-...`，在 stderr 和文件中用它关联同一次运行的记录。日志只用于运行诊断：系统会自动脱敏常见 token/password/secret/API key、Authorization、URL userinfo，以及完整的 `file://` URI path（包括 hosted path 中合法的逗号、引号、`]`、`)` 等字符），不主动记录会话正文；建议不要把其他未识别的敏感内容放进 CLI 参数或日志消息。每次 file write/flush 都重新获取同一 per-log lock；rotation staging 从已打开的 no-follow handle 复制，live rename 前还会复核 inode/metadata，且 current 使用 no-follow fresh open。长生命周期进程不会继续写入已轮转的旧 inode；轮转失败会回滚，rollback 自身失败时会保留 transaction 目录并返回可见的安全错误。

> **外部 rotator 边界**：`.lock` 是 advisory lock，只约束同样取得该锁的 `ccs` 进程或协作工具。未取得锁的系统 `logrotate`、用户脚本或其他程序直接 rename/copy/append 日志时，ccs 无法对其与 staging/rename 的并发提供原子协调保证；文档不把跨进程 ccs 测试泛化为所有外部 rotator 均受保护。

> **Hook debug 的范围**：Hook debug 尚未统一到 ccs 的 DualLogger，仍是后续工作。自动同步 Hook 可能另外写入 `hook-debug.log`；不要把它当成上述平台日志，也不要据此宣称 Hook logging 已完成。

## 故障排查

### 问题 1：No matching local project found

**原因：** 本地没有该项目或路径解析失败

**解决：**
1. 在本地用 Claude Code 打开该项目
2. 确保 `use_project_name_only = true` 已配置
3. 重新执行 `ccs pull`

### 问题 2：Authentication failed

**解决：**
```bash
# 使用 GitHub CLI 认证
gh auth login

# 或配置 SSH key
ssh-keygen -t ed25519
cat ~/.ssh/id_ed25519.pub  # 添加到 GitHub
```

### 问题 3：误删除与找回

**解决：**
如果你在文件夹中使用了 `rm` 等操作不小心删除了会话文件，下一次 `ccs push` 会被拦截并提示存在丢失的会话。
此时你可以使用以下命令进行找回：
```bash
# 交互式查看并恢复缺失的会话
ccs session restore
```
如果你确实想连带云端同步库中的该会话记录一起删除（即不需要恢复）：
```bash
# 强制修剪云端对应记录
ccs push --prune
```

### 问题 4：冲突处理

**自动处理：**
- 冲突文件会保留两个版本
- 远程版本：`session.jsonl`
- 本地版本：`session-conflict-<timestamp>.jsonl`

**手动解决：**
1. 查看冲突报告：`ccs report`
2. 选择需要保留的版本
3. 删除不需要的文件
4. 推送：`ccs push`

### 问题 4：更新失败

```bash
# 检查更新
ccs update --check-only

# 自动更新
ccs update

# 如果 update 命令不可用（旧版本），直接下载替换：
curl -fsSL https://github.com/osen77/cc-session/releases/latest/download/ccs-macos-aarch64.tar.gz | tar xz && sudo mv ccs $(which ccs)
```

---

## 卸载

```bash
# 交互式卸载（逐步确认清理范围）
ccs uninstall

# 强制卸载（跳过确认）
ccs uninstall --force
```

卸载会清理：
1. Claude Code hooks（从 `~/.claude/settings.json` 移除）
2. 启动包装脚本（`claude-sync`）
3. 配置目录（state.json、config.toml、日志等）
4. 同步仓库（需单独确认，可能包含未推送的对话历史）
5. ccs 二进制本身（需单独确认）

---

## 配置文件位置

| 平台 | 配置文件 |
|------|---------|
| Windows | `%APPDATA%\claude-code-sync\config.toml` |
| macOS | `~/Library/Application Support/claude-code-sync/config.toml` |
| Linux | `~/.config/claude-code-sync/config.toml` |

---

## 重要注意事项

### 项目名称一致性

确保不同设备上的项目文件夹名称相同：
- ✅ Windows `C:\Projects\my-app`，Mac `/Users/example/Projects/my-app`
- ❌ Windows `C:\work\app1`，Mac `/Users/example/code/myapp`

### 同步时机

- **开始工作前**：`pull` 或 `sync`
- **结束工作后**：`push` 或 `sync`
- **切换设备时**：先 push，再到新设备 pull

---

## 相关资源

- **仓库**: https://github.com/osen77/cc-session
- **问题追踪**: https://github.com/osen77/cc-session/issues
- **上游项目**: https://github.com/perfectra1n/claude-code-sync

---

*最后更新: 2026-03-26*
