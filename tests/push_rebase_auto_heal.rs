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
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_second_push_rebases_instead_of_silently_failing() {
    let remote = TempDir::new().unwrap();
    git(remote.path(), &["init", "--bare"]);

    let device_a = TempDir::new().unwrap();
    let device_b = TempDir::new().unwrap();

    git(device_a.path(), &["init", "-b", "master"]);
    git(device_a.path(), &["config", "user.name", "test"]);
    git(
        device_a.path(),
        &["config", "user.email", "test@example.com"],
    );
    git(
        device_a.path(),
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );

    fs::write(device_a.path().join("session.jsonl"), "a\n").unwrap();
    git(device_a.path(), &["add", "."]);
    git(device_a.path(), &["commit", "-m", "a1"]);
    git(device_a.path(), &["push", "-u", "origin", "master"]);

    git(
        device_b.path(),
        &["clone", remote.path().to_str().unwrap(), "."],
    );
    git(device_b.path(), &["config", "user.name", "test"]);
    git(
        device_b.path(),
        &["config", "user.email", "test@example.com"],
    );

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

    assert!(
        !push.status.success(),
        "plain git push should reject without auto-heal"
    );
}
