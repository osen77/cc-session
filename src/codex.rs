use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::parser::ParseOutcome;
use crate::session_diagnostics::legacy_walk_entry;

#[derive(Debug, Clone)]
pub struct CodexSession {
    pub session_id: String,
    pub entries: Vec<CodexEntry>,
    pub file_path: PathBuf,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CodexEntry {
    pub entry_type: String,
    pub timestamp: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct CodexDisplayMessage {
    pub role: String,
    pub timestamp: Option<String>,
    pub content: String,
}

impl CodexSession {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Self::from_file_with_report(path)?.value)
    }

    pub fn from_file_with_report<P: AsRef<Path>>(path: P) -> Result<ParseOutcome<Self>> {
        let path = path.as_ref();
        let file =
            File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;
        let reader = BufReader::new(file);

        let mut entries = Vec::new();
        let mut session_id = None;
        let mut cwd = None;
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
                        "Skipping malformed Codex line {} in {}: {}",
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
            let timestamp = value
                .get("timestamp")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let payload = value.get("payload").cloned().unwrap_or(Value::Null);

            if entry_type == "session_meta" {
                if session_id.is_none() {
                    session_id = payload
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                if cwd.is_none() {
                    cwd = payload
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
            }

            entries.push(CodexEntry {
                entry_type,
                timestamp,
                payload,
            });
        }

        let session_id = session_id
            .or_else(|| session_id_from_filename(path))
            .with_context(|| format!("No Codex session ID found: {}", path.display()))?;

        Ok(ParseOutcome {
            value: Self {
                session_id,
                entries,
                file_path: path.to_path_buf(),
                cwd,
            },
            malformed_lines,
        })
    }

    pub fn display_messages(&self, include_tools: bool) -> Vec<CodexDisplayMessage> {
        let mut messages = Vec::new();

        for entry in &self.entries {
            match entry.entry_type.as_str() {
                "response_item" => {
                    let role = entry
                        .payload
                        .get("role")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if role != "user" && role != "assistant" {
                        continue;
                    }
                    if let Some(content) = extract_content_text(&entry.payload) {
                        messages.push(CodexDisplayMessage {
                            role: role.to_string(),
                            timestamp: entry.timestamp.clone(),
                            content,
                        });
                    }
                }
                "event_msg" if include_tools => {
                    if let Some(content) = extract_event_text(&entry.payload) {
                        messages.push(CodexDisplayMessage {
                            role: "tool".to_string(),
                            timestamp: entry.timestamp.clone(),
                            content,
                        });
                    }
                }
                _ => {}
            }
        }

        messages
    }

    pub fn title(&self, history_title: Option<&str>) -> String {
        if let Some(title) = history_title {
            let title = title.trim();
            if !title.is_empty() {
                return title.to_string();
            }
        }

        self.display_messages(false)
            .into_iter()
            .find(|m| m.role == "user" && !m.content.trim().is_empty())
            .map(|m| m.content)
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

    pub fn project_name(&self) -> Option<&str> {
        self.cwd.as_deref().and_then(last_path_component)
    }
}

pub fn codex_sessions_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    Ok(home.join(".codex").join("sessions"))
}

pub fn codex_history_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    Ok(home.join(".codex").join("history.jsonl"))
}

#[allow(dead_code)]
pub fn discover_codex_sessions(base_path: &Path) -> Result<Vec<CodexSession>> {
    if !base_path.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for entry in WalkDir::new(base_path).follow_links(false).into_iter() {
        let Some(entry) = legacy_walk_entry(entry, "codex") else {
            continue;
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        match CodexSession::from_file(path) {
            Ok(session) => sessions.push(session),
            Err(_) => log::warn!(
                target: crate::logger::SCAN_DIAGNOSTICS_TARGET,
                "legacy codex session discovery skipped an unparseable file"
            ),
        }
    }

    sessions.sort_by_key(|s| s.latest_timestamp());
    sessions.reverse();
    Ok(sessions)
}

pub fn load_codex_history_titles(path: &Path) -> Result<std::collections::HashMap<String, String>> {
    let mut titles = std::collections::HashMap::new();
    if !path.exists() {
        return Ok(titles);
    }

    let file =
        File::open(path).with_context(|| format!("Failed to open file: {}", path.display()))?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(session_id) = value.get("session_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(text) = value.get("text").and_then(|v| v.as_str()) else {
            continue;
        };
        titles
            .entry(session_id.to_string())
            .or_insert_with(|| text.to_string());
    }

    Ok(titles)
}

