#![cfg(debug_assertions)]

use serde_json::{json, Value};
use serial_test::serial;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

struct Fixture {
    home: TempDir,
    config: TempDir,
    signals: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home tempdir"),
            config: tempfile::tempdir().expect("config tempdir"),
            signals: tempfile::tempdir().expect("signals tempdir"),
        }
    }

    fn source_root(&self) -> PathBuf {
        self.home.path().join(".claude/projects")
    }

    fn project_dir(&self) -> PathBuf {
        self.source_root().join("-tmp-maintenance-concurrency")
    }

    fn source_relative_path(&self, id: &str) -> PathBuf {
        PathBuf::from("-tmp-maintenance-concurrency").join(format!("{id}.jsonl"))
    }

    fn source_path(&self, id: &str) -> PathBuf {
        self.source_root().join(self.source_relative_path(id))
    }

    fn write_session(&self, id: &str, title: &str) -> String {
        let content = format!(
            concat!(
                "{{\"type\":\"user\",\"sessionId\":\"{}\",\"cwd\":\"/tmp/maintenance-concurrency\",",
                "\"timestamp\":\"2026-07-01T00:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}\n",
                "{{\"type\":\"assistant\",\"sessionId\":\"{}\",\"cwd\":\"/tmp/maintenance-concurrency\",",
                "\"timestamp\":\"2026-07-01T00:00:01Z\",\"message\":{{\"role\":\"assistant\",\"content\":\"answer\"}}}}\n"
            ),
            id, title, id
        );
        fs::create_dir_all(self.project_dir()).expect("project dir");
        fs::write(self.source_path(id), &content).expect("session fixture");
        content
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccs"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("CLAUDE_CODE_SYNC_CONFIG_DIR", self.config.path())
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run ccs")
    }

    fn state_path(&self) -> PathBuf {
        self.config.path().join("session-maintenance.json")
    }

    fn read_state(&self) -> Value {
        let bytes = fs::read(self.state_path()).expect("read maintenance state");
        serde_json::from_slice(&bytes).expect("valid maintenance JSON")
    }

    fn write_hidden_state(&self, id: &str, content: &str) {
        let fingerprint = blake3::hash(content.as_bytes()).to_hex().to_string();
        let state = json!({
            "version": 1,
            "entries": {
                format!("claude:{id}"): {
                    "identity": {"source": "claude", "session_id": id},
                    "original_relative_path": self.source_relative_path(id),
                    "project_name": "maintenance-concurrency",
                    "fingerprint": fingerprint,
                    "lifecycle": "hidden",
                    "classifier_version": 1,
                    "score": 100,
                    "reason_codes": ["explicit_test_marker"],
                    "hidden_since": "2026-07-01T00:00:00Z",
                    "recycled_at": null,
                    "purged_at": null,
                    "keep": false,
                    "explicit_test": true
                }
            },
            "pending": null
        });
        fs::write(
            self.state_path(),
            serde_json::to_vec_pretty(&state).expect("serialize state"),
        )
        .expect("write hidden state");
    }

    fn recycle_path(&self, id: &str, fingerprint: &str) -> PathBuf {
        self.config
            .path()
            .join("session-recycle/claude")
            .join(format!("id-{}", blake3::hash(id.as_bytes()).to_hex()))
            .join(format!("{fingerprint}.jsonl"))
    }
}

struct ChildGuard {
    child: Option<Child>,
    label: String,
}

impl ChildGuard {
    fn spawn(mut command: Command, label: &str) -> Self {
        let child = command.spawn().unwrap_or_else(|error| {
            panic!("spawn {label}: {error}");
        });
        Self {
            child: Some(child),
            label: label.to_string(),
        }
    }

    fn is_running(&mut self) -> bool {
        self.child
            .as_mut()
            .expect("child available")
            .try_wait()
            .expect("poll child")
            .is_none()
    }

