//! Read-only integration for Google Antigravity CLI (`agy`) session history.
//!
//! Reads metadata and FTS5 search indexes from `~/.gemini/antigravity-cli/conversation_summaries.db`
//! in WAL-safe read-only mode, and parses structured conversation transcripts from
//! `~/.gemini/antigravity-cli/brain/<conversation-id>/.system_generated/logs/transcript_full.jsonl`
//! (falling back to `transcript.jsonl`).

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::codex::{is_system_content, last_path_component};
use crate::parser::ParseOutcome;

/// Default filename for the Antigravity CLI SQLite summary and FTS5 index database.
pub const AGY_DB_FILENAME: &str = "conversation_summaries.db";

/// Metadata row loaded from `conversation_summaries` (optionally enriched from `conversations_fts`).
#[derive(Debug, Clone)]
pub struct AgyDbSummary {
    pub conversation_id: String,
    pub title: String,
    pub preview: String,
    pub step_count: usize,
    pub last_modified_time: Option<String>,
    pub cwd: Option<String>,
    pub project_name: String,
    pub parent_conversation_id: String,
    pub last_user_input_time: Option<String>,
    #[allow(dead_code)]
    pub archived: bool,
}

/// FTS5 pre-filter result for accelerating `ccs session search` over `agy` sessions.
#[derive(Debug, Clone, Default)]
pub struct AgyFtsPrefilter {
    /// Conversations that have indexed body chunks in `conversations_fts`, mapped to
    /// their indexed `last_modified_time` in Unix seconds.
    pub indexed_mtimes: HashMap<String, i64>,
    /// Conversation IDs matching at least one search keyword in `conversations_fts`.
    pub matched_ids: HashSet<String>,
}

impl AgyFtsPrefilter {
    /// Decide whether a session must be parsed during search.
    ///
    /// Returns `false` only when the session is confirmed to be freshly indexed in
    /// `conversations_fts` (its transcript file is not newer than the FTS index) and
    /// none of the search keywords matched it in SQLite.
    pub fn should_scan_session(&self, conversation_id: &str, file_path: &Path) -> bool {
        if self.matched_ids.contains(conversation_id) {
            return true;
        }
        let Some(&indexed_mtime_secs) = self.indexed_mtimes.get(conversation_id) else {
            // Not yet indexed in conversations_fts (e.g. brand-new or running session).
            return true;
        };
        if indexed_mtime_secs <= 0 {
            return true;
        }
        // DB-only sessions point at conversation_summaries.db itself; their content is
        // already represented in the DB.
        if is_agy_db_path(file_path) {
            return false;
        }
        match fs::metadata(file_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok())
        {
            // Allow a 2-second slack between DB timestamp and filesystem mtime.
            Some(file_mtime) => file_mtime > indexed_mtime_secs.saturating_add(2),
            None => true,
        }
    }
}

/// A parsed `agy` transcript JSONL file.
#[derive(Debug, Clone)]
pub struct AgySession {
    pub session_id: String,
    pub entries: Vec<AgyEntry>,
    #[allow(dead_code)]
    pub file_path: PathBuf,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

/// One structured step line from `transcript.jsonl` / `transcript_full.jsonl`.
#[derive(Debug, Clone)]
pub struct AgyEntry {
    #[allow(dead_code)]
    pub step_index: Option<u64>,
    #[allow(dead_code)]
    pub source: String,
    pub entry_type: String,
    pub timestamp: Option<String>,
    pub content: Option<String>,
}

/// A clean user or assistant message extracted from an `agy` session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyDisplayMessage {
    pub role: String,
    pub timestamp: Option<String>,
    pub content: String,
}

/// Return the default Antigravity CLI data root (`~/.gemini/antigravity-cli`).
pub fn agy_home_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    Ok(home.join(".gemini").join("antigravity-cli"))
}

/// Return the SQLite summary database path under `agy_root`.
pub fn agy_db_path(agy_root: &Path) -> PathBuf {
    agy_root.join(AGY_DB_FILENAME)
}

/// Return the `brain/` directory under `agy_root`.
pub fn agy_brain_dir(agy_root: &Path) -> PathBuf {
    agy_root.join("brain")
}

/// Check whether `path` refers to an `agy` summary SQLite database file.
pub fn is_agy_db_path(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some(AGY_DB_FILENAME)
}

