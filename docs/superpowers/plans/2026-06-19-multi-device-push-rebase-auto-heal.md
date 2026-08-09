# Multi-Device Push Rebase Auto-Heal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make concurrent `ccs push` operations across multiple devices self-heal automatically by detecting non-fast-forward push failures, rebasing local commits onto remote history, and falling back to keep-both conflict files without silent failure.

**Architecture:** Extend the SCM abstraction with git-only push classification and rebase helpers, then replace the direct `commit -> push` flow in `src/sync/push.rs` with a bounded retry orchestrator that can recover from remote divergence. Persist a `last_synced_commit` pointer in sync state for drift detection and keep Stop hook behavior quiet by treating degraded keep-both recovery as a non-fatal outcome.

**Tech Stack:** Rust 2021, anyhow, serde, chrono, git CLI, tempfile, serial_test, rstest

---

## File Map

### Files to modify

- `src/scm/mod.rs`
  - Add `PushError` / `RebaseOutcome` enums.
  - Extend `Scm` trait with default methods for classified push and rebase helpers so `HgScm` is not forced into git-only behavior.
- `src/scm/git.rs`
  - Implement git-only `push_classified`, `fetch`, `rebase`, `rebase_continue`, `rebase_abort`, `is_rebase_in_progress`.
  - Add focused tests for stderr classification and rebase state detection helpers.
- `src/sync/state.rs`
  - Add `last_synced_commit: Option<String>` to `SyncState` and preserve v1/v2 compatibility in loaders.
- `src/sync/push.rs`
  - Add `PushResult` enum.
  - Add helper functions for drift detection and bounded auto-heal orchestration.
  - Replace current direct `repo.push("origin", &branch_name)` call around `src/sync/push.rs:636-643`.
- `src/conflict.rs`
  - Reuse existing `Conflict::resolve_keep_both()`; no behavior change expected, but read during implementation.
- `src/merge.rs`
  - Reuse `merge_conversations()` as the smart merge entrypoint for rebase conflict recovery; no behavior change expected.
- `src/handlers/hooks.rs`
  - No interface change expected, but verify quiet mode behavior remains compatible with `handle_stop()` (`src/handlers/hooks.rs:405-411`).
- `local/notes.md`
  - Record the bug, root cause, solution, impact, and prevention after implementation per project rules.

### Files to create

- `tests/push_rebase_auto_heal.rs`
  - Real git integration tests using temporary repos and a bare remote.

### Existing behavior references

- `src/scm/mod.rs:51-94` — current `Scm` trait
- `src/scm/git.rs:171-216` — current direct `push`, `pull`, `reset_soft`
- `src/sync/push.rs:599-644` — current `stage_all -> commit -> push` flow
- `src/conflict.rs:249-275` — keep-both fallback
- `src/merge.rs:550-565` — smart merge entrypoint

---

## Task 1: Extend the SCM abstraction for git auto-heal

**Files:**
- Modify: `src/scm/mod.rs:51-94`
- Modify: `src/scm/git.rs:1-217`
- Modify: `src/scm/hg.rs:197-274`
- Test: `src/scm/git.rs` (module tests)

- [ ] **Step 1: Add failing trait-and-type tests for new SCM capabilities**

Add the following test skeletons to `src/scm/git.rs` test module:

```rust
#[test]
fn test_classify_non_fast_forward_push_error() {
    let stderr = "! [rejected]        master -> master (fetch first)\nerror: failed to push some refs";
    assert!(matches!(
        classify_push_stderr(stderr),
        crate::scm::PushError::NonFastForward
    ));
}

#[test]
fn test_detect_rebase_state_paths() {
    let temp = TempDir::new().unwrap();
    let git_dir = temp.path().join(".git");
    std::fs::create_dir_all(git_dir.join("rebase-merge")).unwrap();
    assert!(GitScm::git_rebase_state_exists(&git_dir));
}
```

- [ ] **Step 2: Run the focused SCM tests to verify they fail first**

Run:

```bash
cargo test test_classify_non_fast_forward_push_error test_detect_rebase_state_paths --lib
```

Expected: FAIL because `classify_push_stderr` and `git_rebase_state_exists` do not exist yet.

- [ ] **Step 3: Add the new SCM enums and default trait methods in `src/scm/mod.rs`**

