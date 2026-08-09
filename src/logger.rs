use anyhow::{anyhow, Context, Result};
use chrono::{SecondsFormat, Utc};
use log::{LevelFilter, Log, Metadata, Record};
use regex::{Captures, Regex};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex, OnceLock,
};
use tempfile::TempDir;
use uuid::Uuid;

use crate::config::ConfigManager;

pub(crate) const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024;
pub(crate) const LOG_BACKUP_COUNT: usize = 3;
pub(crate) const SCAN_DIAGNOSTICS_TARGET: &str = "ccs::scan_diagnostics";

static CURRENT_INVOCATION_ID: OnceLock<String> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static ROTATION_FAIL_AFTER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ROTATION_ROLLBACK_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn current_invocation_id() -> Option<&'static str> {
    CURRENT_INVOCATION_ID.get().map(String::as_str)
}

fn generate_invocation_id() -> String {
    format!(
        "I-{}",
        Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect::<String>()
            .to_uppercase()
    )
}

pub(crate) fn validate_invocation_id(value: &str) -> Result<()> {
    if !(3..=64).contains(&value.len())
        || !value.starts_with("I-")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(anyhow!("invalid invocation id"));
    }
    Ok(())
}

pub(crate) fn safe_invocation_id(value: &str) -> String {
    if validate_invocation_id(value).is_ok() {
        value.to_string()
    } else {
        generate_invocation_id()
    }
}

#[cfg(test)]
struct RotationFailureGuard;

#[cfg(test)]
impl Drop for RotationFailureGuard {
    fn drop(&mut self) {
        ROTATION_FAIL_AFTER.with(|fail_after| fail_after.set(0));
        ROTATION_ROLLBACK_FAIL.with(|should_fail| should_fail.set(false));
    }
}

#[cfg(test)]
fn rotation_failure_guard(after_operation: Option<usize>) -> RotationFailureGuard {
    ROTATION_FAIL_AFTER.with(|fail_after| fail_after.set(after_operation.unwrap_or(0)));
    RotationFailureGuard
}

#[cfg(test)]
fn rotation_failure_guard_with_rollback_failure(
    after_operation: Option<usize>,
) -> RotationFailureGuard {
    ROTATION_FAIL_AFTER.with(|fail_after| fail_after.set(after_operation.unwrap_or(0)));
    ROTATION_ROLLBACK_FAIL.with(|should_fail| should_fail.set(true));
    RotationFailureGuard
}

fn rotation_step() -> Result<()> {
    #[cfg(test)]
    {
        let should_fail = ROTATION_FAIL_AFTER.with(|fail_after| {
            let current = fail_after.get();
            if current == 0 {
                false
            } else if current == 1 {
                fail_after.set(0);
                true
            } else {
                fail_after.set(current - 1);
                false
            }
        });
        if should_fail {
            return Err(anyhow!("injected rotation failure"));
        }
    }
    Ok(())
}

pub(crate) fn resolve_console_level(debug: bool, rust_log: Option<&str>) -> LevelFilter {
    rust_log
        .and_then(|value| value.parse::<LevelFilter>().ok())
        .unwrap_or(if debug {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        })
}

pub(crate) fn resolve_file_level(debug: bool) -> LevelFilter {
    if debug {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    }
}

fn redact_quoted_secret_values(message: &str, key_pattern: &Regex) -> String {
    let mut output = String::with_capacity(message.len());
    let mut cursor = 0;
    for key_match in key_pattern.find_iter(message) {
        if key_match.start() < cursor {
            continue;
        }
        output.push_str(&message[cursor..key_match.end()]);
        let value_start = key_match.end();
        let Some(quote) = message[value_start..].chars().next() else {
            cursor = value_start;
            continue;
        };
        if quote != '\'' && quote != '"' {
            cursor = value_start;
            continue;
        }

        let content_start = value_start + quote.len_utf8();
        let mut escaped = false;
        let mut closing_end = None;
        for (offset, character) in message[content_start..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if character == '\\' {
                escaped = true;
            } else if character == quote {
                closing_end = Some(content_start + offset + character.len_utf8());
                break;
            }
        }
        let value_end = closing_end.unwrap_or(message.len());
        output.push(quote);
        output.push_str("<redacted>");
        if closing_end.is_some() {
            output.push(quote);
        }
        cursor = value_end;
    }
    output.push_str(&message[cursor..]);
    output
}