fn extract_content_text(payload: &Value) -> Option<String> {
    let content = payload.get("content")?.as_array()?;
    let parts: Vec<String> = content
        .iter()
        .filter_map(|item| {
            item.get("text")
                .or_else(|| item.get("input_text"))
                .or_else(|| item.get("output_text"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty() && !is_system_content(s))
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn extract_event_text(payload: &Value) -> Option<String> {
    let event_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "exec_command" | "tool_call" | "patch_apply" => {
            let command = payload
                .get("command")
                .and_then(|v| v.as_str())
                .or_else(|| payload.get("name").and_then(|v| v.as_str()));
            let stdout = payload
                .get("stdout")
                .or_else(|| payload.get("stderr"))
                .or_else(|| payload.get("formatted_output"))
                .and_then(|v| v.as_str());

            match (command, stdout) {
                (Some(command), Some(stdout)) if !stdout.trim().is_empty() => {
                    Some(format!("[Tool: {}]\n{}", command, stdout.trim()))
                }
                (Some(command), _) => Some(format!("[Tool: {}]", command)),
                (_, Some(stdout)) if !stdout.trim().is_empty() => Some(stdout.trim().to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn session_id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    stem.rsplit('-').next().map(|s| s.to_string())
}

pub(crate) fn last_path_component(path: &str) -> Option<&str> {
    path.split(&['/', '\\']).rfind(|s| !s.is_empty())
}

pub(crate) fn is_system_content(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("# AGENTS.md instructions")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<permissions instructions>")
        || trimmed.starts_with("<collaboration_mode>")
        || trimmed.starts_with("<skills_instructions>")
        || trimmed.starts_with("<system>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn report_counts_malformed_lines_and_keeps_valid_codex_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-2026-05-10T00-00-00-abc123.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{not valid json\n",
                r#"{"type":"session_meta","payload":{"id":"partial-codex","cwd":"/tmp/demo"}}"#,
                "\n"
            ),
        )
        .unwrap();

        let outcome = CodexSession::from_file_with_report(&path).unwrap();
        assert_eq!(outcome.malformed_lines, 1);
        assert_eq!(outcome.value.session_id, "partial-codex");
        assert_eq!(outcome.value.entries.len(), 1);
    }

    #[test]
    fn parses_codex_session_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-2026-05-10T00-00-00-abc123.jsonl");
        fs::write(
            &path,
            r#"{"type":"session_meta","timestamp":"2026-05-10T00:00:00Z","payload":{"id":"abc123","cwd":"/tmp/demo"}}
{"type":"response_item","timestamp":"2026-05-10T00:01:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello codex"}]}}
{"type":"response_item","timestamp":"2026-05-10T00:02:00Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi user"}]}}
"#,
        )
        .unwrap();

        let session = CodexSession::from_file(&path).unwrap();
        assert_eq!(session.session_id, "abc123");
        assert_eq!(session.project_name(), Some("demo"));

        let messages = session.display_messages(false);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "hello codex");
        assert_eq!(messages[1].role, "assistant");
    }

    #[test]
    fn loads_first_history_title_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        fs::write(
            &path,
            r#"{"session_id":"s1","ts":1,"text":"first title"}
{"session_id":"s1","ts":2,"text":"second title"}
"#,
        )
        .unwrap();

        let titles = load_codex_history_titles(&path).unwrap();
        assert_eq!(titles.get("s1").map(String::as_str), Some("first title"));
    }
}