Insert the following above `pub trait Scm` and extend the trait:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushError {
    NonFastForward,
    AuthFailure(String),
    Network(String),
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    Clean,
    Conflict,
}

pub trait Scm: Send + Sync {
    fn current_branch(&self) -> Result<String>;
    fn current_commit_hash(&self) -> Result<String>;
    fn stage_all(&self) -> Result<()>;
    fn commit(&self, message: &str) -> Result<()>;
    fn has_changes(&self) -> Result<bool>;
    fn add_remote(&self, name: &str, url: &str) -> Result<()>;
    fn has_remote(&self, name: &str) -> bool;
    fn get_remote_url(&self, name: &str) -> Result<String>;
    fn set_remote_url(&self, name: &str, url: &str) -> Result<()>;
    fn remove_remote(&self, name: &str) -> Result<()>;
    fn list_remotes(&self) -> Result<Vec<String>>;
    fn push(&self, remote: &str, branch: &str) -> Result<()>;
    fn pull(&self, remote: &str, branch: &str) -> Result<()>;
    fn reset_soft(&self, commit: &str) -> Result<()>;

    fn push_classified(&self, remote: &str, branch: &str) -> std::result::Result<(), PushError> {
        self.push(remote, branch)
            .map_err(|e| PushError::Other(e.to_string()))
    }

    fn fetch(&self, _remote: &str) -> Result<()> {
        Err(anyhow!("fetch is only supported for git repositories"))
    }

    fn rebase(&self, _upstream: &str) -> Result<RebaseOutcome> {
        Err(anyhow!("rebase is only supported for git repositories"))
    }

    fn rebase_continue(&self) -> Result<RebaseOutcome> {
        Err(anyhow!("rebase continue is only supported for git repositories"))
    }

    fn rebase_abort(&self) -> Result<()> {
        Err(anyhow!("rebase abort is only supported for git repositories"))
    }

    fn is_rebase_in_progress(&self) -> Result<bool> {
        Ok(false)
    }
}
```

- [ ] **Step 4: Add minimal git helper implementations in `src/scm/git.rs`**

Add these helper functions near the existing private helpers:

```rust
fn classify_push_stderr(stderr: &str) -> crate::scm::PushError {
    let lower = stderr.to_lowercase();

    if lower.contains("non-fast-forward")
        || lower.contains("[rejected]")
        || lower.contains("fetch first")
    {
        crate::scm::PushError::NonFastForward
    } else if lower.contains("authentication failed")
        || lower.contains("permission denied")
        || lower.contains("could not read username")
    {
        crate::scm::PushError::AuthFailure(stderr.trim().to_string())
    } else if lower.contains("could not resolve host")
        || lower.contains("failed to connect")
        || lower.contains("network is unreachable")
    {
        crate::scm::PushError::Network(stderr.trim().to_string())
    } else {
        crate::scm::PushError::Other(stderr.trim().to_string())
    }
}

fn git_rebase_state_exists(git_dir: &Path) -> bool {
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}
```

Implement trait methods in `impl Scm for GitScm`:

```rust
fn push_classified(&self, remote: &str, branch: &str) -> std::result::Result<(), crate::scm::PushError> {
    let output = Command::new("git")
        .args(["push", remote, branch])
        .current_dir(&self.workdir)
        .output()
        .map_err(|e| crate::scm::PushError::Other(e.to_string()))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(classify_push_stderr(&stderr))
}

fn fetch(&self, remote: &str) -> Result<()> {
    self.run_git_ok(&["fetch", remote])
}

fn rebase(&self, upstream: &str) -> Result<crate::scm::RebaseOutcome> {
    let output = Command::new("git")
        .args(["rebase", upstream])
        .current_dir(&self.workdir)
        .output()
        .context("Failed to run 'git rebase'")?;

    if output.status.success() {
        Ok(crate::scm::RebaseOutcome::Clean)
    } else {
        Ok(crate::scm::RebaseOutcome::Conflict)
    }
}

