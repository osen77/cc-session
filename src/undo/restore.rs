use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use super::snapshot::Snapshot;

impl Snapshot {
    /// Restore files from this snapshot
    ///
    /// This writes all files from the snapshot back to their original locations,
    /// overwriting any current content.
    ///
    /// For differential snapshots, this recursively loads the base snapshot chain
    /// and applies all changes in order to reconstruct the full state.
    ///
    /// # Security
    /// This method validates all paths to prevent path traversal attacks.
    /// By default, only paths within the home directory are allowed.
    /// For testing, you can pass a custom allowed_base_dir.
    ///
    /// # Arguments
    /// * `allowed_base_dir` - Optional base directory for path validation.
    ///   If None, defaults to home directory for security.
    /// * `snapshots_dir` - Optional snapshots directory (for testing with differential snapshots)
    pub fn restore_with_base_and_snapshots(
        &self,
        allowed_base_dir: Option<&Path>,
        snapshots_dir: Option<&Path>,
    ) -> Result<()> {
        // Determine the allowed base directory
        let allowed_base = if let Some(base) = allowed_base_dir {
            // For testing: use the provided base
            base.canonicalize().with_context(|| {
                format!("Failed to canonicalize base directory: {}", base.display())
            })?
        } else {
            // For production: use home directory
            let home_dir = dirs::home_dir().context("Failed to get home directory")?;
            home_dir
                .canonicalize()
                .context("Failed to canonicalize home directory")?
        };
        let logical_base = match allowed_base_dir {
            Some(base) => base.to_path_buf(),
            None => dirs::home_dir().context("Failed to get home directory")?,
        };

        // Build the complete file state by walking the snapshot chain
        let all_files = self.reconstruct_full_state_with_dir(snapshots_dir)?;

        // Validate the complete batch before creating, deleting, or writing anything.
        // Unknown/mapped symlinks stay forbidden for undo in this release.
        let relative = |path: &Path| -> Result<PathBuf> {
            let rel = path
                .strip_prefix(&logical_base)
                .or_else(|_| path.strip_prefix(&allowed_base))
                .map_err(|_| anyhow!("restore path is outside allowed root: {}", path.display()))?;
            crate::path_security::safe_join_within_root(&allowed_base, rel)?;
            Ok(rel.to_path_buf())
        };
        for path in all_files.keys().chain(self.deleted_files.iter()) {
            let path = Path::new(path);
            relative(path)?;
            match fs::symlink_metadata(path) {
                Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
                    return Err(anyhow!(
                        "restore target is not a regular file: {}",
                        path.display()
                    ));
                }
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into())
                }
                _ => {}
            }
        }
        for deleted_path in &self.deleted_files {
            let rel = relative(Path::new(deleted_path))?;
            let path = crate::path_security::safe_join_within_root(&allowed_base, &rel)?;
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for (path, content) in &all_files {
            let rel = relative(Path::new(path))?;
            let destination =
                crate::path_security::prepare_regular_file_destination(&allowed_base, &rel)?;
            fs::write(&destination, content)
                .with_context(|| format!("Failed to restore file: {}", destination.display()))?;
        }

        Ok(())
    }

    /// Restore files from this snapshot using default snapshots directory
    ///
    /// This is a wrapper around `restore_with_base_and_snapshots` for backwards compatibility.
    pub fn restore_with_base(&self, allowed_base_dir: Option<&Path>) -> Result<()> {
        self.restore_with_base_and_snapshots(allowed_base_dir, None)
    }

    /// Restore files from this snapshot
    ///
    /// This is a convenience wrapper that uses the home directory as the allowed base.
    #[allow(dead_code)]
    pub fn restore(&self) -> Result<()> {
        self.restore_with_base(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_restore_destination_does_not_create_paths_or_delete_valid_files() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = root.path().join("keep.txt");
        fs::write(&victim, b"keep").unwrap();
        let forbidden = outside.path().join("must-not-exist/file");
        let snapshot = Snapshot {
            snapshot_id: "isolated".into(),
            timestamp: chrono::Utc::now(),
            operation_type: crate::history::OperationType::Pull,
            git_commit_hash: None,
            files: [(forbidden.to_string_lossy().into_owned(), b"bad".to_vec())].into(),
            branch: None,
            base_snapshot_id: None,
            deleted_files: vec![victim.to_string_lossy().into_owned()],
        };
        assert!(snapshot.restore_with_base(Some(root.path())).is_err());
        assert!(!forbidden.parent().unwrap().exists());
        assert_eq!(fs::read(victim).unwrap(), b"keep");
    }
}
