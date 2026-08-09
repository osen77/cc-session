use anyhow::{Context, Result};
use colored::Colorize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::filter::FilterConfig;
use crate::parser::ConversationSession;
use crate::path_security::{
    validate_directory_candidate, validate_directory_root, validate_regular_candidate,
};
use crate::session_diagnostics::legacy_walk_entry;

/// Threshold for warning about large conversation files (10 MB)
pub(crate) const LARGE_FILE_WARNING_THRESHOLD: u64 = 10 * 1024 * 1024;

/// Get the Claude Code projects directory
pub(crate) fn claude_projects_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    Ok(home.join(".claude").join("projects"))
}

/// Discover all conversation sessions in Claude Code history
///
/// When multiple files share the same session ID (e.g., main conversation and agent
/// subprocess files), this function deduplicates by keeping the one with the most
/// messages. This prevents agent files from overwriting main conversation files
/// during merge operations.
pub(crate) fn discover_sessions(
    base_path: &Path,
    filter: &FilterConfig,
) -> Result<Vec<ConversationSession>> {
    let mut sessions = Vec::new();

    for entry in WalkDir::new(base_path).follow_links(false).into_iter() {
        let Some(entry) = legacy_walk_entry(entry, "claude") else {
            continue;
        };
        let path = entry.path();

        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            if !filter.should_include(path) {
                continue;
            }
            if validate_regular_candidate(base_path, path).is_err() {
                continue;
            }

            match ConversationSession::from_file(path) {
                Ok(session) => sessions.push(session),
                Err(_) => {
                    log::warn!(target: crate::logger::SCAN_DIAGNOSTICS_TARGET, "legacy Claude session discovery skipped an unparseable file");
                }
            }
        }
    }

    // Deduplicate by session_id, keeping the session with the most messages.
    // This handles cases where agent subprocess files share the same session_id
    // as the main conversation file - we want to keep the main file (more messages).
    let mut session_map: HashMap<String, ConversationSession> = HashMap::new();
    for session in sessions {
        session_map
            .entry(session.session_id.clone())
            .and_modify(|existing| {
                // Keep the session with more messages
                if session.message_count() > existing.message_count() {
                    log::debug!(
                        "Deduplicating session {}: replacing {} messages with {} messages",
                        session.session_id,
                        existing.message_count(),
                        session.message_count()
                    );
                    *existing = session.clone();
                } else {
                    log::debug!(
                        "Deduplicating session {}: keeping {} messages, discarding {} messages",
                        existing.session_id,
                        existing.message_count(),
                        session.message_count()
                    );
                }
            })
            .or_insert(session);
    }

    Ok(session_map.into_values().collect())
}

/// Check for large conversation files and emit warnings
///
/// This helps users identify conversations that may be bloated with excessive
/// file history, token usage, or other data. Large conversations can slow down
/// sync operations and consume significant disk space.
///
/// # Arguments
/// * `file_paths` - Iterator of file paths to check
pub(crate) fn warn_large_files<P, I>(file_paths: I)
where
    P: AsRef<Path>,
    I: IntoIterator<Item = P>,
{
    for path in file_paths {
        let path = path.as_ref();

        if let Ok(metadata) = fs::metadata(path) {
            let size = metadata.len();

            if size >= LARGE_FILE_WARNING_THRESHOLD {
                let size_mb = size as f64 / (1024.0 * 1024.0);
                println!(
                    "  {} Large conversation file detected: {} ({:.1} MB)",
                    "⚠️ ".yellow().bold(),
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown"),
                    size_mb
                );
                println!(
                    "     {}",
                    "Consider archiving or cleaning up this conversation to improve sync performance"
                        .dimmed()
                );
            }
        }
    }
}

