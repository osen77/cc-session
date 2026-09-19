//! Exact, fail-closed authorization for Claude's project-level storage links.
//!
//! Resolutions are observations, not filesystem locks. Callers must resolve again
//! immediately before writes and keep using path_security within physical roots.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::path_security::{
    validate_directory_candidate, validate_directory_root, validate_project_component,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRootMapping {
    pub project_dir: String,
    pub target: PathBuf,
    pub trusted_root: PathBuf,
    pub volume_uuid: String,
}

/// Local-only authorization for the entire Claude projects root.
///
/// The logical `~/.claude/projects` directory remains the stable identity while
/// all physical I/O is redirected to `target` after the volume and link are
/// revalidated. This is intentionally separate from the legacy per-project
/// `project_roots` array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalProjectsRoot {
    pub target: PathBuf,
    pub trusted_root: PathBuf,
    pub volume_uuid: String,
}

pub(crate) const EXTERNAL_ROOT_PROJECT_DIR: &str = "__ccs_external_projects_root__";

impl ExternalProjectsRoot {
    pub(crate) fn as_mapping(&self) -> ProjectRootMapping {
        ProjectRootMapping {
            project_dir: EXTERNAL_ROOT_PROJECT_DIR.to_string(),
            target: self.target.clone(),
            trusted_root: self.trusted_root.clone(),
            volume_uuid: self.volume_uuid.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProjectRoot {
    pub project_dir: String,
    pub logical_root: PathBuf,
    pub physical_root: PathBuf,
    pub is_mapped: bool,
}

/// A probe must establish the mounted volume's identity, not infer it from a path.
#[derive(Debug, Clone)]
pub struct VolumeIdentity {
    pub mount_point: PathBuf,
    pub volume_uuid: String,
    pub device_id: u64,
}

pub trait VolumeProbe {
    fn probe(&self, path: &Path) -> Result<VolumeIdentity>;
}

impl<F: Fn(&Path) -> Result<VolumeIdentity>> VolumeProbe for F {
    fn probe(&self, path: &Path) -> Result<VolumeIdentity> {
        self(path)
    }
}

#[cfg(test)]
thread_local! {
    static TEST_VOLUME: std::cell::RefCell<Option<VolumeIdentity>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_test_volume<T>(volume: VolumeIdentity, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_VOLUME.with(|slot| *slot.borrow_mut() = None);
        }
    }
    TEST_VOLUME.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(volume);
    });
    let _reset = Reset;
    run()
}

fn volume_probe(path: &Path) -> Result<VolumeIdentity> {
    #[cfg(test)]
    if let Some(volume) = TEST_VOLUME.with(|slot| slot.borrow().clone()) {
        return Ok(volume);
    }
    system_volume_probe(path)
}

pub fn enumerate(
    projects_root: &Path,
    mappings: &[ProjectRootMapping],
) -> Result<Vec<ResolvedProjectRoot>> {
    enumerate_with_probe(projects_root, mappings, &volume_probe)
}

pub fn resolve(
    projects_root: &Path,
    mappings: &[ProjectRootMapping],
    logical_project: &Path,
) -> Result<ResolvedProjectRoot> {
    resolve_with_probe(projects_root, mappings, logical_project, &volume_probe)
}

/// One operation's validated configuration and project inventory. Resolutions
/// still probe the selected mapping on every call; this is not a volume cache.
pub(crate) struct ProjectRootIndex<'a> {
    projects_root: &'a Path,
    mappings: std::collections::HashMap<&'a str, &'a ProjectRootMapping>,
    roots: Vec<ResolvedProjectRoot>,
}

impl<'a> ProjectRootIndex<'a> {
    pub(crate) fn new(projects_root: &'a Path, mappings: &'a [ProjectRootMapping]) -> Result<Self> {
        Self::with_probe(projects_root, mappings, &volume_probe)
    }

    fn with_probe(projects_root: &'a Path, mappings: &'a [ProjectRootMapping], probe: &dyn VolumeProbe) -> Result<Self> {
        let roots = enumerate_with_probe(projects_root, mappings, probe)?;
        Ok(Self { projects_root, mappings: mappings.iter().map(|m| (m.project_dir.as_str(), m)).collect(), roots })
    }

    pub(crate) fn roots(&self) -> &[ResolvedProjectRoot] { &self.roots }

    pub(crate) fn resolve(&self, logical_project: &Path) -> Result<ResolvedProjectRoot> {
        self.resolve_with_probe(logical_project, &volume_probe)
    }

