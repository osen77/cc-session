//! Inter-process mutual exclusion for the sync repository.
//!
//! Several entry points write to the same repository — the Stop hook's push,
//! the SessionStart hook's pull, the launcher wrapper, manual commands and
//! session deletion. Git does not tolerate concurrent writers: the loser hits
//! `index.lock` or a non-fast-forward and exits non-zero, which surfaces to the
//! user as a hook failure.
//!
//! Every writer takes this lock first, so they serialize instead of colliding.
//! The lock is advisory and process-wide, held for the duration of the guard.

use anyhow::Result;
use colored::Colorize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::atomic_file::FileLock;
use crate::config::ConfigManager;
use crate::VerbosityLevel;

/// How long an interactive invocation waits for a busy repository.
///
/// Long enough to outlast a normal push (~20s), short enough that a wedged
/// process does not hang a terminal indefinitely.
const INTERACTIVE_WAIT: Duration = Duration::from_secs(120);

/// Whether *this process* already holds the repository lock.
///
/// `flock` is per file descriptor, not per process: a second acquisition from
/// the same process blocks against itself. Nesting two locked calls would
/// therefore deadlock, and such a deadlock is painful to diagnose. The flag
/// makes a nested acquisition a no-op instead.
static REPO_LOCK_HELD: AtomicBool = AtomicBool::new(false);

/// Result of trying to take the repository lock.
pub(crate) enum RepoLockOutcome {
    /// The caller now owns exclusive access.
    Acquired(RepoLock),
    /// Another process is mid-sync; the caller should skip its work.
    Busy,
}

/// RAII guard for exclusive access to one sync repository.
pub(crate) struct RepoLock {
    /// `None` for a re-entrant guard, which owns no file lock.
    _file: Option<FileLock>,
    /// Only the outermost guard clears the process-wide flag.
    owns_process_flag: bool,
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        if self.owns_process_flag {
            REPO_LOCK_HELD.store(false, Ordering::SeqCst);
        }
    }
}

impl RepoLock {
    /// Take the lock, waiting only when a human is watching.
    ///
    /// Non-interactive callers (hooks, wrappers, scripts) must not stall: the
    /// Claude Code hook harness kills them after 60s and reports an error, so
    /// waiting merely trades one failure for another. They get `Busy`
    /// immediately. Interactive callers typed the command and expect it to run,
    /// so they poll up to [`INTERACTIVE_WAIT`].
    pub(crate) fn acquire(repo_path: &Path) -> Result<RepoLockOutcome> {
        // Re-entrant: this process already holds it, so the caller is nested
        // inside another locked section and is already serialized.
        if REPO_LOCK_HELD
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(RepoLockOutcome::Acquired(Self {
                _file: None,
                owns_process_flag: false,
            }));
        }

        // From here on the flag is set, so every exit path must clear it unless
        // it hands back an owning guard.
        let result = Self::acquire_file_lock(repo_path);
        match result {
            Ok(Some(file)) => Ok(RepoLockOutcome::Acquired(Self {
                _file: Some(file),
                owns_process_flag: true,
            })),
            Ok(None) => {
                REPO_LOCK_HELD.store(false, Ordering::SeqCst);
                Ok(RepoLockOutcome::Busy)
            }
            Err(error) => {
                REPO_LOCK_HELD.store(false, Ordering::SeqCst);
                Err(error)
            }
        }
    }

    fn acquire_file_lock(repo_path: &Path) -> Result<Option<FileLock>> {
        let lock_path = ConfigManager::sync_repo_lock_path(repo_path)?;

        if crate::interactive_conflict::is_interactive() {
            FileLock::acquire_with_timeout(&lock_path, INTERACTIVE_WAIT, || {
                println!("  {} 另一个同步正在进行，等待中...", "⏳".yellow());
            })
        } else {
            FileLock::try_acquire(&lock_path)
        }
    }

    /// Take the lock, reporting contention for the caller.
    ///
    /// Returns `None` when the repository is busy — the caller should return
    /// success without doing work. A skipped sync is not an error: returning
    /// non-zero here is exactly what makes Claude Code report a hook failure,
    /// and the next hook will sync anyway.
    pub(crate) fn acquire_or_report(
        repo_path: &Path,
        operation: &str,
        verbosity: VerbosityLevel,
    ) -> Result<Option<Self>> {
        match Self::acquire(repo_path)? {
            RepoLockOutcome::Acquired(lock) => Ok(Some(lock)),
            RepoLockOutcome::Busy => {
                log::info!("another sync is in progress, skipping {operation}");
                if verbosity != VerbosityLevel::Quiet {
                    println!(
                        "  {} 另一个同步正在进行，已跳过本次{operation}",
                        "ℹ".yellow()
                    );
                }
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    /// The process-wide flag is global state; tests that touch it must not run
    /// concurrently with each other.
    fn reset_flag() {
        REPO_LOCK_HELD.store(false, Ordering::SeqCst);
    }

    #[test]
    #[serial]
    fn nested_acquisition_does_not_deadlock() {
        reset_flag();
        let dir = tempdir().unwrap();
        std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", dir.path());

        let outer = RepoLock::acquire(dir.path()).unwrap();
        assert!(matches!(outer, RepoLockOutcome::Acquired(_)));

        // Would block forever against its own flock without the re-entrancy
        // guard, so reaching the assertion at all is the real check.
        let inner = RepoLock::acquire(dir.path()).unwrap();
        assert!(matches!(inner, RepoLockOutcome::Acquired(_)));

        drop(inner);
        // The inner guard must not have released the process flag.
        assert!(REPO_LOCK_HELD.load(Ordering::SeqCst));
        drop(outer);
        assert!(!REPO_LOCK_HELD.load(Ordering::SeqCst));

        std::env::remove_var("CLAUDE_CODE_SYNC_CONFIG_DIR");
    }

    #[test]
    #[serial]
    fn flag_is_cleared_when_guard_drops() {
        reset_flag();
        let dir = tempdir().unwrap();
        std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", dir.path());

        {
            let _lock = RepoLock::acquire(dir.path()).unwrap();
            assert!(REPO_LOCK_HELD.load(Ordering::SeqCst));
        }
        assert!(!REPO_LOCK_HELD.load(Ordering::SeqCst));

        // A fresh acquisition must still succeed after the flag is cleared.
        let again = RepoLock::acquire(dir.path()).unwrap();
        assert!(matches!(again, RepoLockOutcome::Acquired(_)));

        std::env::remove_var("CLAUDE_CODE_SYNC_CONFIG_DIR");
    }

    #[test]
    #[serial]
    fn lock_path_differs_per_repository() {
        let config = tempdir().unwrap();
        std::env::set_var("CLAUDE_CODE_SYNC_CONFIG_DIR", config.path());
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();

        let a = ConfigManager::sync_repo_lock_path(first.path()).unwrap();
        let b = ConfigManager::sync_repo_lock_path(second.path()).unwrap();
        assert_ne!(a, b, "distinct repositories must not share a lock");

        // Same repository, trailing separator: must map to the same lock.
        let with_slash = ConfigManager::sync_repo_lock_path(&first.path().join("")).unwrap();
        assert_eq!(a, with_slash);

        std::env::remove_var("CLAUDE_CODE_SYNC_CONFIG_DIR");
    }
}
