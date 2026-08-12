//! Cross-process behaviour of the sync repository lock.
//!
//! Regression coverage for the failure that motivated the lock: two Stop hooks
//! firing seconds apart each spawned `ccs push`, both wrote the same git
//! repository, and the loser exited 1 — which Claude Code surfaces as
//! "Stop hook error".
//!
//! Contention is created by locking the file from the test process rather than
//! by racing two children, so the assertions do not depend on process timing.

use fs4::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const CHILD_TIMEOUT: Duration = Duration::from_secs(60);
const SESSION_ID: &str = "aaaaaaaa-1111-2222-3333-444444444444";

struct Fixture {
    home: TempDir,
    config: TempDir,
    repo: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            home: tempfile::tempdir().expect("home tempdir"),
            config: tempfile::tempdir().expect("config tempdir"),
            repo: tempfile::tempdir().expect("repo tempdir"),
        };
        fixture.init_repo();
        fixture.write_session();
        fixture.write_state();
        fixture.disable_session_maintenance();
        fixture
    }

    fn init_repo(&self) {
        let repo = self.repo.path();
        run_git(repo, &["init", "-q"]);
        run_git(repo, &["config", "user.email", "test@example.com"]);
        run_git(repo, &["config", "user.name", "test"]);
        run_git(repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    }

    fn project_dir(&self) -> PathBuf {
        self.home
            .path()
            .join(".claude/projects/-tmp-repo-lock-tests")
    }

    fn session_path(&self) -> PathBuf {
        self.project_dir().join(format!("{SESSION_ID}.jsonl"))
    }

    fn write_session(&self) {
        fs::create_dir_all(self.project_dir()).expect("project dir");
        let content = format!(
            concat!(
                "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"/tmp/repo-lock-tests\",",
                "\"timestamp\":\"2026-07-01T00:00:00Z\",",
                "\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n",
                "{{\"type\":\"assistant\",\"sessionId\":\"{id}\",\"cwd\":\"/tmp/repo-lock-tests\",",
                "\"timestamp\":\"2026-07-01T00:00:01Z\",",
                "\"message\":{{\"role\":\"assistant\",\"content\":\"hi\"}}}}\n"
            ),
            id = SESSION_ID
        );
        fs::write(self.session_path(), content).expect("session fixture");
    }

    /// Add enough sessions that a push takes over a second.
    ///
    /// Two children spawned back to back start ~2ms apart, so a work window
    /// this wide makes their overlap certain rather than incidental — without
    /// it the concurrency test silently degrades into two sequential pushes and
    /// stops exercising the lock at all.
    fn write_bulk_sessions(&self, count: usize) {
        fs::create_dir_all(self.project_dir()).expect("project dir");
        for index in 0..count {
            let id = format!("{index:08}-1111-2222-3333-444444444444");
            let mut content = String::new();
            for line in 0..40 {
                content.push_str(&format!(
                    "{{\"type\":\"user\",\"sessionId\":\"{id}\",\
                     \"cwd\":\"/tmp/repo-lock-tests\",\
                     \"timestamp\":\"2026-07-01T00:00:00Z\",\
                     \"message\":{{\"role\":\"user\",\"content\":\"line {line}\"}}}}\n"
                ));
            }
            fs::write(self.project_dir().join(format!("{id}.jsonl")), content)
                .expect("bulk session");
        }
    }

    fn write_state(&self) {
        let state = serde_json::json!({
            "sync_repo_path": self.repo.path(),
            "has_remote": false,
            "is_cloned_repo": false,
            "last_synced_commit": null,
        });
        fs::write(
            self.config.path().join("state.json"),
            serde_json::to_vec_pretty(&state).expect("serialize state"),
        )
        .expect("write state");
    }

    /// Session maintenance defaults to enabled and would hide fixture sessions
    /// living under a temporary cwd, so it is switched off explicitly.
    fn disable_session_maintenance(&self) {
        fs::write(
            self.config.path().join("config.toml"),
            "[session_maintenance]\nenabled = false\n",
        )
        .expect("write config.toml");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccs"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("CLAUDE_CODE_SYNC_CONFIG_DIR", self.config.path())
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run ccs")
    }

    /// The lock file is named from a hash of the repository path, so it is
    /// discovered rather than recomputed — recomputing would duplicate the
    /// naming rule and silently pass if the rule changed.
    fn lock_path(&self) -> PathBuf {
        let mut found: Vec<PathBuf> = fs::read_dir(self.config.path())
            .expect("read config dir")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("sync-repo-") && n.ends_with(".lock"))
            })
            .collect();
        assert_eq!(
            found.len(),
            1,
            "expected exactly one sync-repo lock, found {found:?}"
        );
        found.pop().expect("lock path")
    }

    fn commit_count(&self) -> usize {
        let output = run_git(self.repo.path(), &["log", "--oneline"]);
        String::from_utf8_lossy(&output.stdout).lines().count()
    }

    fn repo_is_clean(&self) -> bool {
        let output = run_git(self.repo.path(), &["status", "--porcelain"]);
        output.stdout.is_empty()
    }
}

fn run_git(repo: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn assert_exit_zero(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} must exit 0 (a non-zero exit is what Claude Code reports as a \
         hook error): status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Hold the repository lock for the lifetime of this guard.
struct LockHolder {
    _file: File,
}

impl LockHolder {
    fn hold(lock_path: &Path) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .expect("open lock file");
        FileExt::lock(&file).expect("hold lock");
        Self { _file: file }
    }
}