/// Extract project name from Claude's encoded project directory name.
///
/// Claude encodes project paths by replacing '/' with '-', so a project at
/// `/Users/abc/Documents/GitHub/myproject` becomes `-Users-abc-Documents-GitHub-myproject`.
/// This function extracts the last segment (the actual project name).
///
/// # Examples
/// - Input: `-Users-abc-Documents-GitHub-myproject` -> Output: `myproject`
/// - Input: `myproject` -> Output: `myproject`
/// - Input: `-root-projects-test` -> Output: `test`
pub fn extract_project_name(encoded_path: &str) -> &str {
    // The encoded path uses '-' as separator (from path encoding)
    // Take the last non-empty segment
    encoded_path
        .rsplit('-')
        .find(|s| !s.is_empty())
        .unwrap_or(encoded_path)
}

/// Find a local Claude project directory that matches the given project name.
///
/// Scans `~/.claude/projects/` for directories that match the specified project name.
/// Uses two strategies and merges results:
/// 1. Fast: extract project name from encoded directory name (unreliable for hyphenated names)
/// 2. Precise: read JSONL files and extract project name from `cwd` field
///
/// # Returns
/// - `Some(PathBuf)` if exactly one matching project directory is found
/// - `None` if no match found or multiple matches (ambiguous)
pub fn find_local_project_by_name(
    claude_projects_dir: &Path,
    project_name: &str,
) -> Option<PathBuf> {
    validate_directory_root(claude_projects_dir).ok()?;
    let entries: Vec<_> = std::fs::read_dir(claude_projects_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let path = entry.path();
            matches!(
                std::fs::symlink_metadata(&path),
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink()
            ) && validate_directory_candidate(claude_projects_dir, &path).is_ok()
        })
        .collect();

    // Pass 1 (fast): match by encoded directory name.
    // Skip directories whose name ends with '-' : Claude Code encodes each
    // non-ASCII char (e.g. Chinese) as a single '-', so a dir like
    // "-Users-mini-Documents-Projects-----" (cwd .../Projects/安装环境) ends with
    // dashes and extract_project_name() would misread the PARENT segment
    // ("Projects") as the project name, causing a false collision with a real
    // sibling of that name. Such dirs are left to the precise Pass 2 (cwd).
    let matches_from_dir: Vec<PathBuf> = entries
        .iter()
        .filter(|e| {
            e.file_name().to_str().is_some_and(|name| {
                !name.ends_with('-') && extract_project_name(name) == project_name
            })
        })
        .map(|e| e.path())
        .collect();

    // Pass 2 (precise): read JSONL cwd to get real project name
    // Always run - extract_project_name is unreliable for hyphenated names
    let mut matches_from_cwd: Vec<PathBuf> = Vec::new();
    for entry in &entries {
        let dir_path = entry.path();
        if let Some(real_name) = get_project_name_from_dir(&dir_path) {
            if real_name == project_name {
                matches_from_cwd.push(dir_path);
            }
        }
    }

    // Merge: cwd matches are authoritative, supplement with dir-name matches
    let mut all_matches = matches_from_cwd;
    for path in matches_from_dir {
        if !all_matches.contains(&path) {
            all_matches.push(path);
        }
    }

    match all_matches.len() {
        1 => Some(all_matches.into_iter().next().unwrap()),
        n if n > 1 => {
            log::warn!(
                "Ambiguous match: {} local directories match project '{}': {:?}",
                n,
                project_name,
                all_matches
            );
            None
        }
        _ => None,
    }
}

/// Extract the real project name from a local project directory by reading its JSONL files.
fn get_project_name_from_dir(dir_path: &Path) -> Option<String> {
    validate_directory_root(dir_path).ok()?;
    let files = std::fs::read_dir(dir_path).ok()?;
    for file_entry in files.filter_map(|file| file.ok()) {
        let file_path = file_entry.path();
        if file_path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&file_path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        if validate_regular_candidate(dir_path, &file_path).is_err() {
            continue;
        }
        if let Ok(session) = crate::parser::ConversationSession::from_file(&file_path) {
            if let Some(real_name) = session.project_name() {
                return Some(real_name.to_string());
            }
        }
    }
    None
}

