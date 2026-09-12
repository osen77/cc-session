//! Claude Code hooks management
//!
//! This module handles installation and management of Claude Code hooks
//! for automatic synchronization.

use anyhow::{Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::atomic_file::{persist_json_atomic, persist_json_pretty_atomic, FileLock};
use crate::config::ConfigManager;
use crate::sync::repo_lock::{RepoLock, RepoLockOutcome};
use crate::sync::SyncState;
use crate::{VerbosityLevel, BINARY_NAME};

/// Executable basenames owned by this project (old name + new name).
const HOOK_EXECUTABLES: &[&str] = &["claude-code-sync", "claude-code-sync.exe", "ccs", "ccs.exe"];
const PUSH_HOOK_THROTTLE_SECS: u64 = 300;
const PUSH_HOOK_ALERT_THRESHOLD: u32 = 3;
const NEW_PROJECT_PULL_COOLDOWN_SECS: u64 = 600;
const HOOK_FIELDS: &[&str] = &["type", "command", "timeout", "statusMessage"];

fn append_hook_debug(message: &str) {
    use std::io::Write;

    let Ok(config_dir) = ConfigManager::ensure_config_dir() else {
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

/// Spawn a ccs subcommand and wait for it to finish.
fn spawn_ccs_subcommand(
    subcommand: &str,
    args: &[&str],
) -> std::io::Result<std::process::ExitStatus> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(BINARY_NAME));
    Command::new(exe)
        .arg(subcommand)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

fn detached_subcommand_command(exe: &Path, subcommand: &str, args: &[&str]) -> Command {
    let mut command = Command::new(exe);
    command
        .arg(subcommand)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Spawn a ccs subcommand without waiting for it.
fn spawn_detached_subcommand(subcommand: &str, args: &[&str]) -> std::io::Result<Child> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(BINARY_NAME));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut command = detached_subcommand_command(&exe, subcommand, args);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn()
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const BASE_FLAGS: u32 = DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;

        let mut command = detached_subcommand_command(&exe, subcommand, args);
        command.creation_flags(BASE_FLAGS | CREATE_BREAKAWAY_FROM_JOB);
        match command.spawn() {
            Ok(child) => Ok(child),
            Err(first_error) => {
                let mut fallback = detached_subcommand_command(&exe, subcommand, args);
                fallback.creation_flags(BASE_FLAGS);
                fallback.spawn().map_err(|fallback_error| {
                    std::io::Error::new(
                        fallback_error.kind(),
                        format!(
                            "detached spawn with breakaway failed ({first_error}); fallback failed ({fallback_error})"
                        ),
                    )
                })
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        detached_subcommand_command(&exe, subcommand, args).spawn()
    }
}

/// Spawn the Stop-hook worker without waiting for it.
fn spawn_detached_worker() -> std::io::Result<Child> {
    spawn_detached_subcommand("hook-stop", &["--worker"])
}

fn spawn_detached_pull() -> std::io::Result<Child> {
    spawn_detached_subcommand("pull", &["--quiet"])
}

/// Get the path to Claude settings file.
fn claude_settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot find home directory")?;
    Ok(home.join(".claude").join("settings.json"))
}

/// Build the command string written into settings.json for a hook subcommand.
fn hook_command(subcommand: &str) -> String {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| BINARY_NAME.to_string());
    format!("\"{}\" {}", exe, subcommand)
}

/// Get the hooks configuration to install.
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
                        "timeout": 10
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

fn first_command_token(cmd: &str) -> Option<&str> {
    let trimmed = cmd.trim_start();
    let first = trimmed.as_bytes().first().copied()?;
    if first == b'"' || first == b'\'' {
        let quote = first as char;
        let rest = &trimmed[1..];
        let end = rest.find(quote)?;
        Some(&rest[..end])
    } else {
        trimmed.split_whitespace().next()
    }
}

fn command_basename(cmd: &str) -> Option<&str> {
    first_command_token(cmd)?
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
}

fn subcommand_of(cmd: &str) -> Option<&str> {
    cmd.split_whitespace()
        .find(|token| token.starts_with("hook-"))
}

fn is_our_hook_command(cmd: &str) -> bool {
    let Some(basename) = command_basename(cmd) else {
        return false;
    };
    HOOK_EXECUTABLES
        .iter()
        .any(|candidate| basename.eq_ignore_ascii_case(candidate))
        && subcommand_of(cmd).is_some()
}

fn is_ours_for(cmd: &str, subcommand: &str) -> bool {
    is_our_hook_command(cmd) && subcommand_of(cmd) == Some(subcommand)
}

fn is_legacy_stop_wrapper(cmd: &str) -> bool {
    command_basename(cmd).is_some_and(|basename| {
        matches!(
            basename.to_ascii_lowercase().as_str(),
            "throttled-stop" | "throttled-stop.sh" | "throttled-stop.bat" | "throttled-stop.ps1"
        )
    })
}

fn hook_values<'a>(settings: &'a Value, event_name: &str) -> Vec<&'a Value> {
    settings
        .get("hooks")
        .and_then(Value::as_object)
        .and_then(|hooks| hooks.get(event_name))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .collect()
}

fn expected_hook<'a>(expected: &'a Value, event_name: &str) -> Option<&'a Value> {
    expected
        .get(event_name)?
        .as_array()?
        .first()?
        .get("hooks")?
        .as_array()?
        .first()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HookDrift {
    Missing { event: String },
    LegacyWrapper { event: String },
    FieldMismatch { event: String, field: String },
}

impl HookDrift {
    fn description(&self) -> String {
        match self {
            Self::Missing { event } => format!("{event}: 缺少 ccs hook"),
            Self::LegacyWrapper { event } => {
                format!("{event}: 仍在使用 throttled-stop wrapper")
            }
            Self::FieldMismatch { event, field } => {
                format!("{event}: {field} 与当前版本不一致")
            }
        }
    }
}