    fn resolve_with_probe(&self, logical_project: &Path, probe: &dyn VolumeProbe) -> Result<ResolvedProjectRoot> {
        let name = logical_project.strip_prefix(self.projects_root)?.to_str().context("project is not UTF-8")?;
        validate_project_component(name)?;
        let position = self.roots.binary_search_by(|root| root.project_dir.as_str().cmp(name))
            .map_err(|_| anyhow!("project was not present at operation preflight"))?;
        let expected = &self.roots[position];
        if let Some(mapping) = self.mappings.get(name) {
            validate_directory_root(self.projects_root)?;
            strict_absolute_directory(self.projects_root)?;
            let observed = validate_mapping(self.projects_root, mapping, probe)?;
            anyhow::ensure!(observed == *expected, "project mapping changed during operation");
            Ok(observed)
        } else if let Some(mapping) = self.mappings.get(EXTERNAL_ROOT_PROJECT_DIR) {
            let observed = validate_external_project(self.projects_root, mapping, name, probe)?;
            anyhow::ensure!(observed == *expected, "external project root changed during operation");
            Ok(observed)
        } else {
            validate_directory_root(self.projects_root)?;
            validate_directory_candidate(self.projects_root, logical_project)?;
            Ok(expected.clone())
        }
    }

    pub(crate) fn file_boundary(&self, relative: &Path) -> Result<(PathBuf, PathBuf)> {
        let mut components = relative.components();
        let Some(Component::Normal(project)) = components.next() else { bail!("invalid logical project path"); };
        let root = self.resolve(&self.projects_root.join(project))?;
        let tail = components.as_path().to_path_buf();
        crate::path_security::safe_join_within_root(&root.physical_root, &tail)?;
        Ok((root.physical_root, tail))
    }

    pub(crate) fn session_project_root(&self, session: &Path) -> Result<ResolvedProjectRoot> {
        let relative = session.strip_prefix(self.projects_root)?;
        let mut components = relative.components();
        let Some(Component::Normal(project)) = components.next() else { bail!("invalid session project component"); };
        let root = self.resolve(&self.projects_root.join(project))?;
        let physical = crate::path_security::safe_join_within_root(&root.physical_root, components.as_path())?;
        crate::path_security::validate_regular_candidate(&root.physical_root, &physical)?;
        Ok(root)
    }
}
/// Resolve a logical file destination without creating anything. New ordinary
/// projects keep the original root boundary; registered missing projects fail.
pub(crate) fn file_boundary(
    projects_root: &Path,
    mappings: &[ProjectRootMapping],
    relative: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let roots = enumerate(projects_root, mappings)?;
    let mut components = relative.components();
    let Some(Component::Normal(project)) = components.next() else {
        bail!("invalid logical project path");
    };
    let tail = components.as_path();
    let root = roots
        .iter()
        .find(|root| std::ffi::OsStr::new(&root.project_dir) == project);
    let (boundary, relative) = match root {
        Some(root) => (root.physical_root.clone(), tail.to_path_buf()),
        None => {
            if let Some(external) = mappings
                .iter()
                .find(|mapping| mapping.project_dir == EXTERNAL_ROOT_PROJECT_DIR)
            {
                (external.target.clone(), relative.to_path_buf())
            } else {
                (projects_root.to_path_buf(), relative.to_path_buf())
            }
        }
    };
    crate::path_security::safe_join_within_root(&boundary, &relative)?;
    Ok((boundary, relative))
}


pub fn resolve_with_probe(
    projects_root: &Path,
    mappings: &[ProjectRootMapping],
    logical_project: &Path,
    probe: &dyn VolumeProbe,
) -> Result<ResolvedProjectRoot> {
    let relative = logical_project
        .strip_prefix(projects_root)
        .context("logical project must be below the Claude projects root")?;
    let name = relative
        .to_str()
        .ok_or_else(|| anyhow!("project name is not UTF-8"))?;
    validate_project_component(name)?;
    // Enumeration also rejects unknown links before any consumer can mutate state.
    enumerate_with_probe(projects_root, mappings, probe)?
        .into_iter()
        .find(|root| root.project_dir == name)
        .ok_or_else(|| {
            anyhow!(
                "Claude project does not exist: {}",
                logical_project.display()
            )
        })
}

pub fn enumerate_with_probe(
    projects_root: &Path,
    mappings: &[ProjectRootMapping],
    probe: &dyn VolumeProbe,
) -> Result<Vec<ResolvedProjectRoot>> {
    crate::sync::push_diagnostics::enumerate();
    let external = mappings
        .iter()
        .find(|mapping| mapping.project_dir == EXTERNAL_ROOT_PROJECT_DIR);
    if let Some(external) = external {
        if mappings.len() != 1 {
            bail!("external projects root cannot be mixed with project mappings");
        }
        return enumerate_external_root(projects_root, external, probe);
    }

    validate_directory_root(projects_root)?;
    if !mappings.is_empty() {
        strict_absolute_directory(projects_root)?;
    }
    let mut names = HashSet::with_capacity(mappings.len());
    let mut targets: Vec<PathBuf> = Vec::with_capacity(mappings.len());
    let mut roots = Vec::new();
    // Validate every mapping, including entries absent from read_dir, first.
    for mapping in mappings {
        validate_project_component(&mapping.project_dir)?;
        if !names.insert(mapping.project_dir.as_str()) {
            bail!("duplicate project root mapping: {}", mapping.project_dir);
        }
        let root = validate_mapping(projects_root, mapping, probe)?;
        let canonical = fs::canonicalize(&root.physical_root)?;
        if targets.iter().any(|target| canonical.starts_with(target) || target.starts_with(&canonical)) {
            bail!("overlapping canonical project root mappings: {}", mapping.project_dir);
        }
        targets.push(canonical);
        roots.push(root);
    }
    for entry in fs::read_dir(projects_root).context("failed to enumerate Claude projects")? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        if names.contains(name.to_str().unwrap_or_default()) {
            continue;
        }
        if file_type.is_symlink() {
            bail!(
                "unregistered Claude project symlink: {}",
                entry.path().display()
            );
        }
        if !file_type.is_dir() {
            continue;
        }
        let project_dir = name
            .into_string()
            .map_err(|_| anyhow!("project name is not UTF-8"))?;
        validate_project_component(&project_dir)?;
        validate_directory_candidate(projects_root, &entry.path())?;
        roots.push(ResolvedProjectRoot {
            project_dir,
            logical_root: entry.path(),
            physical_root: entry.path(),
            is_mapped: false,
        });
    }
    roots.sort_by(|a, b| a.project_dir.cmp(&b.project_dir));
    Ok(roots)
}

