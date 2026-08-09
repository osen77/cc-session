use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const SESSION_ID: &str = "cc-task4";

fn make_fixture(with_malformed_file: bool) -> (TempDir, TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("home tempdir");
    let config = tempfile::tempdir().expect("config tempdir");
    let log_dir = tempfile::tempdir().expect("log tempdir");
    // These fixtures use the exact shape the classifier targets (fixture session IDs
    // under a temporary cwd). Maintenance is on by default, so leave it off here or
    // the sessions get hidden mid-test and the scan diagnostics have nothing to report.
    fs::write(
        config.path().join("config.toml"),
        "[session_maintenance]\nenabled = false\n",
    )
    .expect("write filter config fixture");
    let project_dir = home
        .path()
        .join(".claude")
        .join("projects")
        .join("-tmp-task4-project");
    fs::create_dir_all(&project_dir).expect("create Claude project fixture");

    let session = concat!(
        r#"{"type":"user","sessionId":"cc-task4","cwd":"/tmp/task4-project","timestamp":"2026-08-02T00:00:00Z","message":{"role":"user","content":"needle"}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"cc-task4","cwd":"/tmp/task4-project","timestamp":"2026-08-02T00:00:01Z","message":{"role":"assistant","content":"answer"}}"#,
        "\n",
    );
    fs::write(project_dir.join("session.jsonl"), session).expect("write session fixture");
    if with_malformed_file {
        fs::write(project_dir.join("broken.jsonl"), [0xff, 0xfe, 0xfd])
            .expect("write malformed fixture");
    }

    (home, config, log_dir.path().to_path_buf())
}

fn make_claude_root_file_fixture() -> (TempDir, TempDir, PathBuf) {
    let (home, config, log_dir) = make_fixture(false);
    let claude_root = home.path().join(".claude/projects");
    fs::remove_dir_all(&claude_root).expect("remove Claude projects directory");
    fs::write(&claude_root, b"not a directory").expect("write Claude root file");

    let codex_sessions = home.path().join(".codex/sessions/2026");
    fs::create_dir_all(&codex_sessions).expect("create Codex fixture");
    fs::write(
        codex_sessions.join("valid.jsonl"),
        concat!(
            r#"{"timestamp":"2026-08-02T00:00:00Z","type":"session_meta","payload":{"id":"cx-root-file","cwd":"/tmp/task4-project"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-02T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"needle"}]}}"#,
            "\n",
        ),
    )
    .expect("write Codex session");
    fs::write(
        home.path().join(".codex/history.jsonl"),
        r#"{"session_id":"cx-root-file","ts":1,"text":"Codex title"}"#,
    )
    .expect("write Codex history");

    let omp_sessions = home.path().join(".omp/agent/sessions");
    fs::create_dir_all(&omp_sessions).expect("create OMP fixture");
    fs::write(
        omp_sessions.join("2026-08-02T00-00-00Z_om-root-file.jsonl"),
        concat!(
            r#"{"type":"session","version":3,"id":"om-root-file","timestamp":"2026-08-02T00:00:00Z","cwd":"/tmp/task4-project","title":"OMP title"}"#,
            "\n",
            r#"{"type":"message","timestamp":"2026-08-02T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"needle"}]}}"#,
            "\n",
        ),
    )
    .expect("write OMP session");

    (home, config, log_dir)
}

fn run_ccs(
    home: &Path,
    config: &Path,
    log_dir: &Path,
    label: &str,
    args: &[&str],
) -> (Output, PathBuf) {
    let log_path = log_dir.join(format!("{label}.log"));
    let output = Command::new(env!("CARGO_BIN_EXE_ccs"))
        .arg("--log-file")
        .arg(&log_path)
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CLAUDE_CODE_SYNC_CONFIG_DIR", config)
        .env_remove("RUST_LOG")
        .output()
        .expect("run ccs");
    (output, log_path)
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

fn assert_no_path_leak(output: &Output, log_path: &Path, home: &Path, config: &Path) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let log = fs::read_to_string(log_path).expect("read temporary log");
    let home_text = home.to_string_lossy();
    let config_text = config.to_string_lossy();
    assert!(
        !stdout.contains(home_text.as_ref()),
        "HOME leaked to stdout: {stdout}"
    );
    assert!(
        !stderr.contains(home_text.as_ref()),
        "HOME leaked to stderr: {stderr}"
    );
    assert!(
        !log.contains(home_text.as_ref()),
        "HOME leaked to log: {log}"
    );
    assert!(
        !stdout.contains(config_text.as_ref()),
        "config path leaked to stdout: {stdout}"
    );
    assert!(
        !stderr.contains(config_text.as_ref()),
        "config path leaked to stderr: {stderr}"
    );
    assert!(
        !log.contains(config_text.as_ref()),
        "config path leaked to log: {log}"
    );
}

