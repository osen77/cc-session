# 裸 `ccs` 默认进入会话选择设计

## 目标

将不带子命令的 `ccs` 改为等价于 `ccs session`，直接进入交互式会话选择。所有显式子命令保持不变，双向同步继续通过 `ccs sync` 执行。

## 行为定义

- `ccs`：进入 session 交互模式。
- `ccs session`：行为不变。
- `ccs sync`：继续执行双向同步。
- 未初始化同步仓库时，`ccs` 仍进入 session，不触发 onboarding。
- `ccs --help`、`ccs --version`、全局日志参数和所有显式子命令保持不变。
- 裸 `ccs` 与 `ccs session` 一样被视为本地命令，不启动网络更新检查。

## 实现设计

在 CLI 参数解析和 logger 初始化后，尽早将缺失的子命令解析为默认的 `Commands::Session`：

```text
Commands::Session {
    action: None,
    project: None,
    source: All,
    include_hidden: false,
}
```

后续的本地命令判断、更新检查、onboarding 判断和主分派都基于这个已经确定的命令，避免先将空命令误判为需要网络更新检查。

不采用修改进程参数或隐式插入 `session` 字符串的方案，以免影响 Clap 的全局参数、帮助和错误提示。

## Hooks 与自动同步

现有自动化路径均使用显式子命令，因此不受默认行为变化影响：

- 安装的 hooks 明确执行 `hook-session-start`、`hook-stop` 和 `hook-new-project-check`。
- hook 内部明确执行 `pull --quiet` 或 `push --quiet`。
- Unix、Windows Batch 和 PowerShell wrapper 都明确执行 `pull --quiet`。

不修改 hooks、wrapper 或 automate 的运行逻辑。

## 测试

- 增加默认命令解析测试，断言缺失子命令会得到交互式 `Commands::Session` 默认值。
- 保留并运行现有 session CLI 解析测试。
- 运行 hooks 与 wrapper 相关测试，确认生成命令仍包含显式子命令。
- 运行格式化、Clippy 和相关测试。

## 文档与变更记录

- README 的常用命令中明确说明裸 `ccs` 进入交互式会话管理。
- 用户指南说明默认行为已从隐式 sync 改为 session；同步需显式运行 `ccs sync`。
- 按项目规范在 `local/notes.md` 记录行为变更、影响范围与验证结果。