/// Resolve the preferred transcript path for `conversation_id` under `agy_root`.
///
/// Returns `(show_path, summary_path)` where `show_path` prefers `transcript_full.jsonl`
/// (untruncated content for `show` and `search`) and `summary_path` prefers the lighter
/// `transcript.jsonl` (falling back to whichever file exists).
pub fn resolve_transcript_paths(
    agy_root: &Path,
    conversation_id: &str,
) -> Option<(PathBuf, PathBuf)> {
    let logs_dir = agy_brain_dir(agy_root)
        .join(conversation_id)
        .join(".system_generated")
        .join("logs");
    let full = logs_dir.join("transcript_full.jsonl");
    let compact = logs_dir.join("transcript.jsonl");
    let has_full = is_regular_non_symlink(&full);
    let has_compact = is_regular_non_symlink(&compact);
    match (has_full, has_compact) {
        (true, true) => Some((full, compact)),
        (true, false) => Some((full.clone(), full)),
        (false, true) => Some((compact.clone(), compact)),
        (false, false) => None,
    }
}

fn is_regular_non_symlink(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Ok(meta) if !meta.file_type().is_symlink() && meta.is_file()
    )
}

/// Open `conversation_summaries.db` in read-only, WAL-safe mode without creating or locking it.
///
/// Returns `Ok(None)` when the database does not exist so callers silently skip `agy` when
/// Antigravity CLI is not installed.
pub fn open_readonly_db(db_path: &Path) -> Result<Option<Connection>> {
    match fs::symlink_metadata(db_path) {
        Ok(meta) if !meta.file_type().is_symlink() && meta.is_file() => {}
        Ok(_) => anyhow::bail!("agy database is not a regular file: {}", db_path.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Failed to stat agy database: {}", db_path.display()));
        }
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(db_path, flags).with_context(|| {
        format!(
            "Failed to open agy database read-only: {}",
            db_path.display()
        )
    })?;
    conn.busy_timeout(Duration::from_millis(2000))?;
    let _ = conn.execute_batch("PRAGMA query_only = ON;");
    Ok(Some(conn))
}

/// Load conversation summaries from `conversation_summaries.db`.
///
/// Returns an empty vector if the database file does not exist.
pub fn load_db_summaries(db_path: &Path) -> Result<Vec<AgyDbSummary>> {
    let Some(conn) = open_readonly_db(db_path)? else {
        return Ok(Vec::new());
    };

    // Load optional FTS metadata (workspace fallback + archived flag) if table exists.
    let fts_meta = load_fts_meta_map(&conn);

    let mut stmt = conn.prepare(
        "SELECT conversation_id, title, preview, step_count, last_modified_time, \
                workspace_uris, project_id, parent_conversation_id, last_user_input_time \
         FROM conversation_summaries \
         ORDER BY last_modified_time DESC",
    )?;

    let rows = stmt.query_map([], |row| {
        let conversation_id: String = row.get(0)?;
        let title: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
        let preview: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
        let step_count: i64 = row.get::<_, Option<i64>>(3)?.unwrap_or(0);
        let last_modified_raw: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();
        let workspace_uris: String = row.get::<_, Option<String>>(5)?.unwrap_or_default();
        let project_id: String = row.get::<_, Option<String>>(6)?.unwrap_or_default();
        let parent_conversation_id: String = row.get::<_, Option<String>>(7)?.unwrap_or_default();
        let last_user_input_raw: String = row.get::<_, Option<String>>(8)?.unwrap_or_default();
        Ok((
            conversation_id,
            title,
            preview,
            step_count,
            last_modified_raw,
            workspace_uris,
            project_id,
            parent_conversation_id,
            last_user_input_raw,
        ))
    })?;

    let mut summaries = Vec::new();
    for row in rows {
        let (
            conversation_id,
            title,
            preview,
            step_count,
            last_modified_raw,
            workspace_uris,
            project_id,
            parent_conversation_id,
            last_user_input_raw,
        ) = row?;

        let (fts_workspace, archived) = fts_meta
            .get(&conversation_id)
            .cloned()
            .unwrap_or((None, false));

        let cwd = parse_first_workspace_uri(&workspace_uris);
        let project_name =
            resolve_project_name(cwd.as_deref(), fts_workspace.as_deref(), &project_id);

        summaries.push(AgyDbSummary {
            conversation_id,
            title: title.trim().to_string(),
            preview: preview.trim().to_string(),
            step_count: usize::try_from(step_count.max(0)).unwrap_or(0),
            last_modified_time: normalize_agy_timestamp(&last_modified_raw),
            cwd,
            project_name,
            parent_conversation_id: parent_conversation_id.trim().to_string(),
            last_user_input_time: normalize_agy_timestamp(&last_user_input_raw),
            archived,
        });
    }

    Ok(summaries)
}

fn load_fts_meta_map(conn: &Connection) -> HashMap<String, (Option<String>, bool)> {
    let mut map = HashMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT conversation_id, workspace, archived \
         FROM conversations_fts \
         WHERE chunk_index = -1",
    ) else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let workspace: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
        let archived: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
        Ok((id, workspace, archived != 0))
    }) else {
        return map;
    };
    for row in rows.flatten() {
        let (id, ws, archived) = row;
        let ws = percent_decode(&ws);
        let ws = ws.trim().to_string();
        let ws_opt = (!ws.is_empty()).then_some(ws);
        map.insert(id, (ws_opt, archived));
    }
    map
}