fn assert_json_contract(payload: &Value) -> String {
    assert_eq!(payload["schema_version"], 1);
    assert_eq!(payload["diagnostics"]["schema_version"], 1);
    let diagnostic_id = payload["diagnostics"]["diagnostic_id"]
        .as_str()
        .expect("diagnostic id");
    assert!(diagnostic_id.starts_with("I-"));
    assert!(payload["diagnostics"]["files_seen"].as_u64().unwrap_or(0) >= 1);
    diagnostic_id.to_string()
}

fn assert_log_contains_invocation(log_path: &Path, diagnostic_id: &str) {
    let log = fs::read_to_string(log_path).expect("read temporary log");
    assert!(
        log.contains(&format!("invocation={diagnostic_id}")),
        "diagnostic ID was not present in log invocation: {log}"
    );
}

fn assert_safe_cache_diagnostic(
    output: &Output,
    log_path: &Path,
    home: &Path,
    config: &Path,
    raw_markers: &[&str],
) {
    assert_no_path_leak(output, log_path, home, config);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let log = fs::read_to_string(log_path).expect("read cache diagnostic log");

    for marker in raw_markers {
        assert!(
            !stdout.contains(marker),
            "raw cache detail leaked to stdout: {stdout}"
        );
        assert!(
            !stderr.contains(marker),
            "raw cache detail leaked to stderr: {stderr}"
        );
        assert!(
            !log.contains(marker),
            "raw cache detail leaked to log: {log}"
        );
    }
    assert!(log.contains("ccs::scan_diagnostics"));
    assert!(log.contains("error=cache unknown detail_hash=d-"));
}

#[test]
fn overview_search_and_show_json_include_scan_contract_and_log_id() {
    let (home, config, log_dir) = make_fixture(false);

    let (overview, overview_log) = run_ccs(
        home.path(),
        config.path(),
        &log_dir,
        "overview",
        &["session", "overview", "--json"],
    );
    assert_success(&overview);
    assert_no_path_leak(&overview, &overview_log, home.path(), config.path());
    assert!(!String::from_utf8_lossy(&overview.stderr).contains("Session scan incomplete"));
    let overview_json: Value = serde_json::from_slice(&overview.stdout).expect("overview JSON");
    let overview_id = assert_json_contract(&overview_json);
    assert_log_contains_invocation(&overview_log, &overview_id);
    assert_eq!(overview_json["total_projects"], 1);
    assert_eq!(overview_json["projects"][0]["name"], "task4-project");
    assert_eq!(overview_json["diagnostics"]["degraded"], false);
    assert_eq!(overview_json["diagnostics"]["search_load_ms"], 0);

    let (search, search_log) = run_ccs(
        home.path(),
        config.path(),
        &log_dir,
        "search",
        &["session", "search", "needle", "--json"],
    );
    assert_success(&search);
    assert_no_path_leak(&search, &search_log, home.path(), config.path());
    assert!(!String::from_utf8_lossy(&search.stderr).contains("Session scan incomplete"));
    let search_json: Value = serde_json::from_slice(&search.stdout).expect("search JSON");
    let search_id = assert_json_contract(&search_json);
    assert_log_contains_invocation(&search_log, &search_id);
    assert_eq!(search_json["query"], "needle");
    assert_eq!(search_json["session_results"][0]["session_id"], SESSION_ID);
    assert_eq!(search_json["diagnostics"]["degraded"], false);
    assert!(search_json["diagnostics"]["search_load_ms"].is_number());

    let (show, show_log) = run_ccs(
        home.path(),
        config.path(),
        &log_dir,
        "show",
        &["session", "show", SESSION_ID, "--json", "--head", "1"],
    );
    assert_success(&show);
    assert_no_path_leak(&show, &show_log, home.path(), config.path());
    assert!(!String::from_utf8_lossy(&show.stderr).contains("Session scan incomplete"));
    let show_json: Value = serde_json::from_slice(&show.stdout).expect("show JSON");
    let show_id = assert_json_contract(&show_json);
    assert_log_contains_invocation(&show_log, &show_id);
    assert_eq!(show_json["session_id"], SESSION_ID);
    assert_eq!(show_json["messages"][0]["content"], "needle");
    assert_eq!(show_json["diagnostics"]["degraded"], false);
    assert_eq!(show_json["diagnostics"]["search_load_ms"], 0);
}

