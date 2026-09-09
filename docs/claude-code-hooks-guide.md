# Claude Code Hooks 避坑指南与经验总结

> 基于 claude-code-sync 项目开发过程中的实践经验整理

## 1. SessionStart Hook 的真实行为

### 误解：SessionStart 只在启动时触发

**错误认知**：`SessionStart` 只会在 Claude Code 启动时触发一次。

**实际行为**：`SessionStart` 在以下场景都会触发：

| 场景 | 是否触发 | source 字段值 |
|------|---------|--------------|
| 启动 Claude Code CLI | ✅ | `startup` |
| IDE 打开 Claude 面板 | ✅ | `startup` |
| 执行 `/new` 创建新会话 | ✅ | `startup` |
| 执行 `/clear` 清除对话 | ✅ | `startup` |
| 对话压缩（长对话自动触发）| ✅ | `resume` 或 `compact` |

### 关键发现

1. **`/new` 会创建新进程**：在 IDE 扩展中，`/new` 会新开一个标签页，创建新的 Claude 进程，原进程仍然存在。

2. **`source` 字段无法区分 `/new`**：`/new` 触发时 `source` 值是 `startup`，与真正的首次启动相同。

3. **多窗口有独立 PID**：每个 Claude Code 窗口都是独立进程，有不同的 PID。

## 2. Hook 输入字段

SessionStart hook 接收的 JSON 输入包含：

```json
{
  "source": "startup" | "resume" | "clear" | "compact",
  "model": "...",
  "agent_type": "..."  // 可选
}
```

### source 字段说明

| 值 | 含义 |
|---|------|
| `startup` | 新会话开始（包括首次启动和 `/new`） |
| `resume` | 恢复已有会话 |
| `clear` | `/clear` 命令后 |
| `compact` | 对话压缩后 |

**注意**：文档中说 `/new` 会触发 `clear`，但实测是 `startup`。

## 3. 进程检测方案

### 检测 Claude Code 进程

```bash
# 精确匹配 Claude Code native-binary
ps aux | grep 'native-binary/claude' | grep -v grep | wc -l
```

**注意事项**：
- `pgrep -f` 在 macOS 上有命令行长度限制，可能漏匹配
- `ps aux | grep` 更可靠但需要排除 grep 自身
- 使用 `native-binary/claude` 精确匹配，避免匹配到其他包含 "claude" 的命令

### 在 Rust 中实现

```rust
fn count_claude_processes() -> usize {
    let output = std::process::Command::new("sh")
        .args(["-c", "ps aux | grep 'native-binary/claude' | grep -v grep | wc -l"])
        .output();

    match output {
        Ok(out) => {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or(0)
        }
        Err(_) => 0
    }
}
```

## 4. 首次启动检测的最终方案

### 三重条件检测

```rust
let should_pull = (process_count == 1)      // 没有其他 Claude 实例
    && (source == "startup")                 // 真正的启动，不是 resume/compact
    && !debounce_active;                     // 5分钟防抖未触发
```

### 各条件作用

| 条件 | 排除的场景 |
|------|-----------|
| 进程数 = 1 | `/new`、新窗口 |
| source = "startup" | resume、compact |
| 5分钟防抖 | 快速重启的极端情况 |

## 5. 日志调试技巧

### 日志位置

```bash
# macOS
~/Library/Application Support/claude-code-sync/hook-debug.log
```

### 实时查看日志

```bash
tail -f ~/Library/Application\ Support/claude-code-sync/hook-debug.log
```

### 推荐的日志格式

```
[2026-02-04 21:30:00] SessionStart (source: startup, processes: 1, debounce: false)
[2026-02-04 21:30:00] SessionStart pull completed: exit code 0
[2026-02-04 21:32:00] SessionStart (source: startup, processes: 2, debounce: false)
[2026-02-04 21:32:00] pull skipped (other instances: 2)
```

记录关键判断条件，便于排查问题。

## 6. 常见陷阱

### 陷阱 1：假设 source 能区分所有场景

