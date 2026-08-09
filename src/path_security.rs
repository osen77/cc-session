//! Shared path validation helpers for session and sync filesystem boundaries.

use anyhow::{anyhow, Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Validate a project name as one portable, non-special path component.
pub(crate) fn validate_project_component(component: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.starts_with('/')
        || component.starts_with('\\')
        || component.contains('/')
        || component.contains('\\')
        || component.contains(':')
        || is_windows_absolute(component)
    {
        return Err(anyhow!("invalid project path component: {component:?}"));
    }
    Ok(())
}

fn is_windows_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\'))
        || value.starts_with("//")
        || value.starts_with("\\\\")
}

fn validate_relative_path(relative: &Path) -> Result<()> {
    if relative.is_absolute() {
        return Err(anyhow!("relative path is absolute: {}", relative.display()));
    }
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(anyhow!(
                "relative path contains a non-normal component: {}",
                relative.display()
            ));
        };
        if os_str_contains_separator(value) {
            return Err(anyhow!(
                "relative path component contains a platform separator: {}",
                relative.display()
            ));
        }
    }
    Ok(())
}

fn os_str_contains_separator(value: &OsStr) -> bool {
    if let Some(value) = value.to_str() {
        return value.contains('/') || value.contains('\\');
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value
            .as_bytes()
            .iter()
            .any(|byte| *byte == b'/' || *byte == b'\\')
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        value
            .encode_wide()
            .any(|unit| unit == b'/' as u16 || unit == b'\\' as u16)
    }
    #[cfg(not(any(unix, windows)))]
    false
}

/// Construct a safe `project/file` relative path.
pub(crate) fn safe_project_relative_path(project: &str, filename: &OsStr) -> Result<PathBuf> {
    validate_project_component(project)?;
    let filename_path = Path::new(filename);
    if filename_path.file_name() != Some(filename)
        || filename_path.components().count() != 1
        || matches!(
            filename_path.components().next(),
            Some(Component::CurDir | Component::ParentDir)
        )
        || os_str_contains_separator(filename)
    {
        return Err(anyhow!(
            "invalid session filename: {}",
            filename_path.display()
        ));
    }
    Ok(PathBuf::from(project).join(filename))
}

/// Validate the sync repository's projects root as a trusted directory.
///
/// The root itself must not be a symlink, and its canonical location must stay
/// below the canonical sync repository root. This is intentionally stricter than
/// the scanner alias helper: an untrusted checkout must never redefine the root.
pub(crate) fn validate_sync_projects_root(
    sync_repo_path: &Path,
    projects_root: &Path,
) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(projects_root).with_context(|| {
        format!(
            "failed to inspect sync projects root: {}",
            projects_root.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "sync projects root must be a non-symlink directory: {}",
            projects_root.display()
        ));
    }

    let canonical_repo = fs::canonicalize(sync_repo_path).with_context(|| {
        format!(
            "failed to resolve sync repository root: {}",
            sync_repo_path.display()
        )
    })?;
    let canonical_projects = fs::canonicalize(projects_root).with_context(|| {
        format!(
            "failed to resolve sync projects root: {}",
            projects_root.display()
        )
    })?;
    if !canonical_projects.starts_with(&canonical_repo) {
        return Err(anyhow!(
            "sync projects root escapes sync repository: {}",
            projects_root.display()
        ));
    }
    Ok(canonical_projects)
}

/// Construct a path under the trusted sync repository projects root.
pub(crate) fn safe_join_within_sync_projects_root(
    sync_repo_path: &Path,
    projects_root: &Path,
    relative: &Path,
) -> Result<PathBuf> {
    validate_sync_projects_root(sync_repo_path, projects_root)?;
    safe_join_within_root(projects_root, relative)
}

/// Construct a path under `root` after lexical and existing-component validation.
pub(crate) fn safe_join_within_root(root: &Path, relative: &Path) -> Result<PathBuf> {
    validate_relative_path(relative)?;
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve path-security root: {}", root.display()))?;
    let joined = root.join(relative);
    validate_existing_components(root, &joined)?;

    let mut existing = joined.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| anyhow!("path has no existing root ancestor: {}", joined.display()))?;
    }
    let canonical_existing = fs::canonicalize(existing)
        .with_context(|| format!("failed to resolve path ancestor: {}", existing.display()))?;
    if !canonical_existing.starts_with(&canonical_root) {
        return Err(anyhow!("path escapes root: {}", joined.display()));
    }
    Ok(joined)
}

