//! Stable domain types shared by session scanning, cache, and maintenance.

use crate::codex::CodexSession;
use crate::omp::OmpSession;
use crate::parser::ConversationSession;
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

/// Identifies the system that produced a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionSource {
    /// Claude Code session.
    Claude,
    /// Codex session.
    Codex,
    /// Oh My Pi session.
    Omp,
}

/// Operations supported by a session source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCapabilities {
    /// Whether the session can be opened in its source application.
    pub can_open: bool,
    /// Whether the session can be renamed.
    pub can_rename: bool,
    /// Whether the session can be deleted.
    pub can_delete: bool,
    /// Whether the session participates in synchronization.
    pub participates_in_sync: bool,
}

impl SessionSource {
    /// Returns the stable lowercase source identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Omp => "omp",
        }
    }

    /// Returns the short display label used by session listings.
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "CC",
            Self::Codex => "CX",
            Self::Omp => "OM",
        }
    }

    /// Returns the operations supported by this source.
    pub fn capabilities(self) -> SourceCapabilities {
        match self {
            Self::Claude => SourceCapabilities {
                can_open: true,
                can_rename: true,
                can_delete: true,
                participates_in_sync: true,
            },
            Self::Codex => SourceCapabilities {
                can_open: false,
                can_rename: false,
                can_delete: false,
                participates_in_sync: false,
            },
            Self::Omp => SourceCapabilities {
                can_open: true,
                can_rename: false,
                can_delete: false,
                participates_in_sync: false,
            },
        }
    }
}

impl TryFrom<&str> for SessionSource {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "omp" => Ok(Self::Omp),
            other => anyhow::bail!("Unknown session source: {other}"),
        }
    }
}

/// Stable identity for a session, including its producing source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SessionIdentity {
    /// Source that produced the session.
    pub source: SessionSource,
    /// Source-local session identifier.
    pub session_id: String,
}

/// Filter selecting one or all session sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSourceFilter {
    /// Include sessions from every supported source.
    All,
    /// Include only Claude Code sessions.
    Claude,
    /// Include only Codex sessions.
    Codex,
    /// Include only Oh My Pi sessions.
    Omp,
}

impl SessionSourceFilter {
    pub(crate) fn includes(self, source: SessionSource) -> bool {
        matches!(
            (self, source),
            (Self::All, _)
                | (Self::Claude, SessionSource::Claude)
                | (Self::Codex, SessionSource::Codex)
                | (Self::Omp, SessionSource::Omp)
        )
    }

    pub(crate) fn includes_claude(self) -> bool {
        matches!(self, Self::All | Self::Claude)
    }

    pub(crate) fn includes_codex(self) -> bool {
        matches!(self, Self::All | Self::Codex)
    }

    pub(crate) fn includes_omp(self) -> bool {
        matches!(self, Self::All | Self::Omp)
    }
}

/// Project summary for listing.
#[derive(Debug, Clone)]
pub struct ProjectSummary {
    /// Display name of the project.
    pub name: String,
    /// Filesystem directory containing the project's sessions.
    pub dir_path: PathBuf,
    /// Number of sessions found for the project.
    pub session_count: usize,
    /// Timestamp of the most recent session activity, if available.
    pub last_activity: Option<String>,
}

/// Session summary for listing and operations.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    /// Stable source identifier retained for cache and JSON compatibility.
    pub source: String,
    /// Source-local session identifier.
    pub session_id: String,
    /// User-visible session title.
    pub title: String,
    /// Name of the project associated with the session.
    pub project_name: String,
    /// Filesystem directory associated with the project.
    ///
    /// For Claude this is the encoded `~/.claude/projects/<encoded>` storage
    /// directory, not the directory the session ran in. Use [`SessionSummary::cwd`]
    /// when the real working directory matters.
    pub project_dir: PathBuf,
    /// Real working directory the session ran in, when the source records one.
    pub cwd: Option<String>,
    /// Filesystem path of the source session file.
    pub file_path: PathBuf,
    /// Total number of user and assistant turns.
    pub message_count: usize,
    /// Number of user turns in the session.
    pub user_message_count: usize,
    /// Number of assistant turns in the session.
    pub assistant_message_count: usize,
    /// Timestamp of the first recorded message, if available.
    pub first_timestamp: Option<String>,
    /// Timestamp of the most recent activity, if available.
    pub last_activity: Option<String>,
    /// Size of the source session file in bytes.
    pub file_size: u64,
    /// Whether a user-created custom title protects this session from title replacement.
    pub has_custom_title: bool,
}

