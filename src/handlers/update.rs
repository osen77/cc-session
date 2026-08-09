//! Self-update functionality
//!
//! Provides automatic update checking and self-update capabilities.
//! Downloads prebuilt binaries from GitHub Releases.

use anyhow::{Context, Result};
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use crate::BINARY_NAME;

/// GitHub repository for releases.
const RELEASE_REPO: &str = "osen77/cc-session";

fn latest_release_api_path() -> String {
    format!("repos/{RELEASE_REPO}/releases/latest")
}

fn latest_release_api_url() -> String {
    format!("https://api.github.com/{}", latest_release_api_path())
}

fn release_asset_url(version: &str, asset_name: &str) -> String {
    format!("https://github.com/{RELEASE_REPO}/releases/download/{version}/{asset_name}")
}

/// Timeout for HTTP requests (in seconds)
const REQUEST_TIMEOUT_SECS: u64 = 10;

/// Get current version from Cargo.toml
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Parse tag_name from GitHub API JSON response
fn parse_tag_name(response: &str) -> Option<String> {
    // Handle both compact JSON ("tag_name":"v1.0") and pretty JSON ("tag_name": "v1.0")
    let pos = response.find("\"tag_name\"")?;
    let rest = &response[pos + 10..]; // skip "tag_name"
                                      // Skip optional whitespace and colon
    let rest = rest.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
    // Skip opening quote
    let rest = rest.trim_start_matches('"');
    // Find closing quote
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Fetch release info using gh CLI (authenticated, 5000 req/hr limit)
fn fetch_with_gh(api_path: &str) -> Option<String> {
    let output = Command::new("gh").args(["api", api_path]).output().ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Fetch release info using curl (unauthenticated, 60 req/hr limit)
fn fetch_with_curl(url: &str, timeout: u64) -> Option<String> {
    let user_agent = format!("User-Agent: {}", BINARY_NAME);
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            &timeout.to_string(),
            "-H",
            "Accept: application/vnd.github.v3+json",
            "-H",
            &user_agent,
            url,
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Fetch the latest version from GitHub API
///
/// Prefers `gh` CLI (authenticated) to avoid rate limiting,
/// falls back to `curl` (unauthenticated, 60 req/hr).
pub fn fetch_latest_version() -> Result<String> {
    let api_path = latest_release_api_path();
    let url = latest_release_api_url();

    // Try gh CLI first (authenticated, higher rate limit)
    let response = fetch_with_gh(&api_path)
        // Fallback to curl
        .or_else(|| fetch_with_curl(&url, REQUEST_TIMEOUT_SECS))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to fetch release info. GitHub API rate limit may be exceeded.\n\
                 Install gh CLI (https://cli.github.com) and run 'gh auth login' to avoid this."
            )
        })?;

    parse_tag_name(&response)
        .ok_or_else(|| anyhow::anyhow!("Could not parse version from response"))
}

/// Compare version strings (v0.1.2 vs v0.1.1)
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        s.trim_start_matches('v')
            .split('.')
            .filter_map(|p| p.split('-').next()) // Handle pre-release versions like 0.1.2-beta
            .filter_map(|p| p.parse().ok())
            .collect()
    };

    let latest_parts = parse(latest);
    let current_parts = parse(current);

    latest_parts > current_parts
}

/// Check for available updates
/// Returns Some(new_version) if an update is available, None otherwise
#[allow(dead_code)]
pub fn check_for_update() -> Result<Option<String>> {
    let current = current_version();
    let latest = fetch_latest_version()?;

    if is_newer(&latest, current) {
        Ok(Some(latest))
    } else {
        Ok(None)
    }
}

/// Check for updates silently (for startup check)
/// Swallows errors to avoid disrupting normal operation
pub fn check_for_update_silent() -> Option<String> {
    let api_path = latest_release_api_path();
    let url = latest_release_api_url();

    // Try gh CLI first, fallback to curl with shorter timeout
    let response = fetch_with_gh(&api_path).or_else(|| fetch_with_curl(&url, 5))?;

    let latest = parse_tag_name(&response)?;
    let current = current_version();

    if is_newer(&latest, current) {
        Some(latest)
    } else {
        None
    }
}

