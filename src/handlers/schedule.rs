//! Local-only scheduling. No operation here invokes a sync command.
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;

use crate::atomic_file::{persist_json_pretty_atomic, FileLock};
use crate::config::ConfigManager;
use crate::path_security::{
    prepare_regular_file_destination, safe_join_within_root, validate_directory_root,
};

pub const SCHEDULE_LABEL: &str = "com.claude-code-sync.scheduled-push";
const MAX_RESULT_MESSAGE_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleRule {
    Daily { hour: u8, minute: u8 },
    EveryHours { hours: u64 },
}

impl ScheduleRule {
    pub fn parse(daily: Option<&str>, every_hours: Option<&str>) -> Result<Self> {
        let rule = match (daily, every_hours) {
            (Some(time), None) => {
                let bytes = time.as_bytes();
                if bytes.len() != 5
                    || bytes[2] != b':'
                    || ![bytes[0], bytes[1], bytes[3], bytes[4]]
                        .iter()
                        .all(u8::is_ascii_digit)
                {
                    bail!("daily time must be exactly HH:MM in 24-hour local time");
                }
                Self::Daily {
                    hour: time[..2].parse()?,
                    minute: time[3..].parse()?,
                }
            }
            (None, Some(hours)) => {
                if hours.is_empty() || !hours.bytes().all(|byte| byte.is_ascii_digit()) {
                    bail!("every-hours must be a positive integer");
                }
                Self::EveryHours {
                    hours: hours.parse().context("every-hours is too large")?,
                }
            }
            _ => bail!("specify exactly one of --daily HH:MM or --every-hours N"),
        };
        rule.validate()?;
        Ok(rule)
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Daily { hour, minute } if *hour < 24 && *minute < 60 => Ok(()),
            Self::EveryHours { hours } if *hours > 0 => {
                let seconds = hours
                    .checked_mul(3600)
                    .context("every-hours seconds overflow")?;
                // launchd's StartInterval is a signed 32-bit interval in seconds.
                if seconds > i32::MAX as u64 {
                    bail!("every-hours exceeds launchd's interval limit");
                }
                Ok(())
            }
            _ => bail!("invalid schedule rule"),
        }
    }
}