#[test]
fn push_creates_lock_and_commits_when_uncontended() {
    let fixture = Fixture::new();
    let before = fixture.commit_count();

    let output = fixture.run(&["push", "--quiet"]);
    assert_exit_zero(&output, "uncontended push");

    assert_eq!(
        fixture.commit_count(),
        before + 1,
        "an uncontended push must actually commit"
    );
    assert!(
        fixture.lock_path().exists(),
        "the lock file must be created"
    );
}

#[test]
fn push_skips_instead_of_failing_when_repository_is_locked() {
    let fixture = Fixture::new();
    // First push establishes the lock file and a baseline commit.
    assert_exit_zero(&fixture.run(&["push", "--quiet"]), "warmup push");

    let session_two = fixture
        .project_dir()
        .join("bbbbbbbb-2222-3333-4444-555555555555.jsonl");
    fs::write(
        &session_two,
        fs::read_to_string(fixture.session_path())
            .expect("read fixture session")
            .replace(SESSION_ID, "bbbbbbbb-2222-3333-4444-555555555555"),
    )
    .expect("second session");

    let baseline = fixture.commit_count();
    let _held = LockHolder::hold(&fixture.lock_path());

    let output = fixture.run(&["push"]);
    assert_exit_zero(&output, "contended push");

    let text = combined(&output);
    assert!(
        text.contains("另一个同步正在进行"),
        "contended push must say why it did nothing: {text}"
    );
    assert_eq!(
        fixture.commit_count(),
        baseline,
        "a skipped push must not commit"
    );
}

#[test]
fn quiet_push_stays_silent_on_stdout_when_locked() {
    let fixture = Fixture::new();
    assert_exit_zero(&fixture.run(&["push", "--quiet"]), "warmup push");
    let _held = LockHolder::hold(&fixture.lock_path());

    let output = fixture.run(&["push", "--quiet"]);
    assert_exit_zero(&output, "contended quiet push");
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "--quiet must not print the skip notice to stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn pull_skips_instead_of_failing_when_repository_is_locked() {
    let fixture = Fixture::new();
    assert_exit_zero(&fixture.run(&["push", "--quiet"]), "warmup push");
    let _held = LockHolder::hold(&fixture.lock_path());

    let output = fixture.run(&["pull"]);
    assert_exit_zero(&output, "contended pull");
    assert!(
        combined(&output).contains("另一个同步正在进行"),
        "contended pull must report the skip: {}",
        combined(&output)
    );
}

/// Deletion has the opposite contract from push: skipping it silently would
/// report a removal that never happened, so it must fail loudly and leave the
/// local file in place.
#[test]
fn session_delete_fails_loudly_when_repository_is_locked() {
    let fixture = Fixture::new();
    assert_exit_zero(&fixture.run(&["push", "--quiet"]), "warmup push");
    let _held = LockHolder::hold(&fixture.lock_path());

    let output = fixture.run(&["session", "delete", SESSION_ID, "--force"]);
    assert!(
        !output.status.success(),
        "a blocked deletion must not report success: {}",
        combined(&output)
    );
    assert!(
        combined(&output).contains("另一个同步正在进行"),
        "the failure must explain the contention: {}",
        combined(&output)
    );
    assert!(
        fixture.session_path().exists(),
        "the local session file must survive a blocked deletion"
    );
}

/// The original bug, reproduced end to end: two pushes started together must
/// both exit 0 and leave the repository consistent.
///
/// Before the lock existed, the loser hit git's `index.lock` and exited 1,
/// which Claude Code reported as a Stop hook error.
#[test]
fn concurrent_pushes_both_exit_zero_and_leave_repo_clean() {
    let fixture = Fixture::new();
    fixture.write_bulk_sessions(150);

    let mut first = fixture
        .command(&["push"])
        .spawn()
        .expect("spawn first push");
    let mut second = fixture
        .command(&["push"])
        .spawn()
        .expect("spawn second push");

    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        let first_done = first.try_wait().expect("poll first").is_some();
        let second_done = second.try_wait().expect("poll second").is_some();
        if first_done && second_done {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "concurrent pushes did not finish within {CHILD_TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let first = first.wait_with_output().expect("first output");
    let second = second.wait_with_output().expect("second output");
    assert_exit_zero(&first, "first concurrent push");
    assert_exit_zero(&second, "second concurrent push");

    // Exactly one must have yielded. Zero would mean the two pushes never
    // actually overlapped, so the test would be proving nothing; two would mean
    // the sessions were never pushed at all.
    let skipped = [&first, &second]
        .iter()
        .filter(|output| combined(output).contains("另一个同步正在进行"))
        .count();
    assert_eq!(
        skipped,
        1,
        "exactly one push must yield to the other\nfirst={}\nsecond={}",
        combined(&first),
        combined(&second)
    );

    assert!(
        fixture.repo_is_clean(),
        "concurrent pushes must not leave the working tree dirty"
    );
    assert!(
        !fixture.repo.path().join(".git/index.lock").exists(),
        "no stale git index.lock may remain"
    );
    assert!(
        fixture.commit_count() >= 2,
        "the winning push must have committed the sessions"
    );
}