fn detect_hook_drift(settings: &Value, expected: &Value) -> Vec<HookDrift> {
    let Some(expected_events) = expected.as_object() else {
        return Vec::new();
    };
    let mut drift = Vec::new();

    for event_name in expected_events.keys() {
        let Some(wanted) = expected_hook(expected, event_name) else {
            continue;
        };
        let Some(wanted_command) = wanted.get("command").and_then(Value::as_str) else {
            continue;
        };
        let Some(subcommand) = subcommand_of(wanted_command) else {
            continue;
        };
        let installed = hook_values(settings, event_name);
        let legacy_present = event_name == "Stop"
            && installed.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_legacy_stop_wrapper)
            });
        if legacy_present {
            drift.push(HookDrift::LegacyWrapper {
                event: event_name.clone(),
            });
        }
        let ours = installed.iter().copied().find(|hook| {
            hook.get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| is_ours_for(command, subcommand))
        });

        let Some(actual) = ours else {
            if !legacy_present {
                drift.push(HookDrift::Missing {
                    event: event_name.clone(),
                });
            }
            continue;
        };

        for field in HOOK_FIELDS {
            if let Some(wanted_value) = wanted.get(*field) {
                if actual.get(*field) != Some(wanted_value) {
                    drift.push(HookDrift::FieldMismatch {
                        event: event_name.clone(),
                        field: (*field).to_string(),
                    });
                }
            }
        }
    }

    drift
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RefreshResult {
    refreshed: bool,
    replaced_legacy: bool,
}

/// Refresh matching hook fields in place while retaining user-added fields.
fn refresh_our_hook(existing: &mut [Value], subcommand: &str, wanted: &Value) -> RefreshResult {
    let mut result = RefreshResult::default();
    let Some(wanted_object) = wanted.as_object() else {
        return result;
    };
    let already_has_ours = existing.iter().any(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| is_ours_for(command, subcommand))
            })
    });

    for group in existing {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut index = 0;
        while index < hooks.len() {
            let command = hooks[index]
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let ours = is_ours_for(command, subcommand);
            let legacy = subcommand == "hook-stop" && is_legacy_stop_wrapper(command);
            if already_has_ours && legacy {
                hooks.remove(index);
                result.replaced_legacy = true;
                continue;
            }
            if ours || legacy {
                if let Some(object) = hooks[index].as_object_mut() {
                    for (key, value) in wanted_object {
                        object.insert(key.clone(), value.clone());
                    }
                    result.refreshed |= ours;
                    result.replaced_legacy |= legacy;
                }
            }
            index += 1;
        }
    }

    result
}

/// Install hooks to ~/.claude/settings.json.
pub fn handle_hooks_install() -> Result<()> {
    let settings_path = claude_settings_path()?;

    println!("{}", "Installing Claude Code hooks...".cyan().bold());

    let mut settings: Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)
            .with_context(|| format!("Failed to read {}", settings_path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", settings_path.display()))?
    } else {
        json!({})
    };

    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    let hooks_to_add = get_hooks_config();
    let hooks_obj = settings
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .context("Failed to access hooks object")?;

    for (event_name, new_hooks) in hooks_to_add
        .as_object()
        .context("Expected hook configuration object")?
    {
        let new_hooks_array = new_hooks
            .as_array()
            .context("Expected hook configuration array")?;
        let wanted = expected_hook(&hooks_to_add, event_name)
            .context("Expected hook configuration entry")?;
        let wanted_command = wanted
            .get("command")
            .and_then(Value::as_str)
            .context("Expected hook command")?;
        let subcommand = subcommand_of(wanted_command).context("Expected hook subcommand")?;

        if let Some(existing) = hooks_obj.get_mut(event_name) {
            let existing_array = existing
                .as_array_mut()
                .with_context(|| format!("{event_name} hooks must be an array"))?;
            let refresh = refresh_our_hook(existing_array, subcommand, wanted);
            existing_array.retain(|group| {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_none_or(|hooks| !hooks.is_empty())
            });
            if refresh.replaced_legacy {
                println!("  {} {} legacy wrapper replaced", "↻".cyan(), event_name);
            } else if refresh.refreshed {
                println!("  {} {} hook refreshed", "↻".cyan(), event_name);
            } else {
                existing_array.extend(new_hooks_array.iter().cloned());
                println!("  {} {} hook added", "✓".green(), event_name);
            }
        } else {
            hooks_obj.insert(event_name.clone(), new_hooks.clone());
            println!("  {} {} hook installed", "✓".green(), event_name);
        }
    }

    std::fs::create_dir_all(
        settings_path
            .parent()
            .context("Claude settings path has no parent")?,
    )?;
    persist_json_pretty_atomic(&settings_path, &settings)?;

    println!(
        "\n{} Hooks installed to {}",
        "✓".green(),
        settings_path.display()
    );

    Ok(())
}

/// Uninstall hooks from ~/.claude/settings.json.
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
    let mut removed_count = 0;

    if let Some(hooks_obj) = settings.get_mut("hooks").and_then(Value::as_object_mut) {
        for event_name in ["SessionStart", "Stop", "SessionEnd", "UserPromptSubmit"] {
            let mut remove_event = false;
            if let Some(groups) = hooks_obj.get_mut(event_name).and_then(Value::as_array_mut) {
                for group in groups.iter_mut() {
                    if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                        let before = hooks.len();
                        hooks.retain(|hook| {
                            !hook
                                .get("command")
                                .and_then(Value::as_str)
                                .is_some_and(is_our_hook_command)
                        });
                        removed_count += before - hooks.len();
                    }
                }
                groups.retain(|group| {
                    group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_none_or(|hooks| !hooks.is_empty())
                });
                remove_event = groups.is_empty();
            }
            if remove_event {
                hooks_obj.remove(event_name);
            }
        }
    } else {
        println!("{}", "No hooks configured, nothing to uninstall.".yellow());
        return Ok(());
    }

    if removed_count == 0 {
        println!(
            "{}",
            format!("No {} hooks found to remove.", BINARY_NAME).yellow()
        );
    } else {
        persist_json_pretty_atomic(&settings_path, &settings)?;
        println!("\n{} {} hook(s) removed", "✓".green(), removed_count);
    }

    Ok(())
}