    fn wait_for_marker(&mut self, marker: &Path) {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        while !marker.exists() {
            assert!(
                self.is_running(),
                "{} exited before marker {}",
                self.label,
                marker.display()
            );
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {} marker {}",
                self.label,
                marker.display()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_running_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            assert!(self.is_running(), "{} exited unexpectedly", self.label);
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_output(mut self) -> Output {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        loop {
            if !self.is_running() {
                let child = self.child.take().expect("child available");
                return child.wait_with_output().expect("collect child output");
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                self.label
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed: status={}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn release(marker: &Path) {
    fs::write(marker, b"release").expect("write release marker");
}

#[test]
#[serial]
fn dual_writer_state_transactions_reload_after_lock_and_preserve_both_entries() {
    let fixture = Fixture::new();
    fixture.write_session("writer-a", "writer a");
    fixture.write_session("writer-b", "writer b");

    let ready = fixture.signals.path().join("lock.ready");
    let release_marker = fixture.signals.path().join("lock.release");
    let mut first_command =
        fixture.command(&["session", "mark-test", "writer-a", "--source", "claude"]);
    first_command
        .env("CCS_TEST_MAINTENANCE_LOCK_READY", &ready)
        .env("CCS_TEST_MAINTENANCE_LOCK_RELEASE", &release_marker);
    let mut first = ChildGuard::spawn(first_command, "first maintenance writer");
    first.wait_for_marker(&ready);

    let second_command =
        fixture.command(&["session", "mark-test", "writer-b", "--source", "claude"]);
    let mut second = ChildGuard::spawn(second_command, "second maintenance writer");
    second.assert_running_for(Duration::from_millis(300));

    release(&release_marker);
    let first_output = first.wait_output();
    let second_output = second.wait_output();
    assert_success(&first_output, "first writer");
    assert_success(&second_output, "second writer");

    let state = fixture.read_state();
    assert_eq!(state["version"], 1);
    assert!(state["pending"].is_null());
    for id in ["writer-a", "writer-b"] {
        let entry = &state["entries"][format!("claude:{id}")];
        assert_eq!(entry["identity"]["source"], "claude");
        assert_eq!(entry["identity"]["session_id"], id);
        assert_eq!(entry["explicit_test"], true);
    }
}

#[test]
#[serial]
fn restore_racing_recycle_never_loses_both_durable_copies() {
    let fixture = Fixture::new();
    let id = "restore-recycle-race";
    let content = fixture.write_session(id, "test");
    let fingerprint = blake3::hash(content.as_bytes()).to_hex().to_string();

    let enable = fixture.run(&["session", "maintain", "--enable"]);
    assert_success(&enable, "enable maintenance");
    fixture.write_hidden_state(id, &content);

    let ready = fixture.signals.path().join("pending.ready");
    let release_marker = fixture.signals.path().join("pending.release");
    let mut maintenance_command = fixture.command(&["session", "maintain", "--run"]);
    maintenance_command
        .env("CCS_TEST_MAINTENANCE_AFTER_PENDING_READY", &ready)
        .env(
            "CCS_TEST_MAINTENANCE_AFTER_PENDING_RELEASE",
            &release_marker,
        )
        .env("CCS_TEST_MAINTENANCE_FORCE_COPY", "1");
    let mut maintenance = ChildGuard::spawn(maintenance_command, "maintenance recycle");
    maintenance.wait_for_marker(&ready);

    let restore_command = fixture.command(&["session", "restore", id, "--source", "claude"]);
    let mut restore = ChildGuard::spawn(restore_command, "racing restore");
    restore.assert_running_for(Duration::from_millis(300));

    release(&release_marker);
    let maintenance_output = maintenance.wait_output();
    let restore_output = restore.wait_output();
    assert_success(&maintenance_output, "maintenance recycle");

    let state = fixture.read_state();
    assert!(state["pending"].is_null(), "pending journal must settle");
    let entry = &state["entries"][format!("claude:{id}")];
    let source_exists = fixture.source_path(id).is_file();
    let recycle_exists = fixture.recycle_path(id, &fingerprint).is_file();

    match entry["lifecycle"].as_str().expect("lifecycle") {
        "visible" => {
            assert!(
                restore_output.status.success(),
                "visible result requires successful restore: {}",
                String::from_utf8_lossy(&restore_output.stderr)
            );
            assert_eq!(entry["keep"], true);
            assert!(source_exists, "visible result requires active source");
            assert!(!recycle_exists, "visible result must consume recycle copy");
        }
        "recycled" => {
            assert!(!source_exists, "recycled result must not retain source");
            assert!(
                recycle_exists,
                "recycled result requires durable recycle copy"
            );
        }
        other => panic!("unexpected final lifecycle {other}"),
    }
    assert!(
        source_exists || recycle_exists,
        "forbidden result: both source and recycle copies are missing"
    );
}

#[test]
#[serial]
fn atomic_state_reader_never_observes_partial_json_during_live_cli_writes() {
    let fixture = Fixture::new();
    let id = "atomic-reader";
    fixture.write_session(id, "atomic reader");
    let initial = fixture.run(&["session", "mark-test", id, "--source", "claude"]);
    assert_success(&initial, "initial marker");

    let ready = fixture.signals.path().join("writer.ready");
    let release_marker = fixture.signals.path().join("writer.release");
    let count = fixture.signals.path().join("writer.count");
    let current_exe = std::env::current_exe().expect("test executable");
    let mut command = Command::new(current_exe);
    command
        .args([
            "--ignored",
            "--exact",
            "child_toggles_maintenance_markers",
            "--nocapture",
        ])
        .env("CCS_TEST_MAINTENANCE_WRITER_CHILD", "1")
        .env("CCS_TEST_MAINTENANCE_WRITER_HOME", fixture.home.path())
        .env("CCS_TEST_MAINTENANCE_WRITER_CONFIG", fixture.config.path())
        .env("CCS_TEST_MAINTENANCE_WRITER_ID", id)
        .env("CCS_TEST_MAINTENANCE_WRITER_READY", &ready)
        .env("CCS_TEST_MAINTENANCE_WRITER_RELEASE", &release_marker)
        .env("CCS_TEST_MAINTENANCE_WRITER_COUNT", &count)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut writer = ChildGuard::spawn(command, "atomic state writer");
    writer.wait_for_marker(&ready);

    let mut observed = HashSet::new();
    for _ in 0..200 {
        assert!(writer.is_running(), "writer exited during reader stress");
        let bytes = fs::read(fixture.state_path()).expect("state exists during stress");
        let state: Value = serde_json::from_slice(&bytes).expect("atomic state JSON");
        assert_eq!(state["version"], 1);
        assert!(state["pending"].is_null());
        let entry = &state["entries"][format!("claude:{id}")];
        observed.insert((entry["explicit_test"].as_bool(), entry["keep"].as_bool()));
        thread::sleep(Duration::from_millis(2));
    }

    release(&release_marker);
    let output = writer.wait_output();
    assert_success(&output, "atomic state writer");
    let mutations: usize = fs::read_to_string(count)
        .expect("writer count")
        .parse()
        .expect("numeric writer count");
    assert!(
        mutations >= 8,
        "writer performed only {mutations} mutations"
    );
    assert!(
        observed.len() >= 2,
        "reader did not overlap distinct committed states: {observed:?}"
    );
    let _: Value = serde_json::from_slice(&fs::read(fixture.state_path()).expect("final state"))
        .expect("final state JSON");
}

#[test]
#[ignore = "child process helper"]
fn child_toggles_maintenance_markers() {
    if std::env::var_os("CCS_TEST_MAINTENANCE_WRITER_CHILD").is_none() {
        return;
    }
    let home =
        PathBuf::from(std::env::var_os("CCS_TEST_MAINTENANCE_WRITER_HOME").expect("writer home"));
    let config = PathBuf::from(
        std::env::var_os("CCS_TEST_MAINTENANCE_WRITER_CONFIG").expect("writer config"),
    );
    let id = std::env::var("CCS_TEST_MAINTENANCE_WRITER_ID").expect("writer id");
    let ready =
        PathBuf::from(std::env::var_os("CCS_TEST_MAINTENANCE_WRITER_READY").expect("writer ready"));
    let release_marker = PathBuf::from(
        std::env::var_os("CCS_TEST_MAINTENANCE_WRITER_RELEASE").expect("writer release"),
    );
    let count_path =
        PathBuf::from(std::env::var_os("CCS_TEST_MAINTENANCE_WRITER_COUNT").expect("writer count"));

    let mut mutations = 0usize;
    while !release_marker.exists() {
        for action in ["mark-test", "unmark-test", "keep", "unkeep"] {
            let output = Command::new(env!("CARGO_BIN_EXE_ccs"))
                .args(["session", action, &id, "--source", "claude"])
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("CLAUDE_CODE_SYNC_CONFIG_DIR", &config)
                .env_remove("RUST_LOG")
                .output()
                .expect("run writer mutation");
            assert_success(&output, action);
            mutations += 1;
            fs::write(&count_path, mutations.to_string()).expect("write mutation count");
        }
        if !ready.exists() {
            fs::write(&ready, b"ready").expect("write writer ready");
        }
    }
}