impl std::fmt::Display for ScheduleRule {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Daily { hour, minute } => write!(formatter, "daily {hour:02}:{minute:02}"),
            Self::EveryHours { hours } => write!(formatter, "every {hours} hours"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    pub rule: ScheduleRule,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledOutcome {
    Running,
    Success,
    NoChanges,
    LockBusy,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledResult {
    pub attempted_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub outcome: ScheduledOutcome,
    pub message: Option<String>,
    /// RepoLock owner's attempt, independent of more recent lock-busy attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_attempted_at: Option<DateTime<Utc>>,
}

/// Record an attempt/result without retaining unbounded logs. Call Running after
/// acquiring RepoLock, and a terminal outcome before releasing it. LockBusy is
/// recorded directly by the contending invocation without replacing the active
/// RepoLock owner's attempt identity.
pub fn record_result(outcome: ScheduledOutcome, message: Option<&str>) -> Result<()> {
    record_result_at(&ConfigManager::config_dir()?, outcome, message)
}

fn record_result_at(
    config_dir: &Path,
    outcome: ScheduledOutcome,
    message: Option<&str>,
) -> Result<()> {
    fs::create_dir_all(config_dir)?;
    validate_directory_root(config_dir)?;
    let _lock = FileLock::acquire(&config_dir.join("schedule-result.lock"))?;
    let path = prepare_regular_file_destination(config_dir, Path::new("schedule-result.json"))?;
    let now = Utc::now();
    let previous: Option<ScheduledResult> = read_json_optional(&path)?;
    let active = previous.as_ref().and_then(|previous| {
        previous.active_attempted_at.or_else(|| {
            (previous.outcome == ScheduledOutcome::Running).then_some(previous.attempted_at)
        })
    });
    let (attempted_at, active_attempted_at) = match outcome {
        ScheduledOutcome::Running => (now, Some(now)),
        ScheduledOutcome::LockBusy => (now, active),
        _ => (active.unwrap_or(now), None),
    };
    let message = message.map(|text| {
        let mut end = text.len().min(MAX_RESULT_MESSAGE_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_owned()
    });
    persist_json_pretty_atomic(
        &path,
        &ScheduledResult {
            attempted_at,
            recorded_at: now,
            outcome,
            message,
            active_attempted_at,
        },
    )
}

/// Injectable boundary: implementations must not execute the scheduled job.
pub trait LaunchctlRunner {
    fn is_loaded(&mut self, service: &str) -> Result<bool>;
    fn bootstrap(&mut self, domain: &str, plist: &Path) -> Result<()>;
    fn bootout(&mut self, service: &str) -> Result<()>;
}

struct SystemLaunchctl;

impl LaunchctlRunner for SystemLaunchctl {
    fn is_loaded(&mut self, service: &str) -> Result<bool> {
        let output = Command::new("/bin/launchctl")
            .args(["print", service])
            .output()?;
        if output.status.success() {
            return Ok(true);
        }
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("Could not find service") {
            return Ok(false);
        }
        bail!("launchctl print failed: {error}")
    }
    fn bootstrap(&mut self, domain: &str, plist: &Path) -> Result<()> {
        let output = Command::new("/bin/launchctl")
            .arg("bootstrap")
            .arg(domain)
            .arg(plist)
            .output()?;
        if !output.status.success() {
            bail!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }
    fn bootout(&mut self, service: &str) -> Result<()> {
        let output = Command::new("/bin/launchctl")
            .args(["bootout", service])
            .output()?;
        if !output.status.success() {
            bail!(
                "launchctl bootout failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }
}

pub struct ScheduleManager {
    config_dir: PathBuf,
    home: PathBuf,
    executable: PathBuf,
    domain: String,
    supported: bool,
}

impl ScheduleManager {
    pub fn current() -> Result<Self> {
        #[cfg(unix)]
        let uid = unsafe { libc::geteuid() };
        #[cfg(not(unix))]
        let uid = 0;
        Ok(Self {
            config_dir: std::path::absolute(ConfigManager::config_dir()?)?,
            home: dirs::home_dir().context("home directory unavailable")?,
            executable: std::env::current_exe()?,
            domain: format!("gui/{uid}"),
            supported: cfg!(target_os = "macos"),
        })
    }

    fn config_path(&self) -> PathBuf {
        self.config_dir.join("schedule.json")
    }
    fn plist_path(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{SCHEDULE_LABEL}.plist"))
    }
    fn service(&self) -> String {
        format!("{}/{SCHEDULE_LABEL}", self.domain)
    }

    pub fn load(&self) -> Result<Option<ScheduleConfig>> {
        let config: Option<ScheduleConfig> = read_json_optional(&self.config_path())?;
        if let Some(config) = &config {
            config.rule.validate()?;
        }
        Ok(config)
    }

    pub fn set(&self, rule: ScheduleRule, runner: &mut impl LaunchctlRunner) -> Result<()> {
        rule.validate()?;
        let _lock = self.lock()?;
        let old = self.load()?;
        let enabled = old.as_ref().is_some_and(|config| config.enabled);
        self.apply(ScheduleConfig { rule, enabled }, runner)
    }

    pub fn set_enabled(&self, enabled: bool, runner: &mut impl LaunchctlRunner) -> Result<()> {
        if enabled && !self.supported {
            bail!("system scheduling is currently supported only on macOS");
        }
        let _lock = self.lock()?;
        let Some(mut config) = self.load()? else {
            if enabled {
                bail!("configure a rule with schedule set first");
            }
            return Ok(());
        };
        config.enabled = enabled;
        self.apply(config, runner)
    }

    fn lock(&self) -> Result<FileLock> {
        fs::create_dir_all(&self.config_dir)?;
        validate_directory_root(&self.config_dir)?;
        FileLock::acquire(&self.config_dir.join("schedule.lock"))
    }

    fn apply(&self, config: ScheduleConfig, runner: &mut impl LaunchctlRunner) -> Result<()> {
        if config.enabled && !self.supported {
            bail!("system scheduling is currently supported only on macOS");
        }
        let config_path =
            prepare_regular_file_destination(&self.config_dir, Path::new("schedule.json"))?;
        let old_config = read_bytes_optional(&config_path)?;
        let new_config = serde_json::to_vec_pretty(&config)?;
        let staged_config = stage(&config_path, &new_config)?;
        // Disabled configuration is portable, but no platform other than macOS
        // may inspect or install a system task.
        if !self.supported {
            staged_config.persist(&config_path)?;
            return Ok(());
        }
        let plist_path = self.plist_path();
        validate_directory_root(&self.home)?;
        safe_join_within_root(
            &self.home,
            Path::new("Library/LaunchAgents")
                .join(format!("{SCHEDULE_LABEL}.plist"))
                .as_path(),
        )?;
        let old_plist = read_bytes_optional(&plist_path)?;
        let new_plist = if config.enabled {
            Some(
                render_plist(&config.rule, &self.executable, &self.config_dir, &self.home)?
                    .into_bytes(),
            )
        } else {
            None
        };
        let service = self.service();
        // Do not contact launchd for a first, disabled configuration.
        let loaded = if old_config.is_some() || old_plist.is_some() || config.enabled {
            runner.is_loaded(&service)?
        } else {
            false
        };
        if loaded && old_plist.is_none() {
            bail!("CCS LaunchAgent is loaded but its plist is missing; refusing an update that cannot be rolled back");
        }
        if old_config.as_ref() == Some(&new_config)
            && old_plist == new_plist
            && loaded == config.enabled
        {
            return Ok(());
        }
        let staged_plist = if let Some(bytes) = &new_plist {
            validate_directory_root(&self.home)?;
            let destination = prepare_regular_file_destination(
                &self.home,
                Path::new("Library/LaunchAgents")
                    .join(format!("{SCHEDULE_LABEL}.plist"))
                    .as_path(),
            )?;
            Some(stage(&destination, bytes)?)
        } else {
            None
        };
        // Both files are staged before unloading anything. A failing unload
        // leaves all persistent state untouched.
        if loaded {
            runner.bootout(&service)?;
        }
        let mut new_loaded = false;
        let result: Result<()> = (|| {
            match staged_plist {
                Some(file) => {
                    file.persist(&plist_path)?;
                }
                None if old_plist.is_some() => fs::remove_file(&plist_path)?,
                None => {}
            }
            if config.enabled {
                runner.bootstrap(&self.domain, &plist_path)?;
                new_loaded = true;
            }
            staged_config.persist(&config_path)?;
            Ok(())
        })();
        if let Err(error) = result {
            let rollback: Result<()> = (|| {
                if new_loaded {
                    runner.bootout(&service)?;
                }
                restore_bytes(&plist_path, old_plist.as_deref())?;
                restore_bytes(&config_path, old_config.as_deref())?;
                if loaded {
                    runner.bootstrap(&self.domain, &plist_path)?;
                }
                Ok(())
            })();
            if let Err(rollback_error) = rollback {
                bail!(
                    "schedule update failed: {error:#}; rollback also failed: {rollback_error:#}"
                );
            }
            return Err(
                error.context("schedule update failed; previous configuration and task restored")
            );
        }
        Ok(())
    }
}

fn read_bytes_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!("schedule state must be a regular file: {}", path.display());
            }
            Ok(Some(fs::read(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_json_optional<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    read_bytes_optional(path)?
        .map(|bytes| {
            serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid schedule state: {}", path.display()))
        })
        .transpose()
}

fn stage(path: &Path, bytes: &[u8]) -> Result<NamedTempFile> {
    let mut file = NamedTempFile::new_in(path.parent().context("schedule file has no parent")?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    Ok(file)
}

fn restore_bytes(path: &Path, bytes: Option<&[u8]>) -> Result<()> {
    if let Some(bytes) = bytes {
        stage(path, bytes)?.persist(path)?;
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn xml(value: &str) -> Result<String> {
    if value
        .chars()
        .any(|character| character < ' ' && !matches!(character, '\t' | '\n' | '\r'))
    {
        bail!("path contains characters unsupported by XML");
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

pub fn render_plist(
    rule: &ScheduleRule,
    executable: &Path,
    config_dir: &Path,
    home: &Path,
) -> Result<String> {
    rule.validate()?;
    for path in [executable, config_dir, home] {
        if !path.is_absolute() {
            bail!("LaunchAgent paths must be absolute");
        }
    }
    let executable = xml(executable
        .to_str()
        .context("CCS executable path is not UTF-8")?)?;
    let config_dir = xml(config_dir
        .to_str()
        .context("config directory is not UTF-8")?)?;
    let home = xml(home.to_str().context("home directory is not UTF-8")?)?;
    let trigger = match rule {
        ScheduleRule::Daily { hour, minute } => format!("<key>StartCalendarInterval</key><dict><key>Hour</key><integer>{hour}</integer><key>Minute</key><integer>{minute}</integer></dict>"),
        ScheduleRule::EveryHours { hours } => format!("<key>StartInterval</key><integer>{}</integer>", hours * 3600),
    };
    Ok(format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{SCHEDULE_LABEL}</string><key>ProgramArguments</key><array><string>{executable}</string><string>push</string><string>--scheduled</string></array><key>EnvironmentVariables</key><dict><key>HOME</key><string>{home}</string><key>CLAUDE_CODE_SYNC_CONFIG_DIR</key><string>{config_dir}</string></dict>{trigger}<key>RunAtLoad</key><false/><key>KeepAlive</key><false/></dict></plist>\n"))
}

pub fn set(daily: Option<&str>, every_hours: Option<&str>) -> Result<()> {
    let rule = ScheduleRule::parse(daily, every_hours)?;
    ScheduleManager::current()?.set(rule, &mut SystemLaunchctl)
}

pub fn enable() -> Result<()> {
    ScheduleManager::current()?.set_enabled(true, &mut SystemLaunchctl)
}
pub fn disable() -> Result<()> {
    ScheduleManager::current()?.set_enabled(false, &mut SystemLaunchctl)
}

pub fn show() -> Result<()> {
    let manager = ScheduleManager::current()?;
    let config = manager.load()?;
    match config {
        Some(config) => {
            println!("Rule: {}", config.rule);
            println!(
                "Configured: {}",
                if config.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
        }
        None => println!("Schedule: not configured (disabled)"),
    }
    println!("Local timezone: {}", Local::now().format("%Z (UTC%:z)"));
    if manager.supported {
        println!(
            "LaunchAgent: {}",
            if SystemLaunchctl.is_loaded(&manager.service())? {
                "loaded"
            } else {
                "not loaded"
            }
        );
    } else {
        println!("System scheduling: unsupported on this platform");
    }
    let result: Option<ScheduledResult> =
        read_json_optional(&manager.config_dir.join("schedule-result.json"))?;
    if let Some(result) = result {
        println!(
            "Last attempt: {}",
            result.attempted_at.with_timezone(&Local)
        );
        println!(
            "Last result: {:?} ({})",
            result.outcome,
            result.recorded_at.with_timezone(&Local)
        );
        if let Some(active) = result.active_attempted_at {
            println!(
                "Active push attempt started: {} (no completion recorded)",
                active.with_timezone(&Local)
            );
        }
        if let Some(message) = result.message {
            println!("Details: {message}");
        }
    } else {
        println!("Last result: none");
    }
    println!("Uses system local time; does not wake the computer or replay every missed interval.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockLaunchctl {
        loaded: bool,
        calls: Vec<&'static str>,
        fail_bootstrap: bool,
        fail_bootout: bool,
    }

    impl LaunchctlRunner for MockLaunchctl {
        fn is_loaded(&mut self, _: &str) -> Result<bool> {
            Ok(self.loaded)
        }
        fn bootstrap(&mut self, _: &str, _: &Path) -> Result<()> {
            self.calls.push("bootstrap");
            if std::mem::take(&mut self.fail_bootstrap) {
                bail!("injected bootstrap failure");
            }
            self.loaded = true;
            Ok(())
        }
        fn bootout(&mut self, _: &str) -> Result<()> {
            self.calls.push("bootout");
            if std::mem::take(&mut self.fail_bootout) {
                bail!("injected bootout failure");
            }
            self.loaded = false;
            Ok(())
        }
    }

    fn fixture(root: &Path) -> ScheduleManager {
        std::fs::create_dir_all(root.join("home")).unwrap();
        ScheduleManager {
            config_dir: root.join("config"),
            home: root.join("home"),
            executable: root.join("bin/ccs & safe"),
            domain: "gui/501".into(),
            supported: true,
        }
    }

    #[test]
    fn rejects_ambiguous_or_invalid_rules() {
        for time in [
            "5:00",
            "05:0",
            "24:00",
            "12:60",
            "-1:00",
            " 05:00",
            "05:00 ",
            "０５:00",
        ] {
            assert!(ScheduleRule::parse(Some(time), None).is_err(), "{time}");
        }
        for hours in ["0", "-1", "+1", "1.5", " 1", "18446744073709551615"] {
            assert!(ScheduleRule::parse(None, Some(hours)).is_err(), "{hours}");
        }
        assert!(ScheduleRule::parse(None, None).is_err());
        assert!(ScheduleRule::parse(Some("05:00"), Some("6")).is_err());
        for time in ["00:00", "05:00", "13:47", "23:59"] {
            assert!(ScheduleRule::parse(Some(time), None).is_ok());
        }
        for hours in ["1", "6", "24"] {
            assert!(ScheduleRule::parse(None, Some(hours)).is_ok());
        }
    }

    #[test]
    fn configuration_stays_disabled_until_explicit_enable_and_switches_triggers() -> Result<()> {
        let root = tempfile::tempdir()?;
        let manager = fixture(root.path());
        let mut runner = MockLaunchctl::default();
        manager.set(ScheduleRule::parse(Some("05:00"), None)?, &mut runner)?;
        assert!(!manager.load()?.unwrap().enabled);
        assert!(!manager.plist_path().exists());
        assert!(runner.calls.is_empty());
        manager.set_enabled(true, &mut runner)?;
        manager.set_enabled(true, &mut runner)?;
        assert_eq!(runner.calls, ["bootstrap"]);
        let daily = std::fs::read_to_string(manager.plist_path())?;
        assert!(daily.contains("StartCalendarInterval"));
        assert!(!daily.contains("StartInterval"));
        assert!(daily.contains("ccs &amp; safe"));
        assert!(daily.contains("<string>push</string><string>--scheduled</string>"));
        assert!(daily.contains("<key>RunAtLoad</key><false/>"));
        assert!(daily.contains("<key>KeepAlive</key><false/>"));
        manager.set(ScheduleRule::parse(None, Some("6"))?, &mut runner)?;
        let interval = std::fs::read_to_string(manager.plist_path())?;
        assert!(interval.contains("<key>StartInterval</key><integer>21600</integer>"));
        assert!(!interval.contains("StartCalendarInterval"));
        manager.set_enabled(false, &mut runner)?;
        manager.set_enabled(false, &mut runner)?;
        assert!(!runner.loaded);
        assert!(!manager.plist_path().exists());
        manager.set(ScheduleRule::parse(Some("13:47"), None)?, &mut runner)?;
        manager.set_enabled(true, &mut runner)?;
        assert!(std::fs::read_to_string(manager.plist_path())?
            .contains("<key>Hour</key><integer>13</integer>"));
        Ok(())
    }

    #[test]
    fn reload_failure_restores_old_configuration_and_loaded_task() -> Result<()> {
        let root = tempfile::tempdir()?;
        let manager = fixture(root.path());
        let mut runner = MockLaunchctl::default();
        manager.set(ScheduleRule::parse(Some("05:00"), None)?, &mut runner)?;
        manager.set_enabled(true, &mut runner)?;
        let original_config = std::fs::read(manager.config_path())?;
        let original_plist = std::fs::read(manager.plist_path())?;
        runner.fail_bootstrap = true;
        assert!(manager
            .set(ScheduleRule::parse(None, Some("24"))?, &mut runner)
            .is_err());
        assert_eq!(std::fs::read(manager.config_path())?, original_config);
        assert_eq!(std::fs::read(manager.plist_path())?, original_plist);
        assert!(runner.loaded);
        runner.fail_bootout = true;
        assert!(manager.set_enabled(false, &mut runner).is_err());
        assert_eq!(std::fs::read(manager.config_path())?, original_config);
        assert!(runner.loaded);
        Ok(())
    }

    #[test]
    fn failed_preparation_does_not_unload_or_replace_existing_task() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut manager = fixture(root.path());
        let mut runner = MockLaunchctl::default();
        manager.set(ScheduleRule::parse(Some("05:00"), None)?, &mut runner)?;
        manager.set_enabled(true, &mut runner)?;
        let old_config = fs::read(manager.config_path())?;
        let old_plist = fs::read(manager.plist_path())?;
        let calls = runner.calls.clone();
        manager.executable = PathBuf::from("relative/ccs");
        assert!(manager
            .set(ScheduleRule::parse(None, Some("6"))?, &mut runner)
            .is_err());
        assert_eq!(runner.calls, calls);
        assert!(runner.loaded);
        assert_eq!(fs::read(manager.config_path())?, old_config);
        assert_eq!(fs::read(manager.plist_path())?, old_plist);
        assert!(manager
            .set(
                ScheduleRule::Daily {
                    hour: 24,
                    minute: 0
                },
                &mut runner
            )
            .is_err());
        assert_eq!(fs::read(manager.config_path())?, old_config);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn launchagent_parent_symlink_is_rejected_without_writing_outside_home() -> Result<()> {
        let root = tempfile::tempdir()?;
        let manager = fixture(root.path());
        let outside = root.path().join("outside");
        fs::create_dir(&outside)?;
        let mut runner = MockLaunchctl::default();
        manager.set(ScheduleRule::parse(Some("05:00"), None)?, &mut runner)?;
        std::os::unix::fs::symlink(&outside, manager.home.join("Library"))?;
        assert!(manager.set_enabled(true, &mut runner).is_err());
        assert!(!outside.join("LaunchAgents").exists());
        assert!(!manager.load()?.unwrap().enabled);
        assert!(runner.calls.is_empty());
        Ok(())
    }

    #[test]
    fn unsupported_install_leaves_disabled_configuration_untouched() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut manager = fixture(root.path());
        manager.supported = false;
        let mut runner = MockLaunchctl::default();
        manager.set(ScheduleRule::parse(Some("05:00"), None)?, &mut runner)?;
        assert!(manager.set_enabled(true, &mut runner).is_err());
        assert!(!manager.load()?.unwrap().enabled);
        assert!(runner.calls.is_empty());
        Ok(())
    }

    #[test]
    fn busy_attempt_does_not_steal_running_attempt_identity() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("schedule-result.json");
        record_result_at(root.path(), ScheduledOutcome::Running, None)?;
        let running: ScheduledResult = read_json_optional(&path)?.unwrap();
        record_result_at(
            root.path(),
            ScheduledOutcome::LockBusy,
            Some("another process owns RepoLock"),
        )?;
        let busy: ScheduledResult = read_json_optional(&path)?.unwrap();
        assert_eq!(busy.outcome, ScheduledOutcome::LockBusy);
        assert_eq!(busy.active_attempted_at, Some(running.attempted_at));
        record_result_at(root.path(), ScheduledOutcome::LockBusy, None)?;
        record_result_at(root.path(), ScheduledOutcome::Success, None)?;
        let completed: ScheduledResult = read_json_optional(&path)?.unwrap();
        assert_eq!(completed.outcome, ScheduledOutcome::Success);
        assert_eq!(completed.attempted_at, running.attempted_at);
        assert!(completed.active_attempted_at.is_none());
        record_result_at(root.path(), ScheduledOutcome::LockBusy, None)?;
        let busy: ScheduledResult = read_json_optional(&path)?.unwrap();
        assert!(busy.active_attempted_at.is_none());
        record_result_at(root.path(), ScheduledOutcome::Running, None)?;
        let next: ScheduledResult = read_json_optional(&path)?.unwrap();
        record_result_at(root.path(), ScheduledOutcome::NoChanges, None)?;
        let completed: ScheduledResult = read_json_optional(&path)?.unwrap();
        assert_eq!(completed.attempted_at, next.attempted_at);
        assert_eq!(completed.outcome, ScheduledOutcome::NoChanges);
        assert!(completed.active_attempted_at.is_none());
        Ok(())
    }

    #[test]
    fn results_are_bounded_and_private() -> Result<()> {
        let root = tempfile::tempdir()?;
        record_result_at(
            root.path(),
            ScheduledOutcome::Error,
            Some(&"错".repeat(10000)),
        )?;
        let path = root.path().join("schedule-result.json");
        let result: ScheduledResult = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(result.outcome, ScheduledOutcome::Error);
        assert!(result.message.unwrap().len() <= MAX_RESULT_MESSAGE_BYTES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path)?.permissions().mode() & 0o777,
                0o600
            );
        }
        record_result_at(root.path(), ScheduledOutcome::NoChanges, None)?;
        let result: ScheduledResult = serde_json::from_slice(&std::fs::read(path)?)?;
        assert_eq!(result.outcome, ScheduledOutcome::NoChanges);
        assert!(result.message.is_none());
        Ok(())
    }
}