fn read_hook_settings() -> Result<Value> {
    let settings_path = claude_settings_path()?;
    if !settings_path.exists() {
        return Ok(json!({}));
    }
    let content = std::fs::read_to_string(&settings_path)?;
    serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", settings_path.display()))
}

/// Show current hooks configuration status.
pub fn handle_hooks_show() -> Result<()> {
    let settings_path = claude_settings_path()?;
    let settings = read_hook_settings()?;
    let expected = get_hooks_config();
    let drift = detect_hook_drift(&settings, &expected);

    println!("{}", "Claude Code Hooks Status".cyan().bold());
    println!("Settings file: {}", settings_path.display());
    println!();

    if drift.is_empty() {
        println!("{}", format!("{} hooks: INSTALLED", BINARY_NAME).green());
        println!();
        println!("Installed hooks:");
        println!(
            "  {} {} (Pull on startup)",
            "•".green(),
            "SessionStart".cyan()
        );
        println!("  {} {} (Background push)", "•".green(), "Stop".cyan());
        println!(
            "  {} {} (New project detection)",
            "•".green(),
            "UserPromptSubmit".cyan()
        );
    } else {
        println!("{}", format!("{} hooks: NEED UPDATE", BINARY_NAME).yellow());
        println!();
        println!("Drift:");
        for item in &drift {
            println!("  {} {}", "•".yellow(), item.description());
        }
        println!();
        println!(
            "Run '{}' to update hooks.",
            format!("{} hooks install", BINARY_NAME).cyan()
        );
    }

    Ok(())
}