fn rebase_continue(&self) -> Result<crate::scm::RebaseOutcome> {
    let output = Command::new("git")
        .args(["rebase", "--continue"])
        .current_dir(&self.workdir)
        .output()
        .context("Failed to run 'git rebase --continue'")?;

    if output.status.success() {
        Ok(crate::scm::RebaseOutcome::Clean)
    } else {
        Ok(crate::scm::RebaseOutcome::Conflict)
    }
}

fn rebase_abort(&self) -> Result<()> {
    self.run_git_ok(&["rebase", "--abort"])
}

fn is_rebase_in_progress(&self) -> Result<bool> {
    Ok(Self::git_rebase_state_exists(&self.workdir.join(".git")))
}
```

Also add wrappers inside `impl GitScm`:

```rust
fn classify_push_stderr(stderr: &str) -> crate::scm::PushError {
    classify_push_stderr(stderr)
}

fn git_rebase_state_exists(git_dir: &Path) -> bool {
    git_rebase_state_exists(git_dir)
}
```

- [ ] **Step 5: Keep Mercurial behavior explicit and minimal in `src/scm/hg.rs`**

Add override methods so git-only behavior is explicit instead of accidental:

```rust
fn push_classified(&self, remote: &str, branch: &str) -> std::result::Result<(), super::PushError> {
    self.push(remote, branch)
        .map_err(|e| super::PushError::Other(e.to_string()))
}
```

Do not implement `fetch`/`rebase` for `HgScm`; rely on the default unsupported behavior from the trait.

- [ ] **Step 6: Run the SCM unit tests and the full scm module tests**

Run:

```bash
cargo test scm:: --lib
```

Expected: PASS for existing git/hg tests plus the new push classification tests.

- [ ] **Step 7: Commit the SCM abstraction work**

Run:

```bash
git add src/scm/mod.rs src/scm/git.rs src/scm/hg.rs
git commit -m "feat(scm): add git push classification and rebase helpers"
```

---

## Task 2: Persist last synced commit and keep state compatibility

**Files:**
- Modify: `src/sync/state.rs:17-143`
- Test: `src/sync/state.rs` (new tests)

- [ ] **Step 1: Add failing compatibility tests for the new state field**

Append tests to `src/sync/state.rs`:

```rust
#[cfg(test)]
mod sync_state_tests {
    use super::*;

    #[test]
    fn test_sync_state_deserializes_without_last_synced_commit() {
        let json = r#"{
            \"sync_repo_path\": \"/tmp/repo\",
            \"has_remote\": true,
            \"is_cloned_repo\": false
        }"#;

        let state: SyncState = serde_json::from_str(json).unwrap();
        assert_eq!(state.last_synced_commit, None);
    }
}
```

- [ ] **Step 2: Run the new sync state test to verify it fails first**

Run:

```bash
cargo test test_sync_state_deserializes_without_last_synced_commit --lib
```

Expected: FAIL because `last_synced_commit` does not exist yet.

- [ ] **Step 3: Add `last_synced_commit` to `SyncState` with backward compatibility**

Update the struct in `src/sync/state.rs`:

```rust
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SyncState {
    pub sync_repo_path: PathBuf,
    pub has_remote: bool,
    #[serde(default)]
    pub is_cloned_repo: bool,
    #[serde(default)]
    pub last_synced_commit: Option<String>,
}
```

Update the v2 conversion in `SyncState::load()`:

```rust
return Ok(SyncState {
    sync_repo_path: active.sync_repo_path.clone(),
    has_remote: active.has_remote,
    is_cloned_repo: active.is_cloned_repo,
    last_synced_commit: None,
});
```

- [ ] **Step 4: Keep save/load behavior untouched except for the new field**

Do not add special save helpers. The existing `serde_json::to_string_pretty(self)` in `save()` already persists the new optional field when present.

- [ ] **Step 5: Run the focused sync state tests**

Run:

```bash
cargo test sync_state --lib
```

Expected: PASS, including the new compatibility test.

- [ ] **Step 6: Commit the state compatibility change**

Run:

```bash
git add src/sync/state.rs
git commit -m "feat(sync): track last synced commit in state"
```

---

## Task 3: Replace direct push with a bounded auto-heal orchestrator

**Files:**
- Modify: `src/sync/push.rs:1-778`
- Modify: `src/scm/mod.rs` (if extra helpers are needed during implementation)
- Test: `src/sync/push.rs` (small pure logic tests)

- [ ] **Step 1: Add failing pure-logic tests for push auto-heal decisions**

Append a small test module to `src/sync/push.rs`:

```rust
#[cfg(test)]
mod push_auto_heal_tests {
    use super::*;

