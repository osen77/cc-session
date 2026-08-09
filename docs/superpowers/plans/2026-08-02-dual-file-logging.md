# Dual File Logging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让所有 `log::Record` 按级别同时写入 stderr 和平台日志文件，并提供可控级别、自定义日志路径、隐私清理、调用 ID 和 10 MiB × 3 代轮转。

**Architecture:** 保留现有 `log` facade，使用项目内 `DualLogger` 实现 `log::Log`，避免迁移全仓到 `tracing` 或新增日志框架。纯函数负责级别决策、消息脱敏、格式化和轮转；`init_logger_with_options` 负责打开 sink、注册全局 logger 和返回可见的降级状态。旧 `init_logger`、`log_to_file`、`rotate_log_if_needed` 作为兼容 wrapper 保留。

**Tech Stack:** Rust 2021、`log`、`chrono`、`uuid`、`regex`、clap derive、`tempfile`、`serial_test`；移除不再使用的 `env_logger`。

## Global Constraints

- 不 commit、不 push。
- 不实现 `ScanDiagnostics`、`session doctor`、Hook 日志统一、远程遥测或搜索性能指标。
- 所有日志只保存在本机，不上传。
- 默认文件日志级别为 `Info`；`--debug` 将文件日志提升到 `Debug`。
- `RUST_LOG` 优先控制 console；即使 `RUST_LOG=off`，文件日志仍按 `Info` 或 `--debug` 继续记录。
- 默认路径继续使用 `ConfigManager::log_file_path()`；`--log-file <PATH>` 可覆盖。
- 轮转阈值为 10 MiB，保留 `.1`、`.2`、`.3` 三代。
- 日志格式必须包含 RFC3339 timestamp、level、invocation ID、target 和清理后的 message。
- 不记录原始 hook stdin、会话正文、session title、tool input/output、token、密码或带认证信息 URL。
- 完整 home 路径替换为 `~`；常见 token/password/secret/api_key 值替换为 `<redacted>`；URL userinfo 替换为 `***@`。
- Unix 下日志文件权限为 `0600`；Windows 使用平台默认 ACL。
- 文件 sink 打不开时，console logger 仍可启用，并向调用方返回一次 warning；全局 logger 注册失败必须返回错误。
- 测试只使用 tempfile 与隔离环境变量，不写真实配置目录。

---

## File Structure

- `src/logger.rs`
  - 重写为双 sink logger。
  - 定义 `LoggerOptions`、`LoggerInitStatus`、`DualLogger`。
  - 实现级别解析、格式化、隐私清理、文件打开和多代轮转。
  - 保留旧 public wrapper。
- `src/main.rs`
  - `Cli` 增加 global `--debug`、`--log-file`。
  - 先 parse CLI，再初始化 logger，再启动后台 update check。
  - logger 文件降级警告输出一次 stderr。
- `Cargo.toml`
  - 移除不再使用的 `env_logger`。
- `tests/logger_cli_tests.rs`
  - 子进程验证真实 CLI 能把 `log::debug!("ccs started")` 写到显式日志路径。
- `README.md`
  - 记录 `--debug`、`--log-file`、默认路径和隐私规则。
- `docs/user-guide.md`
  - 增加日志排查章节。
- `local/notes.md`
  - 记录原 logger 只写 stderr、文件日志失真的根因和修复。

---

### Task 1: 纯日志策略、脱敏和三代轮转

**Files:**
- Modify: `src/logger.rs:1-190`
- Test: `src/logger.rs` tests

**Interfaces:**
- Produces:
  - `const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024`
  - `const LOG_BACKUP_COUNT: usize = 3`
  - `fn resolve_console_level(debug: bool, rust_log: Option<&str>) -> LevelFilter`
  - `fn resolve_file_level(debug: bool) -> LevelFilter`
  - `fn sanitize_log_message(message: &str, home: Option<&Path>) -> String`
  - `fn format_log_line(...) -> String`
  - `fn rotate_log_at(path: &Path, max_size: u64, backups: usize) -> Result<bool>`

- [ ] **Step 1: 写级别决策失败测试**