fn strict_absolute_directory(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        bail!(
            "mapped directory must be an absolute, normalized path: {}",
            path.display()
        );
    }
    // Check from the filesystem root: no ancestor may redirect an authorization.
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        validate_directory_root(&current)?;
    }
    Ok(())
}

fn enumerate_external_root(
    projects_root: &Path,
    mapping: &ProjectRootMapping,
    probe: &dyn VolumeProbe,
) -> Result<Vec<ResolvedProjectRoot>> {
    validate_external_root_link(projects_root, mapping, probe)?;
    let mut roots = Vec::new();
    for entry in fs::read_dir(&mapping.target).context("failed to enumerate external Claude projects")? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!("external Claude project symlink is not allowed: {}", entry.path().display());
        }
        if !file_type.is_dir() {
            continue;
        }
        let project_dir = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("external project name is not UTF-8"))?;
        validate_project_component(&project_dir)?;
        validate_directory_candidate(&mapping.target, &entry.path())?;
        roots.push(ResolvedProjectRoot {
            project_dir: project_dir.clone(),
            logical_root: projects_root.join(&project_dir),
            physical_root: entry.path(),
            is_mapped: true,
        });
    }
    roots.sort_by(|a, b| a.project_dir.cmp(&b.project_dir));
    Ok(roots)
}

fn validate_external_project(
    projects_root: &Path,
    mapping: &ProjectRootMapping,
    project_dir: &str,
    probe: &dyn VolumeProbe,
) -> Result<ResolvedProjectRoot> {
    validate_external_root_link(projects_root, mapping, probe)?;
    let logical_root = projects_root.join(project_dir);
    let physical_root = mapping.target.join(project_dir);
    validate_directory_candidate(&mapping.target, &physical_root)?;
    Ok(ResolvedProjectRoot {
        project_dir: project_dir.to_owned(),
        logical_root,
        physical_root,
        is_mapped: true,
    })
}

fn validate_external_root_link(
    projects_root: &Path,
    mapping: &ProjectRootMapping,
    probe: &dyn VolumeProbe,
) -> Result<()> {
    let metadata = fs::symlink_metadata(projects_root)
        .with_context(|| format!("failed to inspect Claude projects root: {}", projects_root.display()))?;
    if !metadata.file_type().is_symlink() {
        bail!("external projects root must be a single-hop symlink: {}", projects_root.display());
    }
    strict_absolute_directory(&mapping.trusted_root)?;
    strict_absolute_directory(&mapping.target)?;
    if mapping.target == mapping.trusted_root || !mapping.target.starts_with(&mapping.trusted_root) {
        bail!("external projects target must remain below trusted root");
    }
    let link_target = fs::read_link(projects_root)?;
    if !link_target.is_absolute() || link_target != mapping.target {
        bail!("external projects root symlink target changed or is not absolute");
    }
    validate_directory_candidate(&mapping.trusted_root, &mapping.target)?;
    let expected_uuid = uuid::Uuid::parse_str(&mapping.volume_uuid).context("invalid external volume UUID")?;
    if expected_uuid.is_nil() {
        bail!("nil external volume UUID is not allowed");
    }
    crate::sync::push_diagnostics::probe();
    let volume = probe.probe(&mapping.trusted_root)?;
    strict_absolute_directory(&volume.mount_point)?;
    if volume.mount_point.parent().is_none()
        || !mapping.trusted_root.starts_with(&volume.mount_point)
        || uuid::Uuid::parse_str(&volume.volume_uuid).context("invalid probed volume UUID")? != expected_uuid
    {
        bail!("external volume mount point or UUID does not match");
    }
    let relative = mapping.target.strip_prefix(&volume.mount_point)?;
    let mut current = volume.mount_point.clone();
    ensure_device(&current, volume.device_id)?;
    for component in relative.components() {
        current.push(component.as_os_str());
        ensure_device(&current, volume.device_id)?;
    }
    if fs::read_link(projects_root)? != mapping.target {
        bail!("external projects root symlink changed during volume validation");
    }
    Ok(())
}