#[test]
fn degraded_text_commands_emit_one_aggregate_warning_without_paths() {
    let (home, config, log_dir) = make_fixture(true);

    let (degraded_json, degraded_json_log) = run_ccs(
        home.path(),
        config.path(),
        &log_dir,
        "degraded-json",
        &["session", "overview", "--json"],
    );
    assert_success(&degraded_json);
    assert_no_path_leak(
        &degraded_json,
        &degraded_json_log,
        home.path(),
        config.path(),
    );
    let degraded_json_payload: Value =
        serde_json::from_slice(&degraded_json.stdout).expect("degraded overview JSON");
    assert_json_contract(&degraded_json_payload);
    assert!(
        degraded_json_payload["diagnostics"]["malformed_files"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    );
    let degraded_json_stderr = String::from_utf8_lossy(&degraded_json.stderr);
    assert!(!degraded_json_stderr.contains("session scan warning"));
    assert!(!degraded_json_stderr.contains("WARNING: Session scan incomplete:"));

    let commands: &[&[&str]] = &[
        &["session", "list"],
        &["session", "projects"],
        &["session", "overview"],
        &["session", "search", "needle"],
        &["session", "show", SESSION_ID, "--head", "1"],
    ];

    for (index, args) in commands.iter().enumerate() {
        let label = format!("degraded-text-{index}");
        let (output, log_path) = run_ccs(home.path(), config.path(), &log_dir, &label, args);
        assert_success(&output);
        assert_no_path_leak(&output, &log_path, home.path(), config.path());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let aggregate_warnings: Vec<&str> = stderr
            .lines()
            .filter(|line| line.starts_with("WARNING: Session scan incomplete:"))
            .collect();
        assert_eq!(aggregate_warnings.len(), 1, "stderr was:\n{stderr}");
        assert!(!stdout.is_empty(), "business stdout was empty for {args:?}");
        assert!(!stderr.contains("session scan warning"));
        assert!(!stderr.contains("broken.jsonl"));
        assert!(!stderr.contains("session doctor"));
        let log = fs::read_to_string(log_path).expect("read degraded log");
        assert!(log.contains("ccs::scan_diagnostics"));
        assert!(!log.contains("broken.jsonl"));
    }
}

#[test]
fn corrupt_cache_is_json_degraded_and_only_safe_diagnostic_is_logged() {
    let (home, config, log_dir) = make_fixture(false);
    fs::write(config.path().join("session_index.json"), b"not-json").expect("write corrupt cache");

    let (output, log_path) = run_ccs(
        home.path(),
        config.path(),
        &log_dir,
        "corrupt-cache",
        &["session", "overview", "--json"],
    );
    assert_success(&output);
    assert_no_path_leak(&output, &log_path, home.path(), config.path());
    let payload: Value = serde_json::from_slice(&output.stdout).expect("corrupt cache JSON");
    let diagnostic_id = assert_json_contract(&payload);
    assert_log_contains_invocation(&log_path, &diagnostic_id);
    assert_eq!(payload["diagnostics"]["cache_errors"], 1);
    assert_eq!(payload["diagnostics"]["degraded"], true);

    let stderr = String::from_utf8_lossy(&output.stderr);
    let log = fs::read_to_string(log_path).expect("read corrupt cache log");
    assert!(!stderr.contains("not-json"));
    assert!(!log.contains("not-json"));
    assert!(!log.contains("session_index.json"));
    assert!(log.contains("ccs::scan_diagnostics"));
    assert!(log.contains("cache"));
}

#[test]
fn cache_read_failure_is_degraded_and_redacted_in_cli_json() {
    let (home, config, log_dir) = make_fixture(false);
    fs::create_dir(config.path().join("session_index.json")).expect("make cache path a directory");

    let (output, log_path) = run_ccs(
        home.path(),
        config.path(),
        &log_dir,
        "cache-read-failure",
        &["session", "overview", "--json"],
    );
    assert_success(&output);
    let payload: Value = serde_json::from_slice(&output.stdout).expect("read failure JSON");
    let diagnostic_id = assert_json_contract(&payload);
    assert_log_contains_invocation(&log_path, &diagnostic_id);
    assert_eq!(payload["diagnostics"]["cache_errors"], 2);
    assert_safe_cache_diagnostic(
        &output,
        &log_path,
        home.path(),
        config.path(),
        &[
            "session_index.json",
            "Is a directory",
            "is a directory",
            "Not a directory",
            "not a directory",
            "os error",
        ],
    );
}

#[test]
fn cache_version_mismatch_rebuilds_quietly_without_degrading_the_scan() {
    let (home, config, log_dir) = make_fixture(false);
    fs::write(
        config.path().join("session_index.json"),
        br#"{"version":999,"entries":{}}"#,
    )
    .expect("write version mismatch cache");

    let (output, log_path) = run_ccs(
        home.path(),
        config.path(),
        &log_dir,
        "cache-version-mismatch",
        &["session", "overview", "--json"],
    );
    assert_success(&output);
    let payload: Value = serde_json::from_slice(&output.stdout).expect("version mismatch JSON");
    let diagnostic_id = assert_json_contract(&payload);
    assert_log_contains_invocation(&log_path, &diagnostic_id);

    // A version bump rebuilds the whole cache and loses nothing, so it is a routine
    // upgrade step rather than a defect: no error count, no degradation, no warning.
    assert_eq!(payload["diagnostics"]["cache_errors"], 0);
    assert_eq!(payload["diagnostics"]["degraded"], false);
    assert_eq!(payload["total_projects"], 1);
    // The rebuild is traceable in the log but never reaches the terminal, so a routine
    // upgrade costs the user nothing to read.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Session scan incomplete"),
        "routine rebuild must not warn: {stderr}"
    );
    assert!(
        !stderr.contains("rebuilding"),
        "routine rebuild must stay off the terminal: {stderr}"
    );

    assert_no_path_leak(&output, &log_path, home.path(), config.path());
    let log = fs::read_to_string(&log_path).expect("read version mismatch log");
    assert!(
        log.contains("rebuilding the index"),
        "the rebuild should still be traceable in the log: {log}"
    );
    for marker in [
        "session_index.json",
        r#""version":999"#,
        "os error",
        "not-json",
    ] {
        assert!(
            !log.contains(marker),
            "raw cache detail leaked to log: {log}"
        );
    }
}