/// Get all project directories in Claude's projects folder that would map to the same project name.
/// Used for collision detection when `use_project_name_only` is enabled.
pub fn find_colliding_projects(
    claude_projects_dir: &Path,
) -> std::collections::HashMap<String, Vec<PathBuf>> {
    use std::collections::HashMap;

    let mut collisions: HashMap<String, Vec<PathBuf>> = HashMap::new();

    if validate_directory_root(claude_projects_dir).is_err() {
        return collisions;
    }
    if let Ok(entries) = std::fs::read_dir(claude_projects_dir) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || validate_directory_candidate(claude_projects_dir, &path).is_err()
            {
                continue;
            }

            // Prefer real project name from JSONL cwd, fall back to dir name extraction.
            // Skip the fallback when the dir name ends with '-': a non-ASCII
            // project name (e.g. Chinese) encodes to trailing dashes, and
            // extract_project_name would misread the PARENT segment as the
            // project name, producing a false collision with a real sibling.
            // Without a reliable name such a dir cannot be grouped at all.
            let project_name = get_project_name_from_dir(&path).unwrap_or_else(|| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .filter(|n| !n.ends_with('-'))
                    .map(|n| extract_project_name(n).to_string())
                    .unwrap_or_default()
            });
            if !project_name.is_empty() {
                collisions.entry(project_name).or_default().push(path);
            }
        }
    }

    // Only keep entries with more than one project (actual collisions)
    collisions.retain(|_, paths| paths.len() > 1);
    collisions
}

/// Result of checking sync repo directory structure consistency
#[derive(Debug)]
#[allow(dead_code)]
pub struct DirectoryStructureCheck {
    /// Directories using full path format (e.g., -Users-abc-project)
    pub full_path_dirs: Vec<String>,
    /// Directories using project name only format (e.g., project)
    pub project_name_dirs: Vec<String>,
    /// Whether the structure is consistent with the given config
    pub is_consistent: bool,
    /// Warning message if inconsistent
    pub warning: Option<String>,
}

/// Check if the sync repo directory structure is consistent with the current config.
///
/// This helps detect when the user has switched modes and may have mixed directory formats.
///
/// # Arguments
/// * `sync_repo_projects_dir` - Path to the projects directory in the sync repo
/// * `use_project_name_only` - The current config setting
///
/// # Returns
/// A `DirectoryStructureCheck` with details about the directory structure
pub fn check_directory_structure_consistency(
    sync_repo_projects_dir: &Path,
    use_project_name_only: bool,
) -> DirectoryStructureCheck {
    let mut full_path_dirs = Vec::new();
    let mut project_name_dirs = Vec::new();

    if let Ok(entries) = std::fs::read_dir(sync_repo_projects_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                    // Skip hidden directories
                    if dir_name.starts_with('.') {
                        continue;
                    }

                    // Check if it looks like a full path (starts with - and contains multiple -)
                    // e.g., -Users-abc-Documents-project
                    let dash_count = dir_name.matches('-').count();
                    if dir_name.starts_with('-') && dash_count >= 3 {
                        full_path_dirs.push(dir_name.to_string());
                    } else {
                        project_name_dirs.push(dir_name.to_string());
                    }
                }
            }
        }
    }

    let has_full_path = !full_path_dirs.is_empty();
    let has_project_name = !project_name_dirs.is_empty();

    // Determine consistency
    let (is_consistent, warning) = if has_full_path && has_project_name {
        // Mixed mode - always inconsistent
        (
            false,
            Some(format!(
                "检测到混合目录格式：{} 个完整路径格式，{} 个项目名格式。\n\
                 这可能导致数据重复。建议清理或统一目录格式。",
                full_path_dirs.len(),
                project_name_dirs.len()
            )),
        )
    } else if use_project_name_only && has_full_path && !has_project_name {
        // Config says project-name-only but repo has full paths
        (
            false,
            Some(format!(
                "配置为「多设备同步」模式，但同步仓库中存在 {} 个完整路径格式的目录。\n\
                 建议清理这些目录或切换回「单设备备份」模式。",
                full_path_dirs.len()
            )),
        )
    } else if !use_project_name_only && has_project_name && !has_full_path {
        // Config says full path but repo has project names only
        (
            false,
            Some(format!(
                "配置为「单设备备份」模式，但同步仓库中存在 {} 个项目名格式的目录。\n\
                 建议切换到「多设备同步」模式以保持一致。",
                project_name_dirs.len()
            )),
        )
    } else {
        (true, None)
    };

    DirectoryStructureCheck {
        full_path_dirs,
        project_name_dirs,
        is_consistent,
        warning,
    }
}