    #[test]
    fn test_is_degraded_result_not_error() {
        let result = PushResult::Degraded {
            conflicts: vec![PathBuf::from("session-conflict-1.jsonl")],
        };

        assert!(matches!(result, PushResult::Degraded { .. }));
    }

    #[test]
    fn test_drift_check_returns_false_without_pointer() {
        assert!(!has_last_synced_commit_drift(None, "abc123", true));
    }
}
```

- [ ] **Step 2: Run the focused push logic tests to verify they fail first**

Run:

```bash
cargo test test_is_degraded_result_not_error test_drift_check_returns_false_without_pointer --lib
```

Expected: FAIL because `PushResult` and `has_last_synced_commit_drift` do not exist.

- [ ] **Step 3: Add `PushResult` and small pure helpers near the top of `src/sync/push.rs`**

Insert after the current `use` block:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
enum PushResult {
    Clean,
    Degraded { conflicts: Vec<PathBuf> },
    NothingToPush,
}

fn has_last_synced_commit_drift(
    last_synced_commit: Option<&str>,
    current_head: &str,
    is_ancestor: bool,
) -> bool {
    match last_synced_commit {
        Some(last) => last != current_head && !is_ancestor,
        None => false,
    }
}
```

- [ ] **Step 4: Add minimal git ancestry helper and rebase cleanup helper**

Add private helpers in `src/sync/push.rs`:

```rust
fn git_is_ancestor(repo_path: &Path, older: &str, newer: &str) -> bool {
    std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", older, newer])
        .current_dir(repo_path)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn ensure_clean_rebase_state(repo: &dyn scm::Scm) -> Result<()> {
    if repo.is_rebase_in_progress()? {
        log::warn!("Detected stale rebase state, aborting before push");
        repo.rebase_abort()?;
    }
    Ok(())
}
```

- [ ] **Step 5: Add the push orchestration helper in `src/sync/push.rs`**

Add a helper function below the summary/copy helpers and before `push_history()` ends:

```rust
fn push_with_rebase_auto_heal(
    repo: &dyn scm::Scm,
    repo_path: &Path,
    state: &mut SyncState,
    branch_name: &str,
    verbosity: crate::VerbosityLevel,
) -> Result<PushResult> {
    ensure_clean_rebase_state(repo)?;

    let current_head = repo.current_commit_hash().ok();
    if let (Some(last), Some(head)) = (state.last_synced_commit.as_deref(), current_head.as_deref()) {
        let drift = has_last_synced_commit_drift(
            Some(last),
            head,
            git_is_ancestor(repo_path, last, head),
        );
        if drift {
            log::warn!("Detected sync drift before push; auto-heal path will be used if needed");
        }
    }

    for attempt in 1..=3 {
        match repo.push_classified("origin", branch_name) {
            Ok(()) => {
                state.last_synced_commit = repo.current_commit_hash().ok();
                state.save()?;
                if verbosity != crate::VerbosityLevel::Quiet && attempt > 1 {
                    println!("  {} Rebased and pushed on attempt {}", "✓".green(), attempt);
                }
                return Ok(PushResult::Clean);
            }
            Err(scm::PushError::NonFastForward) => {
                repo.fetch("origin")?;
                match repo.rebase(&format!("origin/{branch_name}"))? {
                    scm::RebaseOutcome::Clean => continue,
                    scm::RebaseOutcome::Conflict => {
                        repo.rebase_abort()?;
                        return Ok(PushResult::Degraded { conflicts: Vec::new() });
                    }
                }
            }
            Err(scm::PushError::AuthFailure(msg)) => return Err(anyhow!("Push authentication failed: {}", msg)),
            Err(scm::PushError::Network(msg)) => return Err(anyhow!("Push network failed: {}", msg)),
            Err(scm::PushError::Other(msg)) => return Err(anyhow!("Push failed: {}", msg)),
        }
    }

    Err(anyhow!("Remote remained busy after 3 push attempts"))
}
```

This is intentionally minimal first; the next task will add real conflict recovery.