impl SessionSummary {
    /// Create a SessionSummary from a ConversationSession.
    /// Message counts use "turn" granularity: consecutive assistant entries
    /// between two user messages count as one assistant turn.
    pub fn from_session(
        session: &ConversationSession,
        project_name: &str,
        project_dir: &Path,
    ) -> Self {
        let file_size = fs::metadata(&session.file_path)
            .map(|m| m.len())
            .unwrap_or(0);

        let mut user_count = 0;
        let mut assistant_count = 0;
        let mut in_assistant_turn = false;

        for entry in &session.entries {
            match entry.entry_type.as_str() {
                "user" => {
                    if ConversationSession::is_tool_result_entry(entry) {
                        continue;
                    }
                    user_count += 1;
                    in_assistant_turn = false;
                }
                "assistant" => {
                    if !in_assistant_turn {
                        assistant_count += 1;
                        in_assistant_turn = true;
                    }
                }
                _ => {}
            }
        }

        SessionSummary {
            source: SessionSource::Claude.as_str().to_string(),
            session_id: session.session_id.clone(),
            title: session.title().unwrap_or_else(|| "(No title)".to_string()),
            project_name: project_name.to_string(),
            project_dir: project_dir.to_path_buf(),
            cwd: session.cwd().map(str::to_string),
            file_path: PathBuf::from(&session.file_path),
            message_count: user_count + assistant_count,
            user_message_count: user_count,
            assistant_message_count: assistant_count,
            first_timestamp: session.first_timestamp(),
            last_activity: session.latest_timestamp(),
            file_size,
            has_custom_title: session.has_custom_title(),
        }
    }

    /// Return whether this summary contains a usable conversation and title.
    ///
    /// This semantic check is shared by active filesystem scans and recycled-file
    /// queries so parseable metadata-only or title-less sessions are never exposed.
    pub(crate) fn is_valid(&self) -> bool {
        self.message_count > 0 && !self.title.trim().is_empty() && self.title != "(No title)"
    }

    /// Get a truncated title for display (Unicode-safe).
    pub fn display_title(&self, max_chars: usize) -> String {
        let title = self.title.replace('\n', " ");
        let chars: Vec<char> = title.chars().collect();

        if chars.len() > max_chars {
            let truncated: String = chars[..max_chars - 3].iter().collect();
            format!("{}...", truncated)
        } else {
            title
        }
    }

    /// Format relative time for display.
    pub fn relative_time(&self) -> String {
        self.last_activity
            .as_ref()
            .map(|ts| format_relative_time(ts))
            .unwrap_or_else(|| "Unknown".to_string())
    }

    pub(crate) fn source_kind(&self) -> Result<SessionSource> {
        SessionSource::try_from(self.source.as_str())
    }

    pub(crate) fn identity(&self) -> Result<SessionIdentity> {
        Ok(SessionIdentity {
            source: self.source_kind()?,
            session_id: self.session_id.clone(),
        })
    }

    /// Create a SessionSummary from a Codex session.
    pub fn from_codex_session(session: &CodexSession, project_name: &str, title: String) -> Self {
        let file_size = fs::metadata(&session.file_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let messages = session.display_messages(false);
        let user_count = messages.iter().filter(|m| m.role == "user").count();
        let assistant_count = messages.iter().filter(|m| m.role == "assistant").count();

        SessionSummary {
            source: SessionSource::Codex.as_str().to_string(),
            session_id: session.session_id.clone(),
            title,
            project_name: project_name.to_string(),
            project_dir: session
                .cwd
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    session
                        .file_path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_default()
                }),
            cwd: session.cwd.clone(),
            file_path: session.file_path.clone(),
            message_count: user_count + assistant_count,
            user_message_count: user_count,
            assistant_message_count: assistant_count,
            first_timestamp: session.first_timestamp(),
            last_activity: session.latest_timestamp(),
            file_size,
            has_custom_title: false,
        }
    }

    /// Create a SessionSummary from an OMP session.
    pub fn from_omp_session(session: &OmpSession, project_name: &str) -> Self {
        let file_size = fs::metadata(&session.file_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let messages = session.display_messages();
        let user_count = messages.iter().filter(|m| m.role == "user").count();
        let assistant_count = messages.iter().filter(|m| m.role == "assistant").count();
        let title = session.title_from_messages(&messages);

        SessionSummary {
            source: SessionSource::Omp.as_str().to_string(),
            session_id: session.session_id.clone(),
            title,
            project_name: project_name.to_string(),
            project_dir: session
                .cwd
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    session
                        .file_path
                        .parent()
                        .and_then(|p| p.parent())
                        .map(Path::to_path_buf)
                        .unwrap_or_default()
                }),
            cwd: session.cwd.clone(),
            file_path: session.file_path.clone(),
            message_count: user_count + assistant_count,
            user_message_count: user_count,
            assistant_message_count: assistant_count,
            first_timestamp: session.first_timestamp(),
            last_activity: session.latest_timestamp(),
            file_size,
            has_custom_title: false,
        }
    }
}