fn validate_mapping(
    projects_root: &Path,
    mapping: &ProjectRootMapping,
    probe: &dyn VolumeProbe,
) -> Result<ResolvedProjectRoot> {
    let expected_uuid =
        uuid::Uuid::parse_str(&mapping.volume_uuid).context("invalid mapped volume UUID")?;
    if expected_uuid.is_nil() {
        bail!("nil mapped volume UUID is not allowed");
    }
    strict_absolute_directory(&mapping.trusted_root)?;
    strict_absolute_directory(&mapping.target)?;
    if mapping.target == mapping.trusted_root
        || !mapping.target.starts_with(&mapping.trusted_root)
        || mapping.target.file_name() != Some(std::ffi::OsStr::new(&mapping.project_dir))
    {
        bail!("mapped target must retain its project name below the trusted root");
    }
    validate_directory_candidate(&mapping.trusted_root, &mapping.target)?;
    let logical_root = projects_root.join(&mapping.project_dir);
    if !fs::symlink_metadata(&logical_root)?
        .file_type()
        .is_symlink()
    {
        bail!(
            "registered project must be a single-hop symlink: {}",
            logical_root.display()
        );
    }
    if fs::read_link(&logical_root)? != mapping.target {
        bail!(
            "registered project symlink target changed: {}",
            logical_root.display()
        );
    }
    crate::sync::push_diagnostics::probe();
    let volume = probe.probe(&mapping.trusted_root)?;
    strict_absolute_directory(&volume.mount_point)?;
    if volume.mount_point.parent().is_none()
        || !mapping.trusted_root.starts_with(&volume.mount_point)
        || uuid::Uuid::parse_str(&volume.volume_uuid).context("invalid probed volume UUID")?
            != expected_uuid
    {
        bail!("mapped volume mount point or UUID does not match");
    }
    let relative = mapping.target.strip_prefix(&volume.mount_point)?;
    let mut current = volume.mount_point.clone();
    ensure_device(&current, volume.device_id)?;
    for component in relative.components() {
        current.push(component.as_os_str());
        ensure_device(&current, volume.device_id)?;
    }
    // Recheck the link after probing (which can involve a bounded subprocess).
    if fs::read_link(&logical_root)? != mapping.target {
        bail!("registered project symlink changed during volume validation");
    }
    Ok(ResolvedProjectRoot {
        project_dir: mapping.project_dir.clone(),
        logical_root,
        physical_root: mapping.target.clone(),
        is_mapped: true,
    })
}

fn ensure_device(path: &Path, expected: u64) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || device_id(&metadata)? != expected
    {
        bail!(
            "mapped path is not a real directory on the authorized device: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn device_id(metadata: &fs::Metadata) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(metadata.dev())
}

#[cfg(not(unix))]
fn device_id(_: &fs::Metadata) -> Result<u64> {
    bail!("mapped volume device verification is unsupported")
}

#[cfg(not(target_os = "macos"))]
fn system_volume_probe(_: &Path) -> Result<VolumeIdentity> {
    bail!("project root mappings require macOS mounted-volume verification")
}

#[cfg(target_os = "macos")]
fn system_volume_probe(path: &Path) -> Result<VolumeIdentity> {
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;
    strict_absolute_directory(path)?;
    let expected_device = fs::metadata(path)?.dev();
    let mut mount_point = path.to_path_buf();
    loop {
        let parent = mount_point.parent().ok_or_else(|| anyhow!("no non-root mount boundary for mapped directory"))?;
        if fs::metadata(parent)?.dev() != expected_device { break; }
        mount_point = parent.to_path_buf();
    }
    let mut diskutil = Command::new("/usr/sbin/diskutil");
    diskutil.args(["info", "-plist"]).arg(&mount_point);
    crate::sync::push_diagnostics::diskutil();
    let plist = bounded_output(diskutil, None)?;
    // Apple's parser handles XML escaping and plist types; no ad-hoc XML parsing.
    let mut plutil = Command::new("/usr/bin/plutil");
    plutil.args(["-convert", "json", "-o", "-", "--", "-"]);
    let json = bounded_output(plutil, Some(plist))?;
    let info: serde_json::Value =
        serde_json::from_slice(&json).context("invalid diskutil plist")?;
    let text = |key: &str| -> Result<&str> {
        info.get(key)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("diskutil omitted {key}"))
    };
    validate_diskutil_mount(&info, &mount_point)?;
    strict_absolute_directory(&mount_point)?;
    if !path.starts_with(&mount_point) || mount_point.parent().is_none() {
        bail!("diskutil did not identify an external mounted filesystem");
    }
    let device_node = PathBuf::from(text("DeviceNode")?);
    if device_node.parent() != Some(Path::new("/dev")) {
        bail!("diskutil returned an invalid device node");
    }
    let device = fs::symlink_metadata(&device_node)?;
    use std::os::unix::fs::FileTypeExt;
    if !device.file_type().is_block_device() {
        bail!("volume device is not a block device");
    }
    let mounted = fs::metadata(&mount_point)?;
    if mounted.dev() != expected_device { bail!("mapped device changed during volume verification"); }
    let mut component_path = mount_point.clone();
    for component in path.strip_prefix(&mount_point)?.components() {
        component_path.push(component.as_os_str());
        ensure_device(&component_path, expected_device)?;
    }
    if device.rdev() != mounted.dev() {
        bail!("diskutil device and mounted filesystem differ");
    }
    let parent = fs::metadata(mount_point.parent().unwrap())?;
    if parent.dev() == mounted.dev() {
        bail!("reported mount point is not a filesystem boundary");
    }
    Ok(VolumeIdentity {
        mount_point,
        volume_uuid: text("VolumeUUID")?.to_owned(),
        device_id: mounted.dev(),
    })
}