/// Pre-filter indexed `agy` sessions using SQLite FTS5 (`conversations_fts`).
///
/// Uses FTS5 `MATCH` as the fast path for word/phrase queries and supplements with
/// SQLite substring checks on `conversations_fts` for CJK, punctuation, or infix queries
/// that `unicode61` cannot match by prefix alone. Returns `None` if the database or
/// `conversations_fts` table is unavailable (callers fall back to transcript scanning).
pub fn fts_prefilter_candidates(
    db_path: &Path,
    keywords: &[&str],
    user_only: bool,
    phrase: bool,
) -> Option<AgyFtsPrefilter> {
    if keywords.is_empty() {
        return None;
    }
    let conn = open_readonly_db(db_path).ok()??;

    // Collect every conversation that has at least one indexed body chunk along with
    // its header row's `last_modified_time` (stored as Unix nanoseconds by agy).
    let mut indexed_mtimes: HashMap<String, i64> = HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT conversation_id, \
                        MAX(CASE WHEN chunk_index = -1 THEN last_modified_time ELSE 0 END) AS header_mtime, \
                        SUM(CASE WHEN chunk_index >= 0 THEN 1 ELSE 0 END) AS body_chunks \
                 FROM conversations_fts \
                 GROUP BY conversation_id",
            )
            .ok()?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let mtime_raw: i64 = row.get::<_, Option<i64>>(1)?.unwrap_or(0);
                let body_chunks: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
                Ok((id, mtime_raw, body_chunks))
            })
            .ok()?;
        for row in rows.flatten() {
            let (id, mtime_raw, body_chunks) = row;
            if body_chunks > 0 {
                indexed_mtimes.insert(id, fts_timestamp_to_secs(mtime_raw));
            }
        }
    }

    if indexed_mtimes.is_empty() {
        return Some(AgyFtsPrefilter::default());
    }

    let mut matched_ids = HashSet::new();
    let fts_columns = if user_only {
        "user_body"
    } else {
        "{user_body agent_body}"
    };

    for &kw in keywords {
        let trimmed = kw.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut kw_hits = HashSet::new();
        let can_use_fts_only = is_fts_word_query(trimmed);

        // 1. Primary fast path: SQLite FTS5 inverted index MATCH.
        if let Some(expr) = build_fts5_match_expr(fts_columns, trimmed, phrase) {
            if let Ok(mut stmt) = conn.prepare(
                "SELECT DISTINCT conversation_id \
                 FROM conversations_fts \
                 WHERE chunk_index >= 0 AND conversations_fts MATCH ?1",
            ) {
                if let Ok(rows) = stmt.query_map([&expr], |row| row.get::<_, String>(0)) {
                    for id in rows.flatten() {
                        kw_hits.insert(id);
                    }
                }
            }
        }

        // 2. Fallback inside SQLite `conversations_fts` for CJK / punctuation / infix
        //    substrings that `unicode61` prefix indexing does not cover.
        if !can_use_fts_only || kw_hits.is_empty() {
            let pattern = format!("%{}%", escape_like_pattern(&trimmed.to_lowercase()));
            let sql = if user_only {
                "SELECT DISTINCT conversation_id \
                 FROM conversations_fts \
                 WHERE chunk_index >= 0 AND lower(user_body) LIKE ?1 ESCAPE '\\'"
            } else {
                "SELECT DISTINCT conversation_id \
                 FROM conversations_fts \
                 WHERE chunk_index >= 0 \
                   AND (lower(user_body) LIKE ?1 ESCAPE '\\' OR lower(agent_body) LIKE ?1 ESCAPE '\\')"
            };
            if let Ok(mut stmt) = conn.prepare(sql) {
                if let Ok(rows) = stmt.query_map([&pattern], |row| row.get::<_, String>(0)) {
                    for id in rows.flatten() {
                        kw_hits.insert(id);
                    }
                }
            }
        }

        matched_ids.extend(kw_hits);
    }

    Some(AgyFtsPrefilter {
        indexed_mtimes,
        matched_ids,
    })
}