/// Validate a trusted directory root without following a root symlink.
pub(crate) fn validate_directory_root(root: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("failed to inspect directory root: {}", root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "directory root must be a non-symlink directory: {}",
            root.display()
        ));
    }
    fs::canonicalize(root)
        .with_context(|| format!("failed to resolve directory root: {}", root.display()))
}

/// Prepare a regular-file destination below `root` without following symlinks.
///
/// Missing parent directories are created only after every existing component
/// has passed the root-containment and no-symlink checks.
pub(crate) fn prepare_regular_file_destination(root: &Path, relative: &Path) -> Result<PathBuf> {
    validate_directory_root(root)?;
    let destination = safe_join_within_root(root, relative)?;
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("file destination has no parent"))?;

    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(anyhow!(
                "file destination parent must be a non-symlink directory: {}",
                parent.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create file destination parent: {}",
                    parent.display()
                )
            })?;
        }
        Err(error) => return Err(error.into()),
    }
    validate_directory_candidate(root, parent)?;

    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(anyhow!(
                "file destination must be a regular non-symlink file: {}",
                destination.display()
            ));
        }
        Ok(_) => validate_regular_candidate(root, &destination)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    Ok(destination)
}

/// Validate a candidate directory without following symlinks.
pub(crate) fn validate_directory_candidate(root: &Path, path: &Path) -> Result<()> {
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve candidate root: {}", root.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect directory candidate: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(anyhow!(
            "candidate is not a regular non-symlink directory: {}",
            path.display()
        ));
    }
    validate_existing_components(root, path)?;
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve directory candidate: {}", path.display()))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(anyhow!(
            "directory candidate escapes root: {}",
            path.display()
        ));
    }
    Ok(())
}

/// Validate a candidate file before parsing it. File symlinks are never followed.
pub(crate) fn validate_regular_candidate(root: &Path, path: &Path) -> Result<()> {
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve candidate root: {}", root.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect candidate: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(anyhow!(
            "candidate is not a regular non-symlink file: {}",
            path.display()
        ));
    }
    validate_existing_components(root, path)?;
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve candidate: {}", path.display()))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(anyhow!("candidate escapes root: {}", path.display()));
    }
    Ok(())
}

