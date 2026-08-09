//! Claude Code hooks management
//!
//! This module handles installation and management of Claude Code hooks
//! for automatic synchronization.

use anyhow::{Context, Result};
use colored::Colorize;
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::BINARY_NAME;

/// Identifiers for hooks installed by us (old name + new name)
const HOOK_MARKERS: &[&str] = &["claude-code-sync", "ccs"];

fn append_hook_debug(message: &str) {
    use std::io::Write;

    let Ok(config_dir) = crate::config::ConfigManager::ensure_config_dir() else {
        return;
    };
    let debug_log = config_dir.join("hook-debug.log");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(debug_log)
    else {
        return;
    };
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let _ = writeln!(file, "[{timestamp}] {message}");
}

/// Spawn a ccs subcommand as a detached child process.
///
/// Uses `current_exe()` so the child resolves to the same binary regardless of
/// the ambient PATH — important in Claude Code hook environments where PATH
/// may not include the cargo bin directory. Falls back to the bare binary name
/// if `current_exe()` fails (keeps old behavior, never worse).
fn spawn_ccs_subcommand(
    subcommand: &str,
    args: &[&str],
) -> std::io::Result<std::process::ExitStatus> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(BINARY_NAME));
    std::process::Command::new(exe)
        .arg(subcommand)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
}

/// Get the path to Claude settings file
fn claude_settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot find home directory")?;
    Ok(home.join(".claude").join("settings.json"))
}

/// Build the command string written into settings.json for a hook subcommand.
///
/// Claude Code runs this string via its own shell, whose PATH does NOT include
/// the cargo bin directory — so a bare `ccs` fails with "command not found".
/// We resolve the running binary's absolute path at install time via
/// `current_exe()`; each device writes its own real path (hooks are not synced
/// across devices — `config_sync` strips the `hooks` field). The path is always
/// double-quoted so one containing spaces (e.g. Windows
/// `C:\Users\<name with space>\.cargo\bin\ccs.exe`) survives shell
/// word-splitting on both sh and cmd. Falls back to the bare binary name if
/// `current_exe()` fails (no worse than the old behavior).
fn hook_command(subcommand: &str) -> String {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| BINARY_NAME.to_string());
    format!("\"{}\" {}", exe, subcommand)
}

/// Get the hooks configuration to install
fn get_hooks_config() -> Value {
    json!({
        "SessionStart": [
            {
                "hooks": [
                    {
                        "type": "command",
                        "command": hook_command("hook-session-start"),
                        "timeout": 60,
                        "statusMessage": "Syncing conversation history..."
                    }
                ]
            }
        ],
        "Stop": [
            {
                "hooks": [
                    {
                        "type": "command",
                        "command": hook_command("hook-stop"),
                        "timeout": 60
                    }
                ]
            }
        ],
        "UserPromptSubmit": [
            {
                "hooks": [
                    {
                        "type": "command",
                        "command": hook_command("hook-new-project-check"),
                        "timeout": 30
                    }
                ]
            }
        ]
    })
}