/// Get list of memory files that exist in a directory
#[allow(dead_code)]
pub fn list_memory_files(memory_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    if let Ok(entries) = fs::read_dir(memory_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                files.push(entry.path());
            }
        }
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_extract_project_name_basic() {
        // Standard encoded path
        assert_eq!(
            extract_project_name("-Users-abc-Documents-GitHub-myproject"),
            "myproject"
        );
    }

    #[test]
    fn test_extract_project_name_simple() {
        // Already just a project name
        assert_eq!(extract_project_name("myproject"), "myproject");
    }

    #[test]
    fn test_extract_project_name_short_path() {
        // Short encoded path
        assert_eq!(extract_project_name("-root-project"), "project");
    }

    #[test]
    fn test_extract_project_name_empty() {
        // Empty string edge case
        assert_eq!(extract_project_name(""), "");
    }

    #[test]
    fn test_extract_project_name_single_segment() {
        // Path with trailing dash
        assert_eq!(extract_project_name("-myproject"), "myproject");
    }

    #[test]
    fn test_find_local_project_by_name_single_match() {
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        // Create a project directory
        fs::create_dir(projects_dir.join("-Users-abc-Documents-myproject")).unwrap();

        let result = find_local_project_by_name(projects_dir, "myproject");
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("-Users-abc-Documents-myproject"));
    }

    #[test]
    fn test_find_local_project_by_name_no_match() {
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        // Create a project directory with different name
        fs::create_dir(projects_dir.join("-Users-abc-Documents-otherproject")).unwrap();

        let result = find_local_project_by_name(projects_dir, "myproject");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_local_project_by_name_multiple_matches() {
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        // Create two project directories with same project name
        fs::create_dir(projects_dir.join("-Users-abc-work-myproject")).unwrap();
        fs::create_dir(projects_dir.join("-Users-abc-personal-myproject")).unwrap();

        // Should return None for ambiguous matches
        let result = find_local_project_by_name(projects_dir, "myproject");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_colliding_projects_no_collisions() {
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        // Create directories with unique project names
        fs::create_dir(projects_dir.join("-Users-abc-project1")).unwrap();
        fs::create_dir(projects_dir.join("-Users-abc-project2")).unwrap();

        let collisions = find_colliding_projects(projects_dir);
        assert!(collisions.is_empty());
    }

    #[test]
    fn test_find_colliding_projects_with_collisions() {
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        // Create directories that map to the same project name
        fs::create_dir(projects_dir.join("-Users-abc-work-myapp")).unwrap();
        fs::create_dir(projects_dir.join("-Users-abc-personal-myapp")).unwrap();
        fs::create_dir(projects_dir.join("-Users-abc-unique")).unwrap();

        let collisions = find_colliding_projects(projects_dir);
        assert_eq!(collisions.len(), 1);
        assert!(collisions.contains_key("myapp"));
        assert_eq!(collisions.get("myapp").unwrap().len(), 2);
    }

    #[test]
    fn test_discover_sessions_deduplicates_by_session_id() {
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        // Create a main conversation file with many messages
        let main_file = projects_dir.join("session-123.jsonl");
        let mut file = fs::File::create(&main_file).unwrap();
        // Write 10 user/assistant message pairs
        for i in 0..10 {
            writeln!(
                file,
                r#"{{"type":"user","sessionId":"session-123","uuid":"user-{i}","timestamp":"2025-01-01T{i:02}:00:00Z"}}"#,
            )
            .unwrap();
            writeln!(
                file,
                r#"{{"type":"assistant","sessionId":"session-123","uuid":"assistant-{i}","parentUuid":"user-{i}","timestamp":"2025-01-01T{i:02}:01:00Z"}}"#,
            )
            .unwrap();
        }

        // Create an agent subprocess file with only 2 messages (same session ID)
        let agent_file = projects_dir.join("agent-abc.jsonl");
        let mut file = fs::File::create(&agent_file).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","sessionId":"session-123","uuid":"agent-user-1","timestamp":"2025-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","sessionId":"session-123","uuid":"agent-assistant-1","parentUuid":"agent-user-1","timestamp":"2025-01-01T00:01:00Z"}}"#,
        )
        .unwrap();

        // Discover sessions
        let filter = crate::filter::FilterConfig::default();
        let sessions = discover_sessions(projects_dir, &filter).unwrap();

        // Should only have 1 session (deduplicated)
        assert_eq!(sessions.len(), 1, "Should deduplicate to 1 session");

        // The session should have 20 messages (from main file), not 2 (from agent file)
        let session = &sessions[0];
        assert_eq!(session.session_id, "session-123");
        assert_eq!(
            session.message_count(),
            20,
            "Should keep the session with more messages"
        );
    }

    #[test]
    fn test_discover_sessions_no_duplicates() {
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        // Create two files with different session IDs
        let file1 = projects_dir.join("session-1.jsonl");
        let mut file = fs::File::create(&file1).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","sessionId":"session-1","uuid":"1","timestamp":"2025-01-01T00:00:00Z"}}"#,
        )
        .unwrap();

        let file2 = projects_dir.join("session-2.jsonl");
        let mut file = fs::File::create(&file2).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","sessionId":"session-2","uuid":"2","timestamp":"2025-01-01T00:00:00Z"}}"#,
        )
        .unwrap();

        // Discover sessions
        let filter = crate::filter::FilterConfig::default();
        let sessions = discover_sessions(projects_dir, &filter).unwrap();

        // Should have 2 sessions (no deduplication needed)
        assert_eq!(sessions.len(), 2, "Should have 2 distinct sessions");
    }

    /// Helper: create a JSONL file with a cwd field to simulate a real session
    fn create_session_with_cwd(dir: &Path, session_id: &str, cwd: &str) {
        let file_path = dir.join(format!("{}.jsonl", session_id));
        let mut file = fs::File::create(&file_path).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","sessionId":"{session_id}","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","cwd":"{cwd}","message":{{"role":"user","content":"hello"}}}}"#,
        )
        .unwrap();
    }

    /// Helper: create a JSONL file WITHOUT a cwd field (e.g. a snapshot/summary
    /// file). Such files make get_project_name_from_dir fall back to the
    /// encoded directory name.
    fn create_session_without_cwd(dir: &Path, session_id: &str) {
        let file_path = dir.join(format!("{}.jsonl", session_id));
        let mut file = fs::File::create(&file_path).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","sessionId":"{session_id}","uuid":"u1","timestamp":"2025-01-01T00:00:00Z","message":{{"role":"user","content":"hello"}}}}"#,
        )
        .unwrap();
    }

    #[test]
    fn test_find_local_project_hyphenated_name() {
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        // "-Users-abc-ux-workspace" — extract_project_name returns "workspace", not "ux-workspace"
        let project_dir = projects_dir.join("-Users-abc-ux-workspace");
        fs::create_dir(&project_dir).unwrap();
        create_session_with_cwd(&project_dir, "sess-1", "/Users/abc/ux-workspace");

        let result = find_local_project_by_name(projects_dir, "ux-workspace");
        assert!(result.is_some(), "Should match via JSONL cwd");
        assert!(result.unwrap().ends_with("-Users-abc-ux-workspace"));
    }

    #[test]
    fn test_find_local_project_hyphenated_multiple_dirs_ambiguous() {
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        // Two directories both resolve to project name "ux-workspace" via cwd
        let dir1 = projects_dir.join("-Users-abc-Documents-ux-workspace");
        fs::create_dir(&dir1).unwrap();
        create_session_with_cwd(&dir1, "sess-1", "/Users/abc/Documents/ux-workspace");

        let dir2 = projects_dir.join("-Users-abc-Coding-ux-workspace");
        fs::create_dir(&dir2).unwrap();
        create_session_with_cwd(&dir2, "sess-2", "/Users/abc/Coding/ux-workspace");

        let result = find_local_project_by_name(projects_dir, "ux-workspace");
        assert!(result.is_none(), "Should return None for ambiguous match");
    }

    #[test]
    fn test_find_local_project_non_ascii_sibling_no_false_ambiguity() {
        // Regression: a project whose name contains non-ASCII chars (e.g. Chinese
        // "安装环境") is encoded by Claude Code as trailing dashes in the directory
        // name. extract_project_name() then misreads the PARENT segment as the
        // project name, falsely colliding with a real sibling project of that name.
        //
        // Real layout that triggered this on the user's machine:
        //   -Users-mini-Documents-Projects            cwd=/Users/mini/Documents/Projects
        //   -Users-mini-Documents-Projects-----       cwd=/Users/mini/Documents/Projects/安装环境
        // Looking up "Projects" must resolve to the first dir only, NOT None.
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        let real_projects = projects_dir.join("-Users-mini-Documents-Projects");
        fs::create_dir(&real_projects).unwrap();
        create_session_with_cwd(&real_projects, "sess-1", "/Users/mini/Documents/Projects");

        // Non-ASCII project name "安装环境" (4 chars) -> 4 trailing dashes after the
        // separator dash, so the encoded dir name ends with "Projects-----".
        let non_ascii_sibling = projects_dir.join("-Users-mini-Documents-Projects-----");
        fs::create_dir(&non_ascii_sibling).unwrap();
        create_session_with_cwd(
            &non_ascii_sibling,
            "sess-2",
            "/Users/mini/Documents/Projects/安装环境",
        );

        let result = find_local_project_by_name(projects_dir, "Projects");
        assert!(
            result.is_some(),
            "Non-ASCII sibling must not cause false ambiguity with parent-named project"
        );
        assert!(result.unwrap().ends_with("-Users-mini-Documents-Projects"));
    }

    #[test]
    fn test_find_colliding_projects_hyphenated_names() {
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        // Two dirs with hyphenated project names that collide
        let dir1 = projects_dir.join("-Users-abc-my-app");
        fs::create_dir(&dir1).unwrap();
        create_session_with_cwd(&dir1, "sess-1", "/Users/abc/my-app");

        let dir2 = projects_dir.join("-Users-abc-work-my-app");
        fs::create_dir(&dir2).unwrap();
        create_session_with_cwd(&dir2, "sess-2", "/Users/abc/work/my-app");

        let collisions = find_colliding_projects(projects_dir);
        assert_eq!(collisions.len(), 1);
        assert!(collisions.contains_key("my-app"));
        assert_eq!(collisions.get("my-app").unwrap().len(), 2);
    }

    #[test]
    fn test_find_colliding_projects_non_ascii_sibling_no_false_collision() {
        // Companion to test_find_local_project_non_ascii_sibling_no_false_ambiguity:
        // a non-ASCII project (cwd known) must NOT be grouped under the parent
        // project name by find_colliding_projects. "安装环境" is distinct from
        // "Projects", so no collision should be reported.
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        let real_projects = projects_dir.join("-Users-mini-Documents-Projects");
        fs::create_dir(&real_projects).unwrap();
        create_session_with_cwd(&real_projects, "sess-1", "/Users/mini/Documents/Projects");

        let non_ascii_sibling = projects_dir.join("-Users-mini-Documents-Projects-----");
        fs::create_dir(&non_ascii_sibling).unwrap();
        create_session_with_cwd(
            &non_ascii_sibling,
            "sess-2",
            "/Users/mini/Documents/Projects/安装环境",
        );

        let collisions = find_colliding_projects(projects_dir);
        assert!(
            collisions.is_empty(),
            "Non-ASCII sibling with known cwd must not collide with parent-named project"
        );
    }

    #[test]
    fn legacy_claude_parser_warning_uses_safe_diagnostics_record() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/sync/discovery.rs"
        ));
        let implementation = source.split("#[cfg(test)]").next().unwrap();

        assert!(implementation.contains("target: crate::logger::SCAN_DIAGNOSTICS_TARGET"));
        assert!(
            implementation.contains("legacy Claude session discovery skipped an unparseable file")
        );
        assert!(!implementation.contains("Failed to parse {}: {}"));
        assert!(!implementation.contains("path.display(), e"));
    }

    #[test]
    fn test_find_colliding_projects_non_ascii_no_cwd_no_false_collision() {
        // When a non-ASCII project dir has NO cwd (only snapshot/summary files),
        // get_project_name_from_dir returns None and the code falls back to
        // extract_project_name(dir_name). For "...-Projects-----" (安装环境) that
        // misreads the parent segment "Projects" as the project name, falsely
        // colliding with a real sibling project of that name and producing a
        // spurious push warning. A dir whose real name cannot be determined
        // must not be grouped under a misread parent name.
        let temp_dir = tempdir().unwrap();
        let projects_dir = temp_dir.path();

        let real_projects = projects_dir.join("-Users-mini-Documents-Projects");
        fs::create_dir(&real_projects).unwrap();
        create_session_with_cwd(&real_projects, "sess-1", "/Users/mini/Documents/Projects");

        // Non-ASCII sibling whose only JSONL has no cwd -> triggers the fallback.
        let non_ascii_sibling = projects_dir.join("-Users-mini-Documents-Projects-----");
        fs::create_dir(&non_ascii_sibling).unwrap();
        create_session_without_cwd(&non_ascii_sibling, "sess-2");

        let collisions = find_colliding_projects(projects_dir);
        assert!(
            collisions.is_empty(),
            "Non-ASCII dir without cwd must not collide with parent-named project via fallback"
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_matching_rejects_root_and_project_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let outside = temp.path().join("outside");
        let outside_project = outside.join("myproject");
        fs::create_dir_all(&outside_project).unwrap();

        let root_link = temp.path().join("projects-link");
        symlink(&outside, &root_link).unwrap();
        assert!(find_local_project_by_name(&root_link, "myproject").is_none());
        assert!(find_colliding_projects(&root_link).is_empty());

        let projects = temp.path().join("projects");
        fs::create_dir_all(&projects).unwrap();
        symlink(&outside_project, projects.join("myproject")).unwrap();
        assert!(find_local_project_by_name(&projects, "myproject").is_none());
        assert!(find_colliding_projects(&projects).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn discover_sessions_rejects_file_symlink_candidates() {
        use std::os::unix::fs::symlink;

        let temp_dir = tempdir().unwrap();
        let root = temp_dir.path().join("projects");
        fs::create_dir_all(&root).unwrap();
        let outside = temp_dir.path().join("outside.jsonl");
        create_session_with_cwd(temp_dir.path(), "outside", "/tmp/outside");
        symlink(&outside, root.join("alias.jsonl")).unwrap();

        let sessions =
            discover_sessions(&root, &crate::filter::FilterConfig::no_size_limit()).unwrap();
        assert!(
            sessions.is_empty(),
            "file symlink must not be parsed as a session"
        );
    }
}