```rust
#[test]
fn test_resolve_log_levels() {
    assert_eq!(resolve_console_level(false, None), LevelFilter::Info);
    assert_eq!(resolve_console_level(true, None), LevelFilter::Debug);
    assert_eq!(resolve_console_level(true, Some("error")), LevelFilter::Error);
    assert_eq!(resolve_console_level(false, Some("off")), LevelFilter::Off);
    assert_eq!(resolve_console_level(false, Some("invalid")), LevelFilter::Info);
    assert_eq!(resolve_file_level(false), LevelFilter::Info);
    assert_eq!(resolve_file_level(true), LevelFilter::Debug);
}
```

- [ ] **Step 2: 写消息脱敏失败测试**

```rust
#[test]
fn test_sanitize_log_message_redacts_private_values() {
    let home = Path::new("/Users/example");
    let input = "path=/Users/example/.claude token=abc password=hunter2 api_key=key123 https://user:pass@example.com/repo";
    let sanitized = sanitize_log_message(input, Some(home));

    assert!(sanitized.contains("path=~/.claude"));
    assert!(sanitized.contains("token=<redacted>"));
    assert!(sanitized.contains("password=<redacted>"));
    assert!(sanitized.contains("api_key=<redacted>"));
    assert!(sanitized.contains("https://***@example.com/repo"));
    assert!(!sanitized.contains("hunter2"));
    assert!(!sanitized.contains("key123"));
    assert!(!sanitized.contains("user:pass"));
}
```

再增加保留普通文本和不同 key 大小写：

```rust
#[test]
fn test_sanitize_log_message_preserves_safe_text() {
    assert_eq!(
        sanitize_log_message("session scan completed: 42 files", None),
        "session scan completed: 42 files"
    );
    assert_eq!(
        sanitize_log_message("API-KEY=secret-value", None),
        "API-KEY=<redacted>"
    );
}
```

- [ ] **Step 3: 写固定格式失败测试**

```rust
#[test]
fn test_format_log_line_includes_required_fields() {
    let line = format_log_line(
        "2026-08-02T10:20:30Z",
        log::Level::Warn,
        "I-ABC123",
        "ccs::session",
        "path=/Users/example/project token=abc",
        Some(Path::new("/Users/example")),
    );

    assert_eq!(
        line,
        "2026-08-02T10:20:30Z WARN invocation=I-ABC123 target=ccs::session path=~/project token=<redacted>\n"
    );
}
```

- [ ] **Step 4: 写三代轮转失败测试**

使用小阈值避免生成 11 MiB fixture：

```rust
#[test]
fn test_rotate_log_keeps_three_generations() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("ccs.log");

    std::fs::write(&path, b"current-over-limit")?;
    std::fs::write(path.with_extension("log.1"), b"one")?;
    std::fs::write(path.with_extension("log.2"), b"two")?;
    std::fs::write(path.with_extension("log.3"), b"three")?;

    assert!(rotate_log_at(&path, 4, 3)?);
    assert!(!path.exists());
    assert_eq!(std::fs::read(path.with_extension("log.1"))?, b"current-over-limit");
    assert_eq!(std::fs::read(path.with_extension("log.2"))?, b"one");
    assert_eq!(std::fs::read(path.with_extension("log.3"))?, b"two");
    Ok(())
}
```

增加未超阈值不轮转：

```rust
#[test]
fn test_rotate_log_skips_small_file() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("ccs.log");
    std::fs::write(&path, b"ok")?;
    assert!(!rotate_log_at(&path, 10, 3)?);
    assert_eq!(std::fs::read(&path)?, b"ok");
    Ok(())
}
```

- [ ] **Step 5: 运行 tests 确认 RED**

Run:

```bash
cargo test logger::tests -- --nocapture
```

Expected: FAIL，缺少新的 pure helpers 或旧轮转只生成 `.old`。

- [ ] **Step 6: 实现级别与格式 helper**

```rust
const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024;
const LOG_BACKUP_COUNT: usize = 3;

fn resolve_console_level(debug: bool, rust_log: Option<&str>) -> LevelFilter {
    rust_log
        .and_then(|value| value.parse::<LevelFilter>().ok())
        .unwrap_or(if debug {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        })
}

fn resolve_file_level(debug: bool) -> LevelFilter {
    if debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    }
}
```

`sanitize_log_message` 使用 `std::sync::OnceLock<regex::Regex>` 缓存两个 regex：

```rust
static SECRET_RE: OnceLock<Regex> = OnceLock::new();
static URL_USERINFO_RE: OnceLock<Regex> = OnceLock::new();
```

secret pattern 必须覆盖并保留 key：