pub(crate) fn sanitize_log_message(message: &str, home: Option<&Path>) -> String {
    static SECRET_RE: OnceLock<Regex> = OnceLock::new();
    static SECRET_KEY_RE: OnceLock<Regex> = OnceLock::new();
    static AUTHORIZATION_RE: OnceLock<Regex> = OnceLock::new();
    static URL_USERINFO_RE: OnceLock<Regex> = OnceLock::new();
    static FILE_URL_PATH_RE: OnceLock<Regex> = OnceLock::new();
    static QUOTED_ABSOLUTE_PATH_RE: OnceLock<Regex> = OnceLock::new();
    static SINGLE_QUOTED_ABSOLUTE_PATH_RE: OnceLock<Regex> = OnceLock::new();
    static UNIX_ABSOLUTE_PATH_RE: OnceLock<Regex> = OnceLock::new();
    static WINDOWS_ABSOLUTE_PATH_RE: OnceLock<Regex> = OnceLock::new();

    let aliases = r"token|access[_-]?token|refresh[_-]?token|password|passwd|secret|client[_-]?secret|api[_-]?key";
    let secret_re = SECRET_RE.get_or_init(|| {
        Regex::new(&format!(
            r##"(?i)\b({aliases})(\s*[=:]\s*)("[^"\r\n]*"|'[^'\r\n]*'|[^\s,;]+)"##
        ))
        .expect("secret log redaction pattern must be valid")
    });
    let secret_key_re = SECRET_KEY_RE.get_or_init(|| {
        Regex::new(&format!(
            r##"(?i)(?:\b(?:{aliases})\b|["'](?:{aliases})["'])\s*[=:]\s*"##
        ))
        .expect("secret key log redaction pattern must be valid")
    });
    let authorization_re = AUTHORIZATION_RE.get_or_init(|| {
        Regex::new(r"(?i)(\bAuthorization\s*:\s*(?:Bearer|Basic)\s+)([^\s,;]+)")
            .expect("authorization log redaction pattern must be valid")
    });
    let url_userinfo_re = URL_USERINFO_RE.get_or_init(|| {
        Regex::new(r"([a-zA-Z][a-zA-Z0-9+.-]*://)[^/@\s]+@")
            .expect("URL redaction pattern must be valid")
    });
    let file_url_path_re = FILE_URL_PATH_RE.get_or_init(|| {
        Regex::new(r##"(?i)\b(file://)([^/\s]+)?/[^\s]+"##)
            .expect("file URL redaction pattern must be valid")
    });
    let quoted_absolute_path_re = QUOTED_ABSOLUTE_PATH_RE.get_or_init(|| {
        Regex::new(r##""(?:/[^"\r\n]+|[A-Za-z]:[\\/][^"\r\n]+|\\\\[^"\r\n]+)""##)
            .expect("double-quoted path redaction pattern must be valid")
    });
    let single_quoted_absolute_path_re = SINGLE_QUOTED_ABSOLUTE_PATH_RE.get_or_init(|| {
        Regex::new(r##"'(?:/[^'\r\n]+|[A-Za-z]:[\\/][^'\r\n]+|\\\\[^'\r\n]+)'"##)
            .expect("single-quoted path redaction pattern must be valid")
    });
    let unix_absolute_path_re = UNIX_ABSOLUTE_PATH_RE.get_or_init(|| {
        Regex::new(r"(?m)(^|[\s=:()\[])/(?:[^\s,;)\]}]+)")
            .expect("Unix path redaction pattern must be valid")
    });
    let windows_absolute_path_re = WINDOWS_ABSOLUTE_PATH_RE.get_or_init(|| {
        Regex::new(r"(?i)(^|[\s=:()\[])(?:[a-z]:[\\/]|\\\\|//)(?:[^\s,;)\]}]+)")
            .expect("Windows path redaction pattern must be valid")
    });

    let sanitized = redact_quoted_secret_values(message, secret_key_re);
    let sanitized = secret_re.replace_all(&sanitized, |captures: &Captures<'_>| {
        let value = captures
            .get(3)
            .map(|match_| match_.as_str())
            .unwrap_or_default();
        let replacement = if value.starts_with('"') && value.ends_with('"') {
            "\"<redacted>\"".to_string()
        } else if value.starts_with('\'') && value.ends_with('\'') {
            "'<redacted>'".to_string()
        } else {
            "<redacted>".to_string()
        };
        format!("{}{}{}", &captures[1], &captures[2], replacement)
    });
    let sanitized = authorization_re.replace_all(&sanitized, "$1<redacted>");
    let sanitized = url_userinfo_re.replace_all(&sanitized, "$1***@");
    let sanitized = file_url_path_re.replace_all(&sanitized, |captures: &Captures<'_>| {
        let host = captures
            .get(2)
            .map(|host| host.as_str())
            .unwrap_or_default();
        if host.is_empty() {
            format!("{}<path>", &captures[1])
        } else {
            format!("{}{host}/<path>", &captures[1])
        }
    });
    // Protect URL schemes while path patterns process colon-prefixed paths such
    // as `path:/tmp/file`; otherwise the `://` separator looks like a path.
    let sanitized = sanitized.replace("://", "__CCS_URL_SCHEME__");
    let sanitized = match home {
        Some(home) => sanitized.replace(home.to_string_lossy().as_ref(), "~"),
        None => sanitized,
    };
    let sanitized = quoted_absolute_path_re.replace_all(&sanitized, "\"<path>\"");
    let sanitized = single_quoted_absolute_path_re.replace_all(&sanitized, "'<path>'");
    let sanitized = unix_absolute_path_re.replace_all(&sanitized, "$1<path>");
    let sanitized = windows_absolute_path_re.replace_all(&sanitized, "$1<path>");
    let sanitized = sanitized.into_owned().replace("__CCS_URL_SCHEME__", "://");
    sanitized
        .replace("\r\n", "\\n")
        .replace(['\r', '\n'], "\\n")
}

pub(crate) fn format_log_line(
    timestamp: &str,
    level: log::Level,
    invocation_id: &str,
    target: &str,
    message: &str,
    home: Option<&Path>,
) -> String {
    let safe_invocation_id = safe_invocation_id(invocation_id);
    let safe_timestamp = sanitize_log_message(timestamp, None);
    let safe_target = sanitize_log_message(target, home);
    format!(
        "{safe_timestamp} {level} invocation={safe_invocation_id} target={safe_target} {}\n",
        sanitize_log_message(message, home)
    )
}

pub(crate) fn rotated_path(path: &Path, generation: usize) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!("{ext}.{generation}"))
        .unwrap_or_else(|| generation.to_string());
    path.with_extension(extension)
}

fn copy_from_opened_log_handle(source: &File, destination: &Path) -> Result<()> {
    let source_metadata = source
        .metadata()
        .context("failed to inspect opened log source")?;
    if !source_metadata.is_file() {
        return Err(anyhow!("log source must be a regular file"));
    }

    let mut source = source
        .try_clone()
        .context("failed to clone opened log source")?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(NO_FOLLOW_FLAG);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(OPEN_REPARSE_POINT_FLAG);
    }
    let mut destination = options
        .open(destination)
        .context("failed to create staged log generation")?;
    std::io::copy(&mut source, &mut destination).context("failed to copy opened log source")?;
    destination
        .flush()
        .context("failed to flush staged log generation")?;
    set_private_permissions_handle(&destination)?;

    let destination_len = destination
        .metadata()
        .context("failed to inspect staged log generation")?
        .len();
    if destination_len != source_metadata.len() {
        return Err(anyhow!("log source changed while staging"));
    }
    Ok(())
}

fn ensure_log_artifact_safe(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(anyhow!("log path must not be a symlink"));
            }
            if !metadata.file_type().is_file() {
                return Err(anyhow!("log path must be a regular file"));
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
fn rotation_rollback_step() -> Result<()> {
    if ROTATION_ROLLBACK_FAIL.with(|should_fail| should_fail.replace(false)) {
        return Err(anyhow!("injected rotation rollback failure"));
    }
    Ok(())
}

#[cfg(not(test))]
fn rotation_rollback_step() -> Result<()> {
    Ok(())
}

fn rollback_rotation(
    transaction_dir: &Path,
    moved_targets: &[PathBuf],
    installed_targets: &[PathBuf],
) -> Result<()> {
    let mut rollback_error = None;
    for target in installed_targets.iter().rev() {
        if let Err(error) = rotation_rollback_step() {
            if rollback_error.is_none() {
                rollback_error = Some(error);
            }
            continue;
        }
        if let Err(error) = std::fs::remove_file(target) {
            if error.kind() != std::io::ErrorKind::NotFound && rollback_error.is_none() {
                rollback_error = Some(error.into());
            }
        }
    }
    for target in moved_targets.iter().rev() {
        let backup = transaction_dir.join(
            target
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("current")),
        );
        if let Err(error) = rotation_rollback_step() {
            if rollback_error.is_none() {
                rollback_error = Some(error);
            }
            continue;
        }
        if let Err(error) = std::fs::rename(&backup, target) {
            if rollback_error.is_none() {
                rollback_error = Some(error.into());
            }
        }
    }
    if let Some(error) = rollback_error {
        return Err(error).context("rotation rollback failed");
    }
    Ok(())
}