#[cfg(target_os = "macos")]
fn validate_diskutil_mount(info: &serde_json::Value, mount: &Path) -> Result<()> {
    if info.get("Mounted").is_some_and(|value| value.as_bool() != Some(true)) {
        bail!("diskutil reports volume is not mounted");
    }
    if info.get("MountPoint").and_then(|value| value.as_str()).map(Path::new) != Some(mount) {
        bail!("diskutil mount point differs from observed device boundary");
    }
    if info.get("FilesystemType").and_then(|value| value.as_str()).is_none_or(|kind| !kind.eq_ignore_ascii_case("apfs")) {
        bail!("mapped volume must be APFS");
    }
    for key in ["Writable", "WritableVolume", "WritableMedia"] {
        if info.get(key).is_some_and(|value| value.as_bool() != Some(true)) {
            bail!("mapped volume is not writable");
        }
    }
    let uuid = info.get("VolumeUUID").and_then(|value| value.as_str()).ok_or_else(|| anyhow!("diskutil omitted VolumeUUID"))?;
    if uuid::Uuid::parse_str(uuid)?.is_nil() { bail!("nil volume UUID"); }
    Ok(())
}

/// Both process duration and output are bounded; commands are fixed system tools.
#[cfg(target_os = "macos")]
fn bounded_output(mut command: std::process::Command, input: Option<Vec<u8>>) -> Result<Vec<u8>> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    const LIMIT: usize = 128 * 1024;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .context("failed to start volume verification tool")?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().unwrap();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = (|| -> Result<Vec<u8>> {
        let bytes = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("volume verification timed out")??;
        if bytes.len() > LIMIT {
            bail!("volume verification output exceeds limit");
        }
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    bail!("volume verification tool failed: {status}");
                }
                return Ok(bytes);
            }
            if Instant::now() >= deadline {
                bail!("volume verification timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    #[cfg(target_os = "macos")]
    #[test]
    fn captured_diskutil_without_mounted_requires_matching_apfs_identity() {

        let mut command = std::process::Command::new("/usr/bin/plutil");
        command.args(["-convert", "json", "-o", "-", "--", "-"]);
        let json = bounded_output(command, Some(include_bytes!("../tests/fixtures/data-volume-diskutil.plist").to_vec())).unwrap();
        let info: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert!(info.get("Mounted").is_none());
        validate_diskutil_mount(&info, Path::new("/Volumes/Data")).unwrap();
        for (key, value) in [("Mounted", serde_json::json!(false)), ("FilesystemType", serde_json::json!("exfat")), ("MountPoint", serde_json::json!("/Volumes/Other")), ("WritableVolume", serde_json::json!(false)), ("VolumeUUID", serde_json::json!("bad"))] {
            let mut invalid = info.clone(); invalid[key] = value;
            assert!(validate_diskutil_mount(&invalid, Path::new("/Volumes/Data")).is_err());
        }
    }
    #[test]
    fn session_root_rejects_internal_links_and_traversal_suffixes() {
        let f = Fixture::new();
        let project = f.root.join("-local-project");
        fs::create_dir_all(project.join("session/subagents")).unwrap();
        fs::write(project.join("session/subagents/agent.jsonl"), b"{}\n").unwrap();
        fs::remove_file(f.root.join(&f.mapping.project_dir)).unwrap();
        let roots = ProjectRootIndex::new(&f.root, &[]).unwrap();
        assert_eq!(roots.session_project_root(&project.join("session/subagents/agent.jsonl")).unwrap().logical_root, project);
        assert!(roots.session_project_root(&project.join("session/../session/subagents/agent.jsonl")).is_err());
        symlink(project.join("session/subagents"), project.join("alias")).unwrap();
        assert!(roots.session_project_root(&project.join("alias/agent.jsonl")).is_err());
    }
    use tempfile::TempDir;

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
        mapping: ProjectRootMapping,
        volume: VolumeIdentity,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let base = temp.path().canonicalize().unwrap();
            let root = base.join("local/projects");
            let mount = base.join("volume");
            let trusted_root = mount.join("Claude/projects");
            let target = trusted_root.join("-external-project");
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(&target).unwrap();
            fs::create_dir(root.join("-local-project")).unwrap();
            symlink(&target, root.join("-external-project")).unwrap();
            let mapping = ProjectRootMapping {
                project_dir: "-external-project".into(),
                target,
                trusted_root,
                volume_uuid: "AF9C9871-18AE-40C9-8D18-624E415520C6".into(),
            };
            let volume = VolumeIdentity {
                mount_point: mount.clone(),
                volume_uuid: mapping.volume_uuid.clone(),
                device_id: device_id(&fs::metadata(&mount).unwrap()).unwrap(),
            };
            Self {
                _temp: temp,
                root,
                mapping,
                volume,
            }
        }

        fn probe(&self) -> impl VolumeProbe + '_ {
            move |_: &Path| Ok(self.volume.clone())
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires isolated-validation.py --root-parent /Volumes/Data and explicit --ignored"]
    fn production_probe_accepts_private_fixture_subdirectory() {
        let path = std::env::var_os("CCS_TEST_VOLUME_FIXTURE").expect("isolated volume fixture is required");
        let path = PathBuf::from(path);
        let identity = system_volume_probe(&path).unwrap();
        assert_eq!(identity.mount_point, Path::new("/Volumes/Data"));
        assert_eq!(device_id(&fs::metadata(&path).unwrap()).unwrap(), identity.device_id);
    }

    #[test]
    fn rejects_nested_targets_across_distinct_trusted_roots_in_any_order() {
        let f = Fixture::new();
        let child = ProjectRootMapping {
            project_dir: "-nested".into(),
            target: f.mapping.target.join("-nested"),
            trusted_root: f.mapping.target.clone(),
            volume_uuid: f.mapping.volume_uuid.clone(),
        };
        fs::create_dir(&child.target).unwrap();
        symlink(&child.target, f.root.join(&child.project_dir)).unwrap();
        for mappings in [vec![f.mapping.clone(), child.clone()], vec![child.clone(), f.mapping.clone()]] {
            assert!(enumerate_with_probe(&f.root, &mappings, &f.probe()).is_err());
        }
    }

    #[test]
    fn accepts_disjoint_targets_across_distinct_trusted_roots() {
        let f = Fixture::new();
        let trusted = f.volume.mount_point.join("other");
        let other = ProjectRootMapping { project_dir: "-other".into(), target: trusted.join("-other"), trusted_root: trusted, volume_uuid: f.mapping.volume_uuid.clone() };
        fs::create_dir_all(&other.target).unwrap();
        symlink(&other.target, f.root.join(&other.project_dir)).unwrap();
        let roots = enumerate_with_probe(&f.root, &[f.mapping.clone(), other.clone()], &f.probe()).unwrap();
        assert!(roots.iter().any(|root| root.physical_root == other.target));
        assert!(roots.iter().any(|root| root.physical_root == f.mapping.target));
    }

    #[test]
    fn operation_index_probes_selected_mapping_not_every_mapping() {
        let f = Fixture::new();
        let mut mappings = vec![f.mapping.clone()];
        for number in 1..27 {
            let name = format!("-synthetic-{number}");
            let target = f.mapping.trusted_root.join(&name);
            fs::create_dir(&target).unwrap();
            symlink(&target, f.root.join(&name)).unwrap();
            mappings.push(ProjectRootMapping { project_dir: name, target, ..f.mapping.clone() });
        }
        let calls = std::cell::Cell::new(0);
        let probe = |_: &Path| { calls.set(calls.get() + 1); Ok(f.volume.clone()) };
        let roots = ProjectRootIndex::with_probe(&f.root, &mappings, &probe).unwrap();
        assert_eq!(calls.get(), 27);
        for _ in 0..54 {
            assert_eq!(roots.resolve_with_probe(&f.root.join(&f.mapping.project_dir), &probe).unwrap().physical_root, f.mapping.target);
        }
        assert_eq!(calls.get(), 27 + 54);
    }

    #[test]
    fn operation_index_rejects_mapping_changes_after_preflight() {
        for failure in ["uuid", "device", "mount", "target", "link"] {
            let f = Fixture::new();
            let mappings = [f.mapping.clone()];
            let roots = ProjectRootIndex::with_probe(&f.root, &mappings, &f.probe()).unwrap();
            let mut volume = f.volume.clone();
            match failure {
                "uuid" => volume.volume_uuid = uuid::Uuid::nil().to_string(),
                "device" => volume.device_id = volume.device_id.wrapping_add(1),
                "mount" => volume.mount_point = f.root.clone(),
                "target" => fs::remove_dir(&f.mapping.target).unwrap(),
                "link" => {
                    fs::remove_file(f.root.join(&f.mapping.project_dir)).unwrap();
                    symlink(f.root.join("-local-project"), f.root.join(&f.mapping.project_dir)).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(roots.resolve_with_probe(&f.root.join(&f.mapping.project_dir), &|_: &Path| Ok(volume.clone())).is_err(), "{failure}");
        }
    }

    #[test]
    fn missing_or_wrong_volume_blocks_even_unrelated_local_resolution() {
        let f = Fixture::new();
        let local = f.root.join("-local-project");
        let absent = |_: &Path| -> Result<VolumeIdentity> { anyhow::bail!("volume absent") };
        assert!(resolve_with_probe(&f.root, &[f.mapping.clone()], &local, &absent).is_err());
        let wrong = |_: &Path| {
            Ok(VolumeIdentity {
                volume_uuid: uuid::Uuid::nil().to_string(),
                ..f.volume.clone()
            })
        };
        assert!(resolve_with_probe(&f.root, &[f.mapping.clone()], &local, &wrong).is_err());
        fs::remove_dir(&f.mapping.target).unwrap();
        assert!(resolve_with_probe(&f.root, &[f.mapping.clone()], &local, &f.probe()).is_err());
    }

    #[test]
    fn rejects_unknown_links_root_links_and_link_chains() {
        let f = Fixture::new();
        assert!(enumerate_with_probe(&f.root, &[], &f.probe()).is_err());
        let alias = f.root.with_file_name("alias");
        symlink(&f.root, &alias).unwrap();
        assert!(enumerate_with_probe(&alias, &[f.mapping.clone()], &f.probe()).is_err());
        fs::remove_dir(&f.mapping.target).unwrap();
        symlink(f.root.join("-local-project"), &f.mapping.target).unwrap();
        assert!(enumerate_with_probe(&f.root, &[f.mapping.clone()], &f.probe()).is_err());
    }

    #[test]
    fn rejects_duplicates_escape_changed_link_and_cross_device() {
        let f = Fixture::new();
        assert!(
            enumerate_with_probe(&f.root, &[f.mapping.clone(), f.mapping.clone()], &f.probe())
                .is_err()
        );
        let mut bad = f.mapping.clone();
        bad.project_dir = "../escape".into();
        assert!(enumerate_with_probe(&f.root, &[bad], &f.probe()).is_err());
        let wrong_device = |_: &Path| {
            Ok(VolumeIdentity {
                device_id: f.volume.device_id.wrapping_add(1),
                ..f.volume.clone()
            })
        };
        assert!(enumerate_with_probe(&f.root, &[f.mapping.clone()], &wrong_device).is_err());
        fs::remove_file(f.root.join(&f.mapping.project_dir)).unwrap();
        symlink(
            f.root.join("-local-project"),
            f.root.join(&f.mapping.project_dir),
        )
        .unwrap();
        assert!(enumerate_with_probe(&f.root, &[f.mapping.clone()], &f.probe()).is_err());
    }

    #[test]
    fn ordinary_projects_do_not_require_a_supported_volume_probe() {
        let f = Fixture::new();
        fs::remove_file(f.root.join(&f.mapping.project_dir)).unwrap();
        let unsupported = |_: &Path| -> Result<VolumeIdentity> { bail!("unsupported") };
        let roots = enumerate_with_probe(&f.root, &[], &unsupported).unwrap();
        assert_eq!(roots[0].project_dir, "-local-project");
        assert!(!roots[0].is_mapped);
    }

    #[test]
    fn rejects_target_outside_trust_missing_link_and_relative_link() {
        let f = Fixture::new();
        let mut outside = f.mapping.clone();
        outside.trusted_root = f.root.clone();
        assert!(enumerate_with_probe(&f.root, &[outside], &f.probe()).is_err());
        let mut invalid_uuid = f.mapping.clone();
        invalid_uuid.volume_uuid = "not-a-uuid".into();
        assert!(enumerate_with_probe(&f.root, &[invalid_uuid], &f.probe()).is_err());
        fs::remove_file(f.root.join(&f.mapping.project_dir)).unwrap();
        assert!(enumerate_with_probe(&f.root, &[f.mapping.clone()], &f.probe()).is_err());
        symlink(
            Path::new("../../volume/Claude/projects/-external-project"),
            f.root.join(&f.mapping.project_dir),
        )
        .unwrap();
        assert!(enumerate_with_probe(&f.root, &[f.mapping.clone()], &f.probe()).is_err());
    }

    #[test]
    fn external_root_discovers_existing_and_future_projects_with_logical_identity() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let projects_root = base.join("home/.claude/projects");
        let mount = base.join("volume");
        let trusted_root = mount.join("Claude");
        let target = trusted_root.join("projects");
        fs::create_dir_all(projects_root.parent().unwrap()).unwrap();
        fs::create_dir_all(target.join("first")).unwrap();
        fs::create_dir_all(target.join("second")).unwrap();
        symlink(&target, &projects_root).unwrap();
        let external = ExternalProjectsRoot {
            target: target.clone(),
            trusted_root,
            volume_uuid: "AF9C9871-18AE-40C9-8D18-624E415520C6".into(),
        };
        let volume = VolumeIdentity {
            mount_point: mount,
            volume_uuid: external.volume_uuid.clone(),
            device_id: device_id(&fs::metadata(&base).unwrap()).unwrap(),
        };
        with_test_volume(volume, || {
            let mappings = [external.as_mapping()];
            let roots = enumerate(&projects_root, &mappings).unwrap();
            assert_eq!(
                roots.iter().map(|root| root.project_dir.as_str()).collect::<Vec<_>>(),
                vec!["first", "second"]
            );
            assert!(roots.iter().all(|root| root.is_mapped));
            assert!(roots.iter().all(|root| root.logical_root.starts_with(&projects_root)));
            assert!(roots.iter().all(|root| root.physical_root.starts_with(&target)));

            fs::create_dir_all(target.join("future")).unwrap();
            let index = ProjectRootIndex::new(&projects_root, &mappings).unwrap();
            assert!(index.roots().iter().any(|root| root.project_dir == "future"));
            let (boundary, relative) = index.file_boundary(Path::new("future/session.jsonl")).unwrap();
            assert_eq!(boundary, target.join("future"));
            assert_eq!(relative, PathBuf::from("session.jsonl"));
            let (boundary, relative) = file_boundary(
                &projects_root,
                &mappings,
                Path::new("new-project/session.jsonl"),
            )
            .unwrap();
            assert_eq!(boundary, target);
            assert_eq!(relative, PathBuf::from("new-project/session.jsonl"));
        });
    }

    #[test]
    fn external_root_rejects_wrong_link_missing_target_and_mixed_modes() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let projects_root = base.join("home/.claude/projects");
        let mount = base.join("volume");
        let trusted_root = mount.join("Claude");
        let target = trusted_root.join("projects");
        fs::create_dir_all(projects_root.parent().unwrap()).unwrap();
        fs::create_dir_all(target.join("project")).unwrap();
        symlink(&target, &projects_root).unwrap();
        let external = ExternalProjectsRoot {
            target: target.clone(),
            trusted_root: trusted_root.clone(),
            volume_uuid: "AF9C9871-18AE-40C9-8D18-624E415520C6".into(),
        };
        let volume = VolumeIdentity {
            mount_point: mount.clone(),
            volume_uuid: external.volume_uuid.clone(),
            device_id: device_id(&fs::metadata(&base).unwrap()).unwrap(),
        };
        with_test_volume(volume, || {
            let mapping = external.as_mapping();
            fs::remove_file(&projects_root).unwrap();
            fs::create_dir(&projects_root).unwrap();
            assert!(enumerate(&projects_root, &[mapping.clone()]).is_err());
            fs::remove_dir(&projects_root).unwrap();
            symlink(&target, &projects_root).unwrap();
            fs::remove_dir_all(&target).unwrap();
            assert!(enumerate(&projects_root, &[mapping.clone()]).is_err());
            fs::create_dir_all(target.join("project")).unwrap();
            let legacy = ProjectRootMapping {
                project_dir: "legacy".into(),
                target: target.join("legacy"),
                trusted_root: trusted_root.clone(),
                volume_uuid: external.volume_uuid.clone(),
            };
            assert!(enumerate(&projects_root, &[mapping, legacy]).is_err());
        });
    }

    #[test]
    fn mixed_external_and_legacy_filter_config_is_rejected() {
        let mut config = crate::filter::FilterConfig::default();
        config.external_projects_root = Some(ExternalProjectsRoot {
            target: PathBuf::from("/Volumes/Data/Claude/projects"),
            trusted_root: PathBuf::from("/Volumes/Data/Claude"),
            volume_uuid: "AF9C9871-18AE-40C9-8D18-624E415520C6".into(),
        });
        config.project_roots.push(ProjectRootMapping {
            project_dir: "legacy".into(),
            target: PathBuf::from("/Volumes/Data/Claude/projects/legacy"),
            trusted_root: PathBuf::from("/Volumes/Data/Claude/projects"),
            volume_uuid: "AF9C9871-18AE-40C9-8D18-624E415520C6".into(),
        });
        assert!(config.root_mappings().is_err());
        assert!(config.validate().is_err());
    }

    #[test]
    fn mixed_projects_preserve_identity_and_internal_links_remain_forbidden() {
        let f = Fixture::new();
        let roots = enumerate_with_probe(&f.root, &[f.mapping.clone()], &f.probe()).unwrap();
        assert_eq!(
            roots
                .iter()
                .map(|r| r.project_dir.as_str())
                .collect::<Vec<_>>(),
            vec!["-external-project", "-local-project"]
        );
        let external = resolve_with_probe(
            &f.root,
            &[f.mapping.clone()],
            &f.root.join(&f.mapping.project_dir),
            &f.probe(),
        )
        .unwrap();
        assert_eq!(external.physical_root, f.mapping.target);
        assert_eq!(external.logical_root, f.root.join(&f.mapping.project_dir));
        symlink(
            f.root.join("-local-project"),
            external.physical_root.join("memory"),
        )
        .unwrap();
        assert!(crate::path_security::safe_join_within_root(
            &external.physical_root,
            Path::new("memory/file.md")
        )
        .is_err());
        assert!(resolve_with_probe(
            &f.root,
            &[f.mapping.clone()],
            &f.root.join("../outside"),
            &f.probe()
        )
        .is_err());
    }
}