```text
(?i)\b(token|password|secret|api[_-]?key)=([^\s]+)
```

替换为 `$1=<redacted>`。URL userinfo pattern：

```text
([a-zA-Z][a-zA-Z0-9+.-]*://)[^/@\s]+@
```

替换为 `$1***@`。最后若存在 home path，将其字符串替换为 `~`。

`format_log_line` 固定格式：

```rust
format!(
    "{timestamp} {:<5} invocation={invocation_id} target={target} {}\n",
    level,
    sanitize_log_message(message, home)
)
```

- [ ] **Step 7: 实现三代轮转**

```rust
fn rotated_path(path: &Path, generation: usize) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!("{ext}.{generation}"))
        .unwrap_or_else(|| generation.to_string());
    path.with_extension(extension)
}
```

`rotate_log_at`：

1. path 不存在返回 `Ok(false)`；
2. `metadata.len() <= max_size` 返回 `Ok(false)`；
3. 删除最老 `.3`；
4. 从 generation 2 倒序 rename 到 3，1 到 2；
5. base rename 到 1；
6. 返回 `Ok(true)`。

每个 filesystem error 使用 `with_context` 带路径和操作。

- [ ] **Step 8: 运行纯策略 tests**

Run:

```bash
cargo test logger::tests -- --nocapture
```

Expected: 新增级别、脱敏、格式与轮转 tests PASS。

---

### Task 2: DualLogger、文件权限和兼容初始化 API

**Files:**
- Modify: `src/logger.rs`
- Modify: `Cargo.toml:26-28`
- Test: `src/logger.rs` tests

**Interfaces:**
- Consumes: Task 1 pure helpers。
- Produces:
  - `pub struct LoggerOptions`
  - `LoggerOptions::new(debug: bool, log_path: Option<PathBuf>, rust_log: Option<&str>) -> Result<Self>`
  - `pub struct LoggerInitStatus`
  - `struct DualLogger`
  - `pub fn init_logger_with_options(options: LoggerOptions) -> Result<LoggerInitStatus>`
  - 兼容 `pub fn init_logger() -> Result<()>`
  - 兼容 `pub fn log_to_file(message: &str) -> Result<()>`
  - 兼容 `pub fn rotate_log_if_needed() -> Result<()>`

- [ ] **Step 1: 写 options 和 invocation ID 失败测试**

```rust
#[test]
fn test_logger_options_resolve_levels_and_path() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("custom.log");
    let options = LoggerOptions::new(true, Some(path.clone()), Some("error"))?;

    assert_eq!(options.console_level, LevelFilter::Error);
    assert_eq!(options.file_level, LevelFilter::Debug);
    assert_eq!(options.log_path, path);
    assert!(options.invocation_id.starts_with("I-"));
    assert_eq!(options.invocation_id.len(), 10);
    Ok(())
}
```

`LoggerOptions::new` 的第三参数专供调用方传已读取的 `RUST_LOG`；main 传 `std::env::var("RUST_LOG").ok().as_deref()`。

- [ ] **Step 2: 写双 sink 实际 Record 失败测试**

在 tests 定义共享 writer：

```rust
#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
```

测试：

```rust
#[test]
fn test_dual_logger_writes_record_to_console_and_file() {
    let console = SharedBuffer::default();
    let file = SharedBuffer::default();
    let logger = DualLogger::with_writers(
        LevelFilter::Debug,
        LevelFilter::Info,
        "I-TEST123".to_string(),
        Some(PathBuf::from("/Users/example")),
        Box::new(console.clone()),
        Some(Box::new(file.clone())),
    );

    let record = log::Record::builder()
        .level(log::Level::Warn)
        .target("ccs::session")
        .args(format_args!("path=/Users/example token=abc"))
        .build();
    logger.log(&record);

    let console_text = String::from_utf8(console.0.lock().unwrap().clone()).unwrap();
    let file_text = String::from_utf8(file.0.lock().unwrap().clone()).unwrap();
    assert!(console_text.contains("invocation=I-TEST123"));
    assert!(file_text.contains("invocation=I-TEST123"));
    assert!(!console_text.contains("token=abc"));
    assert!(!file_text.contains("token=abc"));
}
```

再测试 Debug 只进入 console、file=Info 时不进入 file。

- [ ] **Step 3: 写文件权限和降级失败测试**

Unix 权限测试使用 `#[cfg(unix)]`：

