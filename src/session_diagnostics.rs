use anyhow::Error as AnyhowError;
use regex::Regex;
use serde::{ser::SerializeStruct, Serialize, Serializer};
use std::fmt;
use std::io::ErrorKind as IoErrorKind;
use std::path::Path;
use std::sync::OnceLock;
use uuid::Uuid;

#[cfg(test)]
use std::cell::RefCell;

pub const SCAN_DIAGNOSTICS_SCHEMA_VERSION: u32 = 1;
pub const MAX_SCAN_WARNINGS: usize = 100;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanWarningCategory {
    Io,
    Data,
    Cache,
}

/// Stable, privacy-safe classification for a scan warning.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanWarningErrorKind {
    PermissionDenied,
    NotFound,
    InvalidData,
    ReadFailed,
    ChangedDuringRead,
    Unknown,
}

/// Marker used when a file changes between cache revalidation reads.
#[derive(Debug, Clone, Copy)]
pub struct ChangedDuringRead;

impl fmt::Display for ChangedDuringRead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("session file changed during read")
    }
}

impl std::error::Error for ChangedDuringRead {}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScanWarning {
    pub source: Option<String>,
    pub operation: String,
    pub category: ScanWarningCategory,
    pub error_kind: ScanWarningErrorKind,
    pub path_hash: Option<String>,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct ScanDiagnostics {
    pub schema_version: u32,
    pub diagnostic_id: String,
    pub files_seen: usize,
    pub files_parsed: usize,
    pub files_skipped: usize,
    pub malformed_files: usize,
    pub io_errors: usize,
    pub cache_errors: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub bytes_considered: u64,
    pub elapsed_ms: u64,
    pub source_discovery_ms: u64,
    pub metadata_ms: u64,
    pub cache_load_ms: u64,
    pub cache_save_ms: u64,
    pub parse_ms: u64,
    pub search_load_ms: u64,
    pub claude_scan_ms: u64,
    pub codex_scan_ms: u64,
    pub omp_scan_ms: u64,
    pub fingerprint_ms: u64,
    pub fingerprinted_bytes: u64,
    pub parsed_bytes: u64,
    pub warnings: Vec<ScanWarning>,
    pub suppressed_warnings: usize,
}

impl Serialize for ScanDiagnostics {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ScanDiagnostics", 27)?;
        state.serialize_field("schema_version", &self.schema_version)?;
        state.serialize_field("diagnostic_id", &self.diagnostic_id)?;
        state.serialize_field("files_seen", &self.files_seen)?;
        state.serialize_field("files_parsed", &self.files_parsed)?;
        state.serialize_field("files_skipped", &self.files_skipped)?;
        state.serialize_field("malformed_files", &self.malformed_files)?;
        state.serialize_field("io_errors", &self.io_errors)?;
        state.serialize_field("cache_errors", &self.cache_errors)?;
        state.serialize_field("cache_hits", &self.cache_hits)?;
        state.serialize_field("cache_misses", &self.cache_misses)?;
        state.serialize_field("bytes_considered", &self.bytes_considered)?;
        state.serialize_field("elapsed_ms", &self.elapsed_ms)?;
        state.serialize_field("source_discovery_ms", &self.source_discovery_ms)?;
        state.serialize_field("metadata_ms", &self.metadata_ms)?;
        state.serialize_field("cache_load_ms", &self.cache_load_ms)?;
        state.serialize_field("cache_save_ms", &self.cache_save_ms)?;
        state.serialize_field("parse_ms", &self.parse_ms)?;
        state.serialize_field("search_load_ms", &self.search_load_ms)?;
        state.serialize_field("claude_scan_ms", &self.claude_scan_ms)?;
        state.serialize_field("codex_scan_ms", &self.codex_scan_ms)?;
        state.serialize_field("omp_scan_ms", &self.omp_scan_ms)?;
        state.serialize_field("fingerprint_ms", &self.fingerprint_ms)?;
        state.serialize_field("fingerprinted_bytes", &self.fingerprinted_bytes)?;
        state.serialize_field("parsed_bytes", &self.parsed_bytes)?;
        state.serialize_field("warnings", &self.warnings)?;
        state.serialize_field("suppressed_warnings", &self.suppressed_warnings)?;
        state.serialize_field("degraded", &self.degraded())?;
        state.end()
    }
}

