use fs4::FileExt;
use serde_json::{json, Value};
use serial_test::serial;
use std::fs::{self, OpenOptions};
#[cfg(unix)]
use std::io::{BufRead, BufReader};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Child;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const SESSION_ID: &str = "bbbbbbbb-1111-2222-3333-444444444444";
const CHILD_TIMEOUT: Duration = Duration::from_secs(10);
const WORKER_TIMEOUT: Duration = Duration::from_secs(30);

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
        fixture.write_state(fixture.repo.path());
        fixture.disable_background_features();
        fixture
    }

    fn init_repo(&self) {
        run_git(self.repo.path(), &["init", "-q"]);
        run_git(
            self.repo.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_git(self.repo.path(), &["config", "user.name", "test"]);
        run_git(
            self.repo.path(),
            &["commit", "-q", "--allow-empty", "-m", "init"],
        );
    }

    fn project_dir(&self) -> PathBuf {
        self.home.path().join(".claude/projects/-tmp-hooks-tests")
    }

    fn session_path(&self) -> PathBuf {
        self.project_dir().join(format!("{SESSION_ID}.jsonl"))
    }

    fn write_session(&self) {
        fs::create_dir_all(self.project_dir()).expect("project dir");
        let content = format!(
            concat!(
                "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"/tmp/hooks-tests\",",
                "\"timestamp\":\"2026-09-09T00:00:00Z\",",
                "\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n",
                "{{\"type\":\"assistant\",\"sessionId\":\"{id}\",\"cwd\":\"/tmp/hooks-tests\",",
                "\"timestamp\":\"2026-09-09T00:00:01Z\",",
                "\"message\":{{\"role\":\"assistant\",\"content\":\"hi\"}}}}\n"
            ),
            id = SESSION_ID
        );
        fs::write(self.session_path(), content).expect("session fixture");
    }

    fn append_session_message(&self, text: &str) {
        let mut file = OpenOptions::new()
            .append(true)
            .open(self.session_path())
            .expect("open session");
        writeln!(
            file,
            "{}",
            json!({
                "type": "user",
                "sessionId": SESSION_ID,
                "cwd": "/tmp/hooks-tests",
                "timestamp": "2026-09-09T00:00:02Z",
                "message": { "role": "user", "content": text }
            })
        )
        .expect("append session");
    }

    fn write_state(&self, repo_path: &Path) {
        let state = json!({
            "sync_repo_path": repo_path,
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

    fn disable_background_features(&self) {
        fs::write(
            self.config.path().join("config.toml"),
            "[config_sync]\nenabled = false\n\
             [session_maintenance]\nenabled = false\n",
        )
        .expect("write config.toml");
    }

    fn settings_path(&self) -> PathBuf {
        self.home.path().join(".claude/settings.json")
    }

    fn write_settings(&self, value: &Value) {
        fs::create_dir_all(self.settings_path().parent().unwrap()).expect("settings parent");
        fs::write(
            self.settings_path(),
            serde_json::to_vec_pretty(value).expect("serialize settings"),
        )
        .expect("write settings");
    }

    fn read_settings(&self) -> Value {
        serde_json::from_slice(&fs::read(self.settings_path()).expect("read settings"))
            .expect("parse settings")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccs"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("CLAUDE_CODE_SYNC_CONFIG_DIR", self.config.path())
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str], stdin: &[u8]) -> Output {
        run_with_timeout(self.command(args), stdin, CHILD_TIMEOUT)
    }

    fn run_stop(&self) -> (Output, Duration) {
        let started = Instant::now();
        let output = self.run(&["hook-stop"], b"{}\n");
        (output, started.elapsed())
    }

    fn debug_log(&self) -> String {
        fs::read_to_string(self.config.path().join("hook-debug.log")).unwrap_or_default()
    }

    fn wait_until(&self, label: &str, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + WORKER_TIMEOUT;
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "timed out waiting for {label}; debug log:\n{}",
            self.debug_log()
        );
    }

    fn wait_for_stamp(&self) {
        self.wait_until("push-hook.stamp", || {
            self.config.path().join("push-hook.stamp").exists()
        });
    }

    fn wait_for_failures(&self, expected: u64) {
        self.wait_until("push worker failure state", || {
            fs::read(self.config.path().join("push-hook-state.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|state| state["consecutive_failures"].as_u64())
                == Some(expected)
        });
    }

    fn commit_count(&self) -> usize {
        String::from_utf8_lossy(&run_git(self.repo.path(), &["log", "--oneline"]).stdout)
            .lines()
            .count()
    }
}

#[cfg(unix)]
struct PythonLockHolder {
    child: Child,
}

#[cfg(unix)]
impl PythonLockHolder {
    fn hold(path: &Path) -> Self {
        let mut child = Command::new("python3")
            .args([
                "-c",
                "import fcntl,sys,time; f=open(sys.argv[1], 'a+'); fcntl.flock(f, fcntl.LOCK_EX); print('ready', flush=True); time.sleep(30)",
                path.to_str().expect("UTF-8 lock path"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn Python lock holder");
        let mut ready = String::new();
        BufReader::new(child.stdout.take().expect("Python stdout"))
            .read_line(&mut ready)
            .expect("read Python ready marker");
        assert_eq!(ready.trim(), "ready");
        Self { child }
    }
}

#[cfg(unix)]
impl Drop for PythonLockHolder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_with_timeout(mut command: Command, stdin: &[u8], timeout: Duration) -> Output {
    let mut child = command.spawn().expect("spawn ccs");
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(stdin).expect("write child stdin");
    }

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("ccs child exceeded {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .expect("read stdout");
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .expect("read stderr");
    Output {
        status,
        stdout,
        stderr,
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

fn hook_commands(settings: &Value, event: &str) -> Vec<String> {
    settings["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|group| group["hooks"].as_array())
        .flatten()
        .filter_map(|hook| hook["command"].as_str().map(str::to_string))
        .collect()
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[serial]
fn install_creates_three_hooks_and_preserves_other_events() {
    let fixture = Fixture::new();
    fixture.write_settings(&json!({
        "hooks": {
            "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "audit.sh" }] }]
        }
    }));

    let output = fixture.run(&["hooks", "install"], b"");
    assert_success(&output, "hooks install");
    let settings = fixture.read_settings();
    assert_eq!(hook_commands(&settings, "SessionStart").len(), 1);
    assert_eq!(hook_commands(&settings, "Stop").len(), 1);
    assert_eq!(hook_commands(&settings, "UserPromptSubmit").len(), 1);
    assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0]["timeout"], 10);
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "audit.sh"
    );
}

#[test]
#[serial]
fn install_rejects_malformed_settings_without_overwriting() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.settings_path().parent().unwrap()).expect("settings parent");
    let malformed = b"{ not valid json";
    fs::write(fixture.settings_path(), malformed).expect("write malformed settings");

    let output = fixture.run(&["hooks", "install"], b"");
    assert!(!output.status.success());
    assert_eq!(fs::read(fixture.settings_path()).unwrap(), malformed);
}

#[test]
#[serial]
fn install_replaces_only_legacy_stop_wrapper() {
    let fixture = Fixture::new();
    let custom = [
        "python notify-stop.py",
        "gstack stop-review",
        "python ph-canvas-reply.py",
    ];
    fixture.write_settings(&json!({
        "hooks": {
            "Stop": [
                { "hooks": [{ "type": "command", "command": "~/.claude/hooks/throttled-stop.sh", "timeout": 60 }] },
                { "hooks": [{ "type": "command", "command": custom[0], "timeout": 10 }] },
                { "hooks": [{ "type": "command", "command": custom[1], "timeout": 10 }] },
                { "hooks": [{ "type": "command", "command": custom[2], "timeout": 10 }] }
            ]
        }
    }));

    let output = fixture.run(&["hooks", "install"], b"");
    assert_success(&output, "hooks install");
    let settings = fixture.read_settings();
    let commands = hook_commands(&settings, "Stop");
    assert_eq!(commands.len(), 4);
    assert!(commands
        .iter()
        .any(|command| command.ends_with(" hook-stop")));
    assert!(!commands
        .iter()
        .any(|command| command.contains("throttled-stop")));
    for expected in custom {
        assert!(commands.iter().any(|command| command == expected));
    }
    assert_eq!(settings["hooks"]["Stop"][0]["hooks"][0]["timeout"], 10);
}

#[test]
#[serial]
fn hooks_check_quiet_tracks_install_and_field_drift() {
    let fixture = Fixture::new();
    let missing = fixture.run(&["hooks", "check", "--quiet"], b"");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("hook 配置与 v"));

    assert_success(&fixture.run(&["hooks", "install"], b""), "hooks install");
    assert_success(
        &fixture.run(&["hooks", "check", "--quiet"], b""),
        "hooks check after install",
    );

    let mut settings = fixture.read_settings();
    settings["hooks"]["Stop"][0]["hooks"][0]["timeout"] = json!(60);
    fixture.write_settings(&settings);
    let drifted = fixture.run(&["hooks", "check", "--quiet"], b"");
    assert!(!drifted.status.success());
}

#[test]
#[serial]
fn uninstall_removes_only_ccs_hooks() {
    let fixture = Fixture::new();
    fixture.write_settings(&json!({
        "hooks": {
            "Stop": [{ "hooks": [{ "type": "command", "command": "python notify-stop.py" }] }],
            "PreToolUse": [{ "hooks": [{ "type": "command", "command": "audit.sh" }] }]
        }
    }));
    assert_success(&fixture.run(&["hooks", "install"], b""), "hooks install");
    assert_success(
        &fixture.run(&["hooks", "uninstall"], b""),
        "hooks uninstall",
    );

    let settings = fixture.read_settings();
    assert_eq!(
        hook_commands(&settings, "Stop"),
        vec!["python notify-stop.py"]
    );
    assert!(hook_commands(&settings, "SessionStart").is_empty());
    assert!(hook_commands(&settings, "UserPromptSubmit").is_empty());
    assert_eq!(hook_commands(&settings, "PreToolUse"), vec!["audit.sh"]);
}

#[test]
#[serial]
fn hook_stop_returns_quickly_and_worker_commits() {
    let fixture = Fixture::new();
    let initial_commits = fixture.commit_count();
    let (output, elapsed) = fixture.run_stop();
    assert_success(&output, "hook-stop foreground");
    assert!(
        elapsed < Duration::from_secs(2),
        "foreground took {elapsed:?}"
    );

    fixture.wait_for_stamp();
    fixture.wait_until("worker commit", || fixture.commit_count() > initial_commits);
    let state: Value = serde_json::from_slice(
        &fs::read(fixture.config.path().join("push-hook-state.json")).expect("hook state"),
    )
    .expect("parse hook state");
    assert_eq!(state["consecutive_failures"], 0);
    assert!(fixture
        .debug_log()
        .contains("Stop worker completed successfully"));
}

#[test]
#[serial]
fn fresh_stamp_throttles_without_new_commit() {
    let fixture = Fixture::new();
    let (first, _) = fixture.run_stop();
    assert_success(&first, "first hook-stop");
    fixture.wait_for_stamp();
    let commits = fixture.commit_count();
    fixture.append_session_message("must wait for next throttle window");

    let (second, _) = fixture.run_stop();
    assert_success(&second, "throttled hook-stop");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fixture.commit_count(), commits);
    assert!(fixture
        .debug_log()
        .contains("Stop hook skipped: throttle active"));
}

#[cfg(unix)]
#[test]
#[serial]
fn foreground_stop_does_not_wait_for_held_log_lock() {
    let fixture = Fixture::new();
    let log_lock = fixture.config.path().join("claude-code-sync.log.lock");
    let holder = PythonLockHolder::hold(&log_lock);

    let started = Instant::now();
    let output = run_with_timeout(
        fixture.command(&["hook-stop"]),
        b"{}\n",
        Duration::from_secs(1),
    );
    assert_success(&output, "hook-stop with held log lock");
    assert!(started.elapsed() < Duration::from_secs(1));

    drop(holder);
    fixture.wait_for_stamp();
}

#[test]
#[serial]
fn colliding_user_commands_survive_install_check_and_uninstall() {
    let fixture = Fixture::new();
    let collisions = [
        "python my-throttled-stop-notify.py",
        "/usr/local/bin/ccs-monitor hook-stop",
    ];
    fixture.write_settings(&json!({
        "hooks": {
            "Stop": [
                { "hooks": [{ "type": "command", "command": collisions[0], "timeout": 10 }] },
                { "hooks": [{ "type": "command", "command": collisions[1], "timeout": 10 }] }
            ]
        }
    }));

    let before = fixture.run(&["hooks", "check", "--quiet"], b"");
    assert!(!before.status.success());
    assert_success(&fixture.run(&["hooks", "install"], b""), "hooks install");
    assert_success(
        &fixture.run(&["hooks", "check", "--quiet"], b""),
        "hooks check",
    );
    let installed = hook_commands(&fixture.read_settings(), "Stop");
    assert_eq!(installed.len(), 3);
    for collision in collisions {
        assert!(installed.iter().any(|command| command == collision));
    }

    assert_success(
        &fixture.run(&["hooks", "uninstall"], b""),
        "hooks uninstall",
    );
    let remaining = hook_commands(&fixture.read_settings(), "Stop");
    assert_eq!(remaining, collisions);
}

#[test]
#[serial]
fn held_worker_lock_prevents_frontend_alert_state_write() {
    let fixture = Fixture::new();
    let state = json!({
        "consecutive_failures": 3,
        "alerted_failures": 0,
        "last_success_unix": null,
        "last_failure_unix": 123,
        "last_error": "boom"
    });
    let state_path = fixture.config.path().join("push-hook-state.json");
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let lock_path = fixture.config.path().join("push-hook.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open hook lock");
    FileExt::lock(&lock).expect("hold hook lock");

    let (output, _) = fixture.run_stop();
    assert_success(&output, "frontend alert check with held worker lock");
    fixture.wait_until("frontend alert lock skip", || {
        fixture
            .debug_log()
            .contains("Stop alert check skipped: worker holds push-hook.lock")
    });
    fixture.wait_until("worker lock skip", || {
        fixture
            .debug_log()
            .contains("another worker holds push-hook.lock")
    });
    let unchanged: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(unchanged, state);
    FileExt::unlock(&lock).expect("unlock hook lock");
}

#[test]
#[serial]
fn held_push_hook_lock_makes_worker_skip() {
    let fixture = Fixture::new();
    let lock_path = fixture.config.path().join("push-hook.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open hook lock");
    FileExt::lock(&lock).expect("hold hook lock");
    let commits = fixture.commit_count();

    let (output, _) = fixture.run_stop();
    assert_success(&output, "hook-stop with held lock");
    fixture.wait_until("worker lock skip", || {
        fixture
            .debug_log()
            .contains("another worker holds push-hook.lock")
    });
    assert_eq!(fixture.commit_count(), commits);
    assert!(!fixture.config.path().join("push-hook.stamp").exists());
    FileExt::unlock(&lock).expect("unlock hook lock");
}

#[test]
#[serial]
fn repeated_worker_failures_alert_once() {
    let fixture = Fixture::new();
    fixture.write_state(&fixture.home.path().join("missing-repo"));

    for expected in 1..=3 {
        let (output, _) = fixture.run_stop();
        assert_success(&output, "failing worker foreground");
        fixture.wait_for_failures(expected);
    }

    let (alert, _) = fixture.run_stop();
    assert!(!alert.status.success());
    assert!(String::from_utf8_lossy(&alert.stderr).contains("连续失败 3 次"));

    let (deduplicated, _) = fixture.run_stop();
    assert_success(&deduplicated, "deduplicated alert invocation");
}