```rust
#[test]
#[cfg(unix)]
fn test_open_log_file_sets_0600() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("secure.log");
    let _file = open_log_file(&path)?;
    assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
    Ok(())
}
```

降级测试通过把 log path 指向目录内的目录项，使 `open_log_file` 失败；测试 `build_logger` 返回 file=None 和 warning，但 console writer 存在。

- [ ] **Step 4: 运行 tests 确认 RED**

Run:

```bash
cargo test logger::tests -- --nocapture
```

Expected: FAIL，缺少 `LoggerOptions`、`DualLogger` 和 open/build API。

- [ ] **Step 5: 实现 DualLogger**

内部结构：

```rust
type BoxedWriter = Box<dyn Write + Send>;

struct DualLogger {
    console_level: LevelFilter,
    file_level: LevelFilter,
    invocation_id: String,
    home_dir: Option<PathBuf>,
    console: Mutex<BoxedWriter>,
    file: Option<Mutex<BoxedWriter>>,
}
```

实现 `log::Log`：

- `enabled`：record level 不高于 console 或 file 任一 level；
- `log`：使用 UTC RFC3339 秒精度 timestamp；格式化一次；按各 sink level 写入；单个 sink 写失败不得 panic；
- `flush`：分别 flush，错误不 panic。

生产 console writer 使用 `Box::new(std::io::stderr())`。

- [ ] **Step 6: 实现安全文件打开**

`open_log_file(path)`：

1. 创建 parent directory；
2. Task 1 `rotate_log_at(path, MAX_LOG_SIZE, LOG_BACKUP_COUNT)`；
3. create+append 打开；
4. Unix create mode 和最终 permissions 均设 `0600`；
5. 返回 File。

- [ ] **Step 7: 实现 options/status 和全局注册**

```rust
pub struct LoggerOptions {
    pub console_level: LevelFilter,
    pub file_level: LevelFilter,
    pub log_path: PathBuf,
    pub invocation_id: String,
}

pub struct LoggerInitStatus {
    pub invocation_id: String,
    pub log_path: PathBuf,
    pub file_logging_enabled: bool,
    pub warning: Option<String>,
}
```

`LoggerOptions::new(debug, log_path, rust_log)`：

- log path override 或 `ConfigManager::log_file_path()`；
- invocation ID：`I-` + UUID simple uppercase 前 8 字符；
- console/file level 使用 Task 1 helper。

若默认 config path 获取失败，返回 Result；显式 `--log-file` 不依赖 config dir。

`init_logger_with_options`：

- 尝试 `open_log_file`；失败时 file=None、保存 warning；
- 构造 DualLogger；
- `log::set_boxed_logger` 失败返回上下文错误，不吞掉；
- `log::set_max_level(max(console_level, file_level))`；
- 返回 status；
- 注册成功后写一条 `log::info!`，内容只包含 level 和日志启用状态，不包含敏感参数。

- [ ] **Step 8: 保留兼容 API**

```rust
pub fn init_logger() -> Result<()> {
    let rust_log = std::env::var("RUST_LOG").ok();
    init_logger_with_options(LoggerOptions::new(false, None, rust_log.as_deref())?)?;
    Ok(())
}
```

`rotate_log_if_needed()` 调 default path + `rotate_log_at`。

`log_to_file(message)` 保留 direct append 行为，但必须复用 `open_log_file`、`sanitize_log_message` 和固定格式，invocation 使用 `I-LEGACY`；添加 deprecated note 推荐 `log` macros。

- [ ] **Step 9: 移除 env_logger 依赖**

从 `Cargo.toml` 删除：

```toml
env_logger = "0.11.8"
```

Run:

```bash
cargo check
```

Expected: PASS，无 env_logger 引用。

- [ ] **Step 10: 运行 logger tests**

Run:

```bash
cargo test logger::tests -- --nocapture
```

Expected: 双 sink、级别、权限、轮转、兼容 helper tests PASS。

---

### Task 3: CLI flags 和初始化顺序

**Files:**
- Modify: `src/main.rs:33-40`
- Modify: `src/main.rs:630-645`
- Test: `src/main.rs` tests
- Create: `tests/logger_cli_tests.rs`

**Interfaces:**
- Consumes: `LoggerOptions::new`、`init_logger_with_options`、`LoggerInitStatus`。
- Produces: global `--debug`、`--log-file <PATH>` 和 parse-before-logger main flow。

