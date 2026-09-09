//! Claude Code hooks management
//!
//! This module handles installation and management of Claude Code hooks
//! for automatic synchronization.

use anyhow::{Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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

fn detached_worker_command(exe: &Path) -> Command {
    let mut command = Command::new(exe);
    command
        .args(["hook-stop", "--worker"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Spawn the Stop-hook worker without waiting for it.
fn spawn_detached_worker() -> std::io::Result<Child> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(BINARY_NAME));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut command = detached_worker_command(&exe);
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

        let mut command = detached_worker_command(&exe);
        command.creation_flags(BASE_FLAGS | CREATE_BREAKAWAY_FROM_JOB);
        match command.spawn() {
            Ok(child) => Ok(child),
            Err(first_error) => {
                let mut fallback = detached_worker_command(&exe);
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
        detached_worker_command(&exe).spawn()
    }
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

    if let Ok(filter) = crate::filter::FilterConfig::load() {
        if filter.config_sync.enabled {
            match super::config_sync::handle_config_push(&filter.config_sync) {
                Ok(()) => append_hook_debug("Stop worker config push completed"),
                Err(error) => append_hook_debug(&format!(
                    "Stop worker config push failed (history push kept): {error:#}"
                )),
            }
        }
    }

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

/// Count running Claude Code processes.
///
/// The unix branch matches `native-binary/claude`, which only covers the native
/// install layout. An npm-global install runs from
/// `@anthropic-ai/claude-code/bin/claude.exe` and matches nothing, so this
/// returns 0 there and `is_first_instance` in `handle_session_start` is always
/// true. Widen the pattern before relying on the count for anything.
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
/// Pulls only on first startup, gated on three conditions:
/// 1. Process count <= 1 (no other Claude instances) — see
///    `count_claude_processes`: inert on npm-global installs, where it counts 0
///    and this condition always passes
/// 2. source = "startup" (not resume/compact)
/// 3. Debounce not active (extra protection)
///
/// So in practice conditions 2 and 3 are what gate the pull.
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
}
