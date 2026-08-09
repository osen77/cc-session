use serde_json::Value;
use serial_test::serial;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Barrier,
};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

use claude_code_sync::session_cache::{CachedEntry, SessionIndexCache};

struct Fixture {
    home: TempDir,
    config: TempDir,
    logs: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            home: tempfile::tempdir().expect("home tempdir"),
            config: tempfile::tempdir().expect("config tempdir"),
            logs: tempfile::tempdir().expect("log tempdir"),
        };
        fixture.write_sessions();
        fixture
    }

    fn write_sessions(&self) {
        let claude_project = self.home.path().join(".claude/projects/-tmp-cache-task4");
        fs::create_dir_all(&claude_project).expect("Claude project root");
        fs::write(
            claude_project.join("claude.jsonl"),
            concat!(
                r#"{"type":"user","sessionId":"cc-cache-task4","cwd":"/tmp/cache-task4","timestamp":"2026-08-02T00:00:00Z","message":{"role":"user","content":"Claude cache entry"}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"cc-cache-task4","cwd":"/tmp/cache-task4","timestamp":"2026-08-02T00:00:01Z","message":{"role":"assistant","content":"answer"}}"#,
                "\n"
            ),
        )
        .expect("Claude session");

        let codex_root = self.home.path().join(".codex/sessions/2026");
        fs::create_dir_all(&codex_root).expect("Codex root");
        fs::write(
            codex_root.join("codex.jsonl"),
            concat!(
                r#"{"timestamp":"2026-08-02T00:00:00Z","type":"session_meta","payload":{"id":"cx-cache-task4","cwd":"/tmp/cache-task4"}}"#,
                "\n",
                r#"{"timestamp":"2026-08-02T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Codex cache entry"}]}}"#,
                "\n"
            ),
        )
        .expect("Codex session");
        fs::write(
            self.home.path().join(".codex/history.jsonl"),
            r#"{"session_id":"cx-cache-task4","ts":1,"text":"Codex cache entry"}"#,
        )
        .expect("Codex history");

        let omp_root = self.home.path().join(".omp/agent/sessions");
        fs::create_dir_all(&omp_root).expect("OMP root");
        fs::write(
            omp_root.join("2026-08-02T00-00-00Z_om-cache-task4.jsonl"),
            concat!(
                r#"{"type":"session","version":3,"id":"om-cache-task4","timestamp":"2026-08-02T00:00:00Z","cwd":"/tmp/cache-task4","title":"OMP cache entry"}"#,
                "\n",
                r#"{"type":"message","timestamp":"2026-08-02T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"OMP cache entry"}]}}"#,
                "\n"
            ),
        )
        .expect("OMP session");
    }

    fn cache_path(&self) -> PathBuf {
        self.config.path().join("session_index.json")
    }

    fn command(&self, label: &str, source: &str) -> Command {
        let log_path = self.logs.path().join(format!("{label}.log"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_ccs"));
        command
            .args([
                "--log-file",
                log_path.to_str().expect("log path"),
                "session",
                "overview",
                "--json",
                "--source",
                source,
            ])
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("CLAUDE_CODE_SYNC_CONFIG_DIR", self.config.path())
            .env_remove("RUST_LOG");
        command
    }

    fn run(&self, label: &str, source: &str) -> (Output, PathBuf) {
        let log_path = self.logs.path().join(format!("{label}.log"));
        let output = self.command(label, source).output().expect("run ccs");
        (output, log_path)
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "ccs failed: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cache_sources(path: &Path) -> Vec<String> {
    let bytes = fs::read(path).expect("read cache JSON");
    let payload: Value = serde_json::from_slice(&bytes).expect("cache must be valid JSON");
    payload["entries"]
        .as_object()
        .expect("cache entries object")
        .values()
        .map(|entry| entry["source"].as_str().expect("cache source").to_string())
        .collect()
}

#[test]
#[serial]
fn source_filtered_cli_retention_keeps_unselected_sources_after_sequential_and_concurrent_scans() {
    let fixture = Fixture::new();

    let (all, _) = fixture.run("retention-all", "all");
    assert_success(&all);
    assert_eq!(cache_sources(&fixture.cache_path()).len(), 3);

    let claude_file = fixture
        .home
        .path()
        .join(".claude/projects/-tmp-cache-task4/claude.jsonl");
    fs::remove_file(&claude_file).expect("remove Claude session");

    let (claude_only, _) = fixture.run("retention-claude", "claude");
    assert_success(&claude_only);
    let after_claude = cache_sources(&fixture.cache_path());
    assert!(!after_claude.iter().any(|source| source == "claude"));
    assert!(after_claude.iter().any(|source| source == "codex"));
    assert!(after_claude.iter().any(|source| source == "omp"));

    let (codex_only, _) = fixture.run("retention-codex", "codex");
    assert_success(&codex_only);
    let after_codex = cache_sources(&fixture.cache_path());
    assert!(after_codex.iter().any(|source| source == "codex"));
    assert!(after_codex.iter().any(|source| source == "omp"));

    let home = fixture.home.path().to_path_buf();
    let config = fixture.config.path().to_path_buf();
    let logs = fixture.logs.path().to_path_buf();
    let claude_gate = merge_gate(&logs, "retention-concurrent-claude");
    let codex_gate = merge_gate(&logs, "retention-concurrent-codex");
    let barrier = Arc::new(Barrier::new(3));
    let claude_barrier = Arc::clone(&barrier);
    let claude_home = home.clone();
    let claude_config = config.clone();
    let claude_logs = logs.clone();
    let claude_gate_for_thread = claude_gate.clone();
    let claude = thread::spawn(move || {
        claude_barrier.wait();
        spawn_child_scan(
            &claude_home,
            &claude_config,
            &claude_logs,
            "retention-concurrent-claude",
            "claude",
            Some(&claude_gate_for_thread),
            MergeTestBehavior::default(),
        )
    });
    let codex_barrier = Arc::clone(&barrier);
    let codex_gate_for_thread = codex_gate.clone();
    let codex = thread::spawn(move || {
        codex_barrier.wait();
        spawn_child_scan(
            &home,
            &config,
            &logs,
            "retention-concurrent-codex",
            "codex",
            Some(&codex_gate_for_thread),
            MergeTestBehavior::default(),
        )
    });
    barrier.wait();
    let gated = run_gated_children(
        join_spawn_thread(claude.join(), "Claude retention"),
        claude_gate,
        "Claude retention child",
        join_spawn_thread(codex.join(), "Codex retention"),
        codex_gate,
        "Codex retention child",
    )
    .unwrap_or_else(|failure| panic!("gated retention scan failed: {failure:?}"));
    assert!(gated.first_status.success());
    assert!(gated.second_status.success());

    let final_sources = cache_sources(&fixture.cache_path());
    assert!(final_sources.iter().any(|source| source == "codex"));
    assert!(final_sources.iter().any(|source| source == "omp"));
    assert!(!final_sources.iter().any(|source| source == "claude"));
    assert_cache_has_session(&fixture.cache_path(), "codex", "cx-cache-task4");
    assert_cache_has_session(&fixture.cache_path(), "omp", "om-cache-task4");
    assert!(!cache_has_session(
        &fixture.cache_path(),
        "claude",
        "cc-cache-task4"
    ));
}

#[derive(Clone)]
struct MergeGate {
    ready: PathBuf,
    release: PathBuf,
}

#[derive(Clone, Copy, Default)]
struct MergeTestBehavior {
    fail_after_release: bool,
    hold_after_release: bool,
}

fn merge_gate(logs: &Path, label: &str) -> MergeGate {
    MergeGate {
        ready: logs.join(format!("{label}.ready")),
        release: logs.join(format!("{label}.release")),
    }
}

fn spawn_child_scan(
    home: &Path,
    config: &Path,
    logs: &Path,
    label: &str,
    source: &str,
    merge_gate: Option<&MergeGate>,
    behavior: MergeTestBehavior,
) -> std::io::Result<Child> {
    let log_path = logs.join(format!("{label}.log"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_ccs"));
    command
        .args([
            "--log-file",
            log_path.to_str().expect("log path"),
            "session",
            "overview",
            "--json",
            "--source",
            source,
        ])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CLAUDE_CODE_SYNC_CONFIG_DIR", config)
        .env_remove("RUST_LOG");
    if let Some(gate) = merge_gate {
        command
            .env("CCS_TEST_SESSION_CACHE_MERGE_READY", &gate.ready)
            .env("CCS_TEST_SESSION_CACHE_MERGE_RELEASE", &gate.release);
    }
    if behavior.fail_after_release {
        command.env("CCS_TEST_SESSION_CACHE_MERGE_FAIL", "1");
    }
    if behavior.hold_after_release {
        command
            .env("CCS_TEST_SESSION_CACHE_HOLD_AFTER_RELEASE", "1")
            .env(
                "CCS_TEST_SESSION_CACHE_HOLD_RELEASE",
                logs.join(format!("{label}.hold.release")),
            );
    }
    command.spawn()
}

fn join_spawn_thread(
    result: thread::Result<std::io::Result<Child>>,
    label: &str,
) -> std::io::Result<Child> {
    result.unwrap_or_else(|_| {
        Err(std::io::Error::other(format!(
            "{label} spawn thread panicked"
        )))
    })
}

#[derive(Debug)]
struct GatedSuccess {
    first_status: ExitStatus,
    second_status: ExitStatus,
}

#[derive(Debug)]
struct GatedFailure {
    message: String,
    reaped_children: usize,
    killed_children: usize,
}

struct GatedChildren {
    first: Option<Child>,
    second: Option<Child>,
    first_gate: MergeGate,
    second_gate: MergeGate,
    first_label: String,
    second_label: String,
}

impl GatedChildren {
    fn from_spawn_results(
        first: std::io::Result<Child>,
        first_gate: MergeGate,
        first_label: &str,
        second: std::io::Result<Child>,
        second_gate: MergeGate,
        second_label: &str,
    ) -> Result<Self, GatedFailure> {
        match (first, second) {
            (Ok(first), Ok(second)) => Ok(Self {
                first: Some(first),
                second: Some(second),
                first_gate,
                second_gate,
                first_label: first_label.to_string(),
                second_label: second_label.to_string(),
            }),
            (Err(first_error), Ok(second)) => {
                let mut second_slot = Some(second);
                let cleanup = cleanup_child(&mut second_slot);
                Err(GatedFailure {
                    message: format!("{first_label} spawn failed: {first_error}"),
                    reaped_children: usize::from(cleanup.reaped),
                    killed_children: usize::from(cleanup.killed),
                })
            }
            (Ok(first), Err(second_error)) => {
                let mut first_slot = Some(first);
                let cleanup = cleanup_child(&mut first_slot);
                Err(GatedFailure {
                    message: format!("{second_label} spawn failed: {second_error}"),
                    reaped_children: usize::from(cleanup.reaped),
                    killed_children: usize::from(cleanup.killed),
                })
            }
            (Err(first_error), Err(second_error)) => Err(GatedFailure {
                message: format!(
                    "{first_label} spawn failed: {first_error}; {second_label} spawn failed: {second_error}"
                ),
                reaped_children: 0,
                killed_children: 0,
            }),
        }
    }

    fn run(mut self) -> Result<GatedSuccess, GatedFailure> {
        let first_gate = self.first_gate.clone();
        let first_label = self.first_label.clone();
        if let Err(error) = self.wait_ready(&first_gate, &first_label, true) {
            return Err(self.fail(error, 0));
        }
        let second_gate = self.second_gate.clone();
        let second_label = self.second_label.clone();
        if let Err(error) = self.wait_ready(&second_gate, &second_label, false) {
            return Err(self.fail(error, 0));
        }
        if let Err(error) = fs::write(&self.first_gate.release, b"release") {
            return Err(self.fail(
                format!("{} release marker write failed: {error}", self.first_label),
                0,
            ));
        }
        if let Err(error) = fs::write(&self.second_gate.release, b"release") {
            return Err(self.fail(
                format!("{} release marker write failed: {error}", self.second_label),
                0,
            ));
        }

        let first_status = match wait_child(&mut self.first, &self.first_label) {
            Ok(status) => status,
            Err(error) => return Err(self.fail(error, 0)),
        };
        if !first_status.success() {
            return Err(self.fail(
                format!("{} exited unsuccessfully: {first_status}", self.first_label),
                1,
            ));
        }

        let second_status = match wait_child(&mut self.second, &self.second_label) {
            Ok(status) => status,
            Err(error) => return Err(self.fail(error, 1)),
        };
        if !second_status.success() {
            return Err(self.fail(
                format!(
                    "{} exited unsuccessfully: {second_status}",
                    self.second_label
                ),
                2,
            ));
        }

        Ok(GatedSuccess {
            first_status,
            second_status,
        })
    }

    fn wait_ready(&mut self, gate: &MergeGate, label: &str, first: bool) -> Result<(), String> {
        let child = if first {
            self.first.as_mut()
        } else {
            self.second.as_mut()
        }
        .ok_or_else(|| format!("{label} child was already reaped"))?;
        wait_for_merge_ready_marker(child, &gate.ready, label)
    }

    fn fail(&mut self, message: String, reaped_before: usize) -> GatedFailure {
        let first_cleanup = cleanup_child(&mut self.first);
        let second_cleanup = cleanup_child(&mut self.second);
        GatedFailure {
            message,
            reaped_children: reaped_before
                + usize::from(first_cleanup.reaped)
                + usize::from(second_cleanup.reaped),
            killed_children: usize::from(first_cleanup.killed) + usize::from(second_cleanup.killed),
        }
    }
}

impl Drop for GatedChildren {
    fn drop(&mut self) {
        let _ = cleanup_child(&mut self.first);
        let _ = cleanup_child(&mut self.second);
    }
}

fn run_gated_children(
    first: std::io::Result<Child>,
    first_gate: MergeGate,
    first_label: &str,
    second: std::io::Result<Child>,
    second_gate: MergeGate,
    second_label: &str,
) -> Result<GatedSuccess, GatedFailure> {
    GatedChildren::from_spawn_results(
        first,
        first_gate,
        first_label,
        second,
        second_gate,
        second_label,
    )?
    .run()
}

fn wait_child(child: &mut Option<Child>, label: &str) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let process = child
            .as_mut()
            .ok_or_else(|| format!("{label} child was already reaped"))?;
        match process
            .try_wait()
            .map_err(|error| format!("poll {label} failed: {error}"))?
        {
            Some(status) => {
                *child = None;
                return Ok(status);
            }
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            None => {
                let _ = process.kill();
                let _ = process.wait();
                *child = None;
                return Err(format!("timed out waiting for {label}"));
            }
        }
    }
}

#[derive(Debug, Default)]
struct CleanupResult {
    reaped: bool,
    killed: bool,
}

fn cleanup_child(child: &mut Option<Child>) -> CleanupResult {
    let Some(process) = child.as_mut() else {
        return CleanupResult::default();
    };
    let mut result = CleanupResult::default();
    match process.try_wait() {
        Ok(Some(_)) => {
            result.reaped = true;
        }
        Ok(None) | Err(_) => {
            result.killed = process.kill().is_ok();
            result.reaped = process.wait().is_ok();
        }
    }
    *child = None;
    result
}

fn wait_for_merge_ready_marker(
    child: &mut Child,
    marker: &Path,
    label: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if marker.exists() {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("poll {label}: {error}"))?
        {
            return Err(format!(
                "{label} exited before merge ready marker: {status}"
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for {label} merge ready marker: {}",
                marker.display()
            ));
        }
        thread::yield_now();
    }
}

fn cache_has_session(path: &Path, source: &str, session_id: &str) -> bool {
    let cache: SessionIndexCache =
        serde_json::from_slice(&fs::read(path).expect("read cache")).expect("parse cache");
    cache
        .entries
        .values()
        .any(|entry| entry.source == source && entry.session_id == session_id)
}

fn assert_cache_has_session(path: &Path, source: &str, session_id: &str) {
    assert!(
        cache_has_session(path, source, session_id),
        "cache does not contain {source}/{session_id}"
    );
}

#[test]
#[serial]
fn cross_process_source_writers_merge_complete_json_without_lost_updates() {
    let fixture = Fixture::new();
    let barrier = Arc::new(Barrier::new(3));
    let home = fixture.home.path().to_path_buf();
    let config = fixture.config.path().to_path_buf();
    let logs = fixture.logs.path().to_path_buf();
    let claude_gate = merge_gate(&logs, "writers-claude");
    let codex_gate = merge_gate(&logs, "writers-codex");

    let claude_barrier = Arc::clone(&barrier);
    let claude_home = home.clone();
    let claude_config = config.clone();
    let claude_logs = logs.clone();
    let claude_gate_for_thread = claude_gate.clone();
    let claude = thread::spawn(move || {
        claude_barrier.wait();
        spawn_child_scan(
            &claude_home,
            &claude_config,
            &claude_logs,
            "writers-claude",
            "claude",
            Some(&claude_gate_for_thread),
            MergeTestBehavior::default(),
        )
    });
    let codex_barrier = Arc::clone(&barrier);
    let codex_gate_for_thread = codex_gate.clone();
    let codex = thread::spawn(move || {
        codex_barrier.wait();
        spawn_child_scan(
            &home,
            &config,
            &logs,
            "writers-codex",
            "codex",
            Some(&codex_gate_for_thread),
            MergeTestBehavior::default(),
        )
    });
    barrier.wait();

    let gated = run_gated_children(
        join_spawn_thread(claude.join(), "Claude writer"),
        claude_gate,
        "Claude writer",
        join_spawn_thread(codex.join(), "Codex writer"),
        codex_gate,
        "Codex writer",
    )
    .unwrap_or_else(|failure| panic!("gated dual writer failed: {failure:?}"));
    assert!(gated.first_status.success());
    assert!(gated.second_status.success());

    let bytes = fs::read(fixture.cache_path()).expect("read final cache");
    let cache: SessionIndexCache = serde_json::from_slice(&bytes).expect("final cache JSON");
    let sources: Vec<&str> = cache
        .entries
        .values()
        .map(|entry| entry.source.as_str())
        .collect();
    assert!(
        sources.contains(&"claude"),
        "Claude entry was lost: {sources:?}"
    );
    assert!(
        sources.contains(&"codex"),
        "Codex entry was lost: {sources:?}"
    );
    assert_cache_has_session(&fixture.cache_path(), "claude", "cc-cache-task4");
    assert_cache_has_session(&fixture.cache_path(), "codex", "cx-cache-task4");
}

#[test]
#[serial]
fn gated_child_failure_reaps_peer_without_orphan() {
    let fixture = Fixture::new();
    let home = fixture.home.path().to_path_buf();
    let config = fixture.config.path().to_path_buf();
    let logs = fixture.logs.path().to_path_buf();
    let failing_gate = merge_gate(&logs, "failure-claude");
    let holding_gate = merge_gate(&logs, "failure-codex");
    let barrier = Arc::new(Barrier::new(3));

    let failing_barrier = Arc::clone(&barrier);
    let failing_home = home.clone();
    let failing_config = config.clone();
    let failing_logs = logs.clone();
    let failing_gate_for_thread = failing_gate.clone();
    let failing = thread::spawn(move || {
        failing_barrier.wait();
        spawn_child_scan(
            &failing_home,
            &failing_config,
            &failing_logs,
            "failure-claude",
            "claude",
            Some(&failing_gate_for_thread),
            MergeTestBehavior {
                fail_after_release: true,
                hold_after_release: false,
            },
        )
    });
    let holding_barrier = Arc::clone(&barrier);
    let holding_gate_for_thread = holding_gate.clone();
    let holding = thread::spawn(move || {
        holding_barrier.wait();
        spawn_child_scan(
            &home,
            &config,
            &logs,
            "failure-codex",
            "codex",
            Some(&holding_gate_for_thread),
            MergeTestBehavior {
                fail_after_release: false,
                hold_after_release: true,
            },
        )
    });
    barrier.wait();

    let failure = run_gated_children(
        join_spawn_thread(failing.join(), "failing child"),
        failing_gate,
        "failing child",
        join_spawn_thread(holding.join(), "holding peer"),
        holding_gate,
        "holding peer",
    )
    .expect_err("forced child failure should fail gated harness");

    assert!(
        failure.message.contains("exited unsuccessfully"),
        "unexpected gated failure: {failure:?}"
    );
    assert_eq!(failure.reaped_children, 2, "both children must be reaped");
    assert_eq!(
        failure.killed_children, 1,
        "the holding peer must be killed"
    );
}

#[test]
#[serial]
fn atomic_reader_stress_observes_only_old_or_new_complete_cache() {
    let fixture = Fixture::new();
    let old_cache = cache_variant("old");
    let new_cache = cache_variant("new");
    old_cache
        .save_with_result(fixture.config.path())
        .expect("initial atomic cache save");

    let old_value = serde_json::to_value(&old_cache).expect("serialize old cache");
    let new_value = serde_json::to_value(&new_cache).expect("serialize new cache");
    let barrier = Arc::new(Barrier::new(2));
    let stop = Arc::new(AtomicBool::new(false));
    let parse_errors = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));

    let writer_barrier = Arc::clone(&barrier);
    let writer_stop = Arc::clone(&stop);
    let writer_config = fixture.config.path().to_path_buf();
    let writer = thread::spawn(move || {
        writer_barrier.wait();
        for index in 0..200 {
            let cache = if index % 2 == 0 {
                &old_cache
            } else {
                &new_cache
            };
            cache
                .save_with_result(&writer_config)
                .expect("atomic writer");
        }
        writer_stop.store(true, Ordering::Release);
    });

    barrier.wait();
    while !stop.load(Ordering::Acquire) || reads.load(Ordering::Relaxed) < 100 {
        reads.fetch_add(1, Ordering::Relaxed);
        let bytes = match fs::read(fixture.cache_path()) {
            Ok(bytes) => bytes,
            Err(_) => {
                parse_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let value = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(_) => {
                parse_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        if value != old_value && value != new_value {
            parse_errors.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if serde_json::from_value::<SessionIndexCache>(value).is_err() {
            parse_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
    writer.join().expect("atomic writer thread");

    assert_eq!(parse_errors.load(Ordering::Acquire), 0);
    assert!(reads.load(Ordering::Acquire) >= 100);
    let final_value: Value =
        serde_json::from_slice(&fs::read(fixture.cache_path()).expect("final cache"))
            .expect("final cache remains parseable");
    assert!(final_value == old_value || final_value == new_value);
    let parsed: SessionIndexCache =
        serde_json::from_value(final_value).expect("final cache object");
    assert_eq!(parsed.version, 4);
    let entry = parsed.entries.values().next().expect("cache entry");
    assert_eq!(entry.has_custom_title, entry.title == "new");
}

fn cache_variant(title: &str) -> SessionIndexCache {
    let entry = CachedEntry {
        file_size: 1,
        mtime_secs: 1,
        content_fingerprint: title.to_string(),
        source: "claude".to_string(),
        session_id: format!("session-{title}"),
        title: title.to_string(),
        has_custom_title: title == "new",
        project_name: "cache-task4".to_string(),
        project_dir: "/tmp/cache-task4".to_string(),
        cwd: Some("/tmp/cache-task4".to_string()),
        message_count: 1,
        user_message_count: 1,
        assistant_message_count: 0,
        first_timestamp: None,
        last_activity: None,
    };
    SessionIndexCache {
        version: 4,
        entries: HashMap::from([(format!("/tmp/{title}.jsonl"), entry)]),
    }
}

#[test]
#[serial]
fn lock_holder_child_blocks_writer_until_marker_release_then_writer_succeeds() {
    let fixture = Fixture::new();
    let (initial, _) = fixture.run("lock-initial", "claude");
    assert_success(&initial);

    let ready = fixture.logs.path().join("lock-ready.marker");
    let release = fixture.logs.path().join("lock-release.marker");
    let child_log = fixture.logs.path().join("lock-holder.log");
    let current_exe = std::env::current_exe().expect("test executable");
    let holder = Command::new(current_exe)
        .args([
            "--ignored",
            "--exact",
            "child_holds_session_cache_lock",
            "--nocapture",
        ])
        .env("CCS_TEST_LOCK_CHILD", "1")
        .env("CCS_TEST_LOCK_CONFIG", fixture.config.path())
        .env("CCS_TEST_LOCK_READY", &ready)
        .env("CCS_TEST_LOCK_RELEASE", &release)
        .env("CCS_TEST_LOCK_LOG", &child_log)
        .env("HOME", fixture.home.path())
        .env("USERPROFILE", fixture.home.path())
        .env("CLAUDE_CODE_SYNC_CONFIG_DIR", fixture.config.path())
        .spawn()
        .expect("spawn lock holder");

    wait_for_marker(&ready);
    let mut writer = fixture
        .command("lock-writer", "claude")
        .spawn()
        .expect("spawn writer");
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        assert!(
            writer
                .try_wait()
                .expect("poll writer before release")
                .is_none(),
            "writer completed before lock-holder release"
        );
        thread::yield_now();
    }
    assert!(
        writer
            .try_wait()
            .expect("final poll before release")
            .is_none(),
        "writer completed before release observation window ended"
    );

    fs::write(&release, b"release").expect("release lock holder");
    let mut writer = Some(writer);
    let writer_status = wait_child(&mut writer, "lock writer").expect("wait lock writer");
    assert!(
        writer_status.success(),
        "lock writer failed: {writer_status}"
    );
    let mut holder = Some(holder);
    let holder_status = wait_child(&mut holder, "lock holder").expect("wait lock holder");
    assert!(
        holder_status.success(),
        "lock holder failed: {holder_status}"
    );
    assert!(
        child_log.exists(),
        "lock holder did not write independent log"
    );
    assert!(cache_sources(&fixture.cache_path()).contains(&"claude".to_string()));
}

#[test]
#[ignore]
fn child_holds_session_cache_lock() {
    if std::env::var_os("CCS_TEST_LOCK_CHILD").is_none() {
        return;
    }
    let config = PathBuf::from(std::env::var_os("CCS_TEST_LOCK_CONFIG").expect("lock config"));
    let ready = PathBuf::from(std::env::var_os("CCS_TEST_LOCK_READY").expect("ready marker"));
    let release = PathBuf::from(std::env::var_os("CCS_TEST_LOCK_RELEASE").expect("release marker"));
    let log = PathBuf::from(std::env::var_os("CCS_TEST_LOCK_LOG").expect("lock log"));
    fs::create_dir_all(&config).expect("lock config dir");
    let lock_path = config.join("session_index.json.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .expect("open lock");
    lock.lock().expect("hold lock");
    fs::write(&ready, b"ready").expect("ready marker");
    fs::write(&log, b"lock acquired\n").expect("lock log");
    while !release.exists() {
        thread::sleep(Duration::from_millis(2));
    }
    fs::write(&log, b"lock acquired\nreleased\n").expect("release log");
    lock.unlock().expect("unlock");
}

fn wait_for_marker(marker: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "marker was not created: {}",
            marker.display()
        );
        thread::yield_now();
    }
}