#[test]
fn claude_root_read_dir_failure_keeps_other_source_results() {
    let (home, config, log_dir) = make_claude_root_file_fixture();

    let (output, log_path) = run_ccs(
        home.path(),
        config.path(),
        &log_dir,
        "claude-root-file",
        &["session", "overview", "--json"],
    );
    assert_success(&output);
    assert_no_path_leak(&output, &log_path, home.path(), config.path());
    let payload: Value = serde_json::from_slice(&output.stdout).expect("root failure JSON");
    let diagnostic_id = assert_json_contract(&payload);
    assert_log_contains_invocation(&log_path, &diagnostic_id);
    assert_eq!(payload["diagnostics"]["io_errors"], 1);
    assert_eq!(payload["diagnostics"]["degraded"], true);
    assert_eq!(payload["total_projects"], 1);
    assert_eq!(payload["projects"][0]["session_count"], 2);
    let log = fs::read_to_string(log_path).expect("read root failure log");
    assert!(log.contains("ccs::scan_diagnostics"));
}

#[test]
fn incomplete_selected_source_root_preserves_cli_cache_entries() {
    let (home, config, log_dir) = make_fixture(false);
    let (initial, initial_log) = run_ccs(
        home.path(),
        config.path(),
        &log_dir,
        "incomplete-initial",
        &["session", "overview", "--json"],
    );
    assert_success(&initial);
    assert_no_path_leak(&initial, &initial_log, home.path(), config.path());

    let claude_root = home.path().join(".claude/projects");
    fs::remove_dir_all(&claude_root).expect("remove Claude root");
    fs::write(&claude_root, b"not a directory").expect("replace Claude root with file");

    let (degraded, degraded_log) = run_ccs(
        home.path(),
        config.path(),
        &log_dir,
        "incomplete-selected",
        &["session", "overview", "--json", "--source", "claude"],
    );
    assert_success(&degraded);
    assert_no_path_leak(&degraded, &degraded_log, home.path(), config.path());
    let payload: Value = serde_json::from_slice(&degraded.stdout).expect("degraded JSON");
    assert_eq!(payload["diagnostics"]["io_errors"], 1);
    assert_eq!(payload["diagnostics"]["degraded"], true);

    let cache: Value = serde_json::from_slice(
        &fs::read(config.path().join("session_index.json")).expect("read cache"),
    )
    .expect("cache JSON");
    assert!(cache["entries"]
        .as_object()
        .expect("cache entries")
        .values()
        .any(|entry| entry["source"] == "claude"));
}