- [ ] **Step 1: 写 CLI parser 失败测试**

在 `main.rs` tests 增加：

```rust
#[test]
fn test_global_logger_flags_parse_before_and_after_subcommand() {
    let before = Cli::try_parse_from([
        "ccs",
        "--debug",
        "--log-file",
        "/tmp/ccs.log",
        "session",
        "projects",
    ])
    .unwrap();
    assert!(before.debug);
    assert_eq!(before.log_file, Some(PathBuf::from("/tmp/ccs.log")));

    let after = Cli::try_parse_from([
        "ccs",
        "session",
        "projects",
        "--debug",
        "--log-file",
        "/tmp/ccs.log",
    ])
    .unwrap();
    assert!(after.debug);
    assert_eq!(after.log_file, Some(PathBuf::from("/tmp/ccs.log")));
}
```

- [ ] **Step 2: 写子进程真实日志失败测试**

`tests/logger_cli_tests.rs`：

```rust
use std::process::Command;

#[test]
fn test_cli_debug_log_reaches_explicit_file() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let config = temp.path().join("config");
    let log_path = temp.path().join("ccs.log");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_ccs"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CLAUDE_CODE_SYNC_CONFIG_DIR", &config)
        .args([
            "--debug",
            "--log-file",
            log_path.to_str().unwrap(),
            "session",
            "projects",
            "--source",
            "codex",
        ])
        .status()
        .unwrap();

    assert!(status.success());
    let contents = std::fs::read_to_string(log_path).unwrap();
    assert!(contents.contains("DEBUG"));
    assert!(contents.contains("ccs started"));
    assert!(contents.contains("invocation=I-"));
}
```

- [ ] **Step 3: 运行 tests 确认 RED**

Run:

```bash
cargo test --bin ccs test_global_logger_flags -- --nocapture
cargo test --test logger_cli_tests -- --nocapture
```

Expected: parser 字段不存在，CLI 不接受 flags 或文件无 debug 记录。

- [ ] **Step 4: 给 Cli 增加 global flags**

```rust
struct Cli {
    /// Enable debug logging for console and file output
    #[arg(long, global = true)]
    debug: bool,

    /// Override the platform log file path
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}
```

所有现有 test 中直接构造 `Cli` 的 literal 必须补字段；优先通过 parser 构造避免重复。

- [ ] **Step 5: 调整 main 初始化顺序**

```rust
fn main() -> Result<()> {
    let cli = Cli::parse();
    let rust_log = std::env::var("RUST_LOG").ok();
    let logger_status = logger::init_logger_with_options(logger::LoggerOptions::new(
        cli.debug,
        cli.log_file.clone(),
        rust_log.as_deref(),
    )?)?;

    if let Some(warning) = &logger_status.warning {
        eprintln!(
            "WARNING: file logging unavailable ({}). Continuing with stderr logs. Log path: {}",
            warning,
            logger_status.log_path.display()
        );
    }

    log::debug!("ccs started");

    // 后续才启动 update check，并继续使用 cli.command
```

删除 main 中独立 `rotate_log_if_needed().ok()` 和 `init_logger().ok()`。轮转由 file open 负责；任何全局注册失败通过 `?` 返回。

- [ ] **Step 6: 运行 parser 和子进程 tests**

Run:

```bash
cargo test --bin ccs test_global_logger_flags -- --nocapture
cargo test --test logger_cli_tests -- --nocapture
```

Expected: PASS，显式 log file 包含 `ccs started` debug 记录。

- [ ] **Step 7: 验证 RUST_LOG=off 仍写文件**

在 integration test 增加第二个 case：

```rust
#[test]
fn test_rust_log_off_keeps_info_file_logging() {
    // 同样隔离环境，传 RUST_LOG=off，不传 --debug
    // 执行 session projects
    // stderr 不做脆弱断言；文件必须包含 INFO logger initialized
}
```

Run:

```bash
cargo test --test logger_cli_tests -- --nocapture
```

Expected: 两个 test PASS。

---

### Task 4: 文档、问题记录和最终验证

**Files:**
- Modify: `README.md`
- Modify: `docs/user-guide.md`
- Modify: `local/notes.md`
- Verify: all Slice files

**Interfaces:**
- Consumes: Task 1-3 的最终 CLI 与日志行为。
- Produces: 可发现的日志排查说明和完整验证证据。