/// Convert `conversations_fts.last_modified_time` (nanoseconds, or microseconds/millis/secs)
/// into Unix seconds.
fn fts_timestamp_to_secs(raw: i64) -> i64 {
    if raw <= 0 {
        return 0;
    }
    if raw > 1_000_000_000_000_000_000 {
        // Nanoseconds (standard Go UnixNano used by agy)
        raw / 1_000_000_000
    } else if raw > 1_000_000_000_000_000 {
        // Microseconds
        raw / 1_000_000
    } else if raw > 1_000_000_000_000 {
        // Milliseconds
        raw / 1_000
    } else {
        raw
    }
}

/// Return true if `kw` consists solely of ASCII alphanumeric characters (and spaces),
/// which `unicode61` tokenizes predictably.
fn is_fts_word_query(kw: &str) -> bool {
    !kw.is_empty()
        && kw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace())
}

/// Build a safe FTS5 MATCH expression for a column selector and keyword/phrase.
fn build_fts5_match_expr(columns: &str, keyword: &str, phrase: bool) -> Option<String> {
    let trimmed = keyword.trim();
    if trimmed.is_empty() {
        return None;
    }
    let escaped = trimmed.replace('"', "\"\"");
    if phrase || trimmed.contains(char::is_whitespace) {
        Some(format!("{columns} : \"{escaped}\""))
    } else if trimmed.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(format!("{columns} : \"{escaped}\"*"))
    } else {
        Some(format!("{columns} : \"{escaped}\""))
    }
}