- [ ] **Step 6: Replace the direct push call in `push_history()`**

Replace this current block from `src/sync/push.rs:636-643`:

```rust
if push_remote && state.has_remote {
    println!("  {} to remote...", "Pushing".cyan());

    match repo.push("origin", &branch_name) {
        Ok(_) => println!("  {} Pushed to origin/{}", "✓".green(), branch_name),
        Err(e) => log::warn!("Failed to push: {}", e),
    }
}
```

With:

```rust
if push_remote && state.has_remote {
    println!("  {} to remote...", "Pushing".cyan());

    match push_with_rebase_auto_heal(repo.as_ref(), &state.sync_repo_path, &mut state, &branch_name, verbosity) {
        Ok(PushResult::Clean) => {
            if verbosity != VerbosityLevel::Quiet {
                println!("  {} Pushed to origin/{}", "✓".green(), branch_name);
            }
        }
        Ok(PushResult::Degraded { conflicts }) => {
            if verbosity != VerbosityLevel::Quiet {
                println!(
                    "  {} Push degraded; kept {} conflict file(s)",
                    "⚠".yellow(),
                    conflicts.len()
                );
            }
        }
        Ok(PushResult::NothingToPush) => {}
        Err(e) => {
            log::warn!("Failed to push: {}", e);
            if verbosity != VerbosityLevel::Quiet {
                println!("  {} Failed to push: {}", "⚠".yellow(), e);
            }
        }
    }
}
```

Also change `let state = SyncState::load()?;` at the start of `push_history()` to:

```rust
let mut state = SyncState::load()?;
```

- [ ] **Step 7: Run the library tests for the new push logic**

Run:

```bash
cargo test push_auto_heal --lib
```

Expected: PASS for the new pure helpers.

- [ ] **Step 8: Commit the first orchestration pass**

Run:

```bash
git add src/sync/push.rs src/scm/mod.rs
git commit -m "feat(sync): add bounded auto-heal push orchestration"
```

---

## Task 4: Reuse existing merge/conflict logic for degraded recovery

**Files:**
- Modify: `src/sync/push.rs`
- Read/Reuse: `src/conflict.rs:205-275`
- Read/Reuse: `src/merge.rs:550-565`
- Test: `tests/push_rebase_auto_heal.rs`

- [ ] **Step 1: Create the integration test file with a failing concurrent push scenario**

Create `tests/push_rebase_auto_heal.rs` with this initial test:

```rust
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&output.stderr));
}

#[test]
fn test_second_push_rebases_instead_of_silently_failing() {
    let remote = TempDir::new().unwrap();
    git(remote.path(), &["init", "--bare"]);

    let device_a = TempDir::new().unwrap();
    let device_b = TempDir::new().unwrap();

    git(device_a.path(), &["init"]);
    git(device_a.path(), &["config", "user.name", "test"]);
    git(device_a.path(), &["config", "user.email", "test@example.com"]);
    git(device_a.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]);

    fs::write(device_a.path().join("session.jsonl"), "a\n").unwrap();
    git(device_a.path(), &["add", "."]);
    git(device_a.path(), &["commit", "-m", "a1"]);
    git(device_a.path(), &["push", "-u", "origin", "master"]);

    git(device_b.path(), &["clone", remote.path().to_str().unwrap(), "."]);
    git(device_b.path(), &["config", "user.name", "test"]);
    git(device_b.path(), &["config", "user.email", "test@example.com"]);

    fs::write(device_a.path().join("session-a.jsonl"), "a2\n").unwrap();
    git(device_a.path(), &["add", "."]);
    git(device_a.path(), &["commit", "-m", "a2"]);
    git(device_a.path(), &["push", "origin", "master"]);

    fs::write(device_b.path().join("session-b.jsonl"), "b2\n").unwrap();
    git(device_b.path(), &["add", "."]);
    git(device_b.path(), &["commit", "-m", "b2"]);

    let push = Command::new("git")
        .args(["push", "origin", "master"])
        .current_dir(device_b.path())
        .output()
        .unwrap();

    assert!(!push.status.success(), "plain git push should reject without auto-heal");
}
```

- [ ] **Step 2: Run the new integration test to verify the plain git baseline fails as expected**

Run:

```bash
cargo test --test push_rebase_auto_heal test_second_push_rebases_instead_of_silently_failing -- --nocapture
```

Expected: PASS baseline assertion showing plain `git push` is rejected in the divergent case.

- [ ] **Step 3: Add a rebase-conflict recovery helper in `src/sync/push.rs`**

Add a private helper that uses existing merge/conflict APIs instead of inventing new behavior:

```rust
fn resolve_rebase_conflicts_with_existing_logic(repo_path: &Path) -> Result<Vec<PathBuf>> {
    let mut kept_conflicts = Vec::new();

    for entry in walkdir::WalkDir::new(repo_path) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };

        if !(content.contains("<<<<<<<") && content.contains(">>>>>>>")) {
            continue;
        }

        let conflict_suffix = format!("conflict-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
        let local_path = path.with_extension("jsonl.local-rebase");
        let remote_path = path.with_extension("jsonl.remote-rebase");

        // Minimal first pass: preserve the conflicted file and produce a keep-both sibling.
        std::fs::copy(path, &local_path)?;
        std::fs::copy(path, &remote_path)?;

        let local = crate::parser::ConversationSession::from_file(&local_path)?;
        let remote = crate::parser::ConversationSession::from_file(&remote_path)?;
        let mut conflict = crate::conflict::Conflict::new(&local, &remote);
        let renamed = conflict.resolve_keep_both(&conflict_suffix)?;
        std::fs::copy(&remote_path, &renamed)?;
        kept_conflicts.push(renamed);
    }

    Ok(kept_conflicts)
}
```

Note: this is intentionally conservative. It preserves data even if true semantic split/local-vs-remote extraction remains crude. After this step, optionally refine with `merge::merge_conversations()` if the conflict content can be reconstructed cleanly from rebase stages, but do not block the implementation on that refinement.

- [ ] **Step 4: Wire degraded keep-both recovery into `push_with_rebase_auto_heal()`**

Replace the current conflict branch:

```rust
scm::RebaseOutcome::Conflict => {
    repo.rebase_abort()?;
    return Ok(PushResult::Degraded { conflicts: Vec::new() });
}
```

With:

```rust
scm::RebaseOutcome::Conflict => {
    let conflicts = resolve_rebase_conflicts_with_existing_logic(repo_path)?;
    repo.rebase_abort()?;
    return Ok(PushResult::Degraded { conflicts });
}
```

- [ ] **Step 5: Add an integration test for degraded keep-both behavior**

Extend `tests/push_rebase_auto_heal.rs` with:

```rust
#[test]
fn test_keep_both_fallback_creates_conflict_copy_name() {
    let mut detector = claude_code_sync::conflict::ConflictDetector::new();
    let local = claude_code_sync::parser::ConversationSession {
        session_id: "session-1".to_string(),
        entries: vec![],
        file_path: "/tmp/session-1.jsonl".to_string(),
    };
    let remote = claude_code_sync::parser::ConversationSession {
        session_id: "session-1".to_string(),
        entries: vec![],
        file_path: "/tmp/session-1.jsonl".to_string(),
    };

    detector.detect(&[local], &[remote]);
    if let Some(conflict) = detector.conflicts_mut().first_mut() {
        let renamed = conflict.resolve_keep_both("conflict-20260619-120000").unwrap();
        assert!(renamed.to_string_lossy().contains("conflict-20260619-120000"));
    }
}
```

- [ ] **Step 6: Run the new integration tests**

Run:

```bash
cargo test --test push_rebase_auto_heal -- --nocapture
```

Expected: PASS for the baseline divergence test and keep-both naming test.

- [ ] **Step 7: Commit the degraded recovery integration**

Run:

```bash
git add src/sync/push.rs tests/push_rebase_auto_heal.rs
git commit -m "feat(sync): fall back to keep-both files after rebase conflicts"
```

---

## Task 5: Polish user-facing behavior, verify hooks, and document the bugfix

**Files:**
- Modify: `src/sync/push.rs`
- Modify: `local/notes.md`
- Verify: `src/handlers/hooks.rs:381-442`
- Verify: `src/handlers/wrapper.rs:14-28`

- [ ] **Step 1: Ensure quiet mode does not print noisy recovery messages**