/// Rotate a log whose caller already holds the per-log advisory lock.
///
/// All source bytes are copied and secured in a staging directory before any
/// live path is moved. A transaction directory keeps every live artifact
/// reversible until the new generation chain is installed.
fn rotate_log_at_locked(path: &Path, max_size: u64, backups: usize) -> Result<bool> {
    let current_exists = ensure_log_artifact_safe(path)?;
    if !current_exists {
        return Ok(false);
    }

    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to read log metadata: {}", path.display()))?;
    if metadata.len() <= max_size {
        return Ok(false);
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    for generation in 1..=backups {
        ensure_log_artifact_safe(&rotated_path(path, generation))?;
    }

    let staging = TempDir::new_in(parent).context("failed to create log rotation staging")?;
    let transaction =
        TempDir::new_in(parent).context("failed to create log rotation transaction")?;
    let mut staged_targets = Vec::new();
    for generation in 1..=backups {
        let source = if generation == 1 {
            path.to_path_buf()
        } else {
            rotated_path(path, generation - 1)
        };
        if !ensure_log_artifact_safe(&source)? {
            continue;
        }
        let source_handle = open_existing_no_follow(&source)
            .with_context(|| format!("failed to open log generation {generation} for staging"))?;
        rotation_step()?;
        let staged = staging.path().join(format!("generation-{generation}"));
        copy_from_opened_log_handle(&source_handle, &staged)
            .with_context(|| format!("failed to stage log generation {generation}"))?;
        staged_targets.push((staged, rotated_path(path, generation)));
    }

    let mut moved_targets = Vec::new();
    let mut installed_targets = Vec::new();
    let live_targets = std::iter::once(path.to_path_buf())
        .chain((1..=backups).map(|generation| rotated_path(path, generation)))
        .collect::<Vec<_>>();
    let commit_result = (|| -> Result<()> {
        for target in &live_targets {
            if !ensure_log_artifact_safe(target)? {
                continue;
            }
            let target_handle = open_existing_no_follow(target)
                .with_context(|| "failed to open live log artifact for rotation")?;
            rotation_step()?;
            if !opened_file_still_matches_path(target, &target_handle)? {
                return Err(anyhow!("live log artifact changed during rotation"));
            }
            let backup = transaction.path().join(
                target
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("current")),
            );
            std::fs::rename(target, &backup)
                .with_context(|| "failed to move live log artifact into transaction")?;
            moved_targets.push(target.clone());
        }
        for (staged, target) in &staged_targets {
            rotation_step()?;
            std::fs::rename(staged, target)
                .with_context(|| "failed to install staged log generation")?;
            installed_targets.push(target.clone());
        }
        Ok(())
    })();

    if let Err(error) = commit_result {
        let rollback = rollback_rotation(transaction.path(), &moved_targets, &installed_targets);
        return match rollback {
            Ok(()) => Err(error).context("log rotation rolled back safely"),
            Err(_rollback_error) => {
                // Do not let TempDir remove the only remaining copies of old logs.
                // The caller receives a visible, deliberately path-free error while
                // the transaction directory remains available for manual recovery.
                std::mem::forget(transaction);
                Err(anyhow!(
                    "log rotation failed; rollback failed and transaction retained for recovery"
                ))
            }
        };
    }

    for target in &moved_targets {
        let backup = transaction.path().join(
            target
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("current")),
        );
        if let Err(error) = std::fs::remove_file(&backup) {
            // Keep the transaction directory alive so the last old generation
            // remains recoverable even when cleanup itself fails.
            std::mem::forget(transaction);
            return Err(error).context("log rotation committed but transaction cleanup failed");
        }
    }
    Ok(true)
}

#[allow(dead_code)]
pub(crate) fn rotate_log_at(path: &Path, max_size: u64, backups: usize) -> Result<bool> {
    let _lock = LogFileLock::acquire(path)?;
    rotate_log_at_locked(path, max_size, backups)
}

/// Initialize the logging system
///
/// Sets up logging to both console and a log file in the config directory.
///
/// **Console logging** can be controlled via the `RUST_LOG` environment variable:
/// - `RUST_LOG=error` - Only errors
/// - `RUST_LOG=warn` - Warnings and errors
/// - `RUST_LOG=info` - Info, warnings, and errors (default)
/// - `RUST_LOG=debug` - Debug and above
/// - `RUST_LOG=trace` - Everything
///
/// **File logging** defaults to `Info` and uses `Debug` when `--debug` is set.
/// `RUST_LOG` controls the console level only. The file is stored at:
/// - Linux: ~/.config/claude-code-sync/claude-code-sync.log or $XDG_CONFIG_HOME/claude-code-sync/claude-code-sync.log
/// - macOS: ~/Library/Application Support/claude-code-sync/claude-code-sync.log
/// - Windows: %APPDATA%\claude-code-sync\claude-code-sync.log
///
/// ## Examples
///
/// ```bash
/// # Show all debug messages on console
/// RUST_LOG=debug ccs sync
///
/// # Only show errors on console
/// RUST_LOG=error ccs push
///
/// # No console output (file logging continues)
/// RUST_LOG=off ccs pull
/// ```
type BoxedWriter = Box<dyn Write + Send>;

/// Options used to initialize the process logger.
pub struct LoggerOptions {
    pub console_level: LevelFilter,
    pub file_level: LevelFilter,
    pub log_path: PathBuf,
    pub invocation_id: String,
}

impl LoggerOptions {
    /// Resolve logging levels, the log path, and a per-process invocation ID.
    pub fn new(debug: bool, log_path: Option<PathBuf>, rust_log: Option<&str>) -> Result<Self> {
        let log_path = match log_path {
            Some(path) => path,
            None => ConfigManager::log_file_path()?,
        };
        let invocation_id = generate_invocation_id();

        Ok(Self {
            console_level: resolve_console_level(debug, rust_log),
            file_level: resolve_file_level(debug),
            log_path,
            invocation_id,
        })
    }
}

/// Result of attempting to initialize both logger sinks.
pub struct LoggerInitStatus {
    /// Invocation ID exposed for library callers that need to correlate logs.
    #[allow(dead_code)]
    pub invocation_id: String,
    #[allow(dead_code)]
    pub log_path: PathBuf,
    /// Whether the file sink opened successfully.
    #[allow(dead_code)]
    pub file_logging_enabled: bool,
    pub warning: Option<String>,
}