fn asset_name_for(os: &str, arch: &str, target_env: &str) -> Result<String> {
    let name = match (os, arch, target_env) {
        ("linux", "x86_64", "musl") => format!("{BINARY_NAME}-linux-x86_64-musl.tar.gz"),
        ("linux", "x86_64", _) => format!("{BINARY_NAME}-linux-x86_64.tar.gz"),
        ("macos", "x86_64", _) => format!("{BINARY_NAME}-macos-x86_64.tar.gz"),
        ("macos", "aarch64", _) => format!("{BINARY_NAME}-macos-aarch64.tar.gz"),
        ("windows", "x86_64", _) => format!("{BINARY_NAME}-windows-x86_64.zip"),
        _ => {
            return Err(anyhow::anyhow!(
                "Automatic updates are not available for {os}/{arch}/{target_env}"
            ));
        }
    };
    Ok(name)
}

/// Get the asset name for the current platform.
fn get_asset_name() -> Result<String> {
    let target_env = if cfg!(target_env = "musl") {
        "musl"
    } else {
        "default"
    };
    asset_name_for(std::env::consts::OS, std::env::consts::ARCH, target_env)
}

/// Download a file using curl
fn download_file(url: &str, dest: &Path) -> Result<()> {
    println!("{}", format!("   {}", url).cyan());

    let status = Command::new("curl")
        .args(["-fSL", "--progress-bar", "-o", dest.to_str().unwrap(), url])
        .status()
        .context("Failed to execute curl")?;

    if !status.success() {
        return Err(anyhow::anyhow!("Download failed"));
    }

    Ok(())
}

/// Download and replace the current binary
fn download_and_replace(version: &str) -> Result<()> {
    let current_exe = std::env::current_exe().context("Failed to get current executable path")?;
    let asset_name = get_asset_name()?;

    let url = release_asset_url(version, &asset_name);

    println!("{}", "📥 正在下载...".cyan());

    // Create temp directory
    let temp_dir = std::env::temp_dir().join(format!("{}-update-{}", BINARY_NAME, version));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).context("Failed to create temp directory")?;

    let archive_path = temp_dir.join(&asset_name);
    download_file(&url, &archive_path)?;

    println!("{}", "✓ 下载完成".green());

    // Extract archive
    println!("{}", "📦 正在解压...".cyan());

    #[cfg(not(windows))]
    {
        // Extract tar.gz on Unix
        let status = Command::new("tar")
            .args([
                "-xzf",
                archive_path.to_str().unwrap(),
                "-C",
                temp_dir.to_str().unwrap(),
            ])
            .status()
            .context("Failed to execute tar")?;

        if !status.success() {
            return Err(anyhow::anyhow!("Failed to extract archive"));
        }
    }

    #[cfg(windows)]
    {
        // Extract zip on Windows using PowerShell
        let status = Command::new("powershell")
            .args([
                "-Command",
                &format!(
                    "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                    archive_path.display(),
                    temp_dir.display()
                ),
            ])
            .status()
            .context("Failed to execute PowerShell")?;

        if !status.success() {
            return Err(anyhow::anyhow!("Failed to extract archive"));
        }
    }

    // Find the extracted binary
    let binary_name_with_ext = if cfg!(windows) {
        format!("{}.exe", BINARY_NAME)
    } else {
        BINARY_NAME.to_string()
    };
    let new_binary = temp_dir.join(&binary_name_with_ext);

    if !new_binary.exists() {
        return Err(anyhow::anyhow!("Binary not found in archive"));
    }

    // Replace binary
    println!("{}", "📦 正在更新...".cyan());

    #[cfg(windows)]
    {
        // On Windows, rename the running executable first
        let old_path = current_exe.with_extension("old");

        // Remove old backup if exists
        let _ = fs::remove_file(&old_path);

        // Rename current to old
        fs::rename(&current_exe, &old_path).context("Failed to rename current executable")?;

        // Copy new to current
        fs::copy(&new_binary, &current_exe).context("Failed to install new executable")?;

        println!("{}", "✓ 更新完成".green());
        println!();
        println!("{}", "注意: 旧版本已保存为 .old 文件，可手动删除".yellow());
    }

    #[cfg(not(windows))]
    {
        // On Unix/macOS, do not overwrite the running executable in place.
        // macOS can kill a Mach-O binary after an in-place overwrite because
        // code-signing state is cached by vnode/path. Install via a fresh inode.
        let install_dir = current_exe
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Current executable has no parent directory"))?;
        let temp_install = install_dir.join(format!(".{}.new-{}", BINARY_NAME, std::process::id()));
        let backup_path = install_dir.join(format!("{}.old", BINARY_NAME));

        let _ = fs::remove_file(&temp_install);
        fs::copy(&new_binary, &temp_install).context("Failed to stage new executable")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp_install, fs::Permissions::from_mode(0o755))
                .context("Failed to set executable permission")?;
        }

        let _ = fs::remove_file(&backup_path);
        fs::rename(&current_exe, &backup_path).context("Failed to move old executable aside")?;

        if let Err(e) = fs::rename(&temp_install, &current_exe) {
            let _ = fs::rename(&backup_path, &current_exe);
            return Err(e).context("Failed to install new executable");
        }

        let _ = fs::remove_file(&backup_path);

        println!("{}", "✓ 更新完成".green());
    }

    // Cleanup temp directory
    let _ = fs::remove_dir_all(&temp_dir);

    Ok(())
}

