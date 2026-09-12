use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::atomic_file::{persist_json_atomic, sync_parent_directory, FileLock};
use crate::config::ConfigManager;
#[cfg(test)]
use crate::session_cache::fingerprint_file;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PullGuardEntry {
    pub(crate) session_id: String,
    pub(crate) remote_relative_path: PathBuf,
    pub(crate) remote_fingerprint: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PullGuardState {
    #[serde(default)]
    entries: HashMap<String, PullGuardEntry>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PullGuardRegistry {
    state: PullGuardState,
}

impl PullGuardRegistry {
    fn state_path() -> Result<PathBuf> {
        Ok(ConfigManager::config_dir()?.join("pull-guard.json"))
    }

    fn lock_path() -> Result<PathBuf> {
        Ok(ConfigManager::config_dir()?.join("pull-guard.lock"))
    }

    pub(crate) fn load() -> Result<Self> {
        let path = Self::state_path()?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Self {
                state: serde_json::from_slice(&bytes).with_context(|| {
                    format!("failed to parse pull guard state: {}", path.display())
                })?,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error)
                .with_context(|| format!("failed to read pull guard state: {}", path.display())),
        }
    }

    pub(crate) fn reconcile(
        protected: impl IntoIterator<Item = PullGuardEntry>,
        completed: impl IntoIterator<Item = (String, PathBuf, String)>,
    ) -> Result<()> {
        let _lock = FileLock::acquire(&Self::lock_path()?)?;
        let mut registry = Self::load()?;
        for (session_id, relative_path, fingerprint) in completed {
            if registry
                .state
                .entries
                .get(&session_id)
                .is_some_and(|entry| {
                    entry.remote_relative_path == relative_path
                        && entry.remote_fingerprint == fingerprint
                })
            {
                registry.state.entries.remove(&session_id);
            }
        }
        for entry in protected {
            registry
                .state
                .entries
                .insert(entry.session_id.clone(), entry);
        }
        let path = Self::state_path()?;
        if registry.state.entries.is_empty() {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    if let Some(parent) = path.parent() {
                        sync_parent_directory(parent)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        } else {
            persist_json_atomic(&path, &registry.state)?;
        }
        Ok(())
    }

    pub(crate) fn suppresses_push(
        &self,
        session_id: &str,
        remote_relative_path: &Path,
        _remote_file: &Path,
    ) -> Result<bool> {
        let Some(entry) = self.state.entries.get(session_id) else {
            return Ok(false);
        };
        Ok(entry.remote_relative_path == remote_relative_path)
    }

    pub(crate) fn protects_remote_path(
        &self,
        remote_relative_path: &Path,
        _remote_file: &Path,
    ) -> Result<bool> {
        Ok(self
            .state
            .entries
            .values()
            .any(|entry| entry.remote_relative_path == remote_relative_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct ConfigGuard(Option<std::ffi::OsString>);

    impl ConfigGuard {
        fn set(path: &Path) -> Self {
            let old = std::env::var_os(crate::config::CONFIG_DIR_ENV);
            std::env::set_var(crate::config::CONFIG_DIR_ENV, path);
            Self(old)
        }
    }

    impl Drop for ConfigGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }

    fn entry(fingerprint: String) -> PullGuardEntry {
        PullGuardEntry {
            session_id: "session".to_string(),
            remote_relative_path: PathBuf::from("project/session.jsonl"),
            remote_fingerprint: fingerprint,
            reason: "incomplete append-only merge".to_string(),
        }
    }

    #[test]
    #[serial]
    fn guard_blocks_same_session_path_across_remote_revisions() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = ConfigGuard::set(temp.path());
        let remote = temp.path().join("remote.jsonl");
        std::fs::write(&remote, b"remote\n").unwrap();
        let fingerprint = fingerprint_file(&remote).unwrap().digest;
        PullGuardRegistry::reconcile([entry(fingerprint)], []).unwrap();
        let registry = PullGuardRegistry::load().unwrap();
        assert!(registry
            .suppresses_push("session", Path::new("project/session.jsonl"), &remote)
            .unwrap());
        std::fs::write(&remote, b"new remote revision\n").unwrap();
        assert!(registry
            .suppresses_push("session", Path::new("project/session.jsonl"), &remote)
            .unwrap());
        assert!(registry
            .protects_remote_path(Path::new("project/session.jsonl"), &remote)
            .unwrap());
    }

    #[test]
    #[serial]
    fn matching_completed_revision_clears_guard() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = ConfigGuard::set(temp.path());
        let remote = temp.path().join("remote.jsonl");
        std::fs::write(&remote, b"remote\n").unwrap();
        let fingerprint = fingerprint_file(&remote).unwrap().digest;
        PullGuardRegistry::reconcile([entry(fingerprint.clone())], []).unwrap();

        PullGuardRegistry::reconcile(
            [],
            [(
                "session".to_string(),
                PathBuf::from("project/session.jsonl"),
                fingerprint,
            )],
        )
        .unwrap();

        assert!(!PullGuardRegistry::load()
            .unwrap()
            .suppresses_push("session", Path::new("project/session.jsonl"), &remote)
            .unwrap());
    }
}