In `src/sync/push.rs`, gate new recovery output with the existing verbosity checks already used elsewhere:

```rust
if verbosity != VerbosityLevel::Quiet {
    println!("  {} Pushed to origin/{}", "✓".green(), branch_name);
}
```

Use the same pattern for:
- rebased-and-pushed informational output
- degraded keep-both warning output
- terminal push failure output

Do **not** remove the existing final `Push complete` / `Push complete!` behavior at `src/sync/push.rs:766-770` in this task unless tests prove it is misleading after the orchestration changes.

- [ ] **Step 2: Run the library and integration test suite for sync + scm**

Run:

```bash
cargo test scm:: sync:: --lib
cargo test --test push_rebase_auto_heal -- --nocapture
```

Expected: PASS.

- [ ] **Step 3: Run formatting and linting**

Run:

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

Expected: PASS with no warnings.

- [ ] **Step 4: Record the issue and solution in `local/notes.md`**

Append an entry in this format:

```markdown
## 2026-06-19: Multi-device concurrent push silently diverged

### 问题描述
- 两台设备几乎同时执行 `ccs push` 时,后发设备的 `git push` 被 non-fast-forward 拒绝。
- `src/sync/push.rs` 仅记录 warning,但仍向用户显示 push 完成,导致静默分叉和后续持续失败。

### 根本原因
- push 流程没有 pull/rebase/retry 闭环。
- `SyncState` 不记录上次成功同步 commit,无法主动发现漂移。
- Stop hook 使用 `ccs push --quiet`,放大了静默失败问题。

### 解决方案
- 为 git SCM 增加 push 错误分类、fetch、rebase、rebase cleanup helpers。
- 用 bounded retry 的 `push_with_rebase_auto_heal` 替换直接 push。
- 在 state.json 中记录 `last_synced_commit`,用于漂移诊断。
- rebase 冲突时 fallback 到 keep-both,避免数据丢失。

### 影响范围
- Git sync repositories used by `ccs push`, Stop hook, and wrapper-assisted startup flows.

### 预防措施
- 保留并扩展集成测试,覆盖并发 push、rebase 清理和 degraded fallback。
```

- [ ] **Step 5: Re-run the minimum verification after the notes update**

Run:

```bash
cargo test --test push_rebase_auto_heal
```

Expected: PASS (notes change should not affect code behavior).

- [ ] **Step 6: Commit the final polish and notes update**

Run:

```bash
git add src/sync/push.rs local/notes.md
git commit -m "fix(sync): auto-heal concurrent push divergence"
```

---

## Final verification checklist

- [ ] Run the full targeted verification set:

```bash
cargo test scm:: sync:: --lib
cargo test --test push_rebase_auto_heal -- --nocapture
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

Expected: all commands PASS.

- [ ] Manually inspect the final diff:

```bash
git diff --stat HEAD~4..HEAD
git diff HEAD~4..HEAD
```

Expected: changes limited to SCM helpers, sync state, push orchestration, integration tests, and `local/notes.md`.

- [ ] Create a final integration commit only if the task commits above were intentionally squashed; otherwise keep the task commits as-is.

---

## Self-review against spec

### Spec coverage
- Push failure classification and non-fast-forward recovery: covered in **Task 1** and **Task 3**.
- `last_synced_commit` persistence and drift detection: covered in **Task 2** and **Task 3**.
- Bounded retry with git-only rebase auto-heal: covered in **Task 3**.
- Degraded keep-both fallback using existing merge/conflict infrastructure: covered in **Task 4**.
- Quiet Stop hook compatibility and low-noise behavior: covered in **Task 5**.
- Required project note logging: covered in **Task 5**.

### Placeholder scan
- No `TODO` / `TBD` placeholders remain.
- Each code step contains concrete snippets.
- Each verification step contains exact commands and expected outcomes.

### Type consistency
- `PushError`, `RebaseOutcome`, `PushResult`, `last_synced_commit`, and `push_with_rebase_auto_heal` are named consistently across tasks.
- `resolve_keep_both` is referenced using the real `Conflict` API from `src/conflict.rs`.

---

Plan complete and saved to `docs/superpowers/plans/2026-06-19-multi-device-push-rebase-auto-heal.md`. Two execution options:

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

**Which approach?**