/// Handle the update command
pub fn handle_update(check_only: bool) -> Result<()> {
    let current = current_version();

    println!();
    println!("{}", "🔄 检查更新".cyan().bold());
    println!("   {} v{}", "当前版本:".cyan(), current);

    let latest = match fetch_latest_version() {
        Ok(v) => v,
        Err(e) => {
            println!("{} {}", "❌ 检查更新失败:".red(), e);
            return Err(e);
        }
    };

    println!("   {} {}", "最新版本:".cyan(), latest);
    println!();

    if !is_newer(&latest, current) {
        println!("{}", "✓ 已是最新版本".green());
        return Ok(());
    }

    println!(
        "{}",
        format!("💡 发现新版本: {} → {}", current, latest)
            .yellow()
            .bold()
    );
    println!();

    if check_only {
        println!(
            "{}",
            format!("运行 '{} update' 进行更新", BINARY_NAME).cyan()
        );
        return Ok(());
    }

    // Confirm update
    print!("{}", "是否立即更新? [Y/n] ".cyan());
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;

    if input.trim().to_lowercase() == "n" {
        println!("{}", "已取消更新".yellow());
        return Ok(());
    }

    println!();

    // Perform update
    download_and_replace(&latest)?;

    println!();
    println!("{}", "🎉 更新成功！".green().bold());
    println!("   新版本: {}", latest);
    println!();

    Ok(())
}

/// Print update notification (for startup check)
pub fn print_update_notification(new_version: &str) {
    let current = current_version();
    eprintln!();
    eprintln!(
        "{}",
        format!("💡 发现新版本 {} (当前 v{})", new_version, current).yellow()
    );
    eprintln!(
        "{}",
        format!("   运行 '{} update' 更新", BINARY_NAME).yellow()
    );
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.2", "0.1.1"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("v0.1.10", "v0.1.9"));

        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(!is_newer("0.1.1", "0.1.2"));
    }

    #[test]
    fn test_is_newer_with_prerelease() {
        // Pre-release versions should compare by main version only
        assert!(is_newer("v0.2.0-beta", "0.1.0"));
        assert!(!is_newer("v0.1.0-beta", "0.1.0"));
    }

    #[test]
    fn test_get_asset_name() {
        let name = get_asset_name().unwrap();
        // Should contain os and arch (matching release-new.yml naming)
        assert!(name.contains("macos") || name.contains("linux") || name.contains("windows"));
        assert!(name.contains("x86_64") || name.contains("aarch64"));
        // Should have archive extension
        assert!(name.ends_with(".tar.gz") || name.ends_with(".zip"));
    }

    #[test]
    fn test_asset_names_match_release_matrix() {
        assert_eq!(
            asset_name_for("linux", "x86_64", "gnu").unwrap(),
            "ccs-linux-x86_64.tar.gz"
        );
        assert_eq!(
            asset_name_for("linux", "x86_64", "musl").unwrap(),
            "ccs-linux-x86_64-musl.tar.gz"
        );
        assert_eq!(
            asset_name_for("macos", "aarch64", "default").unwrap(),
            "ccs-macos-aarch64.tar.gz"
        );
        assert_eq!(
            asset_name_for("windows", "x86_64", "msvc").unwrap(),
            "ccs-windows-x86_64.zip"
        );
        assert!(asset_name_for("linux", "aarch64", "gnu").is_err());
        assert!(asset_name_for("windows", "aarch64", "msvc").is_err());
    }

    #[test]
    fn test_current_version() {
        let version = current_version();
        // Should be a valid semver
        assert!(version.split('.').count() >= 2);
    }

    #[test]
    fn test_release_endpoints_use_new_repository() {
        assert_eq!(
            latest_release_api_path(),
            "repos/osen77/cc-session/releases/latest"
        );
        assert_eq!(
            latest_release_api_url(),
            "https://api.github.com/repos/osen77/cc-session/releases/latest"
        );
        assert_eq!(
            release_asset_url("v0.5.3", "ccs-macos-aarch64.tar.gz"),
            "https://github.com/osen77/cc-session/releases/download/v0.5.3/ccs-macos-aarch64.tar.gz"
        );
    }
}