#[derive(Default)]
struct SinkHealth {
    write_failures: AtomicUsize,
    flush_failures: AtomicUsize,
    poisoned: AtomicUsize,
    fallback_reported: AtomicBool,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
struct SinkStatusSnapshot {
    write_failures: usize,
    flush_failures: usize,
    poisoned: usize,
    fallback_reports: usize,
}

impl SinkHealth {
    fn report(&self, kind: SinkFailureKind, sink: &str) {
        match kind {
            SinkFailureKind::Write => {
                self.write_failures.fetch_add(1, Ordering::Relaxed);
            }
            SinkFailureKind::Flush => {
                self.flush_failures.fetch_add(1, Ordering::Relaxed);
            }
            SinkFailureKind::Poisoned => {
                self.poisoned.fetch_add(1, Ordering::Relaxed);
            }
        }
        if self
            .fallback_reported
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let message = match sink {
                "console" => "WARNING: console logging sink failed; continuing safely",
                _ => "WARNING: file logging sink failed; continuing with stderr logs",
            };
            write_safe_stderr(message);
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> SinkStatusSnapshot {
        SinkStatusSnapshot {
            write_failures: self.write_failures.load(Ordering::Relaxed),
            flush_failures: self.flush_failures.load(Ordering::Relaxed),
            poisoned: self.poisoned.load(Ordering::Relaxed),
            fallback_reports: usize::from(self.fallback_reported.load(Ordering::Relaxed)),
        }
    }
}

enum SinkFailureKind {
    Write,
    Flush,
    Poisoned,
}

const MAX_SCAN_WARNING_RECORDS: usize = 100;
pub(crate) const SCAN_WARNINGS_SUPPRESSED_MESSAGE: &str =
    "session scan warnings suppressed after reaching detail cap";

struct ScanWarningBudget {
    emitted: AtomicUsize,
    suppression_reported: AtomicBool,
}

impl Default for ScanWarningBudget {
    fn default() -> Self {
        Self {
            emitted: AtomicUsize::new(0),
            suppression_reported: AtomicBool::new(false),
        }
    }
}

impl ScanWarningBudget {
    fn reserve_detail(&self) -> bool {
        self.emitted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < MAX_SCAN_WARNING_RECORDS).then_some(current + 1)
            })
            .is_ok()
    }

    fn report_suppression_once(&self) -> bool {
        self.suppression_reported
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

enum FileSink {
    Writer(Mutex<BoxedWriter>),
    Path(PathBuf),
}

impl<W> From<Box<W>> for FileSink
where
    W: Write + Send + 'static,
{
    fn from(writer: Box<W>) -> Self {
        FileSink::Writer(Mutex::new(writer))
    }
}

struct DualLogger {
    console_level: LevelFilter,
    file_level: LevelFilter,
    invocation_id: String,
    home_dir: Option<PathBuf>,
    console: Mutex<BoxedWriter>,
    file: Option<FileSink>,
    console_health: SinkHealth,
    file_health: SinkHealth,
    scan_warning_budget: ScanWarningBudget,
}

impl DualLogger {
    fn with_writers<F>(
        console_level: LevelFilter,
        file_level: LevelFilter,
        invocation_id: String,
        home_dir: Option<PathBuf>,
        console: BoxedWriter,
        file: Option<F>,
    ) -> Self
    where
        F: Into<FileSink>,
    {
        Self {
            console_level,
            file_level,
            invocation_id,
            home_dir,
            console: Mutex::new(console),
            file: file.map(Into::into),
            console_health: SinkHealth::default(),
            file_health: SinkHealth::default(),
            scan_warning_budget: ScanWarningBudget::default(),
        }
    }

    #[cfg(test)]
    fn sink_status(&self) -> SinkStatus {
        SinkStatus {
            console: self.console_health.snapshot(),
            file: self.file_health.snapshot(),
        }
    }

    fn write_sink(sink: &Mutex<BoxedWriter>, health: &SinkHealth, sink_name: &str, message: &[u8]) {
        let mut writer = match sink.lock() {
            Ok(writer) => writer,
            Err(poisoned) => {
                health.report(SinkFailureKind::Poisoned, sink_name);
                poisoned.into_inner()
            }
        };
        if writer.write_all(message).is_err() {
            health.report(SinkFailureKind::Write, sink_name);
        }
    }

    fn flush_sink(sink: &Mutex<BoxedWriter>, health: &SinkHealth, sink_name: &str) {
        let mut writer = match sink.lock() {
            Ok(writer) => writer,
            Err(poisoned) => {
                health.report(SinkFailureKind::Poisoned, sink_name);
                poisoned.into_inner()
            }
        };
        if writer.flush().is_err() {
            health.report(SinkFailureKind::Flush, sink_name);
        }
    }

    fn write_file_sink(&self, sink: &FileSink, message: &[u8]) {
        match sink {
            FileSink::Writer(writer) => {
                Self::write_sink(writer, &self.file_health, "file", message);
            }
            FileSink::Path(path) => {
                let _lock = match LogFileLock::acquire(path) {
                    Ok(lock) => lock,
                    Err(_) => {
                        self.file_health.report(SinkFailureKind::Write, "file");
                        return;
                    }
                };
                let mut file = match open_log_file_locked(path) {
                    Ok(file) => file,
                    Err(_) => {
                        self.file_health.report(SinkFailureKind::Write, "file");
                        return;
                    }
                };
                if file.write_all(message).is_err() {
                    self.file_health.report(SinkFailureKind::Write, "file");
                    return;
                }
                if file.flush().is_err() {
                    self.file_health.report(SinkFailureKind::Flush, "file");
                }
            }
        }
    }

    fn flush_file_sink(&self, sink: &FileSink) {
        match sink {
            FileSink::Writer(writer) => {
                Self::flush_sink(writer, &self.file_health, "file");
            }
            FileSink::Path(path) => {
                let result = (|| -> Result<()> {
                    let _lock = LogFileLock::acquire(path)?;
                    let mut file = open_log_file_locked(path)?;
                    file.flush().context("failed to flush log record")?;
                    Ok(())
                })();
                if result.is_err() {
                    self.file_health.report(SinkFailureKind::Flush, "file");
                }
            }
        }
    }

    fn write_scan_suppression(&self) {
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let line = format_log_line(
            &timestamp,
            log::Level::Warn,
            &self.invocation_id,
            SCAN_DIAGNOSTICS_TARGET,
            SCAN_WARNINGS_SUPPRESSED_MESSAGE,
            self.home_dir.as_deref(),
        );
        if let Some(file) = &self.file {
            self.write_file_sink(file, line.as_bytes());
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct SinkStatus {
    console: SinkStatusSnapshot,
    file: SinkStatusSnapshot,
}

impl Log for DualLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.console_level >= metadata.level() || self.file_level >= metadata.level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let message = format_log_line(
            &timestamp,
            record.level(),
            &self.invocation_id,
            record.target(),
            &record.args().to_string(),
            self.home_dir.as_deref(),
        );

        let console_enabled =
            self.console_level >= record.level() && record.target() != SCAN_DIAGNOSTICS_TARGET;
        if console_enabled {
            Self::write_sink(
                &self.console,
                &self.console_health,
                "console",
                message.as_bytes(),
            );
        }
        if self.file_level >= record.level() {
            if let Some(file) = &self.file {
                if record.target() == SCAN_DIAGNOSTICS_TARGET && record.level() == log::Level::Warn
                {
                    let raw_message = record.args().to_string();
                    if raw_message == SCAN_WARNINGS_SUPPRESSED_MESSAGE {
                        if self.scan_warning_budget.report_suppression_once() {
                            self.write_file_sink(file, message.as_bytes());
                        }
                    } else if self.scan_warning_budget.reserve_detail() {
                        self.write_file_sink(file, message.as_bytes());
                    } else if self.scan_warning_budget.report_suppression_once() {
                        self.write_scan_suppression();
                    }
                } else {
                    self.write_file_sink(file, message.as_bytes());
                }
            }
        }
    }

    fn flush(&self) {
        Self::flush_sink(&self.console, &self.console_health, "console");
        if let Some(file) = &self.file {
            self.flush_file_sink(file);
        }
    }
}

fn write_safe_stderr(message: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
    let _ = stderr.flush();
}

fn file_sink_warning() -> &'static str {
    "WARNING: file logging unavailable; continuing with stderr logs"
}

#[cfg(target_os = "linux")]
const NO_FOLLOW_FLAG: i32 = 0x20000;

#[cfg(target_os = "macos")]
const NO_FOLLOW_FLAG: i32 = 0x100;

#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
const NO_FOLLOW_FLAG: i32 = 0x100;

#[cfg(windows)]
const OPEN_REPARSE_POINT_FLAG: u32 = 0x00200000;

fn open_existing_no_follow(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(NO_FOLLOW_FLAG);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(OPEN_REPARSE_POINT_FLAG);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open log artifact: {}", path.display()))?;
    if matches!(std::fs::symlink_metadata(path), Ok(metadata) if metadata.file_type().is_symlink())
    {
        return Err(anyhow!("log path must not be a symlink"));
    }
    Ok(file)
}

fn opened_file_still_matches_path(path: &Path, file: &File) -> Result<bool> {
    let path_metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Ok(false);
    }
    let handle_metadata = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(path_metadata.dev() == handle_metadata.dev()
            && path_metadata.ino() == handle_metadata.ino())
    }
    #[cfg(not(unix))]
    {
        Ok(path_metadata.len() == handle_metadata.len()
            && path_metadata.modified().ok() == handle_metadata.modified().ok())
    }
}

