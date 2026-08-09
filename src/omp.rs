use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::parser::ParseOutcome;
use crate::session_diagnostics::legacy_walk_entry;

#[derive(Debug, Clone)]
pub struct OmpSession {
    pub session_id: String,
    pub entries: Vec<OmpEntry>,
    pub file_path: PathBuf,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OmpEntry {
    pub entry_type: String,
    pub timestamp: Option<String>,
    pub role: Option<String>,
    pub content_items: Vec<OmpContentItem>,
}

#[derive(Debug, Clone)]
pub struct OmpContentItem {
    pub item_type: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OmpDisplayMessage {
    pub role: String,
    pub timestamp: Option<String>,
    pub content: String,
}

impl OmpSession {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Self::from_file_with_report(path)?.value)
    }

    pub fn from_file_with_report<P: AsRef<Path>>(path: P) -> Result<ParseOutcome<Self>> {
        let path = path.as_ref();
        let file =
            File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;
        let reader = BufReader::new(file);

        let mut entries = Vec::new();
        let mut session_id: Option<String> = None;
        let mut cwd: Option<String> = None;
        let mut title: Option<String> = None;
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
                        "Skipping malformed OMP line {} in {}: {}",
                        line_num + 1,
                        path.display(),
                        e
                    );
                    continue;
                }
            };

            let entry_type = value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let line_timestamp = value
                .get("timestamp")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if entry_type == "session" {
                if session_id.is_none() {
                    session_id = value
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                if cwd.is_none() {
                    cwd = value
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                if title.is_none() {
                    title = value
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .filter(|s| !s.is_empty());
                }
            }

            let role = if entry_type == "message" {
                value
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            } else {
                None
            };

            let content_items = if entry_type == "message" {
                value
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|item| {
                                let item_type = item
                                    .get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string();
                                let text = item
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                OmpContentItem { item_type, text }
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            entries.push(OmpEntry {
                entry_type,
                timestamp: line_timestamp,
                role,
                content_items,
            });
        }

        let session_id = session_id
            .or_else(|| session_id_from_path(path))
            .with_context(|| format!("No OMP session ID found: {}", path.display()))?;

        Ok(ParseOutcome {
            value: Self {
                session_id,
                entries,
                file_path: path.to_path_buf(),
                cwd,
                title,
            },
            malformed_lines,
        })
    }

    pub fn display_messages(&self) -> Vec<OmpDisplayMessage> {
        let mut messages = Vec::new();

        for entry in &self.entries {
            if entry.entry_type != "message" {
                continue;
            }
            let role = match entry.role.as_deref() {
                Some(role @ ("user" | "assistant")) => role,
                _ => continue,
            };

            let parts: Vec<String> = entry
                .content_items
                .iter()
                .filter_map(|item| {
                    if item.item_type == "text" {
                        item.text.as_ref().map(|t| t.trim().to_string())
                    } else {
                        None
                    }
                })
                .filter(|s| !s.is_empty() && !crate::codex::is_system_content(s))
                .collect();

            if parts.is_empty() {
                continue;
            }

            messages.push(OmpDisplayMessage {
                role: role.to_string(),
                timestamp: entry.timestamp.clone(),
                content: parts.join("\n"),
            });
        }

        messages
    }

    #[allow(dead_code)]
    pub fn title(&self) -> String {
        self.title_from_messages(&self.display_messages())
    }

    /// Compute the title reusing an already-built `messages` slice to avoid a
    /// second pass over `display_messages()`.
    pub(crate) fn title_from_messages(&self, messages: &[OmpDisplayMessage]) -> String {
        if let Some(title) = self.title.as_deref() {
            let title = title.trim();
            if !title.is_empty() {
                return title.to_string();
            }
        }

        messages
            .iter()
            .find(|m| m.role == "user" && !m.content.trim().is_empty())
            .map(|m| m.content.clone())
            .or_else(|| self.project_name().map(|s| s.to_string()))
            .unwrap_or_else(|| self.session_id.clone())
    }

    pub fn latest_timestamp(&self) -> Option<String> {
        self.entries
            .iter()
            .filter_map(|e| e.timestamp.clone())
            .max()
    }

    pub fn first_timestamp(&self) -> Option<String> {
        self.entries.iter().find_map(|e| e.timestamp.clone())
    }

    pub fn project_name(&self) -> Option<String> {
        if let Some(name) = self
            .cwd
            .as_deref()
            .and_then(crate::codex::last_path_component)
            .map(|s| s.to_string())
        {
            return Some(name);
        }
        self.file_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
    }
}

pub fn omp_sessions_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    Ok(home.join(".omp").join("agent").join("sessions"))
}