`source` 字段只能区分 `startup` vs `resume/compact`，无法区分"首次启动"和"`/new`"。

### 陷阱 2：PPID 检测不可靠

最初考虑用 PPID（父进程 ID）检测，但发现：
- `/new` 创建的是新进程，PPID 是 IDE 进程，不是原 Claude 进程
- 多个窗口的 PPID 可能相同（都是 IDE 的子进程）

### 陷阱 3：pgrep 的长度限制

macOS 上 `pgrep -f` 匹配命令行时有长度限制，可能导致漏匹配。用 `ps aux | grep` 更可靠。

### 陷阱 4：Hook 触发两次

某些情况下 SessionStart 会触发两次（如 IDE 的多次初始化）。需要用防抖机制处理。

### 陷阱 5：Stop hook 同步执行会阻塞每一轮对话

`hook-stop` 要 git push，一次约 20 秒。Stop 事件在**每轮对话结束时**同步等待，所以这 20 秒直接加在用户面前。

只加节流不够。实测 556 次运行：中位 **14.0 秒**、p90 19.2 秒、最差 60 秒并撞了 2 次超时——因为对话轮次的实际间隔通常比 `THROTTLE=300` 还长，多数 Stop 事件都能通过节流检查，于是每轮都在付全额 push。节流只在密集对话时有用，而密集对话恰恰不是常态。

正确做法是在 `ccs hook-stop` 内把 push 分离到后台 detached worker，前台仅做节流与告警检查并立即返回（hook timeout 缩减到 10 秒）。几个关键的底层处理细节：

- **前台不持锁，由 worker 自行取锁**：标准库打开的 fd 均带有 `O_CLOEXEC`，文件锁（flock）无法传递给子进程。因此由后台 worker 启动后通过 `FileLock::try_acquire` 竞争 `push-hook.lock`，竞争失败说明已有 worker 在运行，直接退出 0 跳过；锁随进程消亡由操作系统自动释放，省去维护锁龄的额外逻辑。
- **三路标准流全重定向到 null**（stdin/stdout/stderr 置空）：如果只后台化子进程而不重定向 stdio，hook runner 仍会阻塞等待管道关闭，达不到异步返回的效果。
- **脱离会话与孤儿进程保活**：Unix 下在 `pre_exec` 中调用 `libc::setsid()` 建立新 session 脱离控制终端；Windows 下配置 `creation_flags` 为 `DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`（并尝试 `CREATE_BREAKAWAY_FROM_JOB` 脱离作业对象），确保父进程退出后 worker 进程不被关联终止。

后台化后，worker 的退出码无法直接传递给 Claude Code。因此改用持久化状态文件（`push-hook-state.json`）记录失败情况：
- **连续失败告警**：worker 成功时重置计数并更新 `push-hook.stamp`（300 秒节流时间戳）；连续失败达到阈值 3 次时，由**下一次前台运行**输出告警。
- **退出码选择 1 而非 2**：Stop 钩子的 `exit 2` 是 blocking error，会把 stderr 注入上下文并阻止本轮结束；退出码 1 是 non-blocking error，只把 stderr（包含查看 `hook-debug.log` 的提示）显示给用户，不打断交互。
- **告警去重**：通过 `alerted_failures` 记录已告警次数，仅在失败次数增加时再次提醒，避免每轮刷屏。告警滞后一轮（前台读到的是上一轮 worker 的结果），数字是下界。

## 7. 最佳实践

1. **多条件组合**：单一条件往往不够，需要组合多个条件
2. **详细日志**：记录所有判断条件的值，便于调试
3. **防抖兜底**：时间戳防抖作为最后一道防线
4. **进程检测**：检测同类进程数量是判断"首次"的可靠方法
5. **实际测试**：在真实环境中测试所有场景，不要依赖文档

## 8. 相关资源

- [Claude Code Hooks 官方文档](https://code.claude.com/docs/en/hooks)
- [Hooks 配置教程](https://claude.com/blog/how-to-configure-hooks)

---

*最后更新: 2026-09-09*