fn escape_like_pattern(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Load fallback display messages directly from `conversation_summaries.db` when a
/// conversation has no `transcript*.jsonl` file on disk.
pub fn load_display_messages_from_db(
    db_path: &Path,
    conversation_id: &str,
) -> Result<Vec<AgyDisplayMessage>> {
    let Some(conn) = open_readonly_db(db_path)? else {
        return Ok(Vec::new());
    };

    let mut messages = Vec::new();

    if let Ok(mut stmt) = conn.prepare(
        "SELECT user_body, agent_body \
         FROM conversations_fts \
         WHERE conversation_id = ?1 AND chunk_index >= 0 \
         ORDER BY chunk_index ASC",
    ) {
        if let Ok(rows) = stmt.query_map([conversation_id], |row| {
            let user_body: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
            let agent_body: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            Ok((user_body, agent_body))
        }) {
            for (user_body, agent_body) in rows.flatten() {
                let u = user_body.trim();
                if !u.is_empty() {
                    messages.push(AgyDisplayMessage {
                        role: "user".to_string(),
                        timestamp: None,
                        content: u.to_string(),
                    });
                }
                let a = agent_body.trim();
                if !a.is_empty() {
                    messages.push(AgyDisplayMessage {
                        role: "assistant".to_string(),
                        timestamp: None,
                        content: a.to_string(),
                    });
                }
            }
        }
    }

    if messages.is_empty() {
        if let Ok(mut stmt) = conn.prepare(
            "SELECT preview, last_modified_time \
             FROM conversation_summaries \
             WHERE conversation_id = ?1",
        ) {
            if let Ok((preview, ts_raw)) = stmt.query_row([conversation_id], |row| {
                let preview: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
                let ts: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
                Ok((preview, ts))
            }) {
                let text = preview.trim();
                if !text.is_empty() {
                    messages.push(AgyDisplayMessage {
                        role: "user".to_string(),
                        timestamp: normalize_agy_timestamp(&ts_raw),
                        content: text.to_string(),
                    });
                }
            }
        }
    }

    Ok(messages)
}

impl AgySession {
    /// Parse an `agy` transcript JSONL file.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Self::from_file_with_report(path)?.value)
    }

    /// Parse an `agy` transcript JSONL file and report any malformed lines skipped.
    pub fn from_file_with_report<P: AsRef<Path>>(path: P) -> Result<ParseOutcome<Self>> {
        let path = path.as_ref();
        let file =
            File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;
        let reader = BufReader::new(file);

        let mut entries = Vec::new();
        let mut malformed_lines = 0;

        for (line_num, line) in reader.lines().enumerate() {
            let line = line.with_context(|| {
                format!("Failed to read line {} in {}", line_num + 1, path.display())
            })?;
            if line.trim().is_empty() {
                continue;
            }

            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(e) => {
                    malformed_lines += 1;
                    log::debug!(
                        "Skipping malformed agy transcript line {} in {}: {}",
                        line_num + 1,
                        path.display(),
                        e
                    );
                    continue;
                }
            };

            let step_index = value.get("step_index").and_then(|v| v.as_u64());
            let source = value
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let entry_type = value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let timestamp = value
                .get("created_at")
                .and_then(|v| v.as_str())
                .and_then(normalize_agy_timestamp);
            let content = value
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            entries.push(AgyEntry {
                step_index,
                source,
                entry_type,
                timestamp,
                content,
            });
        }

        let session_id = session_id_from_transcript_path(path)
            .with_context(|| format!("No agy conversation ID found for: {}", path.display()))?;

        Ok(ParseOutcome {
            value: Self {
                session_id,
                entries,
                file_path: path.to_path_buf(),
                cwd: None,
                title: None,
            },
            malformed_lines,
        })
    }

    /// Extract clean user and assistant display messages from this transcript.
    ///
    /// - `USER_INPUT`: strips `<USER_REQUEST>`, `<ADDITIONAL_METADATA>`, and
    ///   `<USER_SETTINGS_CHANGE>` wrappers and filters system injections.
    /// - `PLANNER_RESPONSE`: extracts non-empty `content` (skipping tool-only steps
    ///   and internal `thinking`).
    /// - `GENERIC`, `SYSTEM_MESSAGE`, `ERROR_MESSAGE`: skipped.
    pub fn display_messages(&self) -> Vec<AgyDisplayMessage> {
        let mut messages = Vec::new();

        for entry in &self.entries {
            match entry.entry_type.as_str() {
                "USER_INPUT" => {
                    if let Some(raw) = entry.content.as_deref() {
                        let cleaned = strip_user_request_wrapper(raw);
                        if !cleaned.is_empty() && !is_system_content(&cleaned) {
                            messages.push(AgyDisplayMessage {
                                role: "user".to_string(),
                                timestamp: entry.timestamp.clone(),
                                content: cleaned,
                            });
                        }
                    }
                }
                "PLANNER_RESPONSE" => {
                    if let Some(raw) = entry.content.as_deref() {
                        let trimmed = raw.trim();
                        if !trimmed.is_empty() && !is_system_content(trimmed) {
                            messages.push(AgyDisplayMessage {
                                role: "assistant".to_string(),
                                timestamp: entry.timestamp.clone(),
                                content: trimmed.to_string(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        messages
    }

    /// Compute the session title using DB title/preview first, then the first real user message.
    pub fn title_from_messages(
        &self,
        messages: &[AgyDisplayMessage],
        db_title: Option<&str>,
        db_preview: Option<&str>,
    ) -> String {
        for candidate in [self.title.as_deref(), db_title, db_preview]
            .into_iter()
            .flatten()
        {
            let trimmed = candidate.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }

        messages
            .iter()
            .find(|m| m.role == "user" && !m.content.trim().is_empty())
            .map(|m| m.content.clone())
            .or_else(|| self.project_name().map(str::to_string))
            .unwrap_or_else(|| "(No title)".to_string())
    }

    pub fn first_timestamp(&self) -> Option<String> {
        self.entries.iter().find_map(|e| e.timestamp.clone())
    }

    pub fn latest_timestamp(&self) -> Option<String> {
        self.entries
            .iter()
            .filter_map(|e| e.timestamp.clone())
            .max()
    }

    pub fn project_name(&self) -> Option<&str> {
        self.cwd.as_deref().and_then(last_path_component)
    }
}

/// Strip `<USER_REQUEST>...</USER_REQUEST>`, `<ADDITIONAL_METADATA>...</ADDITIONAL_METADATA>`,
/// and `<USER_SETTINGS_CHANGE>...</USER_SETTINGS_CHANGE>` wrappers from an `agy` user prompt.
pub fn strip_user_request_wrapper(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some(after_open) = trimmed.strip_prefix("<USER_REQUEST>") {
        let after_open = after_open
            .strip_prefix("\r\n")
            .or_else(|| after_open.strip_prefix('\n'))
            .unwrap_or(after_open);
        // Use rfind so a prompt that literally mentions `</USER_REQUEST>` keeps its full body.
        if let Some(close_idx) = after_open.rfind("</USER_REQUEST>") {
            return after_open[..close_idx].trim().to_string();
        }
        return strip_trailing_metadata_tags(after_open).trim().to_string();
    }

    strip_trailing_metadata_tags(trimmed).trim().to_string()
}

fn strip_trailing_metadata_tags(input: &str) -> String {
    let mut text = input.to_string();
    for tag in [
        "ADDITIONAL_METADATA",
        "USER_SETTINGS_CHANGE",
        "SYSTEM_MESSAGE",
    ] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        loop {
            let Some(start) = text.find(&open) else {
                break;
            };
            if let Some(end_rel) = text[start + open.len()..].find(&close) {
                let end = start + open.len() + end_rel + close.len();
                text.replace_range(start..end, "");
            } else {
                // Unclosed trailing metadata block (e.g. truncated transcript line).
                text.truncate(start);
                break;
            }
        }
    }
    text
}

/// Normalize an `agy` timestamp (SQLite `'YYYY-MM-DD HH:MM:SS.ffffff+00:00'` or RFC3339)
/// into canonical UTC RFC3339 (`YYYY-MM-DDTHH:MM:SSZ`). Returns `None` for empty or
/// Go zero-time (`0001-01-01...`) values.
pub fn normalize_agy_timestamp(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with("0001-01-01") {
        return None;
    }
    let normalized = if trimmed.len() >= 19 && trimmed.as_bytes().get(10) == Some(&b' ') {
        let mut s = trimmed.to_string();
        s.replace_range(10..11, "T");
        s
    } else {
        trimmed.to_string()
    };
    DateTime::parse_from_rfc3339(&normalized).ok().map(|dt| {
        dt.with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Secs, true)
    })
}

/// Extract and decode the first workspace path from `conversation_summaries.workspace_uris`.
pub fn parse_first_workspace_uri(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(trimmed) {
        for item in items {
            if let Some(uri) = item.as_str() {
                if let Some(path) = decode_workspace_uri(uri) {
                    return Some(path);
                }
            }
        }
        return None;
    }
    decode_workspace_uri(trimmed)
}

/// Decode a `file://` workspace URI into a local filesystem path (supports UTF-8 percent-encoding
/// and Windows drive-letter URIs like `file:///C:/Users/...`).
pub fn decode_workspace_uri(uri: &str) -> Option<String> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return None;
    }
    let raw_path = trimmed
        .strip_prefix("file://localhost")
        .or_else(|| trimmed.strip_prefix("file://"))
        .unwrap_or(trimmed);
    let decoded = percent_decode(raw_path);
    // Strip leading '/' before Windows drive letter: `/C:/Users/...` -> `C:/Users/...`
    let cleaned = if decoded.len() >= 3
        && decoded.as_bytes()[0] == b'/'
        && decoded.as_bytes()[1].is_ascii_alphabetic()
        && decoded.as_bytes()[2] == b':'
    {
        decoded[1..].to_string()
    } else {
        decoded
    };
    let cleaned = cleaned.trim().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn resolve_project_name(
    cwd: Option<&str>,
    fts_workspace: Option<&str>,
    project_id: &str,
) -> String {
    if let Some(name) = cwd.and_then(last_path_component) {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    if let Some(ws) = fts_workspace {
        if let Some(name) = last_path_component(ws) {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    let pid = project_id.trim();
    if !pid.is_empty() && pid != "default-cli-project" && pid != "outside-of-project" {
        return pid.to_string();
    }
    "agy".to_string()
}

/// Derive a conversation ID from `.../brain/<conversation-id>/.system_generated/logs/transcript*.jsonl`.
fn session_id_from_transcript_path(path: &Path) -> Option<String> {
    // Walk up past `logs` and `.system_generated` if present.
    let mut cur = path.parent()?;
    if cur.file_name().and_then(|s| s.to_str()) == Some("logs") {
        cur = cur.parent()?;
    }
    if cur.file_name().and_then(|s| s.to_str()) == Some(".system_generated") {
        cur = cur.parent()?;
    }
    if let Some(dir_name) = cur.file_name().and_then(|s| s.to_str()) {
        if !dir_name.is_empty() && dir_name != "brain" {
            return Some(dir_name.to_string());
        }
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE conversation_summaries (
                 conversation_id text PRIMARY KEY,
                 title text NOT NULL DEFAULT '',
                 preview text NOT NULL DEFAULT '',
                 step_count integer NOT NULL DEFAULT 0,
                 last_modified_time datetime NOT NULL,
                 workspace_uris text NOT NULL DEFAULT '',
                 status text NOT NULL DEFAULT '',
                 source text NOT NULL DEFAULT '',
                 project_id text NOT NULL DEFAULT '',
                 agent_name text NOT NULL DEFAULT '',
                 parent_conversation_id text NOT NULL DEFAULT '',
                 nesting_depth integer NOT NULL DEFAULT 0,
                 battle_id text NOT NULL DEFAULT '',
                 winning_conversation_id text NOT NULL DEFAULT '',
                 not_fully_idle numeric NOT NULL DEFAULT false,
                 killed numeric NOT NULL DEFAULT false,
                 last_user_input_time datetime NOT NULL DEFAULT '0001-01-01 00:00:00+00:00',
                 last_user_input_step_index integer NOT NULL DEFAULT -1,
                 app_data_dir text NOT NULL DEFAULT '',
                 raw_summary blob,
                 group_id text NOT NULL DEFAULT ''
             );
             CREATE VIRTUAL TABLE conversations_fts USING fts5(
                 conversation_id UNINDEXED,
                 chunk_index UNINDEXED,
                 chunk_first_step UNINDEXED,
                 chunk_last_step UNINDEXED,
                 last_modified_time UNINDEXED,
                 step_count UNINDEXED,
                 last_step_created_at UNINDEXED,
                 archived UNINDEXED,
                 user_step_byte_offsets UNINDEXED,
                 agent_step_byte_offsets UNINDEXED,
                 title,
                 workspace,
                 project,
                 user_body,
                 agent_body,
                 tokenize = 'unicode61 remove_diacritics 2',
                 prefix = '1 2 3'
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn strips_user_request_and_metadata_wrappers_including_nested_tag_mentions() {
        let wrapped = "<USER_REQUEST>\n请剥离 `<USER_REQUEST>`/`</USER_REQUEST>` 标签并提取正文\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-26T23:59:36+08:00.\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nThe user changed setting\n</USER_SETTINGS_CHANGE>";
        assert_eq!(
            strip_user_request_wrapper(wrapped),
            "请剥离 `<USER_REQUEST>`/`</USER_REQUEST>` 标签并提取正文"
        );

        let plain = "plain prompt\n<ADDITIONAL_METADATA>\ntime\n</ADDITIONAL_METADATA>";
        assert_eq!(strip_user_request_wrapper(plain), "plain prompt");
    }

    #[test]
    fn decodes_percent_encoded_and_windows_workspace_uris() {
        assert_eq!(
            parse_first_workspace_uri(
                r#"["file:///Volumes/Data/Projects/%E5%AE%89%E8%A3%85%E7%8E%AF%E5%A2%83"]"#
            ),
            Some("/Volumes/Data/Projects/安装环境".to_string())
        );
        assert_eq!(
            parse_first_workspace_uri(r#"["file:///C:/Users/OSEN/%E9%A1%B9%E7%9B%AE"]"#),
            Some("C:/Users/OSEN/项目".to_string())
        );
        assert_eq!(
            resolve_project_name(Some("C:/Users/OSEN/项目"), None, "default-cli-project"),
            "项目"
        );
        assert_eq!(
            resolve_project_name(None, None, "outside-of-project"),
            "agy"
        );
    }

    #[test]
    fn normalizes_sqlite_and_rfc3339_timestamps_and_ignores_go_zero_time() {
        assert_eq!(
            normalize_agy_timestamp("2026-11-26 16:00:00.708556+00:00"),
            Some("2026-11-26T16:00:00Z".to_string())
        );
        assert_eq!(
            normalize_agy_timestamp("2026-11-26T15:14:32Z"),
            Some("2026-11-26T15:14:32Z".to_string())
        );
        assert_eq!(normalize_agy_timestamp("0001-01-01 00:00:00+00:00"), None);
        assert_eq!(normalize_agy_timestamp(""), None);
    }

    #[test]
    fn parses_transcript_filters_noise_and_skips_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir
            .path()
            .join("brain/fda33c60-94ee-424f-8b59-bda78f88e945/.system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        let path = logs.join("transcript_full.jsonl");
        fs::write(
            &path,
            concat!(
                "{not valid json\n",
                r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-11-26T15:14:32Z","content":"<USER_REQUEST>\nHello Agy\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nmeta\n</ADDITIONAL_METADATA>"}"#,
                "\n",
                r#"{"step_index":1,"source":"SYSTEM_SDK","type":"SYSTEM_MESSAGE","created_at":"2026-11-26T15:14:32Z","content":"<SYSTEM_MESSAGE>injected</SYSTEM_MESSAGE>"}"#,
                "\n",
                r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-26T15:14:33Z","thinking":"internal thought","tool_calls":[{"name":"read_file"}]}"#,
                "\n",
                r#"{"step_index":3,"source":"MODEL","type":"GENERIC","created_at":"2026-11-26T15:14:34Z","content":"Command output: noise"}"#,
                "\n",
                r#"{"step_index":4,"source":"SYSTEM","type":"ERROR_MESSAGE","created_at":"2026-11-26T15:14:35Z","error":"503"}"#,
                "\n",
                r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-26T15:14:36Z","content":"Hi from Agy!"}"#,
                "\n",
            ),
        )
        .unwrap();

        let outcome = AgySession::from_file_with_report(&path).unwrap();
        assert_eq!(outcome.malformed_lines, 1);
        assert_eq!(
            outcome.value.session_id,
            "fda33c60-94ee-424f-8b59-bda78f88e945"
        );
        let msgs = outcome.value.display_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "Hello Agy");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content, "Hi from Agy!");
        assert_eq!(
            outcome.value.first_timestamp().as_deref(),
            Some("2026-11-26T15:14:32Z")
        );
        assert_eq!(
            outcome.value.latest_timestamp().as_deref(),
            Some("2026-11-26T15:14:36Z")
        );
    }

    #[test]
    fn loads_db_summaries_and_fts_prefilter_handles_ascii_cjk_and_user_only() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = agy_db_path(dir.path());
        let conn = create_test_db(&db_path);
        conn.execute(
            "INSERT INTO conversation_summaries \
             (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, project_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "11111111-2222-3333-4444-555555555555",
                "Using Agy with Claude Code",
                "first prompt",
                10,
                "2026-11-26 16:00:00.123456+00:00",
                r#"["file:///Volumes/Data/Projects/%E5%AE%89%E8%A3%85%E7%8E%AF%E5%A2%83"]"#,
                "default-cli-project",
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations_fts \
             (conversation_id, chunk_index, chunk_first_step, chunk_last_step, last_modified_time, step_count, last_step_created_at, archived, user_step_byte_offsets, agent_step_byte_offsets, title, workspace, project, user_body, agent_body) \
             VALUES (?1, -1, 0, 0, 1795708800000000000, 10, 1795708800000000000, 0, '', '', 'Using Agy with Claude Code', '%E5%AE%89%E8%A3%85%E7%8E%AF%E5%A2%83', 'CLI', '', '')",
            ["11111111-2222-3333-4444-555555555555"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations_fts \
             (conversation_id, chunk_index, chunk_first_step, chunk_last_step, last_modified_time, step_count, last_step_created_at, archived, user_step_byte_offsets, agent_step_byte_offsets, title, workspace, project, user_body, agent_body) \
             VALUES (?1, 0, 0, 9, 0, 0, 0, 0, '', '', '', '', '', '如何利用检索库迁移 Claudecode 配置', '可以通过 SessionStart hook 自动加载')",
            ["11111111-2222-3333-4444-555555555555"],
        )
        .unwrap();
        drop(conn);

        let summaries = load_db_summaries(&db_path).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].project_name, "安装环境");
        assert_eq!(
            summaries[0].cwd.as_deref(),
            Some("/Volumes/Data/Projects/安装环境")
        );
        assert_eq!(
            summaries[0].last_modified_time.as_deref(),
            Some("2026-11-26T16:00:00Z")
        );

        // ASCII FTS5 match on assistant body
        let pf = fts_prefilter_candidates(&db_path, &["SessionStart"], false, false).unwrap();
        assert!(pf
            .matched_ids
            .contains("11111111-2222-3333-4444-555555555555"));

        // user_only excludes assistant-only keyword
        let pf_user = fts_prefilter_candidates(&db_path, &["SessionStart"], true, false).unwrap();
        assert!(!pf_user
            .matched_ids
            .contains("11111111-2222-3333-4444-555555555555"));

        // CJK substring match inside Chinese phrase
        let pf_cjk = fts_prefilter_candidates(&db_path, &["检索库"], true, false).unwrap();
        assert!(pf_cjk
            .matched_ids
            .contains("11111111-2222-3333-4444-555555555555"));

        // Infix ASCII substring fallback ("code" inside "Claudecode")
        let pf_infix = fts_prefilter_candidates(&db_path, &["code"], true, false).unwrap();
        assert!(pf_infix
            .matched_ids
            .contains("11111111-2222-3333-4444-555555555555"));
    }

    #[test]
    fn missing_database_returns_empty_silently() {
        let dir = tempfile::tempdir().unwrap();
        let missing = agy_db_path(dir.path());
        assert!(load_db_summaries(&missing).unwrap().is_empty());
        assert!(fts_prefilter_candidates(&missing, &["foo"], false, false).is_none());
    }
}