impl ScanDiagnostics {
    pub fn new() -> Self {
        let diagnostic_id = crate::logger::current_invocation_id()
            .map(crate::logger::safe_invocation_id)
            .unwrap_or_else(|| {
                let uuid = Uuid::new_v4().simple().to_string();
                crate::logger::safe_invocation_id(&format!("I-{}", uuid[..8].to_uppercase()))
            });
        Self::with_id(diagnostic_id)
    }

    pub fn with_id(diagnostic_id: impl Into<String>) -> Self {
        let diagnostic_id = crate::logger::safe_invocation_id(&diagnostic_id.into());
        Self {
            schema_version: SCAN_DIAGNOSTICS_SCHEMA_VERSION,
            diagnostic_id,
            files_seen: 0,
            files_parsed: 0,
            files_skipped: 0,
            malformed_files: 0,
            io_errors: 0,
            cache_errors: 0,
            cache_hits: 0,
            cache_misses: 0,
            bytes_considered: 0,
            elapsed_ms: 0,
            source_discovery_ms: 0,
            metadata_ms: 0,
            cache_load_ms: 0,
            cache_save_ms: 0,
            parse_ms: 0,
            search_load_ms: 0,
            claude_scan_ms: 0,
            codex_scan_ms: 0,
            omp_scan_ms: 0,
            fingerprint_ms: 0,
            fingerprinted_bytes: 0,
            parsed_bytes: 0,
            warnings: Vec::new(),
            suppressed_warnings: 0,
        }
    }

    pub fn record_warning(
        &mut self,
        source: Option<&str>,
        operation: &str,
        category: ScanWarningCategory,
        path: Option<&Path>,
        error: &str,
    ) {
        self.record_warning_with_kind(
            source,
            operation,
            category,
            default_error_kind(category),
            path,
            error,
        );
    }

    /// Record a warning while deriving a controlled kind from an anyhow error chain.
    pub fn record_warning_from_error(
        &mut self,
        source: Option<&str>,
        operation: &str,
        category: ScanWarningCategory,
        path: Option<&Path>,
        error: &AnyhowError,
    ) {
        let kind = error_kind_from_error(error);
        let error_text = format!("{error:#}");
        self.record_warning_with_kind(source, operation, category, kind, path, &error_text);
    }

    /// Record a warning with an explicitly controlled kind.
    pub fn record_warning_with_kind(
        &mut self,
        source: Option<&str>,
        operation: &str,
        category: ScanWarningCategory,
        error_kind: ScanWarningErrorKind,
        path: Option<&Path>,
        error: &str,
    ) {
        match category {
            ScanWarningCategory::Io => self.io_errors += 1,
            ScanWarningCategory::Data => self.malformed_files += 1,
            ScanWarningCategory::Cache => self.cache_errors += 1,
        }

        let path_hash = path.map(stable_path_hash);
        let source = normalize_source(source);
        let operation = normalize_operation(operation);
        let error = safe_error_summary(category, error_kind, error, path);
        let warning = ScanWarning {
            source: source.clone(),
            operation: operation.clone(),
            category,
            error_kind,
            path_hash: path_hash.clone(),
            error: error.clone(),
        };

        if self.warnings.len() < MAX_SCAN_WARNINGS {
            self.warnings.push(warning);
            let log_message = format_warning_log(
                source.as_deref(),
                &operation,
                category,
                path_hash.as_deref(),
                &error,
            );
            emit_warning_log(&log_message, false);
        } else {
            self.suppressed_warnings += 1;
            if self.suppressed_warnings == 1 {
                emit_warning_log(crate::logger::SCAN_WARNINGS_SUPPRESSED_MESSAGE, true);
            }
        }
    }

    pub fn degraded(&self) -> bool {
        self.malformed_files > 0
            || self.io_errors > 0
            || self.cache_errors > 0
            || self.suppressed_warnings > 0
    }

