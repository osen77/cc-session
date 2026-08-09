//! Git SCM backend using CLI commands.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::{PushError, RebaseOutcome, Scm};
use crate::BINARY_NAME;

fn classify_push_stderr(stderr: &str) -> Option<PushError> {
    let stderr = stderr.to_ascii_lowercase();
    if stderr.contains("non-fast-forward")
        || stderr.contains("fetch first")
        || stderr.contains("tip of your current branch is behind")
        || stderr.contains("failed to push some refs") && stderr.contains("[rejected]")
    {
        Some(PushError::NonFastForward)
    } else {
        None
    }
}

fn is_git_repo_path(path: &Path) -> bool {
    let git_path = path.join(".git");
    git_path.is_dir() || git_path.is_file()
}

fn git_rebase_state_exists(git_dir: &Path) -> bool {
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

fn output_text(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    match (stdout.trim(), stderr.trim()) {
        ("", "") => String::new(),
        ("", stderr) => stderr.to_string(),
        (stdout, "") => stdout.to_string(),
        (stdout, stderr) => format!("{stdout}\n{stderr}"),
    }
}

fn classify_rebase_failure_text(text: &str) -> Option<RebaseOutcome> {
    let text = text.to_ascii_lowercase();
    if text.contains("resolve all conflicts manually")
        || text.contains("fix conflicts and then run")
        || text.contains("you must edit all merge conflicts")
        || text.contains("could not apply")
    {
        Some(RebaseOutcome::InProgress)
    } else {
        None
    }
}

fn build_push_failure(remote: &str, stderr: &str) -> anyhow::Error {
    anyhow!(
        "Failed to push to remote '{}': {}\n\n\
        Possible causes:\n\
        1. Authentication failed - ensure credentials are configured\n\
        2. No permission to push to this repository\n\
        3. Network connectivity issues\n\
        4. Remote branch protection rules\n\n\
        For HTTPS: Run 'git config --global credential.helper store' and try again\n\
        For SSH: Ensure SSH keys are set up with 'ssh -T git@github.com'",
        remote,
        stderr
    )
}

fn rebase_in_progress_from_failure(git_dir: &Path, output: &Output) -> bool {
    git_rebase_state_exists(git_dir) || classify_rebase_failure_text(&output_text(output)).is_some()
}

/// Git SCM implementation using the git CLI.
pub struct GitScm {
    workdir: PathBuf,
}

impl GitScm {
    /// Open an existing Git repository.
    pub fn open(path: &Path) -> Result<Self> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        if !is_git_repo_path(&path) {
            return Err(anyhow!(
                "Not a git repository: '{}' (no .git directory or gitdir file)",
                path.display()
            ));
        }

        Ok(Self { workdir: path })
    }

    /// Initialize a new Git repository.
    pub fn init(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("Failed to create directory '{}'", path.display()))?;

        let output = Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .context("Failed to run 'git init'")?;

        if !output.status.success() {
            return Err(anyhow!(
                "git init failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        // Configure user name and email if not set
        let _ = Command::new("git")
            .args(["config", "user.name", "Claude Code Sync"])
            .current_dir(path)
            .output();
        let email = format!("{}@local", BINARY_NAME);
        let _ = Command::new("git")
            .args(["config", "user.email", &email])
            .current_dir(path)
            .output();

        Self::open(path)
    }

    /// Clone a remote repository.
    pub fn clone(url: &str, path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directory for '{}'", path.display())
            })?;
        }

        let output = Command::new("git")
            .args(["clone", url, &path.to_string_lossy()])
            .output()
            .context("Failed to run 'git clone'")?;

        if !output.status.success() {
            return Err(anyhow!(
                "git clone failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Self::open(path)
    }

    /// Run a git command and return stdout as a string.
    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.workdir)
            .output()
            .with_context(|| format!("Failed to run 'git {}'", args.join(" ")))?;

        if !output.status.success() {
            return Err(anyhow!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Run a git command, returning Ok if it succeeds (ignoring stdout).
    fn run_git_ok(&self, args: &[&str]) -> Result<()> {
        self.run_git(args)?;
        Ok(())
    }

    /// Check if a git command succeeds (exit code 0).
    fn git_succeeds(&self, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(&self.workdir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn run_git_output(&self, args: &[&str]) -> Result<Output> {
        Command::new("git")
            .args(args)
            .current_dir(&self.workdir)
            .output()
            .with_context(|| format!("Failed to run 'git {}'", args.join(" ")))
    }

    fn git_dir(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(
            self.run_git(&["rev-parse", "--absolute-git-dir"])?,
        ))
    }
}

impl Scm for GitScm {
    fn current_branch(&self) -> Result<String> {
        self.run_git(&["branch", "--show-current"])
    }

    fn current_commit_hash(&self) -> Result<String> {
        self.run_git(&["rev-parse", "HEAD"])
    }

    fn stage_all(&self) -> Result<()> {
        self.run_git_ok(&["add", "-A"])
    }

    fn commit(&self, message: &str) -> Result<()> {
        self.run_git_ok(&["commit", "-m", message])
    }

    fn has_changes(&self) -> Result<bool> {
        let output = self.run_git(&["status", "--porcelain"])?;
        Ok(!output.is_empty())
    }

    fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        self.run_git_ok(&["remote", "add", name, url])
    }

    fn has_remote(&self, name: &str) -> bool {
        self.git_succeeds(&["remote", "get-url", name])
    }

    fn get_remote_url(&self, name: &str) -> Result<String> {
        self.run_git(&["remote", "get-url", name])
    }

    fn set_remote_url(&self, name: &str, url: &str) -> Result<()> {
        self.run_git_ok(&["remote", "set-url", name, url])
    }

    fn remove_remote(&self, name: &str) -> Result<()> {
        self.run_git_ok(&["remote", "remove", name])
    }

    fn list_remotes(&self) -> Result<Vec<String>> {
        let output = self.run_git(&["remote"])?;
        if output.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(output.lines().map(|s| s.to_string()).collect())
        }
    }

    fn push(&self, remote: &str, branch: &str) -> Result<()> {
        self.push_classified(remote, branch)
            .map_err(|err| match err {
                PushError::NonFastForward => anyhow!(
                    "Failed to push to remote '{}': remote contains commits not present locally",
                    remote
                ),
                PushError::Other(err) => err,
            })
    }

    fn push_classified(&self, remote: &str, branch: &str) -> std::result::Result<(), PushError> {
        let output = self
            .run_git_output(&["push", remote, branch])
            .map_err(PushError::Other)?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if let Some(error) = classify_push_stderr(&stderr) {
            return Err(error);
        }

        Err(PushError::Other(build_push_failure(remote, &stderr)))
    }

    fn fetch(&self, remote: &str) -> Result<()> {
        self.run_git_ok(&["fetch", remote])
    }

    fn rebase(&self, upstream: &str) -> Result<RebaseOutcome> {
        let output = self.run_git_output(&["rebase", upstream])?;
        if output.status.success() {
            return Ok(RebaseOutcome::Completed);
        }

        let git_dir = self.git_dir()?;
        if rebase_in_progress_from_failure(&git_dir, &output) {
            return Ok(RebaseOutcome::InProgress);
        }

        Err(anyhow!(
            "git rebase {} failed: {}",
            upstream,
            output_text(&output)
        ))
    }

    fn rebase_continue(&self) -> Result<RebaseOutcome> {
        let output = self.run_git_output(&["rebase", "--continue"])?;
        if output.status.success() {
            return Ok(RebaseOutcome::Completed);
        }

        let git_dir = self.git_dir()?;
        if rebase_in_progress_from_failure(&git_dir, &output) {
            return Ok(RebaseOutcome::InProgress);
        }

        Err(anyhow!(
            "git rebase --continue failed: {}",
            output_text(&output)
        ))
    }

    fn rebase_abort(&self) -> Result<()> {
        self.run_git_ok(&["rebase", "--abort"])
    }

    fn is_rebase_in_progress(&self) -> Result<bool> {
        Ok(git_rebase_state_exists(&self.git_dir()?))
    }

    fn pull(&self, remote: &str, branch: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["pull", remote, branch])
            .current_dir(&self.workdir)
            .output()
            .context("Failed to run 'git pull'")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "Failed to pull from remote '{}': {}",
                remote,
                stderr
            ));
        }

        Ok(())
    }

    fn reset_soft(&self, commit: &str) -> Result<()> {
        self.run_git_ok(&["reset", "--soft", commit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_git_init_and_open() {
        let temp = TempDir::new().unwrap();
        let _scm = GitScm::init(temp.path()).unwrap();

        assert!(temp.path().join(".git").exists());

        // Verify we can open the initialized repo
        let _reopened = GitScm::open(temp.path()).unwrap();
    }

    #[test]
    fn test_git_stage_commit() {
        let temp = TempDir::new().unwrap();
        let scm = GitScm::init(temp.path()).unwrap();

        // Initially no changes
        assert!(!scm.has_changes().unwrap());

        // Create a file
        std::fs::write(temp.path().join("test.txt"), "hello").unwrap();
        assert!(scm.has_changes().unwrap());

        // Stage and commit
        scm.stage_all().unwrap();
        scm.commit("Initial commit").unwrap();
        assert!(!scm.has_changes().unwrap());

        // Verify commit hash
        let hash = scm.current_commit_hash().unwrap();
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 40); // Full SHA
    }

    #[test]
    fn test_git_branch() {
        let temp = TempDir::new().unwrap();
        let scm = GitScm::init(temp.path()).unwrap();

        // Create initial commit (needed for branch to exist)
        std::fs::write(temp.path().join("test.txt"), "hello").unwrap();
        scm.stage_all().unwrap();
        scm.commit("Initial commit").unwrap();

        // Check branch (default is master or main depending on git config)
        let branch = scm.current_branch().unwrap();
        assert!(!branch.is_empty());
    }

    #[test]
    fn test_git_remote() {
        let temp = TempDir::new().unwrap();
        let scm = GitScm::init(temp.path()).unwrap();

        assert!(!scm.has_remote("origin"));

        scm.add_remote("origin", "https://github.com/test/repo.git")
            .unwrap();
        assert!(scm.has_remote("origin"));
        assert!(!scm.has_remote("upstream"));
    }

    #[test]
    fn test_classify_non_fast_forward_push_error() {
        let stderr = "To /tmp/remote.git\n ! [rejected]        main -> main (non-fast-forward)\nerror: failed to push some refs to '/tmp/remote.git'\nhint: Updates were rejected because the tip of your current branch is behind\n";
        assert!(matches!(
            classify_push_stderr(stderr),
            Some(super::super::PushError::NonFastForward)
        ));
    }

    #[test]
    fn test_detect_rebase_state_paths() {
        let temp = TempDir::new().unwrap();
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();

        assert!(!git_rebase_state_exists(&git_dir));

        std::fs::create_dir(git_dir.join("rebase-merge")).unwrap();
        assert!(git_rebase_state_exists(&git_dir));

        std::fs::remove_dir_all(git_dir.join("rebase-merge")).unwrap();
        assert!(!git_rebase_state_exists(&git_dir));

        std::fs::create_dir(git_dir.join("rebase-apply")).unwrap();
        assert!(git_rebase_state_exists(&git_dir));
    }

    #[test]
    fn test_open_accepts_gitdir_file_repository() {
        let temp = TempDir::new().unwrap();
        let actual_git_dir = temp.path().join("actual-git-dir");
        std::fs::create_dir_all(actual_git_dir.join("refs")).unwrap();
        std::fs::write(actual_git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(temp.path().join(".git"), "gitdir: actual-git-dir\n").unwrap();

        let reopened = GitScm::open(temp.path()).unwrap();
        assert!(reopened.workdir.join(".git").is_file());
    }

    #[test]
    fn test_git_dir_uses_absolute_git_dir_for_gitdir_file_repo() {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path().join("repo");
        let scm = GitScm::init(&repo_path).unwrap();
        let actual_git_dir = temp.path().join("external-git-dir");
        std::fs::rename(repo_path.join(".git"), &actual_git_dir).unwrap();
        std::fs::write(
            repo_path.join(".git"),
            format!("gitdir: {}\n", actual_git_dir.display()),
        )
        .unwrap();

        assert_eq!(
            scm.git_dir().unwrap().canonicalize().unwrap(),
            actual_git_dir.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_classify_rebase_continue_conflict_state_from_stdout() {
        let output = Output {
            status: Command::new("true").status().unwrap(),
            stdout: b"You must edit all merge conflicts and then\nrun git rebase --continue\n"
                .to_vec(),
            stderr: Vec::new(),
        };

        assert_eq!(
            classify_rebase_failure_text(&output_text(&output)),
            Some(RebaseOutcome::InProgress)
        );
    }

    #[test]
    fn test_classify_rebase_continue_conflict_state() {
        let stderr = "error: could not apply 1234567... example\nhint: Resolve all conflicts manually, mark them as resolved with\n";
        assert_eq!(
            classify_rebase_failure_text(stderr),
            Some(RebaseOutcome::InProgress)
        );
    }

    #[test]
    fn test_classify_rebase_continue_non_conflict_failure() {
        let stderr = "fatal: no rebase in progress\n";
        assert_eq!(classify_rebase_failure_text(stderr), None);
    }
}