#[allow(dead_code)]
pub fn discover_omp_sessions(base_path: &Path) -> Result<Vec<OmpSession>> {
    if !base_path.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for entry in WalkDir::new(base_path).follow_links(false).into_iter() {
        let Some(entry) = legacy_walk_entry(entry, "omp") else {
            continue;
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        match OmpSession::from_file(path) {
            Ok(session) => sessions.push(session),
            Err(_) => log::warn!(
                target: crate::logger::SCAN_DIAGNOSTICS_TARGET,
                "legacy OMP session discovery skipped an unparseable file"
            ),
        }
    }

    sessions.sort_by_key(|s| s.latest_timestamp());
    sessions.reverse();
    Ok(sessions)
}

fn session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    stem.rsplit('_').next().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write `content` to a tempdir file named `<prefix>_<id>.jsonl`.
    fn write_session(content: &str, id: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        // Mirror the real OMP naming: `<timestamp>_<session-id>.jsonl`
        let path = dir
            .path()
            .join(format!("2026-06-23T11-53-13-905Z_{id}.jsonl"));
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn report_counts_malformed_lines_and_keeps_valid_omp_entries() {
        let (_dir, path) = write_session(
            concat!(
                "{not valid json\n",
                r#"{"type":"session","id":"partial-omp","cwd":"/tmp/demo"}"#,
                "\n"
            ),
            "partial-omp",
        );

        let outcome = OmpSession::from_file_with_report(&path).unwrap();
        assert_eq!(outcome.malformed_lines, 1);
        assert_eq!(outcome.value.session_id, "partial-omp");
        assert_eq!(outcome.value.entries.len(), 1);
    }

    #[test]
    fn parses_omp_session_messages() {
        let (dir, path) = write_session(
            r##"{"type":"session","version":3,"id":"omp-abc","timestamp":"2026-06-23T11:53:13.905Z","cwd":"/tmp/demo","title":"Demo Title"}
{"type":"model_change","timestamp":"2026-06-23T11:53:30.000Z"}
{"type":"message","timestamp":"2026-06-23T11:53:52.345Z","message":{"role":"user","content":[{"type":"text","text":"hello omp"}]}}
{"type":"message","timestamp":"2026-06-23T11:54:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi user"}]}}
"##,
            "omp-abc",
        );
        let _ = dir;

        let session = OmpSession::from_file(&path).unwrap();
        assert_eq!(session.session_id, "omp-abc");
        assert_eq!(session.cwd.as_deref(), Some("/tmp/demo"));
        assert_eq!(session.project_name().as_deref(), Some("demo"));
        assert_eq!(session.title(), "Demo Title");

        let messages = session.display_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "hello omp");
        assert_eq!(
            messages[0].timestamp.as_deref(),
            Some("2026-06-23T11:53:52.345Z")
        );
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "hi user");
    }

    #[test]
    fn title_falls_back_to_first_user_message() {
        // No `title` field on the session line — title must come from the
        // first real user message.
        let (_dir, path) = write_session(
            r##"{"type":"session","id":"omp-1","timestamp":"2026-06-23T11:53:13.905Z","cwd":"/tmp/demo"}
{"type":"message","timestamp":"2026-06-23T11:53:52.345Z","message":{"role":"user","content":[{"type":"text","text":"  my first prompt  "}]}}
{"type":"message","timestamp":"2026-06-23T11:54:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"reply"}]}}
"##,
            "omp-1",
        );

        let session = OmpSession::from_file(&path).unwrap();
        assert_eq!(session.title(), "my first prompt");
    }

    #[test]
    fn title_falls_back_to_project_name_when_no_user_message() {
        // No title field, no user messages — title falls back to project name.
        let (_dir, path) = write_session(
            r##"{"type":"session","id":"omp-2","timestamp":"2026-06-23T11:53:13.905Z","cwd":"/tmp/lonely-project"}
{"type":"message","timestamp":"2026-06-23T11:54:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"solo reply"}]}}
"##,
            "omp-2",
        );
        let session = OmpSession::from_file(&path).unwrap();
        assert_eq!(session.title(), "lonely-project");

        // No cwd either — project_name() then falls back to the parent dir
        // name of the file, so title tracks project_name() rather than the
        // session id.
        let (_dir, path) = write_session(
            r##"{"type":"session","id":"omp-3","timestamp":"2026-06-23T11:53:13.905Z"}
{"type":"message","timestamp":"2026-06-23T11:54:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"solo reply"}]}}
"##,
            "omp-3",
        );
        let session = OmpSession::from_file(&path).unwrap();
        assert_eq!(session.title(), session.project_name().unwrap());
    }

    #[test]
    fn skips_system_content_and_non_text_items() {
        let (_dir, path) = write_session(
            r##"{"type":"session","id":"omp-4","timestamp":"2026-06-23T11:53:13.905Z","cwd":"/tmp/demo"}
{"type":"message","timestamp":"2026-06-23T11:53:52.345Z","message":{"role":"user","content":[{"type":"text","text":"# AGENTS.md instructions\nloaded"}]}}
{"type":"message","timestamp":"2026-06-23T11:53:53.000Z","message":{"role":"user","content":[{"type":"tool_use","text":"ignored tool item"}]}}
{"type":"message","timestamp":"2026-06-23T11:53:54.000Z","message":{"role":"user","content":[{"type":"text","text":"real prompt"}]}}
"##,
            "omp-4",
        );
        let session = OmpSession::from_file(&path).unwrap();
        let messages = session.display_messages();
        // The AGENTS.md system block and the non-text tool_use item are dropped;
        // only the real prompt survives.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "real prompt");
        // Title falls back to that real prompt (system content is not a title candidate).
        assert_eq!(session.title(), "real prompt");
    }

    #[test]
    fn project_name_supports_windows_path_separator() {
        // Windows cwd pushed from a Windows machine must resolve on Unix too.
        let (_dir, path) = write_session(
            r##"{"type":"session","id":"omp-5","timestamp":"2026-06-23T11:53:13.905Z","cwd":"C:\\Users\\OSEN\\安装环境"}
"##,
            "omp-5",
        );
        let session = OmpSession::from_file(&path).unwrap();
        assert_eq!(session.project_name().as_deref(), Some("安装环境"));
    }

    #[test]
    fn first_and_latest_timestamp() {
        let (_dir, path) = write_session(
            r##"{"type":"session","id":"omp-6","timestamp":"2026-06-23T11:53:13.905Z","cwd":"/tmp/demo"}
{"type":"model_change","timestamp":"2026-06-23T11:53:30.000Z"}
{"type":"message","timestamp":"2026-06-23T11:54:00.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
"##,
            "omp-6",
        );
        let session = OmpSession::from_file(&path).unwrap();
        assert_eq!(
            session.first_timestamp().as_deref(),
            Some("2026-06-23T11:53:13.905Z")
        );
        assert_eq!(
            session.latest_timestamp().as_deref(),
            Some("2026-06-23T11:54:00.000Z")
        );
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let (_dir, path) = write_session(
            r##"{not valid json
{"type":"session","id":"omp-7","timestamp":"2026-06-23T11:53:13.905Z","cwd":"/tmp/demo"}
{"type":"message","timestamp":"2026-06-23T11:53:52.345Z","message":{"role":"user","content":[{"type":"text","text":"ok"}]}}
"##,
            "omp-7",
        );
        let session = OmpSession::from_file(&path).unwrap();
        // Malformed first line is skipped; parsing continues.
        assert_eq!(session.session_id, "omp-7");
        let messages = session.display_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "ok");
    }

    #[test]
    fn session_id_from_path_when_no_session_line() {
        // No `type:"session"` line at all — id must be derived from the
        // `<timestamp>_<id>.jsonl` filename.
        let (_dir, path) = write_session(
            r##"{"type":"message","timestamp":"2026-06-23T11:53:52.345Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
"##,
            "derived-id-123",
        );
        let session = OmpSession::from_file(&path).unwrap();
        assert_eq!(session.session_id, "derived-id-123");
    }

    #[test]
    fn from_file_errors_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let err = OmpSession::from_file(&path).unwrap_err();
        assert!(
            err.to_string().contains("Failed to open file"),
            "unexpected error: {err}"
        );
    }
}