    pub fn summary_line(&self) -> String {
        format!(
            "Session scan incomplete: {} malformed, {} I/O, {} cache error. Diagnostic ID: {}",
            self.malformed_files, self.io_errors, self.cache_errors, self.diagnostic_id
        )
    }
}

impl Default for ScanDiagnostics {
    fn default() -> Self {
        Self::new()
    }
}

pub fn stable_path_hash(path: &Path) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("p-{hash:016x}")
}

fn normalize_source(source: Option<&str>) -> Option<String> {
    source.map(|value| match value {
        "claude" | "codex" | "omp" => value.to_string(),
        _ => "unknown".to_string(),
    })
}

fn normalize_operation(operation: &str) -> String {
    match operation {
        "parse" | "read" | "scan" | "cache" | "load" | "save" | "merge" | "decode"
        | "serialize" | "fingerprint" | "discover" | "stat" | "open" | "read_dir" | "metadata" => {
            operation.to_string()
        }
        _ => "unknown".to_string(),
    }
}

fn category_label(category: ScanWarningCategory) -> &'static str {
    match category {
        ScanWarningCategory::Io => "io",
        ScanWarningCategory::Data => "data",
        ScanWarningCategory::Cache => "cache",
    }
}

fn default_error_kind(category: ScanWarningCategory) -> ScanWarningErrorKind {
    match category {
        ScanWarningCategory::Io => ScanWarningErrorKind::ReadFailed,
        ScanWarningCategory::Data => ScanWarningErrorKind::InvalidData,
        ScanWarningCategory::Cache => ScanWarningErrorKind::Unknown,
    }
}

/// Derive a stable kind from an anyhow error without exposing its text.
pub fn error_kind_from_error(error: &AnyhowError) -> ScanWarningErrorKind {
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<ChangedDuringRead>().is_some())
    {
        return ScanWarningErrorKind::ChangedDuringRead;
    }

    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .map(|io_error| match io_error.kind() {
            IoErrorKind::PermissionDenied => ScanWarningErrorKind::PermissionDenied,
            IoErrorKind::NotFound => ScanWarningErrorKind::NotFound,
            IoErrorKind::InvalidData => ScanWarningErrorKind::InvalidData,
            _ => ScanWarningErrorKind::ReadFailed,
        })
        .unwrap_or(ScanWarningErrorKind::Unknown)
}

fn error_kind_label(kind: ScanWarningErrorKind) -> &'static str {
    match kind {
        ScanWarningErrorKind::PermissionDenied => "permission_denied",
        ScanWarningErrorKind::NotFound => "not_found",
        ScanWarningErrorKind::InvalidData => "invalid_data",
        ScanWarningErrorKind::ReadFailed => "read_failed",
        ScanWarningErrorKind::ChangedDuringRead => "changed_during_read",
        ScanWarningErrorKind::Unknown => "unknown",
    }
}

fn stable_detail_hash(value: &str) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("d-{hash:016x}")
}

fn extract_line_column(value: &str) -> Option<(u32, u32)> {
    static LINE_COLUMN_RE: OnceLock<Regex> = OnceLock::new();
    let captures = LINE_COLUMN_RE
        .get_or_init(|| {
            Regex::new(r"(?i)\bline\s+(\d{1,10})\b[^\d\n]{1,30}\bcolumn\s+(\d{1,10})\b")
                .expect("line and column pattern must be valid")
        })
        .captures(value)?;
    Some((
        captures.get(1)?.as_str().parse().ok()?,
        captures.get(2)?.as_str().parse().ok()?,
    ))
}

fn safe_error_summary(
    category: ScanWarningCategory,
    error_kind: ScanWarningErrorKind,
    error: &str,
    path: Option<&Path>,
) -> String {
    let detail_hash = stable_detail_hash(error);
    let replaced = match path {
        Some(path) => error.replace(path.to_string_lossy().as_ref(), "<path>"),
        None => error.to_string(),
    };
    let sanitized = crate::logger::sanitize_log_message(&replaced, dirs::home_dir().as_deref());
    let mut summary = format!(
        "{} {} detail_hash={detail_hash}",
        category_label(category),
        error_kind_label(error_kind)
    );
    if let Some((line, column)) = extract_line_column(&sanitized) {
        summary.push_str(&format!(" line={line} column={column}"));
    }
    summary
}