/// Format a timestamp as relative time (e.g., "Today", "Yesterday", "3 days ago").
pub(crate) fn format_relative_time(timestamp: &str) -> String {
    use chrono::{DateTime, Utc};

    if let Ok(dt) = DateTime::parse_from_rfc3339(timestamp) {
        let now = Utc::now();
        let dt_utc = dt.with_timezone(&Utc);
        let duration = now.signed_duration_since(dt_utc);

        let days = duration.num_days();
        let hours = duration.num_hours();
        let minutes = duration.num_minutes();

        if days == 0 {
            if hours == 0 {
                if minutes <= 1 {
                    "Just now".to_string()
                } else {
                    format!("{} min ago", minutes)
                }
            } else if hours == 1 {
                "1 hour ago".to_string()
            } else {
                format!("{} hours ago", hours)
            }
        } else if days == 1 {
            "Yesterday".to_string()
        } else if days < 7 {
            format!("{} days ago", days)
        } else if days < 30 {
            let weeks = days / 7;
            if weeks == 1 {
                "1 week ago".to_string()
            } else {
                format!("{} weeks ago", weeks)
            }
        } else {
            let months = days / 30;
            if months == 1 {
                "1 month ago".to_string()
            } else {
                format!("{} months ago", months)
            }
        }
    } else {
        "Unknown".to_string()
    }
}

/// Extract a Claude session ID from a JSONL filename without depending on project layout.
pub(crate) fn claude_session_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    Some(name.strip_suffix(".jsonl")?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_session_preserves_custom_title_protection_signal() {
        let user_entry: crate::parser::ConversationEntry = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":"hello"},"sessionId":"s1"}"#,
        )
        .unwrap();
        let custom_title_entry: crate::parser::ConversationEntry = serde_json::from_str(
            r#"{"type":"custom-title","customTitle":"renamed","sessionId":"s1"}"#,
        )
        .unwrap();

        let plain = ConversationSession {
            session_id: "s1".to_string(),
            entries: vec![user_entry.clone()],
            file_path: "s1.jsonl".to_string(),
        };
        let renamed = ConversationSession {
            session_id: "s1".to_string(),
            entries: vec![user_entry, custom_title_entry],
            file_path: "s1.jsonl".to_string(),
        };

        assert!(!SessionSummary::from_session(&plain, "project", Path::new(".")).has_custom_title);
        assert!(SessionSummary::from_session(&renamed, "project", Path::new(".")).has_custom_title);
    }

    #[test]
    fn claude_session_id_from_path_preserves_file_stem_exactly() {
        assert_eq!(
            claude_session_id_from_path(Path::new("encoded/project/session-abc.jsonl")),
            Some("session-abc".to_string())
        );
        assert_eq!(
            claude_session_id_from_path(Path::new(
                "project/550e8400-e29b-41d4-a716-446655440000.jsonl"
            )),
            Some("550e8400-e29b-41d4-a716-446655440000".to_string())
        );
        assert_eq!(
            claude_session_id_from_path(Path::new("project/abc.txt")),
            None
        );
    }

    #[test]
    fn is_valid_requires_messages_and_a_real_title() {
        let mut summary = SessionSummary {
            source: "claude".to_string(),
            session_id: "s1".to_string(),
            title: "A real title".to_string(),
            project_name: "project".to_string(),
            project_dir: PathBuf::from("."),
            cwd: None,
            file_path: PathBuf::from("s1.jsonl"),
            message_count: 1,
            user_message_count: 1,
            assistant_message_count: 0,
            first_timestamp: None,
            last_activity: None,
            file_size: 1,
            has_custom_title: false,
        };
        assert!(summary.is_valid());

        summary.message_count = 0;
        assert!(!summary.is_valid());
        summary.message_count = 1;
        summary.title = "(No title)".to_string();
        assert!(!summary.is_valid());
        summary.title.clear();
        assert!(!summary.is_valid());
    }
}