fn validate_existing_components(root: &Path, path: &Path) -> Result<()> {
    let mut current = path.to_path_buf();
    while let Some(_name) = current.file_name() {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(anyhow!(
                    "symlink path component is not allowed: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(anyhow!(error)),
        }
        if current == root {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    Ok(())
}

/// Return a canonical UTF-8 cache identity. Non-UTF-8 paths are intentionally not cacheable.
pub(crate) fn canonical_utf8_key(path: &Path) -> Option<String> {
    fs::canonicalize(path).ok()?.to_str().map(str::to_owned)
}

/// Return a safe relative path for an existing file below `root`.
pub(crate) fn safe_relative_path_within_root(root: &Path, path: &Path) -> Result<PathBuf> {
    validate_regular_candidate(root, path)?;
    let canonical_root = fs::canonicalize(root)?;
    let canonical_path = fs::canonicalize(path)?;
    let relative = canonical_path
        .strip_prefix(&canonical_root)
        .map_err(|_| anyhow!("path is outside root: {}", path.display()))?;
    validate_relative_path(relative)?;
    Ok(relative.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn project_component_rejects_empty_dot_dotdot_absolute_and_platform_separators() {
        for value in [
            "",
            ".",
            "..",
            "/tmp",
            "\\tmp",
            "project/name",
            "project\\name",
            "C:\\tmp",
            "C:/tmp",
            "C:tmp",
            "project:alternate",
        ] {
            assert!(
                validate_project_component(value).is_err(),
                "accepted {value:?}"
            );
        }
        assert!(validate_project_component("my-project").is_ok());
        assert!(validate_project_component("项目").is_ok());
    }

    #[test]
    fn project_relative_path_is_single_safe_project_component_and_filename() {
        let relative = safe_project_relative_path("my-project", OsStr::new("session.jsonl"))
            .expect("valid project relative path");
        assert_eq!(relative, PathBuf::from("my-project/session.jsonl"));

        for (project, filename) in [
            ("..", "session.jsonl"),
            ("project/name", "session.jsonl"),
            ("project", "../session.jsonl"),
            ("project", "nested/session.jsonl"),
        ] {
            assert!(safe_project_relative_path(project, OsStr::new(filename)).is_err());
        }
    }

    #[test]
    fn safe_join_rejects_lexical_and_symlink_escape() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();

        let inside = safe_join_within_root(&root, Path::new("project/file.jsonl"));
        assert!(inside.is_ok(), "{inside:?}");
        assert!(safe_join_within_root(&root, Path::new("../outside/file.jsonl")).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside, root.join("alias")).unwrap();
            assert!(safe_join_within_root(&root, Path::new("alias/file.jsonl")).is_err());
        }
    }

    #[test]
    fn sync_projects_root_requires_non_symlink_directory_inside_repo() {
        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        let projects = repo.join("projects");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&projects).unwrap();
        fs::create_dir_all(&outside).unwrap();

        assert!(validate_sync_projects_root(&repo, &projects).is_ok());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let root_link = repo.join("projects-link");
            symlink(&outside, &root_link).unwrap();
            assert!(validate_sync_projects_root(&repo, &root_link).is_err());

            let escape = repo.join("projects-escape");
            symlink(&outside, &escape).unwrap();
            assert!(validate_sync_projects_root(&repo, &escape).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_destination_rejects_root_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let outside = temp.path().join("outside");
        let root_link = temp.path().join("root-link");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, &root_link).unwrap();

        assert!(prepare_regular_file_destination(&root_link, Path::new("file.jsonl")).is_err());
        assert!(!outside.join("file.jsonl").exists());
    }

    #[test]
    fn candidate_validation_rejects_file_symlink_and_accepts_regular_file() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside.jsonl");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("session.jsonl"), b"{}").unwrap();
        fs::write(&outside, b"{}").unwrap();

        let regular = validate_regular_candidate(&root, &root.join("session.jsonl"));
        assert!(regular.is_ok(), "{regular:?}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = root.join("alias.jsonl");
            symlink(&outside, &link).unwrap();
            assert!(validate_regular_candidate(&root, &link).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn strict_sync_projects_path_rejects_root_project_memory_and_file_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        let projects = repo.join("projects");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&projects).unwrap();
        fs::create_dir_all(&outside).unwrap();

        let regular = safe_join_within_sync_projects_root(
            &repo,
            &projects,
            Path::new("project/memory/note.md"),
        )
        .unwrap();
        assert_eq!(regular, projects.join("project/memory/note.md"));

        let root_link = repo.join("projects-root-link");
        symlink(&outside, &root_link).unwrap();
        assert!(safe_join_within_sync_projects_root(
            &repo,
            &root_link,
            Path::new("project/memory/note.md"),
        )
        .is_err());

        let project_link = projects.join("project");
        symlink(&outside, &project_link).unwrap();
        assert!(safe_join_within_sync_projects_root(
            &repo,
            &projects,
            Path::new("project/memory/note.md"),
        )
        .is_err());
        fs::remove_file(&project_link).unwrap();

        let project = projects.join("project");
        fs::create_dir_all(&project).unwrap();
        let memory_link = project.join("memory");
        symlink(&outside, &memory_link).unwrap();
        assert!(safe_join_within_sync_projects_root(
            &repo,
            &projects,
            Path::new("project/memory/note.md"),
        )
        .is_err());
        fs::remove_file(&memory_link).unwrap();

        fs::create_dir_all(&memory_link).unwrap();
        let outside_file = outside.join("note.md");
        fs::write(&outside_file, b"outside").unwrap();
        let file_link = memory_link.join("note.md");
        symlink(&outside_file, &file_link).unwrap();
        assert!(safe_join_within_sync_projects_root(
            &repo,
            &projects,
            Path::new("project/memory/note.md"),
        )
        .is_err());
    }
}