fn set_private_permissions_handle(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .context("failed to set private log permissions")?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn set_private_permissions(path: &Path) -> Result<()> {
    let file = open_existing_no_follow(path)?;
    set_private_permissions_handle(&file)
}

fn secure_log_generations(path: &Path, backups: usize) -> Result<()> {
    for generation in 0..=backups {
        let candidate = if generation == 0 {
            path.to_path_buf()
        } else {
            rotated_path(path, generation)
        };
        if ensure_log_artifact_safe(&candidate)? {
            set_private_permissions(&candidate)?;
        }
    }
    Ok(())
}

struct LogFileLock {
    file: File,
}

impl LogFileLock {
    fn acquire(log_path: &Path) -> Result<Self> {
        let parent = log_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log directory: {}", parent.display()))?;
        }
        let lock_path = lock_path_for(log_path);
        if let Ok(metadata) = std::fs::symlink_metadata(&lock_path) {
            if metadata.file_type().is_symlink() {
                return Err(anyhow!("log lock path must not be a symlink"));
            }
        }
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(NO_FOLLOW_FLAG);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(OPEN_REPARSE_POINT_FLAG);
        }
        let file = options
            .open(&lock_path)
            .with_context(|| format!("failed to open log lock file: {}", lock_path.display()))?;
        if matches!(std::fs::symlink_metadata(&lock_path), Ok(metadata) if metadata.file_type().is_symlink())
        {
            return Err(anyhow!("log lock path must not be a symlink"));
        }
        set_private_permissions_handle(&file)?;
        fs4::FileExt::lock(&file)
            .with_context(|| format!("failed to acquire log lock: {}", lock_path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for LogFileLock {
    fn drop(&mut self) {
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

fn lock_path_for(path: &Path) -> PathBuf {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| path.with_extension(format!("{extension}.lock")))
        .unwrap_or_else(|| path.with_extension("lock"))
}

fn open_log_file_locked(path: &Path) -> Result<File> {
    ensure_log_artifact_safe(path)?;
    for generation in 1..=LOG_BACKUP_COUNT {
        ensure_log_artifact_safe(&rotated_path(path, generation))?;
    }
    if ensure_log_artifact_safe(path)? {
        set_private_permissions(path)?;
    }
    rotate_log_at_locked(path, MAX_LOG_SIZE, LOG_BACKUP_COUNT)?;
    secure_log_generations(path, LOG_BACKUP_COUNT)?;

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(NO_FOLLOW_FLAG);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(OPEN_REPARSE_POINT_FLAG);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open log file: {}", path.display()))?;
    if matches!(std::fs::symlink_metadata(path), Ok(metadata) if metadata.file_type().is_symlink())
    {
        return Err(anyhow!("log path must not be a symlink"));
    }
    set_private_permissions_handle(&file)?;
    Ok(file)
}

fn open_log_file(path: &Path) -> Result<File> {
    let _lock = LogFileLock::acquire(path)?;
    open_log_file_locked(path)
}

fn build_logger(options: &LoggerOptions) -> Result<(DualLogger, Option<String>)> {
    let (file, warning) = match open_log_file(&options.log_path) {
        Ok(_) => (Some(FileSink::Path(options.log_path.clone())), None),
        Err(_) => (
            None,
            Some("file logging unavailable; continuing with stderr logs".to_string()),
        ),
    };
    let mut logger = DualLogger::with_writers(
        options.console_level,
        options.file_level,
        options.invocation_id.clone(),
        dirs::home_dir(),
        Box::new(std::io::stderr()),
        Some(Box::new(std::io::sink())),
    );
    logger.file = file;
    Ok((logger, warning))
}

/// Initialize the logger with explicit options and report file-sink degradation.
pub fn init_logger_with_options(mut options: LoggerOptions) -> Result<LoggerInitStatus> {
    options.invocation_id =
        validate_invocation_id(&options.invocation_id).map(|_| options.invocation_id.clone())?;
    let log_path = options.log_path.clone();
    let invocation_id = options.invocation_id.clone();
    let (logger, warning) = build_logger(&options)?;
    let file_logging_enabled = logger.file.is_some();
    let max_level = std::cmp::max(options.console_level, options.file_level);

    log::set_boxed_logger(Box::new(logger)).context("failed to register global logger")?;
    let _ = CURRENT_INVOCATION_ID.set(invocation_id.clone());
    log::set_max_level(max_level);
    log::info!(
        "logger initialized console_level={} file_logging_enabled={file_logging_enabled}",
        options.console_level
    );

    Ok(LoggerInitStatus {
        invocation_id,
        log_path,
        file_logging_enabled,
        warning,
    })
}

/// Initialize logging using the legacy default options.
///
/// Kept as a compatibility entry point; the CLI uses `init_logger_with_options`.
#[allow(dead_code)]
pub fn init_logger() -> Result<()> {
    let rust_log = std::env::var("RUST_LOG").ok();
    let status = init_logger_with_options(LoggerOptions::new(false, None, rust_log.as_deref())?)?;
    if status.warning.is_some() {
        write_safe_stderr(file_sink_warning());
    }
    Ok(())
}

/// Append one sanitized legacy log line. Prefer `log` macros for new code.
#[allow(dead_code)]
#[deprecated(note = "use log macros after init_logger instead")]
pub fn log_to_file(message: &str) -> Result<()> {
    let log_path = ConfigManager::log_file_path()?;
    let _lock = LogFileLock::acquire(&log_path)?;
    let mut file = open_log_file_locked(&log_path)?;
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let line = format_log_line(
        &timestamp,
        log::Level::Info,
        "I-LEGACY",
        "legacy",
        message,
        dirs::home_dir().as_deref(),
    );
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Rotate the default log file if it exceeds the configured size limit.
///
/// Kept as a compatibility entry point for library callers.
#[allow(dead_code)]
pub fn rotate_log_if_needed() -> Result<()> {
    let log_path = ConfigManager::log_file_path()?;
    let _lock = LogFileLock::acquire(&log_path)?;
    if log_path.exists() {
        set_private_permissions(&log_path)?;
    }
    rotate_log_at_locked(&log_path, MAX_LOG_SIZE, LOG_BACKUP_COUNT)?;
    secure_log_generations(&log_path, LOG_BACKUP_COUNT)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_DIR_ENV;
    use serial_test::serial;
    use std::env;
    use std::fs::File;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    struct ConfigEnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl ConfigEnvGuard {
        fn new(path: &Path) -> Self {
            let previous = env::var_os(CONFIG_DIR_ENV);
            env::set_var(CONFIG_DIR_ENV, path);
            Self { previous }
        }
    }

    impl Drop for ConfigEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => env::set_var(CONFIG_DIR_ENV, value),
                None => env::remove_var(CONFIG_DIR_ENV),
            }
        }
    }

    #[test]
    fn test_resolve_log_levels() {
        assert_eq!(resolve_console_level(false, None), LevelFilter::Info);
        assert_eq!(resolve_console_level(true, None), LevelFilter::Debug);
        assert_eq!(
            resolve_console_level(true, Some("error")),
            LevelFilter::Error
        );
        assert_eq!(resolve_console_level(false, Some("off")), LevelFilter::Off);
        assert_eq!(
            resolve_console_level(false, Some("invalid")),
            LevelFilter::Info
        );
        assert_eq!(resolve_file_level(false), LevelFilter::Info);
        assert_eq!(resolve_file_level(true), LevelFilter::Debug);
    }

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

    #[test]
    fn test_sanitize_log_message_redacts_absolute_paths_outside_home() {
        let input = concat!(
            "/var/folders/abc/project/session.jsonl ",
            "quoted=\"/tmp/project with spaces/session.jsonl\" ",
            "path=C:\\Users\\Name\\project\\session.jsonl ",
            "module=ccs::session: done"
        );
        let sanitized = sanitize_log_message(input, Some(Path::new("/Users/example")));

        for raw_path in [
            "/var/folders/abc/project/session.jsonl",
            "/tmp/project with spaces/session.jsonl",
            r"C:\Users\Name\project\session.jsonl",
        ] {
            assert!(
                !sanitized.contains(raw_path),
                "absolute path leaked: {raw_path}"
            );
        }
        assert!(sanitized.contains("module=ccs::session"));
        assert!(sanitized.contains("done"));
    }

    #[test]
    fn test_sanitize_log_message_redacts_colon_and_windows_path_formats() {
        let input = concat!(
            "path:/tmp/project/session.jsonl ",
            "file:C:/Users/Name/project/session.jsonl ",
            r#"file:C:\Users\Name\project\session.jsonl "#,
            r#"unc=\\server\share\project\session.jsonl "#,
            "quoted=\"C:/Users/Name/project/session.jsonl\""
        );
        let sanitized = sanitize_log_message(input, Some(Path::new("/Users/example")));

        for raw_path in [
            "/tmp/project/session.jsonl",
            "C:/Users/Name/project/session.jsonl",
            r"C:\Users\Name\project\session.jsonl",
            r"\\server\share\project\session.jsonl",
        ] {
            assert!(
                !sanitized.contains(raw_path),
                "absolute path leaked: {raw_path}"
            );
        }
        assert!(sanitized.contains("path:<path>"));
        assert!(sanitized.contains("file:<path>"));
    }

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

    #[test]
    fn test_sanitize_log_message_redacts_extended_credentials_and_authorization() {
        let input = concat!(
            "access_token=abc client_secret: abc apiKey=abc ",
            "\"token\":\"abc\" 'refresh-token' = 'abc' ",
            "Authorization: Bearer abc Authorization: Basic abc"
        );
        let sanitized = sanitize_log_message(input, None);

        for original in [
            "access_token=abc",
            "client_secret: abc",
            "apiKey=abc",
            "\"token\":\"abc\"",
            "Authorization: Bearer abc",
            "Authorization: Basic abc",
        ] {
            assert!(
                !sanitized.contains(original),
                "credential leaked in sanitized output: {original}; output={sanitized}"
            );
        }
        assert!(sanitized.contains("access_token=<redacted>"));
        assert!(sanitized.contains("client_secret: <redacted>"));
        assert!(sanitized.contains("apiKey=<redacted>"));
        assert!(sanitized.contains("\"token\":\"<redacted>\""));
        assert!(sanitized.contains("Authorization: Bearer <redacted>"));
        assert!(sanitized.contains("Authorization: Basic <redacted>"));
    }

    #[test]
    fn test_format_log_line_normalizes_multiline_messages() {
        let line = format_log_line(
            "2026-08-02T10:20:30Z",
            log::Level::Warn,
            "I-ABC123",
            "ccs::session",
            "first\r\nsecond\rthird\nfourth",
            None,
        );

        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
        assert!(line.contains("first\\nsecond\\nthird\\nfourth"));
    }

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

    #[test]
    #[serial]
    fn test_rotate_log_keeps_three_generations() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("ccs.log");

        std::fs::write(&path, b"current-over-limit")?;
        std::fs::write(path.with_extension("log.1"), b"one")?;
        std::fs::write(path.with_extension("log.2"), b"two")?;
        std::fs::write(path.with_extension("log.3"), b"three")?;

        assert!(rotate_log_at(&path, 4, 3)?);
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(path.with_extension("log.1"))?,
            b"current-over-limit"
        );
        assert_eq!(std::fs::read(path.with_extension("log.2"))?, b"one");
        assert_eq!(std::fs::read(path.with_extension("log.3"))?, b"two");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_rotate_log_skips_small_file() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("ccs.log");
        std::fs::write(&path, b"ok")?;
        assert!(!rotate_log_at(&path, 10, 3)?);
        assert_eq!(std::fs::read(&path)?, b"ok");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_init_logger_succeeds() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let config_dir = temp_dir.path().join("claude-code-sync");
        std::fs::create_dir_all(&config_dir)?;
        let _config_env = ConfigEnvGuard::new(&config_dir);

        // Should not panic or write outside the isolated test config directory.
        init_logger()?;
        let invocation_id =
            current_invocation_id().expect("logger initialization sets invocation ID");
        assert!(invocation_id.starts_with("I-"));
        assert_eq!(invocation_id.len(), 10);
        Ok(())
    }

    #[test]
    #[serial]
    #[allow(deprecated)]
    fn test_log_to_file() -> Result<()> {
        // Set up isolated test environment
        let temp_dir = tempfile::TempDir::new()?;
        let config_dir = temp_dir.path().join("claude-code-sync");
        std::fs::create_dir_all(&config_dir)?;
        let _config_env = ConfigEnvGuard::new(&config_dir);

        log_to_file("Test log message")?;

        let log_path = ConfigManager::log_file_path()?;
        assert!(log_path.exists());

        let contents = std::fs::read_to_string(&log_path)?;
        assert!(contents.contains("Test log message"));

        Ok(())
    }

    #[test]
    #[serial]
    fn test_rotate_log_creates_backup() -> Result<()> {
        // Set up isolated test environment
        let temp_dir = tempfile::TempDir::new()?;
        let config_dir = temp_dir.path().join("claude-code-sync");
        std::fs::create_dir_all(&config_dir)?;
        let _config_env = ConfigEnvGuard::new(&config_dir);

        // Create a large log file
        let log_path = ConfigManager::log_file_path()?;
        let mut file = File::create(&log_path)?;

        // Write 11MB of data
        let data = vec![b'a'; 11 * 1024 * 1024];
        file.write_all(&data)?;
        drop(file);

        // Rotate
        rotate_log_if_needed()?;

        // Check that the first rotated generation was created
        let old_log_path = log_path.with_extension("log.1");
        assert!(old_log_path.exists());

        // Original log should be fresh (or not exist)
        if log_path.exists() {
            let metadata = std::fs::metadata(&log_path)?;
            assert!(metadata.len() < 11 * 1024 * 1024);
        }

        Ok(())
    }

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

    #[test]
    fn test_dual_logger_redacts_absolute_paths_for_ordinary_targets_in_both_sinks() {
        let console = SharedBuffer::default();
        let file = SharedBuffer::default();
        let logger = DualLogger::with_writers(
            LevelFilter::Debug,
            LevelFilter::Info,
            "I-PATH0001".to_string(),
            Some(PathBuf::from("/Users/example")),
            Box::new(console.clone()),
            Some(Box::new(file.clone())),
        );

        let record = log::Record::builder()
            .level(log::Level::Warn)
            .target("path:/var/folders/abc/project/session.jsonl")
            .args(format_args!(
                "path:/tmp/project/session.jsonl quoted=\"/tmp/project with spaces/session.jsonl\" windows=C:/Users/Name/project/session.jsonl unc=\\\\server\\share\\project\\session.jsonl"
            ))
            .build();
        logger.log(&record);

        for sink in [console, file] {
            let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
            assert!(!text.contains("/var/folders/abc/project/session.jsonl"));
            assert!(!text.contains("/tmp/project with spaces/session.jsonl"));
            assert!(!text.contains(r"C:\Users\Name\project\session.jsonl"));
            assert!(text.contains("target=path:<path>"));
        }
    }

    #[test]
    fn test_scan_diagnostics_warning_stays_in_file_but_not_console() {
        let console = SharedBuffer::default();
        let file = SharedBuffer::default();
        let logger = DualLogger::with_writers(
            LevelFilter::Debug,
            LevelFilter::Info,
            "I-TEST123".to_string(),
            None,
            Box::new(console.clone()),
            Some(Box::new(file.clone())),
        );

        let record = log::Record::builder()
            .level(log::Level::Warn)
            .target(SCAN_DIAGNOSTICS_TARGET)
            .args(format_args!("session scan warning"))
            .build();
        logger.log(&record);

        assert!(console.0.lock().unwrap().is_empty());
        let file_text = String::from_utf8(file.0.lock().unwrap().clone()).unwrap();
        assert!(file_text.contains("session scan warning"));
        assert!(file_text.contains(SCAN_DIAGNOSTICS_TARGET));
    }

    #[test]
    fn logger_bounds_legacy_scan_warning_records_per_invocation() {
        let console = SharedBuffer::default();
        let file = SharedBuffer::default();
        let logger = DualLogger::with_writers(
            LevelFilter::Off,
            LevelFilter::Info,
            "I-BUDGET01".to_string(),
            None,
            Box::new(console),
            Some(Box::new(file.clone())),
        );

        for _ in 0..(MAX_SCAN_WARNING_RECORDS + 5) {
            let record = log::Record::builder()
                .level(log::Level::Warn)
                .target(SCAN_DIAGNOSTICS_TARGET)
                .args(format_args!("legacy warning"))
                .build();
            logger.log(&record);
        }

        let text = String::from_utf8(file.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            text.matches("target=ccs::scan_diagnostics legacy warning")
                .count(),
            MAX_SCAN_WARNING_RECORDS
        );
        assert_eq!(text.matches(SCAN_WARNINGS_SUPPRESSED_MESSAGE).count(), 1);
    }

    #[test]
    fn test_dual_logger_respects_independent_levels() {
        let console = SharedBuffer::default();
        let file = SharedBuffer::default();
        let logger = DualLogger::with_writers(
            LevelFilter::Debug,
            LevelFilter::Info,
            "I-TEST123".to_string(),
            None,
            Box::new(console.clone()),
            Some(Box::new(file.clone())),
        );

        let record = log::Record::builder()
            .level(log::Level::Debug)
            .target("ccs::test")
            .args(format_args!("debug message"))
            .build();
        logger.log(&record);

        assert!(!console.0.lock().unwrap().is_empty());
        assert!(file.0.lock().unwrap().is_empty());
    }

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

    #[test]
    #[cfg(unix)]
    fn test_open_log_file_secures_current_and_rotated_generations() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("secure.log");
        let backup_one = rotated_path(&path, 1);
        let backup_two = rotated_path(&path, 2);
        std::fs::write(&path, vec![b'c'; (MAX_LOG_SIZE + 1) as usize])?;
        std::fs::write(&backup_one, b"one")?;
        std::fs::write(&backup_two, b"two")?;
        for candidate in [&path, &backup_one, &backup_two] {
            std::fs::set_permissions(candidate, std::fs::Permissions::from_mode(0o644))?;
        }

        let _file = open_log_file(&path)?;

        for generation in 0..=3 {
            let candidate = if generation == 0 {
                path.clone()
            } else {
                rotated_path(&path, generation)
            };
            assert!(
                candidate.exists(),
                "missing log generation {}",
                candidate.display()
            );
            assert_eq!(
                std::fs::metadata(candidate)?.permissions().mode() & 0o777,
                0o600
            );
        }
        Ok(())
    }

    #[test]
    fn test_build_logger_degrades_when_file_open_fails() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("not-a-file");
        std::fs::create_dir(&path)?;
        let options = LoggerOptions::new(false, Some(path.clone()), None)?;

        let (logger, warning) = build_logger(&options)?;
        assert!(logger.file.is_none());
        let warning = warning.expect("fallback warning");
        assert!(warning.contains("file logging unavailable"));
        assert!(!warning.contains(path.to_string_lossy().as_ref()));
        assert!(!warning.contains("Is a directory"));
        Ok(())
    }

    #[test]
    #[serial]
    fn rotation_failure_rolls_back_current_and_generations() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("ccs.log");
        std::fs::write(&path, b"current")?;
        std::fs::write(rotated_path(&path, 1), b"one")?;
        std::fs::write(rotated_path(&path, 2), b"two")?;
        std::fs::write(rotated_path(&path, 3), b"three")?;

        let _failure_guard = rotation_failure_guard(Some(10));
        let result = rotate_log_at(&path, 1, 3);

        assert!(result.is_err());
        assert_eq!(std::fs::read(&path)?, b"current");
        assert_eq!(std::fs::read(rotated_path(&path, 1))?, b"one");
        assert_eq!(std::fs::read(rotated_path(&path, 2))?, b"two");
        assert_eq!(std::fs::read(rotated_path(&path, 3))?, b"three");
        Ok(())
    }

    #[test]
    fn sanitizer_redacts_file_urls_and_quoted_multiword_secrets() {
        let input = concat!(
            "file:///tmp/secret/session.jsonl ",
            "file://localhost/etc/passwd ",
            "file://server/share/private/session.jsonl ",
            "token='secret value' token=\"secret value\" ",
            "token=\"secret \\\"value\\\"\" ",
            "token=\"secret\nmultiline\""
        );
        let sanitized = sanitize_log_message(input, None);

        for raw in [
            "file:///tmp/secret/session.jsonl",
            "file://localhost/etc/passwd",
            "file://server/share/private/session.jsonl",
            "secret value",
            "secret \\\"value\\\"",
            "secret\nmultiline",
        ] {
            assert!(
                !sanitized.contains(raw),
                "sanitized output leaked: {raw}; {sanitized}"
            );
        }
        assert!(sanitized.contains("file://"));
        assert!(sanitized.contains("file://server/<path>"));
        assert!(sanitized.contains("token='<redacted>'"));
        assert!(sanitized.contains("token=\"<redacted>\""));
    }

    #[test]
    fn sanitizer_redacts_complete_hosted_file_uri_paths_with_uri_punctuation() {
        let input = concat!(
            "file://server/share/private,secret/session.jsonl ",
            "file://server/share/private'quoted/session.jsonl ",
            "file://server/share/private]bracket/session.jsonl ",
            "file://server/share/private)paren/session.jsonl"
        );
        let sanitized = sanitize_log_message(input, None);

        for raw in [
            "file://server/share/private,secret/session.jsonl",
            "file://server/share/private'quoted/session.jsonl",
            "file://server/share/private]bracket/session.jsonl",
            "file://server/share/private)paren/session.jsonl",
        ] {
            assert!(
                !sanitized.contains(raw),
                "hosted URI path leaked: {raw}; output={sanitized}"
            );
        }
        assert_eq!(
            sanitized,
            "file://server/<path> file://server/<path> file://server/<path> file://server/<path>"
        );
    }

    #[test]
    fn copy_from_opened_log_handle_survives_source_symlink_swap() -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let temp = tempfile::tempdir()?;
            let source = temp.path().join("source.log");
            let external = temp.path().join("external.log");
            let staged = temp.path().join("staged.log");
            std::fs::write(&source, b"trusted-source")?;
            std::fs::write(&external, b"attacker-source")?;

            let source_handle = open_existing_no_follow(&source)?;
            std::fs::remove_file(&source)?;
            symlink(&external, &source)?;

            copy_from_opened_log_handle(&source_handle, &staged)?;
            assert_eq!(std::fs::read(&staged)?, b"trusted-source");
            assert_eq!(std::fs::read(&external)?, b"attacker-source");
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn rotation_failure_keeps_transaction_when_rollback_also_fails() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("ccs.log");
        std::fs::write(&path, b"current")?;
        std::fs::write(rotated_path(&path, 1), b"one")?;
        std::fs::write(rotated_path(&path, 2), b"two")?;
        std::fs::write(rotated_path(&path, 3), b"three")?;

        let _failure_guard = rotation_failure_guard_with_rollback_failure(Some(5));
        let result = rotate_log_at(&path, 1, 3);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rollback failed"));

        let recoverable = std::fs::read_dir(temp.path())?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .flat_map(|entry| std::fs::read_dir(entry.path()).into_iter().flatten())
            .filter_map(Result::ok)
            .any(|entry| {
                std::fs::read(entry.path())
                    .map(|bytes| bytes == b"current")
                    .unwrap_or(false)
            });
        assert!(recoverable, "rollback failure discarded transaction data");
        Ok(())
    }

    #[test]
    fn format_log_line_rejects_control_injection_from_invocation_id() {
        let line = format_log_line(
            "2026-08-02T10:20:30Z\nFAKE_TIME",
            log::Level::Warn,
            "I-GOOD\nFAKE\rtarget=evil",
            "ccs::test",
            "safe",
            None,
        );

        assert_eq!(line.matches('\n').count(), 1);
        assert!(!line.contains("\nFAKE"));
        assert!(!line.contains("\rtarget=evil"));
    }

    #[test]
    fn invocation_id_validation_rejects_control_and_accepts_legacy_safe_ids() {
        assert!(validate_invocation_id("I-GOOD1234").is_ok());
        assert!(validate_invocation_id("I-ROOT-claude").is_ok());
        assert!(validate_invocation_id("I-GOOD\nFAKE").is_err());
        assert!(validate_invocation_id("not-an-invocation").is_err());
    }

    #[test]
    fn public_logger_options_reject_control_injection_at_init_boundary() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut options = LoggerOptions::new(false, Some(temp.path().join("ccs.log")), None)?;
        options.invocation_id = "I-OK\nforged=1".to_string();
        assert!(init_logger_with_options(options).is_err());
        Ok(())
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test failure",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test failure",
            ))
        }
    }

    #[test]
    fn sink_write_and_flush_failures_are_counted_and_reported_once() {
        let logger = DualLogger::with_writers(
            LevelFilter::Debug,
            LevelFilter::Debug,
            "I-TEST123".to_string(),
            None,
            Box::new(FailingWriter),
            Some(Box::new(FailingWriter)),
        );
        let record = log::Record::builder()
            .level(log::Level::Warn)
            .target("ccs::test")
            .args(format_args!("safe"))
            .build();

        logger.log(&record);
        logger.log(&record);
        logger.flush();
        let status = logger.sink_status();
        assert!(status.console.write_failures >= 2);
        assert!(status.console.flush_failures >= 1);
        assert!(status.file.write_failures >= 2);
        assert!(status.file.flush_failures >= 1);
        assert_eq!(status.console.fallback_reports, 1);
        assert_eq!(status.file.fallback_reports, 1);
    }

    #[test]
    fn poisoned_sink_mutex_is_reported_without_panicking() {
        let logger = DualLogger::with_writers(
            LevelFilter::Debug,
            LevelFilter::Off,
            "I-TEST123".to_string(),
            None,
            Box::new(SharedBuffer::default()),
            None::<Box<std::io::Sink>>,
        );
        let logger_ref = &logger;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = logger_ref.console.lock().unwrap();
            panic!("poison test");
        }));

        let record = log::Record::builder()
            .level(log::Level::Warn)
            .target("ccs::test")
            .args(format_args!("safe"))
            .build();
        logger.log(&record);
        assert_eq!(logger.sink_status().console.poisoned, 1);
    }

    #[cfg(unix)]
    #[test]
    fn open_log_file_rejects_symlink_without_touching_target() -> Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("outside.log");
        let link = temp.path().join("ccs.log");
        std::fs::write(&target, b"outside")?;
        symlink(&target, &link)?;

        assert!(open_log_file(&link).is_err());
        assert_eq!(std::fs::read(&target)?, b"outside");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn open_log_file_rejects_symlink_lock_and_generation() -> Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("outside.log");
        std::fs::write(&target, b"outside")?;

        let lock_path = temp.path().join("ccs.log.lock");
        symlink(&target, &lock_path)?;
        assert!(open_log_file(&temp.path().join("ccs.log")).is_err());
        std::fs::remove_file(&lock_path)?;

        let path = temp.path().join("ccs.log");
        std::fs::write(&path, vec![b'x'; (MAX_LOG_SIZE + 1) as usize])?;
        let generation = rotated_path(&path, 1);
        symlink(&target, &generation)?;
        assert!(open_log_file(&path).is_err());
        assert_eq!(std::fs::read(&target)?, b"outside");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unix_log_open_uses_no_follow_flag() {
        const { assert!(NO_FOLLOW_FLAG != 0) };
    }

    #[cfg(windows)]
    #[test]
    fn windows_log_open_uses_reparse_point_open_flag() {
        assert_eq!(OPEN_REPARSE_POINT_FLAG, 0x00200000);
    }

    #[test]
    fn legacy_warning_text_is_fixed_and_safe() {
        assert_eq!(
            file_sink_warning(),
            "WARNING: file logging unavailable; continuing with stderr logs"
        );
    }
}
