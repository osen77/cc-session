use std::collections::HashSet;
use std::io::Write;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use claude_code_sync::logger::{init_logger_with_options, LoggerOptions};

fn isolated_command(temp: &TempDir) -> Command {
    let home = temp.path().join("home");
    let config = temp.path().join("config");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_ccs"));
    command
        .env("HOME", home)
        .env("USERPROFILE", temp.path().join("home"))
        .env("CLAUDE_CODE_SYNC_CONFIG_DIR", config);
    command
}

#[test]
fn logger_long_lived_writer_child() {
    let Some(path) = std::env::var_os("CCS_LOGGER_LONG_LIVED_PATH") else {
        return;
    };
    let ready = std::path::PathBuf::from(
        std::env::var_os("CCS_LOGGER_LONG_LIVED_READY").expect("ready marker"),
    );
    let release = std::path::PathBuf::from(
        std::env::var_os("CCS_LOGGER_LONG_LIVED_RELEASE").expect("release marker"),
    );
    let done = std::path::PathBuf::from(
        std::env::var_os("CCS_LOGGER_LONG_LIVED_DONE").expect("done marker"),
    );

    let options = LoggerOptions::new(false, Some(path.into()), None).unwrap();
    let status = init_logger_with_options(options).unwrap();
    assert!(status.file_logging_enabled);
    log::info!(target: "logger::long_lived_test", "long-lived-before");
    log::logger().flush();
    std::fs::write(&ready, b"ready").unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.exists() {
        assert!(
            Instant::now() < deadline,
            "rotation release marker timed out"
        );
        thread::sleep(Duration::from_millis(10));
    }

    log::info!(target: "logger::long_lived_test", "long-lived-after");
    log::logger().flush();
    std::fs::write(done, b"done").unwrap();
}

#[test]
fn test_long_lived_writer_survives_external_rotation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("long-lived.log");
    let ready = temp.path().join("writer.ready");
    let release = temp.path().join("writer.release");
    let done = temp.path().join("writer.done");
    let home = temp.path().join("home");
    let config = temp.path().join("config");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config).unwrap();

    let child = std::env::current_exe().unwrap();
    let mut writer = Command::new(&child);
    writer
        .args(["--exact", "logger_long_lived_writer_child", "--nocapture"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CLAUDE_CODE_SYNC_CONFIG_DIR", &config)
        .env("CCS_LOGGER_LONG_LIVED_PATH", &path)
        .env("CCS_LOGGER_LONG_LIVED_READY", &ready)
        .env("CCS_LOGGER_LONG_LIVED_RELEASE", &release)
        .env("CCS_LOGGER_LONG_LIVED_DONE", &done);
    let mut writer = writer.spawn().unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "long-lived writer did not initialize"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let filler = vec![b'F'; 11 * 1024 * 1024];
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&filler)
        .unwrap();

    let rotation = isolated_command(&temp)
        .args([
            "--debug",
            "--log-file",
            path.to_str().unwrap(),
            "session",
            "projects",
            "--source",
            "codex",
        ])
        .status()
        .unwrap();
    assert!(rotation.success());
    std::fs::write(&release, b"release").unwrap();

    let writer_status = writer.wait().unwrap();
    assert!(writer_status.success());
    assert!(done.exists());

    let mut all_logs = String::new();
    for generation in 0..=3 {
        let candidate = if generation == 0 {
            path.clone()
        } else {
            path.with_extension(format!("log.{generation}"))
        };
        if candidate.exists() {
            all_logs.push_str(&std::fs::read_to_string(candidate).unwrap());
        }
    }
    assert!(all_logs.contains("long-lived-before"));
    assert!(all_logs.contains("long-lived-after"));
}

#[test]
fn test_cli_debug_log_reaches_explicit_file() {
    let temp = tempfile::tempdir().unwrap();
    let log_path = temp.path().join("ccs.log");

    let status = isolated_command(&temp)
        .args([
            "--debug",
            "--log-file",
            log_path.to_str().unwrap(),
            "session",
            "projects",
            "--source",
            "codex",
        ])
        .status()
        .unwrap();

    assert!(status.success());
    let contents = std::fs::read_to_string(log_path).unwrap();
    assert!(contents.contains("DEBUG"));
    assert!(contents.contains("ccs started"));
    assert!(contents.contains("invocation=I-"));
}

#[test]
fn test_rust_log_off_keeps_info_file_logging() {
    let temp = tempfile::tempdir().unwrap();
    let log_path = temp.path().join("config").join("claude-code-sync.log");

    let status = isolated_command(&temp)
        .env("RUST_LOG", "off")
        .args(["session", "projects", "--source", "codex"])
        .status()
        .unwrap();

    assert!(status.success());
    let contents = std::fs::read_to_string(log_path).unwrap();
    assert!(contents.contains("INFO"));
    assert!(contents.contains("logger initialized"));
}