/// Check if a hook array contains one of our hooks (matching by subcommand suffix)
fn contains_our_hook(hooks_array: &[Value], subcommand: &str) -> bool {
    hooks_array.iter().any(|group| {
        group
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| {
                            HOOK_MARKERS.iter().any(|marker| cmd.contains(marker))
                                && cmd.contains(subcommand)
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

/// Check if a hook command belongs to us (matches any of HOOK_MARKERS)
fn is_our_hook_command(cmd: &str) -> bool {
    HOOK_MARKERS.iter().any(|marker| cmd.contains(marker))
}

/// Refresh our existing hook's command string to `new_command` in place.
///
/// Matches precisely on our marker ("ccs" / "claude-code-sync") AND the
/// `hook-*` subcommand, so custom user wrappers (e.g. `throttled-stop.sh`,
/// which carries neither marker nor the subcommand) are never touched. This is
/// what lets `hooks install` self-heal a device that was set up with an older
/// bare `ccs hook-*` command: re-running install rewrites it to the absolute
/// path instead of skipping. Returns true if a matching hook was updated.
fn update_our_hook_command(existing: &mut [Value], subcommand: &str, new_command: &str) -> bool {
    let mut updated = false;
    for group in existing.iter_mut() {
        if let Some(hooks) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
            for hook in hooks.iter_mut() {
                let is_ours = hook
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(|cmd| is_our_hook_command(cmd) && cmd.contains(subcommand))
                    .unwrap_or(false);
                if is_ours {
                    hook["command"] = json!(new_command);
                    updated = true;
                }
            }
        }
    }
    updated
}

/// Install hooks to ~/.claude/settings.json
pub fn handle_hooks_install() -> Result<()> {
    let settings_path = claude_settings_path()?;

    println!("{}", "Installing Claude Code hooks...".cyan().bold());

    // Read existing settings or create new
    let mut settings: Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content).unwrap_or(json!({}))
    } else {
        json!({})
    };

    // Ensure hooks object exists
    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    let hooks_to_add = get_hooks_config();
    let hooks_obj = settings
        .get_mut("hooks")
        .and_then(|v| v.as_object_mut())
        .context("Failed to access hooks object")?;

    // Merge each hook type
    for (event_name, new_hooks) in hooks_to_add.as_object().unwrap() {
        let new_hooks_array = new_hooks.as_array().unwrap();

        if let Some(existing) = hooks_obj.get_mut(event_name) {
            // The command we want in settings.json for this event (absolute path).
            let new_command = new_hooks_array
                .first()
                .and_then(|g| g.get("hooks"))
                .and_then(|h| h.as_array())
                .and_then(|hooks| hooks.first())
                .and_then(|h| h.get("command"))
                .and_then(|c| c.as_str())
                .unwrap_or("");
            // The `hook-*` subcommand token used for precise matching. Found by
            // prefix rather than positional index so it survives a quoted,
            // space-containing absolute path (e.g. `"/a b/ccs" hook-stop`).
            let subcommand = new_command
                .split_whitespace()
                .find(|t| t.starts_with("hook-"))
                .unwrap_or("");

            if let Some(existing_array) = existing.as_array_mut() {
                // Self-heal: if our hook is already there, refresh its command to
                // the absolute path instead of skipping (handles devices set up
                // with an older bare `ccs hook-*`). Custom wrappers untouched.
                if update_our_hook_command(existing_array, subcommand, new_command) {
                    println!(
                        "  {} {} hook refreshed (absolute path)",
                        "↻".cyan(),
                        event_name
                    );
                    continue;
                }

                // Not present yet — append our hook to the existing array.
                for hook in new_hooks_array {
                    existing_array.push(hook.clone());
                }
                println!("  {} {} hook added", "✓".green(), event_name);
            }
        } else {
            // Create new hook array
            hooks_obj.insert(event_name.clone(), new_hooks.clone());
            println!("  {} {} hook installed", "✓".green(), event_name);
        }
    }

    // Write back
    std::fs::create_dir_all(settings_path.parent().unwrap())?;
    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

    println!(
        "\n{} Hooks installed to {}",
        "✓".green(),
        settings_path.display()
    );

    Ok(())
}

/// Uninstall hooks from ~/.claude/settings.json
pub fn handle_hooks_uninstall() -> Result<()> {
    let settings_path = claude_settings_path()?;

    if !settings_path.exists() {
        println!(
            "{}",
            "No settings file found, nothing to uninstall.".yellow()
        );
        return Ok(());
    }

    println!("{}", "Removing Claude Code hooks...".cyan().bold());

    let content = std::fs::read_to_string(&settings_path)?;
    let mut settings: Value = serde_json::from_str(&content)?;

    if let Some(hooks_obj) = settings.get_mut("hooks").and_then(|v| v.as_object_mut()) {
        let mut removed_count = 0;

        // Remove our hooks from each event type (including legacy SessionEnd)
        for event_name in &["SessionStart", "Stop", "SessionEnd", "UserPromptSubmit"] {
            if let Some(hooks_array) = hooks_obj
                .get_mut(*event_name)
                .and_then(|v| v.as_array_mut())
            {
                let original_len = hooks_array.len();

                // Filter out our hooks
                hooks_array.retain(|group| {
                    !group
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|hooks| {
                            hooks.iter().any(|hook| {
                                hook.get("command")
                                    .and_then(|c| c.as_str())
                                    .map(is_our_hook_command)
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                });

                if hooks_array.len() < original_len {
                    removed_count += original_len - hooks_array.len();
                    println!("  {} Removed {} hook", "✓".green(), event_name);
                }

                // Remove empty arrays
                if hooks_array.is_empty() {
                    hooks_obj.remove(*event_name);
                }
            }
        }

        if removed_count == 0 {
            println!(
                "{}",
                format!("No {} hooks found to remove.", BINARY_NAME).yellow()
            );
        } else {
            // Write back
            std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
            println!("\n{} {} hook(s) removed", "✓".green(), removed_count);
        }
    } else {
        println!("{}", "No hooks configured, nothing to uninstall.".yellow());
    }

    Ok(())
}

/// Show current hooks configuration status
pub fn handle_hooks_show() -> Result<()> {
    let settings_path = claude_settings_path()?;

    println!("{}", "Claude Code Hooks Status".cyan().bold());
    println!("Settings file: {}", settings_path.display());
    println!();

    if !settings_path.exists() {
        println!("{}", "No settings file found.".yellow());
        println!();
        println!(
            "Run '{}' to install hooks.",
            format!("{} hooks install", BINARY_NAME).cyan()
        );
        return Ok(());
    }

    let content = std::fs::read_to_string(&settings_path)?;
    let settings: Value = serde_json::from_str(&content)?;

    let hooks_installed = if let Some(hooks_obj) = settings.get("hooks").and_then(|v| v.as_object())
    {
        let mut found = Vec::new();

        // Check SessionStart
        if let Some(hooks_array) = hooks_obj.get("SessionStart").and_then(|v| v.as_array()) {
            if contains_our_hook(hooks_array, "hook-session-start") {
                found.push("SessionStart");
            }
        }

        // Check Stop
        if let Some(hooks_array) = hooks_obj.get("Stop").and_then(|v| v.as_array()) {
            if contains_our_hook(hooks_array, "hook-stop") {
                found.push("Stop");
            }
        }

        // Check UserPromptSubmit
        if let Some(hooks_array) = hooks_obj.get("UserPromptSubmit").and_then(|v| v.as_array()) {
            if contains_our_hook(hooks_array, "hook-new-project-check") {
                found.push("UserPromptSubmit");
            }
        }

        found
    } else {
        Vec::new()
    };

    if hooks_installed.is_empty() {
        println!(
            "{}",
            format!("{} hooks: NOT installed", BINARY_NAME).yellow()
        );
        println!();
        println!(
            "Run '{}' to install hooks.",
            format!("{} hooks install", BINARY_NAME).cyan()
        );
    } else {
        println!("{}", format!("{} hooks: INSTALLED", BINARY_NAME).green());
        println!();
        println!("Installed hooks:");
        for hook in &hooks_installed {
            let description = match *hook {
                "SessionStart" => "Pull on startup (IDE support)",
                "Stop" => "Push after each response",
                "UserPromptSubmit" => "New project detection",
                _ => "",
            };
            println!("  {} {} ({})", "•".green(), hook.cyan(), description);
        }

        if hooks_installed.len() < 3 {
            println!();
            println!(
                "{}",
                format!(
                    "Note: Some hooks are missing. Run '{} hooks install' to reinstall.",
                    BINARY_NAME
                )
                .yellow()
            );
        }
    }

    Ok(())
}

/// Handle the hook-new-project-check command
/// This is called by the UserPromptSubmit hook to detect new projects
/// Reads JSON from stdin, outputs JSON to stdout
pub fn handle_new_project_check() -> Result<()> {
    use crate::sync::discovery::{claude_projects_dir, find_local_project_by_name};

    // Read hook input from stdin
    let input: Value = serde_json::from_reader(std::io::stdin())
        .context("Failed to read hook input from stdin")?;

    let cwd = match input.get("cwd").and_then(|v| v.as_str()) {
        Some(cwd) => cwd,
        None => {
            // No cwd provided, silently exit
            return Ok(());
        }
    };

    // Extract project name from cwd (handle both Unix and Windows paths)
    let project_name = cwd
        .split(&['/', '\\'])
        .rfind(|s| !s.is_empty())
        .unwrap_or("unknown");

    let claude_dir = match claude_projects_dir() {
        Ok(dir) => dir,
        Err(_) => return Ok(()), // Silently exit if we can't find the projects dir
    };

    // Check if local project directory exists
    let has_local_project = find_local_project_by_name(&claude_dir, project_name).is_some();

    if !has_local_project {
        // This is a new project, try to pull from remote
        log::info!("New project detected: {}", project_name);

        // Spawn via current_exe() so it works even when the hook environment
        // PATH does not include the cargo bin directory.
        let pull_result = spawn_ccs_subcommand("pull", &["--quiet"]);

        if pull_result.is_ok() {
            // Check if we now have a local project after pull
            if find_local_project_by_name(&claude_dir, project_name).is_some() {
                // Found remote history, notify user via hook output
                let output = json!({
                    "additionalContext": format!(
                        "Detected remote conversation history for project '{}'. \
                         It has been pulled. Consider running /clear or restarting \
                         Claude Code to load the history.",
                        project_name
                    )
                });
                println!("{}", serde_json::to_string(&output)?);
            }
        }
    }

    Ok(())
}

/// Handle the hook-stop command
/// This is called by the Stop hook after each AI response to push history
/// Reads JSON from stdin
pub fn handle_stop() -> Result<()> {
    append_hook_debug("Stop hook executed");

    // Read hook input from stdin (required by Claude Code hooks)
    let _input: Value = serde_json::from_reader(std::io::stdin()).unwrap_or(json!({}));

    // Execute push quietly after each response.
    // Spawn via current_exe() so it works even when the hook environment
    // PATH does not include the cargo bin directory.
    let push_result = spawn_ccs_subcommand("push", &["--quiet"]);

    match &push_result {
        Ok(status) if status.success() => {
            append_hook_debug(&format!("Stop push completed: exit code {status}"));
        }
        Ok(status) => {
            append_hook_debug(&format!("Stop push FAILED: exit code {status}"));
        }
        Err(error) => {
            append_hook_debug(&format!("Stop push failed to execute: {error}"));
        }
    }

    // Also sync config if enabled. config_sync is a direct function call (not a
    // spawned subprocess), so it is unaffected by PATH issues that can break
    // the push above — keep running it regardless of push outcome.
    if let Ok(filter) = crate::filter::FilterConfig::load() {
        if filter.config_sync.enabled {
            let _ = super::config_sync::handle_config_push(&filter.config_sync);
        }
    }

    // Propagate push failure so the throttled-stop.sh wrapper sees a non-zero
    // exit code and does NOT advance the throttle timestamp — otherwise the
    // next 5 minutes of Stop hooks would be silently skipped despite the push
    // never succeeding. `ccs push` returns Ok (exit 0) when there is nothing to
    // push, so this only fires on real failure.
    match push_result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            log::warn!("ccs push exited with {}", status);
            Err(anyhow::anyhow!("ccs push exited with {}", status))
        }
        Err(e) => {
            log::warn!("ccs push failed to execute: {}", e);
            Err(anyhow::anyhow!("ccs push failed to execute: {}", e))
        }
    }
}

/// Debounce interval for SessionStart pull (in seconds)
/// Extra protection layer to prevent duplicate pulls
const SESSION_START_DEBOUNCE_SECS: u64 = 300; // 5 minutes

/// Count running Claude Code processes.
#[cfg(unix)]
fn count_claude_processes() -> usize {
    let output = std::process::Command::new("sh")
        .args([
            "-c",
            "ps aux | grep 'native-binary/claude' | grep -v grep | wc -l",
        ])
        .output();

    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(1),
        Err(_) => 1,
    }
}

#[cfg(windows)]
fn count_claude_processes() -> usize {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq claude.exe", "/FO", "CSV", "/NH"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.to_ascii_lowercase().contains("claude.exe"))
            .count()
            .max(1),
        _ => 1,
    }
}

