use std::fs;
use std::path::PathBuf;

const NEW_REPO: &str = "osen77/cc-session";
const OLD_REPO: &str = "osen77/claude-code-sync-cn";

fn repo_file(path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

#[test]
fn public_installers_download_from_new_repository() {
    let shell = repo_file("install.sh");
    let powershell = repo_file("install.ps1");

    assert!(shell.contains(&format!("REPO=\"{NEW_REPO}\"")));
    assert!(!shell.contains(&format!("REPO=\"{OLD_REPO}\"")));
    assert!(powershell.contains(&format!("$REPO = \"{NEW_REPO}\"")));
    assert!(powershell.contains("$ASSET_NAME = \"ccs-windows-x86_64.zip\""));
    assert!(powershell.contains("Expand-Archive"));
    assert!(powershell.contains("ccs.exe"));
    assert!(!powershell.contains("ccs-windows-x64.exe"));
}

#[test]
fn main_project_links_use_new_repository() {
    for path in [
        "README.md",
        "docs/user-guide.md",
        "scripts/release.sh",
        "CLAUDE.md",
    ] {
        let content = repo_file(path);
        assert!(content.contains(NEW_REPO), "{path} must link to {NEW_REPO}");
        assert!(
            !content.contains(OLD_REPO),
            "{path} must not retain the legacy repository URL"
        );
    }
}

#[test]
fn release_workflow_uses_repository_urls_and_complete_archive_matrix() {
    let workflow = repo_file(".github/workflows/release-new.yml");

    assert!(!workflow.contains(OLD_REPO));
    let release_url_base =
        "https://github.com/${{ github.repository }}/releases/download/${{ steps.get_version.outputs.version }}/";
    assert_eq!(workflow.matches(release_url_base).count(), 4);

    for (target, artifact_name, asset_name) in [
        ("x86_64-unknown-linux-gnu", "ccs", "ccs-linux-x86_64"),
        ("x86_64-unknown-linux-musl", "ccs", "ccs-linux-x86_64-musl"),
        ("x86_64-apple-darwin", "ccs", "ccs-macos-x86_64"),
        ("aarch64-apple-darwin", "ccs", "ccs-macos-aarch64"),
        ("x86_64-pc-windows-msvc", "ccs.exe", "ccs-windows-x86_64"),
    ] {
        let row = format!(
            "            target: {target}\n            artifact_name: {artifact_name}\n            asset_name: {asset_name}"
        );
        assert!(
            workflow.contains(&row),
            "workflow must include matrix row {row}"
        );
    }

    for archive in [
        "ccs-linux-x86_64.tar.gz",
        "ccs-linux-x86_64-musl.tar.gz",
        "ccs-macos-x86_64.tar.gz",
        "ccs-macos-aarch64.tar.gz",
        "ccs-windows-x86_64.zip",
    ] {
        assert!(
            workflow.contains(archive),
            "workflow must include {archive}"
        );
        assert!(
            workflow.contains(&format!("{archive}.sha256")),
            "workflow must include checksum for {archive}"
        );
    }

    // Build jobs hand assets to a single-writer finalize job via artifacts (7c8f52b):
    // parallel jobs must not upload to the release themselves, or concurrent
    // softprops uploads recreate the duplicated-draft incident.
    assert!(workflow.contains(
        "            target/${{ matrix.target }}/release/${{ matrix.asset_name }}.tar.gz*\n            target/${{ matrix.target }}/release/${{ matrix.asset_name }}.zip*"
    ));
    assert!(workflow.contains("uses: actions/upload-artifact@v4"));
    assert!(workflow.contains("merge-multiple: true"));
    assert!(workflow.contains("gh release upload \"$VERSION\" release-assets/*"));
    // Only the create-release job may use it; build jobs uploading through it in
    // parallel is what duplicated the drafts.
    assert_eq!(
        workflow
            .matches("uses: softprops/action-gh-release")
            .count(),
        1
    );

    assert!(workflow
        .contains("$checksumLine = \"$($hash.Hash.ToLower())  ${{ matrix.asset_name }}.zip`n\""));
    assert!(workflow.contains(
        "$checksumPath = Join-Path (Get-Location).Path \"${{ matrix.asset_name }}.zip.sha256\""
    ));
    assert!(workflow.contains(
        "[System.IO.File]::WriteAllText($checksumPath, $checksumLine, [System.Text.Encoding]::ASCII)"
    ));
    assert!(!workflow.contains("Out-File -FilePath ${{ matrix.asset_name }}.zip.sha256"));
}
