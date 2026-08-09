//! Time-boxed "delete unlock" window.
//!
//! When active, `ccs push` treats locally-missing sessions as intentional
//! deletions and prunes them from the sync repo (same as `--prune`, but with
//! NO tombstone). The window expires passively — there is no background
//! process; every consumer re-checks the stored expiry timestamp.

use crate::config::ConfigManager;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize, Deserialize)]
struct UnlockState {
    /// Absolute expiry in unix seconds (timezone-independent).
    expires_at: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        // Fail-closed: on clock error, return MAX so `remaining_at` treats the
        // window as expired rather than reporting it active with a huge
        // remaining (宁可放行失败，绝不误删).
        .unwrap_or(u64::MAX)
}

/// Remaining seconds until expiry, or `None` if already expired.
/// Pure function — no IO — so expiry logic is unit-testable in isolation.
fn remaining_at(expires_at: u64, now: u64) -> Option<u64> {
    if now < expires_at {
        Some(expires_at - now)
    } else {
        None
    }
}

fn state_path() -> Result<PathBuf> {
    ConfigManager::delete_unlock_path()
}

/// Compute the expiry timestamp for a `minutes`-long window starting at `now`.
/// Fail-closed on the clock-error sentinel (`u64::MAX` from `now_secs`): refuse
/// to open the window instead of writing a permanent deadline that would leave
/// protection off forever (对齐读路径的 fail-closed，绝不因时钟异常永久放行).
/// Pure function — no IO — so the guard is unit-testable without a real clock.
fn compute_expiry(now: u64, minutes: u64) -> Result<u64> {
    if now == u64::MAX {
        anyhow::bail!("系统时钟异常，无法开启删除放行窗口（请检查系统时间）");
    }
    Ok(now.saturating_add(minutes.saturating_mul(60)))
}

/// Open (or extend) the window for `minutes`. Overwrites any existing state,
/// so calling again simply renews the deadline. Returns the expiry unix ts.
pub fn unlock(minutes: u64) -> Result<u64> {
    let expires_at = compute_expiry(now_secs(), minutes)?;
    ConfigManager::ensure_config_dir()?;
    let path = state_path()?;
    let json = serde_json::to_string(&UnlockState { expires_at })?;
    std::fs::write(&path, json)
        .with_context(|| format!("Failed to write delete-unlock state: {}", path.display()))?;
    Ok(expires_at)
}

/// Close the window. Idempotent: a missing file is treated as success.
pub fn disable() -> Result<()> {
    let path = state_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("Failed to remove delete-unlock state: {}", path.display()))?;
    }
    Ok(())
}

/// Remaining seconds if the window is active, else `None` (expired/absent).
pub fn status() -> Result<Option<u64>> {
    let path = state_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read delete-unlock state: {}", path.display()))?;
    let state: UnlockState = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse delete-unlock state: {}", path.display()))?;
    Ok(remaining_at(state.expires_at, now_secs()))
}

/// Fail-safe active check for push consumption. ANY error (missing/corrupt/
/// unreadable state) resolves to `false` so push falls back to protection.
#[allow(dead_code)]
pub fn is_active() -> bool {
    matches!(status(), Ok(Some(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_DIR_ENV;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    fn with_temp_config(f: impl FnOnce() + std::panic::UnwindSafe) {
        let saved = env::var(CONFIG_DIR_ENV).ok();
        let tmp = TempDir::new().unwrap();
        env::set_var(CONFIG_DIR_ENV, tmp.path());
        let result = std::panic::catch_unwind(f);
        match saved {
            Some(v) => env::set_var(CONFIG_DIR_ENV, v),
            None => env::remove_var(CONFIG_DIR_ENV),
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn test_remaining_at_active() {
        assert_eq!(remaining_at(100, 40), Some(60));
    }

    #[test]
    fn test_remaining_at_expired() {
        assert_eq!(remaining_at(100, 100), None);
        assert_eq!(remaining_at(100, 150), None);
    }

    #[test]
    fn test_compute_expiry_normal() {
        // 15 minutes = 900s past `now`.
        assert_eq!(compute_expiry(1000, 15).unwrap(), 1900);
    }

    #[test]
    fn test_compute_expiry_saturates_no_panic() {
        // Huge --minutes must saturate rather than overflow-panic.
        assert_eq!(compute_expiry(1000, u64::MAX).unwrap(), u64::MAX);
    }

    #[test]
    fn test_compute_expiry_clock_error_fails_closed() {
        // now_secs() sentinel for a clock error must refuse to open the window,
        // never write a permanent (u64::MAX) deadline.
        assert!(compute_expiry(u64::MAX, 15).is_err());
    }

    #[test]
    #[serial]
    fn test_unlock_then_status_roundtrip() {
        with_temp_config(|| {
            unlock(15).unwrap();
            let remaining = status().unwrap().expect("window should be active");
            // 15 minutes = 900s; allow a little slack for test execution.
            assert!(remaining > 890 && remaining <= 900, "remaining={remaining}");
            assert!(is_active());
        });
    }

    #[test]
    #[serial]
    fn test_disable_clears_window() {
        with_temp_config(|| {
            unlock(15).unwrap();
            disable().unwrap();
            assert_eq!(status().unwrap(), None);
            assert!(!is_active());
        });
    }

    #[test]
    #[serial]
    fn test_absent_file_is_inactive() {
        with_temp_config(|| {
            assert_eq!(status().unwrap(), None);
            assert!(!is_active());
        });
    }

    #[test]
    #[serial]
    fn test_corrupt_file_is_failsafe_inactive() {
        with_temp_config(|| {
            ConfigManager::ensure_config_dir().unwrap();
            std::fs::write(ConfigManager::delete_unlock_path().unwrap(), "not json {{").unwrap();
            // status() surfaces the parse error, but is_active() must be fail-safe.
            assert!(status().is_err());
            assert!(!is_active());
        });
    }
}