/// Check whether installed hooks match this ccs version.
pub fn handle_hooks_check(quiet: bool) -> Result<()> {
    let drift = detect_hook_drift(&read_hook_settings()?, &get_hooks_config());
    if drift.is_empty() {
        return Ok(());
    }

    if quiet {
        eprintln!(
            "hook 配置与 v{} 不一致，运行 `{} hooks install` 更新",
            env!("CARGO_PKG_VERSION"),
            BINARY_NAME
        );
    } else {
        eprintln!("检测到 hook 配置漂移：");
        for item in &drift {
            eprintln!("  - {}", item.description());
        }
        eprintln!("运行 `{} hooks install` 更新", BINARY_NAME);
    }

    Err(anyhow::anyhow!("hook configuration drift detected"))
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct NewProjectPullAttempt {
    #[serde(default)]
    last_attempt_unix: u64,
    #[serde(default)]
    pending_notify: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct NewProjectPullState {
    #[serde(default)]
    projects: HashMap<String, NewProjectPullAttempt>,
}

impl NewProjectPullState {
    fn load() -> Self {
        let Ok(path) = ConfigManager::new_project_pull_state_path() else {
            return Self::default();
        };
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    fn save(&self) {
        let Ok(path) = ConfigManager::new_project_pull_state_path() else {
            return;
        };
        let _ = persist_json_atomic(&path, self);
    }
}

fn new_project_notification(project_name: &str) -> String {
    format!(
        "Detected remote conversation history for project '{}'. \\
         It has been pulled. Consider running /clear or restarting \\
         Claude Code to load the history.",
        project_name
    )
}

fn process_new_project_check(
    cwd: &str,
    claude_dir: &Path,
    now: u64,
    mut spawn_pull: impl FnMut() -> std::io::Result<()>,
) -> Option<String> {
    use crate::sync::discovery::find_local_project_by_name;

    // UserPromptSubmit is only meaningful at the git repository root. In
    // particular, do not treat a nested source directory or a temporary cwd as
    // a new project.
    if !Path::new(cwd).join(".git").exists() {
        return None;
    }

    let project_name = cwd
        .split(&['/', '\\'])
        .rfind(|s| !s.is_empty())
        .unwrap_or("unknown");
    let has_local_project = find_local_project_by_name(claude_dir, project_name).is_some();
    let mut state = NewProjectPullState::load();

    if has_local_project {
        let attempt = state.projects.get_mut(project_name)?;
        if !attempt.pending_notify {
            return None;
        }
        attempt.pending_notify = false;
        state.save();
        return Some(new_project_notification(project_name));
    }

    if state.projects.get(project_name).is_some_and(|attempt| {
        now.saturating_sub(attempt.last_attempt_unix) < NEW_PROJECT_PULL_COOLDOWN_SECS
    }) {
        return None;
    }

    log::info!("New project detected: {}", project_name);
    let attempt = state.projects.entry(project_name.to_string()).or_default();
    attempt.last_attempt_unix = now;
    attempt.pending_notify = true;
    state.save();

    // Spawn via current_exe() so it works even when the hook environment PATH
    // does not include the cargo bin directory. The hook does not wait for pull.
    if let Err(error) = spawn_pull() {
        log::debug!("New project pull spawn failed: {}", error);
    }
    None
}

/// Handle the hook-new-project-check command
/// This is called by the UserPromptSubmit hook to detect new projects
/// Reads JSON from stdin, outputs JSON to stdout
pub fn handle_new_project_check() -> Result<()> {
    use crate::sync::discovery::claude_projects_dir;

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

    // Gate before touching Claude's potentially large projects directory.
    if !Path::new(cwd).join(".git").exists() {
        return Ok(());
    }

    let claude_dir = match claude_projects_dir() {
        Ok(dir) => dir,
        Err(_) => return Ok(()), // Silently exit if we can't find the projects dir
    };
    let Some(notification) = process_new_project_check(cwd, &claude_dir, unix_now(), || {
        spawn_detached_pull().map(|_| ())
    }) else {
        return Ok(());
    };

    let output = json!({ "additionalContext": notification });
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PushHookState {
    #[serde(default)]
    consecutive_failures: u32,
    #[serde(default)]
    alerted_failures: u32,
    #[serde(default)]
    last_success_unix: Option<u64>,
    #[serde(default)]
    last_failure_unix: Option<u64>,
    #[serde(default)]
    last_error: Option<String>,
}

impl PushHookState {
    fn load() -> Self {
        let Ok(path) = ConfigManager::push_hook_state_path() else {
            return Self::default();
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice(&bytes) {
            Ok(state) => state,
            Err(error) => {
                append_hook_debug(&format!(
                    "push worker state is invalid, using defaults: {error}"
                ));
                Self::default()
            }
        }
    }

    fn save(&self) -> Result<()> {
        persist_json_atomic(&ConfigManager::push_hook_state_path()?, self)
    }

    fn record_success(&mut self, now: u64) {
        self.consecutive_failures = 0;
        self.alerted_failures = 0;
        self.last_success_unix = Some(now);
        self.last_failure_unix = None;
        self.last_error = None;
    }

    fn record_failure(&mut self, now: u64, error: &anyhow::Error) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_failure_unix = Some(now);
        self.last_error = Some(format!("{error:#}"));
    }
}

fn pending_alert(state: &PushHookState) -> bool {
    state.consecutive_failures >= PUSH_HOOK_ALERT_THRESHOLD
        && state.consecutive_failures > state.alerted_failures
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn throttle_active(stamp_path: &Path, now: SystemTime) -> bool {
    let Ok(metadata) = std::fs::metadata(stamp_path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    now.duration_since(modified).unwrap_or_default().as_secs() < PUSH_HOOK_THROTTLE_SECS
}

fn touch_stamp() -> Result<()> {
    let path = ConfigManager::push_hook_stamp_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("Failed to touch {}", path.display()))?;
    Ok(())
}

fn record_worker_failure(error: anyhow::Error) {
    append_hook_debug(&format!("Stop worker FAILED: {error:#}"));
    let mut state = PushHookState::load();
    state.record_failure(unix_now(), &error);
    if let Err(save_error) = state.save() {
        append_hook_debug(&format!(
            "failed to save push worker failure state: {save_error:#}"
        ));
    }
}

fn run_stop_worker() {
    let lock_path = match ConfigManager::push_hook_lock_path() {
        Ok(path) => path,
        Err(error) => {
            append_hook_debug(&format!("Stop worker lock path unavailable: {error:#}"));
            return;
        }
    };
    let _worker_lock = match FileLock::try_acquire(&lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            append_hook_debug("Stop worker skipped: another worker holds push-hook.lock");
            return;
        }
        Err(error) => {
            append_hook_debug(&format!("Stop worker lock unavailable: {error:#}"));
            return;
        }
    };

    let stamp_path = match ConfigManager::push_hook_stamp_path() {
        Ok(path) => path,
        Err(error) => {
            record_worker_failure(error);
            return;
        }
    };
    if throttle_active(&stamp_path, SystemTime::now()) {
        append_hook_debug("Stop worker skipped: throttle active");
        return;
    }

    let sync_state = match SyncState::load() {
        Ok(state) => state,
        Err(error) => {
            record_worker_failure(error);
            return;
        }
    };
    let _repo_lock = match RepoLock::acquire(&sync_state.sync_repo_path) {
        Ok(RepoLockOutcome::Acquired(lock)) => lock,
        Ok(RepoLockOutcome::Busy) => {
            append_hook_debug("Stop worker skipped: sync repository is busy");
            return;
        }
        Err(error) => {
            record_worker_failure(error);
            return;
        }
    };

    let push_result = crate::sync::push_history(
        None,
        true,
        None,
        false,
        true,
        false,
        false,
        VerbosityLevel::Quiet,
    );
    if let Err(error) = push_result {
        record_worker_failure(error);
        return;
    }

    // Device config sync already happens inside push_history (sync_config=true),
    // which respects the user's push_with_config setting; do not push config again.

    let mut state = PushHookState::load();
    state.record_success(unix_now());
    if let Err(error) = state.save() {
        record_worker_failure(error.context("failed to save push worker success state"));
        return;
    }
    if let Err(error) = touch_stamp() {
        record_worker_failure(error);
        return;
    }
    append_hook_debug("Stop worker completed successfully");
}

fn report_pending_alert() -> Result<()> {
    let lock_path = match ConfigManager::push_hook_lock_path() {
        Ok(path) => path,
        Err(error) => {
            append_hook_debug(&format!(
                "Stop alert check skipped: lock path unavailable: {error:#}"
            ));
            return Ok(());
        }
    };
    let _alert_lock = match FileLock::try_acquire(&lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            append_hook_debug("Stop alert check skipped: worker holds push-hook.lock");
            return Ok(());
        }
        Err(error) => {
            append_hook_debug(&format!(
                "Stop alert check skipped: lock unavailable: {error:#}"
            ));
            return Ok(());
        }
    };

    let mut state = PushHookState::load();
    if !pending_alert(&state) {
        return Ok(());
    }
    let failures = state.consecutive_failures;
    state.alerted_failures = failures;
    state.save()?;
    let debug_log = ConfigManager::config_dir()?.join("hook-debug.log");
    Err(anyhow::anyhow!(
        "Stop 后台推送已连续失败 {failures} 次。查看详情：tail -n 20 '{}'",
        debug_log.display()
    ))
}

/// Handle the hook-stop command.
///
/// The foreground invocation only checks alerts/throttling and spawns a detached
/// worker. The worker owns both the hook lock and repository lock, so the hook
/// harness can return immediately without leaving lock lifetime ambiguous.
pub fn handle_stop(worker: bool) -> Result<()> {
    if worker {
        run_stop_worker();
        return Ok(());
    }

    let _input: Value = serde_json::from_reader(std::io::stdin()).unwrap_or(json!({}));
    append_hook_debug("Stop hook executed");

    report_pending_alert()?;

    let stamp_path = ConfigManager::push_hook_stamp_path()?;
    if throttle_active(&stamp_path, SystemTime::now()) {
        append_hook_debug("Stop hook skipped: throttle active");
        return Ok(());
    }

    match spawn_detached_worker() {
        Ok(child) => append_hook_debug(&format!("Stop worker spawned: pid {}", child.id())),
        Err(error) => append_hook_debug(&format!("Stop worker spawn failed: {error}")),
    }
    Ok(())
}

/// Debounce interval for SessionStart pull (in seconds)
/// Extra protection layer to prevent duplicate pulls
const SESSION_START_DEBOUNCE_SECS: u64 = 300; // 5 minutes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessObservation {
    Known(usize),
    Unknown,
}

fn process_basename(value: &str) -> &str {
    value
        .rsplit(&['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(value)
}

fn is_excluded_claude_process(args: &str) -> bool {
    let lower = args.to_ascii_lowercase();
    [
        "--bg-pty-host",
        "--bg-spare",
        "--mcp-server",
        "mcp-server",
        "modelcontextprotocol",
        "claude-code-daemon",
        "claude daemon",
        " daemon run",
        " hook-session-start",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_claude_main_process(command: &str, args: &str) -> bool {
    if is_excluded_claude_process(args) {
        return false;
    }
    let command = process_basename(command).to_ascii_lowercase();
    let argv0 = args
        .split_whitespace()
        .next()
        .map(|value| process_basename(value.trim_matches(['\"', '\''])))
        .unwrap_or("")
        .to_ascii_lowercase();
    let args_lower = args.to_ascii_lowercase();
    let native = matches!(command.as_str(), "claude" | "claude.exe")
        || matches!(argv0.as_str(), "claude" | "claude.exe");
    let npm = (matches!(command.as_str(), "node" | "node.exe")
        || matches!(argv0.as_str(), "node" | "node.exe"))
        && (args_lower.contains("@anthropic-ai/claude-code")
            || args_lower.contains("@anthropic-ai\\claude-code"))
        && (args_lower.contains("cli.js")
            || args_lower.contains("bin/claude")
            || args_lower.contains("bin\\claude"));
    native || npm
}

#[cfg_attr(not(any(test, unix)), allow(dead_code))]
fn parse_unix_process_listing(success: bool, stdout: &[u8]) -> ProcessObservation {
    if !success {
        return ProcessObservation::Unknown;
    }
    let Ok(listing) = std::str::from_utf8(stdout) else {
        return ProcessObservation::Unknown;
    };
    let mut count = 0usize;
    for line in listing.lines().filter(|line| !line.trim().is_empty()) {
        let mut parts = line.trim_start().splitn(2, char::is_whitespace);
        let Some(command) = parts.next() else {
            return ProcessObservation::Unknown;
        };
        let Some(args) = parts.next() else {
            return ProcessObservation::Unknown;
        };
        let args = args.trim_start();
        if is_claude_main_process(command, args) {
            count += 1;
        }
    }
    ProcessObservation::Known(count)
}

#[cfg_attr(not(any(test, windows)), allow(dead_code))]
fn parse_windows_process_listing(success: bool, stdout: &[u8]) -> ProcessObservation {
    if !success {
        return ProcessObservation::Unknown;
    }
    let Ok(listing) = std::str::from_utf8(stdout) else {
        return ProcessObservation::Unknown;
    };
    let mut count = 0usize;
    for line in listing.lines().filter(|line| !line.trim().is_empty()) {
        let Some((command, args)) = line.split_once('\t') else {
            return ProcessObservation::Unknown;
        };
        if is_claude_main_process(command.trim(), args.trim()) {
            count += 1;
        }
    }
    ProcessObservation::Known(count)
}

#[cfg(unix)]
fn observe_claude_processes() -> ProcessObservation {
    match Command::new("ps").args(["-axo", "comm=,args="]).output() {
        Ok(output) => parse_unix_process_listing(output.status.success(), &output.stdout),
        Err(_) => ProcessObservation::Unknown,
    }
}

#[cfg(windows)]
fn observe_claude_processes() -> ProcessObservation {
    let script = concat!(
        "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new(); ",
        "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.Name)`t$($_.CommandLine)\" }"
    );
    match Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
    {
        Ok(output) => parse_windows_process_listing(output.status.success(), &output.stdout),
        Err(_) => ProcessObservation::Unknown,
    }
}

#[cfg(not(any(unix, windows)))]
fn observe_claude_processes() -> ProcessObservation {
    ProcessObservation::Unknown
}

fn session_start_debounce_active(stamp_path: &Path, now: SystemTime) -> bool {
    let metadata = match std::fs::metadata(stamp_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    now.duration_since(modified).unwrap_or_default().as_secs() < SESSION_START_DEBOUNCE_SECS
}

fn should_run_session_start_pull(
    observation: ProcessObservation,
    source: &str,
    debounce_active: bool,
) -> bool {
    matches!(observation, ProcessObservation::Known(1)) && source == "startup" && !debounce_active
}

fn run_session_start_gate(
    observation: ProcessObservation,
    source: &str,
    lock_path: &Path,
    stamp_path: &Path,
    now: SystemTime,
    run_pull: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    if !matches!(observation, ProcessObservation::Known(1)) || source != "startup" {
        return Ok(false);
    }
    let Some(_lock) = FileLock::try_acquire(lock_path)? else {
        return Ok(false);
    };
    let debounce_active = session_start_debounce_active(stamp_path, now);
    if !should_run_session_start_pull(observation, source, debounce_active) {
        return Ok(false);
    }
    run_pull()?;
    if let Some(parent) = stamp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stamp = std::fs::File::create(stamp_path)?;
    stamp.set_modified(now)?;
    stamp.sync_all()?;
    Ok(true)
}

/// Handle the hook-session-start command. Automatic pull is fail-closed: only
/// one confidently identified startup process may pull, and the cooldown check,
/// stamp update, and pull execution share one lock.
pub fn handle_session_start() -> Result<()> {
    let input: Value = serde_json::from_reader(std::io::stdin()).unwrap_or(json!({}));
    let source = input
        .get("source")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let observation = observe_claude_processes();
    let config_dir = match ConfigManager::config_dir() {
        Ok(path) => path,
        Err(error) => {
            append_hook_debug(&format!(
                "SessionStart pull skipped: config directory unavailable: {error}"
            ));
            return Ok(());
        }
    };
    let stamp_path = config_dir.join("last-session-pull");
    let lock_path = config_dir.join("last-session-pull.lock");
    append_hook_debug(&format!(
        "SessionStart (source: {source}, processes: {observation:?})"
    ));

    let result = run_session_start_gate(
        observation,
        source,
        &lock_path,
        &stamp_path,
        SystemTime::now(),
        || {
            let status = spawn_ccs_subcommand("pull", &["--quiet"])?;
            append_hook_debug(&format!("SessionStart pull completed: exit code {status}"));
            if !status.success() {
                anyhow::bail!("SessionStart pull exited with {status}");
            }
            Ok(())
        },
    );
    let ran = match result {
        Ok(ran) => ran,
        Err(error) => {
            append_hook_debug(&format!(
                "SessionStart pull failed or skipped safely: {error}"
            ));
            false
        }
    };
    if !ran {
        append_hook_debug("SessionStart pull skipped by fail-closed gate");
        return Ok(());
    }

    if let Ok(filter) = crate::filter::FilterConfig::load() {
        if filter.config_sync.enabled && filter.config_sync.auto_apply_claude_md {
            let _ = super::config_sync::auto_apply_claude_md(&filter.config_sync);
        }
    }
    Ok(())
}

/// Check if hooks are installed and match this ccs version.
pub fn are_hooks_installed() -> Result<bool> {
    Ok(detect_hook_drift(&read_hook_settings()?, &get_hooks_config()).is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs::File;
    use std::time::Duration;
    use tempfile::tempdir;

    struct EnvGuard {
        original: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(path: &Path) -> Self {
            let original = std::env::var_os(crate::config::CONFIG_DIR_ENV);
            std::env::set_var(crate::config::CONFIG_DIR_ENV, path);
            Self { original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.original.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }

    fn expected() -> Value {
        let mut expected = get_hooks_config();
        for (event, subcommand) in [
            ("SessionStart", "hook-session-start"),
            ("Stop", "hook-stop"),
            ("UserPromptSubmit", "hook-new-project-check"),
        ] {
            expected[event][0]["hooks"][0]["command"] =
                json!(format!("\"/test/bin/ccs\" {subcommand}"));
        }
        expected
    }

    fn expected_settings() -> Value {
        json!({ "hooks": expected() })
    }

    #[test]
    fn hook_command_is_quoted_absolute_path() {
        let cmd = hook_command("hook-stop");
        assert!(cmd.starts_with('"'), "path must be quoted: {cmd}");
        assert!(cmd.ends_with(" hook-stop"), "must carry subcommand: {cmd}");
        let quoted = cmd.split('"').nth(1).unwrap();
        assert!(
            Path::new(quoted).is_absolute(),
            "path should be absolute: {quoted}"
        );
    }

    #[test]
    fn hook_ownership_requires_exact_executable_basename() {
        assert!(is_our_hook_command("\"/Users/x/.local/bin/ccs\" hook-stop"));
        assert!(is_our_hook_command(
            "C:\\Users\\x\\claude-code-sync.exe hook-session-start"
        ));
        assert!(!is_our_hook_command("/usr/local/bin/ccs-monitor hook-stop"));
        assert!(!is_our_hook_command("python ccs hook-stop"));
        assert!(!is_our_hook_command("ccs --version"));
    }

    #[test]
    fn legacy_wrapper_requires_exact_supported_basename() {
        for command in [
            "throttled-stop",
            "~/.claude/hooks/throttled-stop.sh",
            "C:\\hooks\\throttled-stop.bat",
            "'C:\\hook dir\\throttled-stop.ps1'",
        ] {
            assert!(is_legacy_stop_wrapper(command), "must match {command}");
        }
        assert!(!is_legacy_stop_wrapper(
            "python my-throttled-stop-notify.py"
        ));
        assert!(!is_legacy_stop_wrapper("throttled-stop.py"));
    }

    #[test]
    fn drift_reports_all_missing_hooks() {
        let drift = detect_hook_drift(&json!({}), &expected());
        assert_eq!(drift.len(), 3);
        assert!(drift
            .iter()
            .all(|item| matches!(item, HookDrift::Missing { .. })));
    }

    #[test]
    fn drift_reports_stop_timeout_mismatch() {
        let mut settings = expected_settings();
        settings["hooks"]["Stop"][0]["hooks"][0]["timeout"] = json!(60);
        assert_eq!(
            detect_hook_drift(&settings, &expected()),
            vec![HookDrift::FieldMismatch {
                event: "Stop".to_string(),
                field: "timeout".to_string(),
            }]
        );
    }

    #[test]
    fn drift_reports_legacy_stop_wrapper() {
        let mut settings = expected_settings();
        settings["hooks"]["Stop"][0]["hooks"][0]["command"] =
            json!("~/.claude/hooks/throttled-stop.sh");
        assert_eq!(
            detect_hook_drift(&settings, &expected()),
            vec![HookDrift::LegacyWrapper {
                event: "Stop".to_string(),
            }]
        );
    }

    #[test]
    fn unrelated_stop_hook_is_not_classified_as_legacy() {
        let mut settings = expected_settings();
        settings["hooks"]["Stop"] = json!([{
            "hooks": [{ "type": "command", "command": "python notify-stop.py", "timeout": 10 }]
        }]);
        let drift = detect_hook_drift(&settings, &expected());
        assert_eq!(
            drift,
            vec![HookDrift::Missing {
                event: "Stop".to_string(),
            }]
        );
    }

    #[test]
    fn extra_async_field_does_not_drift() {
        let mut settings = expected_settings();
        settings["hooks"]["Stop"][0]["hooks"][0]["async"] = json!(true);
        assert!(detect_hook_drift(&settings, &expected()).is_empty());
    }

    #[test]
    fn refresh_replaces_legacy_and_preserves_extra_fields() {
        let mut groups = vec![json!({
            "hooks": [{
                "type": "command",
                "command": "~/.claude/hooks/throttled-stop.sh",
                "timeout": 60,
                "async": true
            }]
        })];
        let wanted = expected_hook(&expected(), "Stop").unwrap().clone();
        let result = refresh_our_hook(&mut groups, "hook-stop", &wanted);
        assert_eq!(
            result,
            RefreshResult {
                refreshed: false,
                replaced_legacy: true,
            }
        );
        assert_eq!(groups[0]["hooks"][0]["command"], wanted["command"]);
        assert_eq!(groups[0]["hooks"][0]["timeout"], 10);
        assert_eq!(groups[0]["hooks"][0]["async"], true);
    }

    #[test]
    fn refresh_removes_legacy_when_current_hook_already_exists() {
        let wanted = expected_hook(&expected(), "Stop").unwrap().clone();
        let mut groups = vec![
            json!({ "hooks": [wanted.clone()] }),
            json!({ "hooks": [{ "type": "command", "command": "throttled-stop.sh", "timeout": 60 }] }),
        ];
        let result = refresh_our_hook(&mut groups, "hook-stop", &wanted);
        assert!(result.refreshed);
        assert!(result.replaced_legacy);
        assert!(groups[1]["hooks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn refresh_does_not_touch_unrelated_wrapper() {
        let mut groups = vec![json!({
            "hooks": [{ "type": "command", "command": "python notify-stop.py", "timeout": 60 }]
        })];
        let wanted = expected_hook(&expected(), "Stop").unwrap().clone();
        let original = groups.clone();
        assert_eq!(
            refresh_our_hook(&mut groups, "hook-stop", &wanted),
            RefreshResult::default()
        );
        assert_eq!(groups, original);
    }

    #[test]
    fn pending_alert_obeys_threshold_and_deduplication() {
        let state = |failures, alerted| PushHookState {
            consecutive_failures: failures,
            alerted_failures: alerted,
            ..PushHookState::default()
        };
        assert!(!pending_alert(&state(2, 0)));
        assert!(pending_alert(&state(3, 0)));
        assert!(!pending_alert(&state(3, 3)));
        assert!(pending_alert(&state(4, 3)));
    }

    #[test]
    #[serial]
    fn push_hook_state_round_trip_and_corrupt_fallback() {
        let dir = tempdir().unwrap();
        let _guard = EnvGuard::set(dir.path());
        let state = PushHookState {
            consecutive_failures: 4,
            alerted_failures: 3,
            last_success_unix: Some(1),
            last_failure_unix: Some(2),
            last_error: Some("boom".to_string()),
        };
        state.save().unwrap();
        assert_eq!(PushHookState::load(), state);

        std::fs::write(ConfigManager::push_hook_state_path().unwrap(), b"not json").unwrap();
        assert_eq!(PushHookState::load(), PushHookState::default());
    }

    #[test]
    #[serial]
    fn new_project_check_ignores_non_git_cwds_without_state_or_pull() {
        let config_dir = tempdir().unwrap();
        let _guard = EnvGuard::set(config_dir.path());
        let claude_dir = tempdir().unwrap();
        let git_root = tempdir().unwrap();
        std::fs::create_dir(git_root.path().join(".git")).unwrap();
        let nested = git_root.path().join("src");
        std::fs::create_dir(&nested).unwrap();
        let missing = git_root.path().join("missing");
        let mut spawn_count = 0;

        for cwd in [nested, missing] {
            assert_eq!(
                process_new_project_check(cwd.to_str().unwrap(), claude_dir.path(), 100, || {
                    spawn_count += 1;
                    Ok(())
                }),
                None
            );
        }

        assert_eq!(spawn_count, 0);
        assert!(!ConfigManager::new_project_pull_state_path()
            .unwrap()
            .exists());
    }

    #[test]
    #[serial]
    fn new_project_check_cools_down_and_spawns_after_expiry() {
        let config_dir = tempdir().unwrap();
        let _guard = EnvGuard::set(config_dir.path());
        let claude_dir = tempdir().unwrap();
        let git_root = tempdir().unwrap();
        std::fs::create_dir(git_root.path().join(".git")).unwrap();
        let cwd = git_root.path().to_str().unwrap();
        let mut spawn_count = 0;
        let spawn = || {
            spawn_count += 1;
            Ok(())
        };

        assert_eq!(
            process_new_project_check(cwd, claude_dir.path(), 100, spawn),
            None
        );
        assert_eq!(spawn_count, 1);

        assert_eq!(
            process_new_project_check(cwd, claude_dir.path(), 200, || {
                spawn_count += 1;
                Ok(())
            }),
            None
        );
        assert_eq!(spawn_count, 1);

        assert_eq!(
            process_new_project_check(cwd, claude_dir.path(), 701, || {
                spawn_count += 1;
                Ok(())
            }),
            None
        );
        assert_eq!(spawn_count, 2);
    }

    #[test]
    #[serial]
    fn new_project_check_notifies_pending_history_and_clears_flag() {
        let config_dir = tempdir().unwrap();
        let _guard = EnvGuard::set(config_dir.path());
        let claude_dir = tempdir().unwrap();
        let git_root = tempdir().unwrap();
        std::fs::create_dir(git_root.path().join(".git")).unwrap();
        let project_name = git_root.path().file_name().unwrap().to_str().unwrap();
        std::fs::create_dir(claude_dir.path().join(format!("-tmp-{project_name}"))).unwrap();
        let mut state = NewProjectPullState::default();
        state.projects.insert(
            project_name.to_string(),
            NewProjectPullAttempt {
                last_attempt_unix: 100,
                pending_notify: true,
            },
        );
        state.save();

        let notification = process_new_project_check(
            git_root.path().to_str().unwrap(),
            claude_dir.path(),
            200,
            || Ok(()),
        );

        assert_eq!(
            notification.as_deref(),
            Some(new_project_notification(project_name).as_str())
        );
        assert!(
            !NewProjectPullState::load()
                .projects
                .get(project_name)
                .unwrap()
                .pending_notify
        );
    }

    #[test]
    #[serial]
    fn new_project_check_does_not_notify_while_project_is_still_missing() {
        let config_dir = tempdir().unwrap();
        let _guard = EnvGuard::set(config_dir.path());
        let claude_dir = tempdir().unwrap();
        let git_root = tempdir().unwrap();
        std::fs::create_dir(git_root.path().join(".git")).unwrap();
        let project_name = git_root.path().file_name().unwrap().to_str().unwrap();
        let mut state = NewProjectPullState::default();
        state.projects.insert(
            project_name.to_string(),
            NewProjectPullAttempt {
                last_attempt_unix: 100,
                pending_notify: true,
            },
        );
        state.save();
        let mut spawn_count = 0;

        assert_eq!(
            process_new_project_check(
                git_root.path().to_str().unwrap(),
                claude_dir.path(),
                200,
                || {
                    spawn_count += 1;
                    Ok(())
                },
            ),
            None
        );
        assert_eq!(spawn_count, 0);
        assert!(
            NewProjectPullState::load()
                .projects
                .get(project_name)
                .unwrap()
                .pending_notify
        );
    }

    #[test]
    fn throttle_active_handles_missing_fresh_and_old_stamps() {
        let dir = tempdir().unwrap();
        let stamp = dir.path().join("stamp");
        let now = SystemTime::now();
        assert!(!throttle_active(&stamp, now));

        let file = File::create(&stamp).unwrap();
        file.set_modified(now).unwrap();
        assert!(throttle_active(&stamp, now));

        file.set_modified(now - Duration::from_secs(PUSH_HOOK_THROTTLE_SECS + 1))
            .unwrap();
        assert!(!throttle_active(&stamp, now));
    }
    #[test]
    fn unix_process_listing_recognizes_native_and_npm_main_processes_only() {
        let listing = r#"claude /Users/me/.local/share/claude/versions/2.1.0/claude
node node /opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js
claude /path/claude --bg-pty-host
claude /path/claude --bg-spare
node node /opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js --mcp-server
claude-code-sync /Users/me/.local/bin/claude-code-sync hook-session-start
python python claude-daemon.py
"#;
        assert_eq!(
            parse_unix_process_listing(true, listing.as_bytes()),
            ProcessObservation::Known(2)
        );
        assert_eq!(
            parse_unix_process_listing(false, listing.as_bytes()),
            ProcessObservation::Unknown
        );
        assert_eq!(
            parse_unix_process_listing(true, b""),
            ProcessObservation::Known(0)
        );
        assert_eq!(
            parse_unix_process_listing(true, b"claude\n"),
            ProcessObservation::Unknown
        );
        let truncated = b"/Users/me/.local/share/claude/versions/2.1.0/clau /Users/me/.local/share/claude/versions/2.1.0/claude\n";
        assert_eq!(
            parse_unix_process_listing(true, truncated),
            ProcessObservation::Known(1)
        );
    }

    #[test]
    fn windows_process_listing_recognizes_native_and_npm_fixtures() {
        let listing = concat!(
            "claude.exe\tC:\\Users\\me\\claude.exe\r\n",
            "node.exe\tnode.exe C:\\npm\\node_modules\\@anthropic-ai\\claude-code\\cli.js\r\n",
            "node.exe\tnode.exe C:\\npm\\node_modules\\@anthropic-ai\\claude-code\\cli.js --bg-spare\r\n",
            "ccs.exe\tccs.exe hook-session-start\r\n",
            "claude.exe\tC:\\Users\\me\\claude.exe daemon run\r\n"
        );
        assert_eq!(
            parse_windows_process_listing(true, listing.as_bytes()),
            ProcessObservation::Known(2)
        );
        assert_eq!(
            parse_windows_process_listing(false, listing.as_bytes()),
            ProcessObservation::Unknown
        );
    }

    #[test]
    fn session_start_gate_fails_closed_except_known_single_startup() {
        for observation in [
            ProcessObservation::Known(0),
            ProcessObservation::Known(2),
            ProcessObservation::Unknown,
        ] {
            assert!(!should_run_session_start_pull(
                observation,
                "startup",
                false
            ));
        }
        for source in ["resume", "clear", "compact", "fork", "unknown"] {
            assert!(!should_run_session_start_pull(
                ProcessObservation::Known(1),
                source,
                false
            ));
        }
        assert!(!should_run_session_start_pull(
            ProcessObservation::Known(1),
            "startup",
            true
        ));
        assert!(should_run_session_start_pull(
            ProcessObservation::Known(1),
            "startup",
            false
        ));
    }

    #[test]
    fn session_start_lock_allows_only_one_concurrent_pull() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("last-session-pull.lock");
        let stamp_path = dir.path().join("last-session-pull");
        let barrier = Arc::new(Barrier::new(3));
        let pulls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut joins = Vec::new();
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            let pulls = Arc::clone(&pulls);
            let lock_path = lock_path.clone();
            let stamp_path = stamp_path.clone();
            joins.push(std::thread::spawn(move || {
                barrier.wait();
                run_session_start_gate(
                    ProcessObservation::Known(1),
                    "startup",
                    &lock_path,
                    &stamp_path,
                    SystemTime::now(),
                    || {
                        pulls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        Ok(())
                    },
                )
                .unwrap()
            }));
        }
        barrier.wait();
        let outcomes = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(pulls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(outcomes.iter().filter(|ran| **ran).count(), 1);
    }
    #[test]
    fn failed_session_start_pull_does_not_write_cooldown_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("last-session-pull.lock");
        let stamp_path = dir.path().join("last-session-pull");
        let failed = run_session_start_gate(
            ProcessObservation::Known(1),
            "startup",
            &lock_path,
            &stamp_path,
            SystemTime::now(),
            || anyhow::bail!("pull failed"),
        );
        assert!(failed.is_err());
        assert!(!stamp_path.exists());

        let retried = run_session_start_gate(
            ProcessObservation::Known(1),
            "startup",
            &lock_path,
            &stamp_path,
            SystemTime::now(),
            || Ok(()),
        )
        .unwrap();
        assert!(retried);
        assert!(stamp_path.exists());
    }
}