#[test]
fn test_file_logging_fallback_warning_is_visible_once() {
    let temp = tempfile::tempdir().unwrap();
    let unavailable_log_path = temp.path().join("not-a-file");
    std::fs::create_dir(&unavailable_log_path).unwrap();

    let output = isolated_command(&temp)
        .args([
            "--log-file",
            unavailable_log_path.to_str().unwrap(),
            "session",
            "projects",
            "--source",
            "codex",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("WARNING: file logging unavailable").count(),
        1,
        "expected one file logging warning, got stderr: {stderr}"
    );
    assert!(!stderr.contains(unavailable_log_path.to_string_lossy().as_ref()));
    assert!(!stderr.contains("Is a directory"));
}

#[test]
fn test_concurrent_file_logger_initialization_rotates_without_losing_either_process() {
    let temp = tempfile::tempdir().unwrap();
    let log_path = temp.path().join("shared.log");
    let fixture = std::fs::File::create(&log_path).unwrap();
    fixture.set_len(11 * 1024 * 1024).unwrap();
    drop(fixture);

    let first = isolated_command(&temp)
        .args([
            "--debug",
            "--log-file",
            log_path.to_str().unwrap(),
            "session",
            "projects",
            "--source",
            "codex",
        ])
        .spawn()
        .unwrap();
    let second = isolated_command(&temp)
        .args([
            "--debug",
            "--log-file",
            log_path.to_str().unwrap(),
            "session",
            "projects",
            "--source",
            "codex",
        ])
        .spawn()
        .unwrap();

    let first_output = first.wait_with_output().unwrap();
    let second_output = second.wait_with_output().unwrap();
    assert!(first_output.status.success());
    assert!(second_output.status.success());
    for output in [first_output, second_output] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("file logging unavailable"),
            "unexpected file logging fallback: {stderr}"
        );
    }

    let current = std::fs::read_to_string(&log_path).unwrap();
    let backup = log_path.with_extension("log.1");
    assert!(backup.exists());
    let backup_contents = std::fs::read_to_string(&backup).unwrap();
    let invocation_ids: HashSet<_> = current
        .lines()
        .chain(backup_contents.lines())
        .flat_map(str::split_whitespace)
        .filter_map(|field| field.strip_prefix("invocation="))
        .filter(|id| id.starts_with("I-") && id.len() == 10)
        .collect();
    assert!(
        invocation_ids.len() >= 2,
        "expected at least two distinct invocation IDs, got {invocation_ids:?}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let lock = log_path.with_extension("log.lock");
        for candidate in [&lock, &log_path, &backup] {
            assert!(
                candidate.exists(),
                "missing log artifact: {}",
                candidate.display()
            );
            assert_eq!(
                std::fs::metadata(candidate).unwrap().permissions().mode() & 0o777,
                0o600,
                "unexpected permissions for {}",
                candidate.display()
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn test_symlink_log_file_is_rejected_without_writing_external_target() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("external.log");
    let link = temp.path().join("link.log");
    std::fs::write(&target, b"external-before").unwrap();
    symlink(&target, &link).unwrap();

    let output = isolated_command(&temp)
        .args([
            "--log-file",
            link.to_str().unwrap(),
            "session",
            "projects",
            "--source",
            "codex",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(std::fs::read(&target).unwrap(), b"external-before");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("WARNING: file logging unavailable").count(),
        1
    );
    assert!(!stderr.contains(link.to_string_lossy().as_ref()));
}

#[test]
fn test_warning_detail_cap_emits_one_suppressed_file_record() {
    let temp = tempfile::tempdir().unwrap();
    let project_dir = temp
        .path()
        .join("home")
        .join(".claude")
        .join("projects")
        .join("-tmp-warning-cap");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        project_dir.join("valid.jsonl"),
        concat!(
            r#"{"type":"user","sessionId":"cap","cwd":"/tmp/warning-cap","timestamp":"2026-08-02T00:00:00Z","message":{"role":"user","content":"ok"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"cap","cwd":"/tmp/warning-cap","timestamp":"2026-08-02T00:00:01Z","message":{"role":"assistant","content":"answer"}}"#,
            "\n"
        ),
    )
    .unwrap();
    for index in 0..103 {
        std::fs::write(project_dir.join(format!("bad-{index}.jsonl")), b"not json").unwrap();
    }
    let log_path = temp.path().join("warning-cap.log");

    let output = isolated_command(&temp)
        .args([
            "--log-file",
            log_path.to_str().unwrap(),
            "session",
            "overview",
            "--json",
            "--source",
            "claude",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let log = std::fs::read_to_string(log_path).unwrap();
    assert_eq!(log.matches("session scan warning source=").count(), 100);
    assert_eq!(
        log.matches("session scan warnings suppressed after reaching detail cap")
            .count(),
        1
    );
}