#[cfg(not(any(unix, windows)))]
fn count_claude_processes() -> usize {
    1
}

/// Handle the hook-session-start command
/// This is called by the SessionStart hook to pull latest history
/// Reads JSON from stdin, outputs JSON to stdout
///
/// Uses triple-condition detection to only pull on first startup:
/// 1. Process count = 1 (no other Claude instances)
/// 2. source = "startup" (not resume/compact)
/// 3. Debounce not active (extra protection)
pub fn handle_session_start() -> Result<()> {
    // Read hook input from stdin (required by Claude Code hooks)
    let input: Value = serde_json::from_reader(std::io::stdin()).unwrap_or(json!({}));

    // Extract source field
    let source = input
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Count Claude Code processes
    let process_count = count_claude_processes();
    let is_first_instance = process_count <= 1;
    let is_startup = source == "startup";

    // Get timestamp file path for debouncing
    let timestamp_file =
        crate::config::ConfigManager::config_dir().map(|d| d.join("last-session-pull"));

    // Check debounce
    let debounce_active = if let Ok(ref ts_path) = timestamp_file {
        if ts_path.exists() {
            if let Ok(metadata) = std::fs::metadata(ts_path) {
                if let Ok(modified) = metadata.modified() {
                    let elapsed = std::time::SystemTime::now()
                        .duration_since(modified)
                        .unwrap_or_default();
                    elapsed.as_secs() < SESSION_START_DEBOUNCE_SECS
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    append_hook_debug(&format!(
        "SessionStart (source: {source}, processes: {process_count}, debounce: {debounce_active})"
    ));

    // Triple-condition check: first instance + startup + no debounce
    if !is_first_instance {
        append_hook_debug(&format!("pull skipped (other instances: {process_count})"));
        return Ok(());
    }

    if !is_startup {
        append_hook_debug(&format!("pull skipped (source: {source} != startup)"));
        return Ok(());
    }

    if debounce_active {
        append_hook_debug("pull skipped (debounce active)");
        return Ok(());
    }

    // Update timestamp file before pull
    if let Ok(ref ts_path) = timestamp_file {
        let _ = std::fs::write(ts_path, "");
    }

    // Execute pull quietly (first start confirmed).
    // Spawn via current_exe() so it works even when the hook environment
    // PATH does not include the cargo bin directory.
    let pull_result = spawn_ccs_subcommand("pull", &["--quiet"]);

    match &pull_result {
        Ok(status) => {
            append_hook_debug(&format!("SessionStart pull completed: exit code {status}"));
        }
        Err(error) => {
            append_hook_debug(&format!("SessionStart pull failed: {error}"));
        }
    }

    // If pull succeeded and we got new content, we could notify the user
    // But for SessionStart, we just silently sync - the user will see the history
    if let Err(e) = &pull_result {
        log::debug!("SessionStart pull failed: {}", e);
    }

    // Auto-apply CLAUDE.md after pull
    if let Ok(filter) = crate::filter::FilterConfig::load() {
        if filter.config_sync.enabled && filter.config_sync.auto_apply_claude_md {
            let _ = super::config_sync::auto_apply_claude_md(&filter.config_sync);
        }
    }

    // Exit successfully - no output needed for SessionStart unless we want to add context
    Ok(())
}

/// Check if hooks are installed
pub fn are_hooks_installed() -> Result<bool> {
    let settings_path = claude_settings_path()?;

    if !settings_path.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&settings_path)?;
    let settings: Value = serde_json::from_str(&content)?;

    if let Some(hooks_obj) = settings.get("hooks").and_then(|v| v.as_object()) {
        // Check all required hooks
        let has_session_start = hooks_obj
            .get("SessionStart")
            .and_then(|v| v.as_array())
            .map(|arr| contains_our_hook(arr, "hook-session-start"))
            .unwrap_or(false);

        let has_stop = hooks_obj
            .get("Stop")
            .and_then(|v| v.as_array())
            .map(|arr| contains_our_hook(arr, "hook-stop"))
            .unwrap_or(false);

        let has_prompt_submit = hooks_obj
            .get("UserPromptSubmit")
            .and_then(|v| v.as_array())
            .map(|arr| contains_our_hook(arr, "hook-new-project-check"))
            .unwrap_or(false);

        Ok(has_session_start && has_stop && has_prompt_submit)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `spawn_ccs_subcommand` must never panic and always return a Result.
    /// In tests, `current_exe()` points at the test binary, which treats an
    /// unknown subcommand as a test filter and exits 0 — so we cannot assert a
    /// specific exit status here. The real verification happens by triggering
    /// the Stop hook and reading hook-debug.log.
    #[test]
    fn spawn_ccs_subcommand_returns_result_without_panic() {
        let _ = spawn_ccs_subcommand("__definitely_not_a_subcommand__", &[]);
    }

    /// The command written to settings.json must be an absolute, double-quoted
    /// path plus the subcommand — never a bare `ccs` (which fails in the hook
    /// shell whose PATH excludes the cargo bin dir).
    #[test]
    fn hook_command_is_quoted_absolute_path() {
        let cmd = hook_command("hook-stop");
        assert!(cmd.starts_with('"'), "path must be quoted: {cmd}");
        assert!(cmd.ends_with(" hook-stop"), "must carry subcommand: {cmd}");
        // The quoted segment resolves to the running test binary's real path,
        // which is absolute on every platform.
        let quoted = cmd
            .split('"')
            .nth(1)
            .expect("command should contain a quoted path");
        assert!(
            std::path::Path::new(quoted).is_absolute(),
            "path should be absolute: {quoted}"
        );
    }

    /// Self-heal: an existing bare `ccs hook-stop` is rewritten to the new
    /// absolute command; a matching hook returns true.
    #[test]
    fn update_our_hook_command_refreshes_bare_command() {
        let mut arr = vec![json!({
            "hooks": [{ "type": "command", "command": "ccs hook-stop", "timeout": 60 }]
        })];
        let updated = update_our_hook_command(&mut arr, "hook-stop", "\"/abs/ccs\" hook-stop");
        assert!(updated);
        assert_eq!(arr[0]["hooks"][0]["command"], "\"/abs/ccs\" hook-stop");

        // Idempotent: a second pass with the same target keeps it stable.
        let again = update_our_hook_command(&mut arr, "hook-stop", "\"/abs/ccs\" hook-stop");
        assert!(again);
        assert_eq!(arr[0]["hooks"][0]["command"], "\"/abs/ccs\" hook-stop");
    }

    /// Custom user wrappers (no marker, no subcommand) must never be touched.
    #[test]
    fn update_our_hook_command_ignores_custom_wrapper() {
        let mut arr = vec![json!({
            "hooks": [{ "type": "command", "command": "~/.claude/hooks/throttled-stop.sh", "timeout": 60 }]
        })];
        let updated = update_our_hook_command(&mut arr, "hook-stop", "\"/abs/ccs\" hook-stop");
        assert!(!updated, "custom wrapper should not match");
        assert_eq!(
            arr[0]["hooks"][0]["command"],
            "~/.claude/hooks/throttled-stop.sh"
        );
    }

    /// The `hook-*` subcommand token must be recoverable from a quoted,
    /// space-containing absolute path (the fragile positional `nth(1)` failed
    /// here). Mirrors the extraction in `handle_hooks_install`.
    #[test]
    fn subcommand_extracted_from_quoted_spaced_path() {
        let cmd = "\"/a b/ccs\" hook-session-start";
        let sub = cmd
            .split_whitespace()
            .find(|t| t.starts_with("hook-"))
            .unwrap_or("");
        assert_eq!(sub, "hook-session-start");
    }
}