- [ ] **Step 1: 更新 README**

必须说明：

```bash
ccs --debug session list
ccs --log-file ./ccs-debug.log session search "keyword"
RUST_LOG=error ccs session list
```

列出默认平台日志路径、10 MiB × 3 轮转、`RUST_LOG=off` 只关闭 console、不关闭 file、日志不记录会话正文和 credential。

- [ ] **Step 2: 更新 user guide 日志排查章节**

包含：

- 正常用户先查看 stderr；
- 后台或难复现错误查看平台日志文件；
- 临时开启 `--debug`；
- 使用 `--log-file` 收集隔离日志；
- invocation ID 用于关联同一次运行；
- 日志隐私边界；
- Hook debug 尚未统一，明确仍是后续工作，不能宣称已完成。

- [ ] **Step 3: 更新 local/notes**

按项目模板增加：

```markdown
## 2026-08-02: 文件日志未接收普通 log 记录

### 问题描述
- 文档宣称 console+file，但普通 log::warn!/debug! 只进入 stderr。
- init/rotation 错误在 main 中被 .ok() 静默吞掉。

### 根本原因
- env_logger 只配置 stderr；log_to_file 是独立手工 helper。
- logger 在 Cli::parse 前初始化，无法消费 --debug/--log-file。

### 解决方案
- 自定义 DualLogger 同时写 stderr/file。
- parse CLI 后初始化；文件失败显式降级，注册失败返回错误。
- 三代轮转、0600 权限、invocation ID 和脱敏。

### 影响范围
- logger.rs、main.rs、Cargo.toml、logger CLI tests、README/user guide。

### 预防措施
- logger sink 行为必须有真实 Record 与 CLI 子进程测试。
- 禁止对 logger 初始化错误无条件 .ok()。
```

- [ ] **Step 4: 跑目标测试**

```bash
cargo test logger::tests -- --nocapture
cargo test --bin ccs test_global_logger_flags -- --nocapture
cargo test --test logger_cli_tests -- --nocapture
```

Expected: PASS。

- [ ] **Step 5: 跑全部 gate**

```bash
cargo fmt
cargo fmt --check
cargo clippy -- -D warnings
cargo test
git diff --check
```

Expected: 全部 exit 0。

- [ ] **Step 6: 安全实跑**

```bash
TMP_DIR=$(mktemp -d)
HOME="$TMP_DIR/home" \
USERPROFILE="$TMP_DIR/home" \
CLAUDE_CODE_SYNC_CONFIG_DIR="$TMP_DIR/config" \
cargo run -- --debug --log-file "$TMP_DIR/ccs.log" session projects --source codex
```

Expected:

- exit 0；
- `$TMP_DIR/ccs.log` 存在；
- 包含 `DEBUG`、`invocation=I-`、`ccs started`；
- 不含真实 home 路径或 credential。

- [ ] **Step 7: 审查 scope**

```bash
git status --short
git diff --stat
git diff -- src/logger.rs src/main.rs Cargo.toml tests/logger_cli_tests.rs README.md docs/user-guide.md local/notes.md
```

Expected: 功能改动只涉及计划文件；不包含 ScanDiagnostics、doctor、Hook logging 或 session cache 行为修改。

---

## Plan Self-Review

### Spec coverage

- 所有 log::Record 双 sink：Task 2。
- 默认路径与 `--log-file`：Task 2-3。
- parse-before-init 和错误可见性：Task 3。
- 10 MiB × 3 轮转：Task 1-2。
- `--debug`、RUST_LOG precedence、RUST_LOG=off file continued：Task 1、3。
- invocation ID、隐私清理和 0600：Task 1-2。
- 真实 Record 与子进程测试：Task 2-3。
- 文档和 notes：Task 4。
- ScanDiagnostics、doctor、Hook 统一明确排除。

### Type consistency

- `LoggerOptions::new(debug, log_path, rust_log)` 在 Task 2 定义、Task 3 消费。
- `init_logger_with_options` 返回 `LoggerInitStatus`，main 使用 warning/log_path。
- `DualLogger` 内部 writer 使用 `Box<dyn Write + Send>`，生产和测试共享同一逻辑。
- 兼容 wrapper 不依赖 CLI flags，保持现有 public function 可编译。

### Placeholder scan

计划无 TBD、TODO、未定义接口或“添加适当处理”类占位步骤。后续可观测性子系统被明确排除并单独规划。