fn emit_warning_log(message: &str, suppressed: bool) {
    #[cfg(not(test))]
    let _ = suppressed;
    #[cfg(test)]
    WARNING_LOG_CAPTURE.with(|capture| {
        if let Some(capture) = capture.borrow_mut().as_mut() {
            if suppressed {
                capture.suppressed_logs += 1;
            } else {
                capture.detail_logs += 1;
            }
        }
    });
    log::warn!(target: crate::logger::SCAN_DIAGNOSTICS_TARGET, "{message}");
}

#[cfg(test)]
#[derive(Clone, Copy, Default)]
struct WarningLogCapture {
    detail_logs: usize,
    suppressed_logs: usize,
}

#[cfg(test)]
thread_local! {
    static WARNING_LOG_CAPTURE: RefCell<Option<WarningLogCapture>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn reset_warning_log_capture_for_test() {
    WARNING_LOG_CAPTURE.with(|capture| *capture.borrow_mut() = Some(WarningLogCapture::default()));
}

#[cfg(test)]
fn warning_log_capture_for_test() -> WarningLogCapture {
    WARNING_LOG_CAPTURE.with(|capture| capture.borrow().unwrap_or_default())
}

pub(crate) fn legacy_walk_entry(
    result: Result<walkdir::DirEntry, walkdir::Error>,
    source: &str,
) -> Option<walkdir::DirEntry> {
    match result {
        Ok(entry) => Some(entry),
        Err(error) => {
            let error = AnyhowError::new(error);
            log::warn!(
                target: crate::logger::SCAN_DIAGNOSTICS_TARGET,
                "legacy {source} session discovery skipped an unreadable directory entry error_kind={}",
                error_kind_label(error_kind_from_error(&error))
            );
            None
        }
    }
}

pub(crate) fn legacy_io_warning(source: &str, operation: &str) {
    log::warn!(
        target: crate::logger::SCAN_DIAGNOSTICS_TARGET,
        "legacy {source} session discovery degraded during {operation} error_kind=unknown"
    );
}

pub(crate) fn legacy_io_warning_from_error(source: &str, operation: &str, error: &AnyhowError) {
    log::warn!(
        target: crate::logger::SCAN_DIAGNOSTICS_TARGET,
        "legacy {source} session discovery degraded during {operation} error_kind={}",
        error_kind_label(error_kind_from_error(error))
    );
}

fn format_warning_log(
    source: Option<&str>,
    operation: &str,
    category: ScanWarningCategory,
    path_hash: Option<&str>,
    error: &str,
) -> String {
    format!(
        "session scan warning source={} operation={operation} category={} path_hash={} error={error}",
        source.unwrap_or("none"),
        category_label(category),
        path_hash.unwrap_or("none"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_walk_entry_helper_handles_real_walkdir_errors() {
        let missing = tempfile::tempdir().unwrap().path().join("missing");
        let result = walkdir::WalkDir::new(&missing).into_iter().next().unwrap();
        assert!(legacy_walk_entry(result, "legacy-test").is_none());

        let valid = tempfile::tempdir().unwrap();
        let result = walkdir::WalkDir::new(valid.path())
            .into_iter()
            .next()
            .unwrap();
        assert!(legacy_walk_entry(result, "legacy-test").is_some());
    }

    #[test]
    fn stable_hash_is_deterministic_and_hides_path() {
        let path = Path::new("/Users/example/private/project/session.jsonl");
        let first = stable_path_hash(path);
        let second = stable_path_hash(path);
        assert_eq!(first, second);
        assert!(first.starts_with("p-"));
        assert_eq!(first.len(), 18);
        assert!(!first.contains("example"));
        assert_ne!(
            first,
            stable_path_hash(Path::new("/Users/example/other.jsonl"))
        );
    }

    #[test]
    fn diagnostics_json_always_exposes_computed_degraded() {
        let clean = ScanDiagnostics::with_id("I-CLEAN0001");
        let clean_json = serde_json::to_value(&clean).unwrap();
        assert_eq!(clean_json["degraded"], false);

        let mut degraded = ScanDiagnostics::with_id("I-DEGRAD01");
        degraded.malformed_files = 1;
        let degraded_json = serde_json::to_value(&degraded).unwrap();
        assert_eq!(degraded_json["degraded"], true);
    }

    #[test]
    fn diagnostics_caps_warning_details_and_counts_suppressed() {
        let mut diagnostics = ScanDiagnostics::with_id("I-TEST0001");
        for index in 0..105 {
            diagnostics.record_warning(
                Some("claude"),
                "parse",
                ScanWarningCategory::Data,
                Some(Path::new(&format!("/private/{index}.jsonl"))),
                "invalid JSON in /private/file.jsonl token=secret",
            );
        }

        assert_eq!(diagnostics.warnings.len(), 100);
        assert_eq!(diagnostics.suppressed_warnings, 5);
        assert!(diagnostics.degraded());
        let json = serde_json::to_value(&diagnostics).unwrap();
        assert_eq!(json["diagnostic_id"], "I-TEST0001");
        assert_eq!(json["suppressed_warnings"], 5);
        let text = serde_json::to_string(&diagnostics).unwrap();
        assert!(!text.contains("/private/"));
        assert!(!text.contains("token=secret"));
    }

    #[test]
    fn diagnostics_normalizes_untrusted_ids_without_control_characters() {
        let diagnostics = ScanDiagnostics::with_id("I-VALID\\nspoof=1\\rnext");
        assert!(!diagnostics.diagnostic_id.contains(['\n', '\r']));
        assert!(diagnostics.diagnostic_id.starts_with("I-"));
    }

    #[test]
    fn diagnostics_emits_one_fixed_log_after_warning_detail_cap() {
        reset_warning_log_capture_for_test();
        let mut diagnostics = ScanDiagnostics::with_id("I-TEST0001");
        for index in 0..(MAX_SCAN_WARNINGS + 3) {
            diagnostics.record_warning(
                Some("claude"),
                "parse",
                ScanWarningCategory::Data,
                Some(Path::new(&format!("/private/{index}.jsonl"))),
                "invalid JSON",
            );
        }

        let capture = warning_log_capture_for_test();
        assert_eq!(capture.detail_logs, MAX_SCAN_WARNINGS);
        assert_eq!(capture.suppressed_logs, 1);
        assert_eq!(diagnostics.suppressed_warnings, 3);
    }

    #[test]
    fn diagnostics_json_exposes_phase_metrics() {
        let diagnostics = ScanDiagnostics::with_id("I-METRIC01");
        let json = serde_json::to_value(&diagnostics).unwrap();
        for field in [
            "source_discovery_ms",
            "metadata_ms",
            "cache_load_ms",
            "cache_save_ms",
            "parse_ms",
            "search_load_ms",
            "claude_scan_ms",
            "codex_scan_ms",
            "omp_scan_ms",
            "fingerprint_ms",
            "fingerprinted_bytes",
            "parsed_bytes",
        ] {
            assert!(json.get(field).is_some(), "missing metric {field}");
        }
    }

    #[test]
    fn new_generates_invocation_id_without_logger_initialization() {
        let diagnostics = ScanDiagnostics::new();
        assert!(diagnostics.diagnostic_id.starts_with("I-"));
        assert_eq!(diagnostics.diagnostic_id.len(), 10);
    }

    #[test]
    fn warning_security_summary_never_retains_untrusted_fields() {
        let mut diagnostics = ScanDiagnostics::with_id("I-TEST0003");
        let malicious_source = "claude\nCookie=session=secret; PRIVATE KEY";
        let malicious_operation = "parse /Users/alice/private C:\\Users\\alice\\secret.jsonl\n";
        let raw_error = concat!(
            "invalid JSON at line 12 column 34: ",
            "Cookie=session=secret; ",
            "-----BEGIN PRIVATE KEY----- ",
            "/Users/alice/private/session.jsonl ",
            "C:\\Users\\alice\\private\\session.jsonl"
        );
        diagnostics.record_warning(
            Some(malicious_source),
            malicious_operation,
            ScanWarningCategory::Data,
            Some(Path::new("C:\\Users\\alice\\private\\session.jsonl")),
            raw_error,
        );

        let warning = &diagnostics.warnings[0];
        assert_eq!(warning.source.as_deref(), Some("unknown"));
        assert_eq!(warning.operation, "unknown");
        assert!(warning
            .error
            .starts_with("data invalid_data detail_hash=d-"));
        assert!(warning.error.contains("line=12 column=34"));
        let serialized = serde_json::to_string(&diagnostics).unwrap();
        let log_message = format_warning_log(
            warning.source.as_deref(),
            &warning.operation,
            warning.category,
            warning.path_hash.as_deref(),
            &warning.error,
        );
        for raw in [
            malicious_source,
            malicious_operation,
            raw_error,
            "/Users/alice/private/session.jsonl",
            r"C:\Users\alice\private\session.jsonl",
            "Cookie=session=secret",
            "PRIVATE KEY",
        ] {
            assert!(!serialized.contains(raw), "serialized leak: {raw}");
            assert!(!log_message.contains(raw), "log leak: {raw}");
        }
        assert!(!log_message.contains("\n"));
    }

    #[test]
    fn warning_detail_hash_is_stable_without_retaining_error_text() {
        let first = safe_error_summary(
            ScanWarningCategory::Io,
            ScanWarningErrorKind::ReadFailed,
            "read failed at line 7 column 9: cookie=secret",
            None,
        );
        let second = safe_error_summary(
            ScanWarningCategory::Io,
            ScanWarningErrorKind::ReadFailed,
            "read failed at line 7 column 9: cookie=secret",
            None,
        );
        assert_eq!(first, second);
        assert!(first.starts_with("io read_failed detail_hash=d-"));
        assert!(first.contains("line=7 column=9"));
        assert!(!first.contains("cookie=secret"));
    }

    #[test]
    fn warning_labels_allow_known_values_only() {
        assert_eq!(normalize_source(Some("claude")), Some("claude".to_string()));
        assert_eq!(normalize_source(Some("codex")), Some("codex".to_string()));
        assert_eq!(normalize_source(Some("omp")), Some("omp".to_string()));
        assert_eq!(normalize_source(None), None);
        assert_eq!(
            normalize_source(Some("claude\nsecret")),
            Some("unknown".to_string())
        );
        assert_eq!(normalize_operation("parse"), "parse");
        assert_eq!(normalize_operation("merge"), "merge");
        assert_eq!(normalize_operation("parse\nprivate-key"), "unknown");
    }

    #[test]
    fn summary_line_is_compact_and_path_free() {
        let mut diagnostics = ScanDiagnostics::with_id("I-TEST0002");
        diagnostics.malformed_files = 2;
        diagnostics.io_errors = 3;
        diagnostics.cache_errors = 1;
        assert_eq!(
            diagnostics.summary_line(),
            "Session scan incomplete: 2 malformed, 3 I/O, 1 cache error. Diagnostic ID: I-TEST0002"
        );
    }

    #[test]
    fn warning_preserves_controlled_error_kind_without_raw_error_text() {
        let mut diagnostics = ScanDiagnostics::with_id("I-KIND0001");
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private path /Users/alice/secret token=abc",
        ));

        diagnostics.record_warning_from_error(
            Some("claude"),
            "parse",
            ScanWarningCategory::Io,
            Some(Path::new("/Users/alice/private.jsonl")),
            &error,
        );

        let warning = &diagnostics.warnings[0];
        assert_eq!(warning.error_kind, ScanWarningErrorKind::PermissionDenied);
        assert!(warning
            .error
            .starts_with("io permission_denied detail_hash=d-"));
        let serialized = serde_json::to_string(&diagnostics).unwrap();
        assert!(serialized.contains("permission_denied"));
        assert!(!serialized.contains("Permission denied"));
        assert!(!serialized.contains("/Users/alice"));
        assert!(!serialized.contains("token=abc"));
    }
}
