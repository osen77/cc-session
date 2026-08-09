//! Platform-specific content filter for CLAUDE.md
//!
//! Filters content based on platform tags to enable cross-platform configuration sync.
//!
//! ## Tag Format
//!
//! ```markdown
//! <!-- platform:macos -->
//! macOS specific content here
//! <!-- end-platform -->
//!
//! <!-- platform:windows -->
//! Windows specific content here
//! <!-- end-platform -->
//! ```

use regex::Regex;
use std::sync::LazyLock;

/// Supported platforms
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOS,
    Windows,
    Linux,
}

impl Platform {
    /// Get the current platform
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOS
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }

    /// Get platform name as used in tags
    pub fn tag_name(&self) -> &'static str {
        match self {
            Platform::MacOS => "macos",
            Platform::Windows => "windows",
            Platform::Linux => "linux",
        }
    }

    /// Parse platform from tag name
    #[allow(dead_code)]
    pub fn from_tag_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "macos" | "mac" | "darwin" => Some(Platform::MacOS),
            "windows" | "win" => Some(Platform::Windows),
            "linux" => Some(Platform::Linux),
            _ => None,
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.tag_name())
    }
}

/// Regex pattern for matching platform blocks
/// Matches: <!-- platform:PLATFORM --> ... <!-- end-platform -->
static PLATFORM_BLOCK_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)<!--\s*platform:\s*(macos|mac|darwin|windows|win|linux)\s*-->(.*?)<!--\s*end-platform\s*-->"
    ).expect("Invalid regex pattern")
});

/// Filter CLAUDE.md content for target platform
///
/// - Removes content blocks for other platforms
/// - Keeps content blocks for the target platform (without the tags)
/// - Keeps all content outside platform blocks
#[allow(dead_code)]
pub fn filter_for_platform(content: &str, target: Platform) -> String {
    let target_names: Vec<&str> = match target {
        Platform::MacOS => vec!["macos", "mac", "darwin"],
        Platform::Windows => vec!["windows", "win"],
        Platform::Linux => vec!["linux"],
    };

    let result = PLATFORM_BLOCK_REGEX.replace_all(content, |caps: &regex::Captures| {
        let platform_name = caps
            .get(1)
            .map(|m| m.as_str().to_lowercase())
            .unwrap_or_default();
        let block_content = caps.get(2).map(|m| m.as_str()).unwrap_or("");

        if target_names.contains(&platform_name.as_str()) {
            // Keep this block's content (strip the tags)
            block_content.to_string()
        } else {
            // Remove this block entirely
            String::new()
        }
    });

    // Clean up multiple consecutive blank lines left by removed blocks
    cleanup_blank_lines(&result)
}

/// Clean up excessive blank lines (more than 2 consecutive)
fn cleanup_blank_lines(content: &str) -> String {
    static BLANK_LINES_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\n{3,}").expect("Invalid regex pattern"));

    BLANK_LINES_REGEX.replace_all(content, "\n\n").to_string()
}

/// Check if content contains platform-specific blocks
pub fn has_platform_blocks(content: &str) -> bool {
    PLATFORM_BLOCK_REGEX.is_match(content)
}

/// Extract all platform blocks from content (for analysis)
#[allow(dead_code)]
pub fn extract_platform_blocks(content: &str) -> Vec<(Platform, String)> {
    PLATFORM_BLOCK_REGEX
        .captures_iter(content)
        .filter_map(|caps| {
            let platform_name = caps.get(1)?.as_str();
            let block_content = caps.get(2)?.as_str().to_string();
            let platform = Platform::from_tag_name(platform_name)?;
            Some((platform, block_content))
        })
        .collect()
}

/// Extract platform block with tags preserved (for merging)
pub fn extract_current_platform_block(content: &str, platform: Platform) -> Option<String> {
    let target_names: Vec<&str> = match platform {
        Platform::MacOS => vec!["macos", "mac", "darwin"],
        Platform::Windows => vec!["windows", "win"],
        Platform::Linux => vec!["linux"],
    };

    for caps in PLATFORM_BLOCK_REGEX.captures_iter(content) {
        let platform_name = caps
            .get(1)
            .map(|m| m.as_str().to_lowercase())
            .unwrap_or_default();
        if target_names.contains(&platform_name.as_str()) {
            // Return the full match including tags
            return Some(caps.get(0)?.as_str().to_string());
        }
    }
    None
}

/// Merge CLAUDE.md from source to target, preserving target's current platform block
///
/// Logic:
/// 1. Filter source content: remove non-current-platform blocks, keep common content
/// 2. Extract target's current platform block (with tags)
/// 3. Merge: filtered source + target's platform block at the end
pub fn merge_claude_md(source_content: &str, target_content: &str, current: Platform) -> String {
    // Step 1: Filter source - remove all platform blocks (keep only common content)
    let source_common = PLATFORM_BLOCK_REGEX.replace_all(source_content, "");
    let source_common = cleanup_blank_lines(&source_common);

    // Step 2: Extract target's current platform block (preserved with tags)
    let target_platform_block = extract_current_platform_block(target_content, current);

    // Step 3: Merge
    if let Some(block) = target_platform_block {
        format!("{}\n{}\n", source_common.trim_end(), block)
    } else {
        source_common.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_from_tag_name() {
        assert_eq!(Platform::from_tag_name("macos"), Some(Platform::MacOS));
        assert_eq!(Platform::from_tag_name("mac"), Some(Platform::MacOS));
        assert_eq!(Platform::from_tag_name("darwin"), Some(Platform::MacOS));
        assert_eq!(Platform::from_tag_name("windows"), Some(Platform::Windows));
        assert_eq!(Platform::from_tag_name("win"), Some(Platform::Windows));
        assert_eq!(Platform::from_tag_name("linux"), Some(Platform::Linux));
        assert_eq!(Platform::from_tag_name("unknown"), None);
    }

    #[test]
    fn test_filter_for_platform_macos() {
        let content = r#"# Common content

## Environment

<!-- platform:macos -->
- Use fnm for node management
- Homebrew path: /opt/homebrew/bin
<!-- end-platform -->

<!-- platform:windows -->
- Use nvm-windows for node management
- Use backslash for paths
<!-- end-platform -->

## Other common content
"#;

        let filtered = filter_for_platform(content, Platform::MacOS);

        assert!(filtered.contains("Use fnm for node management"));
        assert!(filtered.contains("Homebrew path: /opt/homebrew/bin"));
        assert!(!filtered.contains("nvm-windows"));
        assert!(!filtered.contains("backslash"));
        assert!(filtered.contains("Common content"));
        assert!(filtered.contains("Other common content"));
    }

    #[test]
    fn test_filter_for_platform_windows() {
        let content = r#"# Common

<!-- platform:macos -->
macOS content
<!-- end-platform -->

<!-- platform:windows -->
Windows content
<!-- end-platform -->
"#;

        let filtered = filter_for_platform(content, Platform::Windows);

        assert!(!filtered.contains("macOS content"));
        assert!(filtered.contains("Windows content"));
        assert!(filtered.contains("Common"));
    }

    #[test]
    fn test_filter_preserves_content_without_tags() {
        let content = "# No platform tags\n\nJust regular content.";
        let filtered = filter_for_platform(content, Platform::MacOS);
        assert_eq!(filtered, content);
    }

    #[test]
    fn test_has_platform_blocks() {
        assert!(has_platform_blocks(
            "<!-- platform:macos -->\ncontent\n<!-- end-platform -->"
        ));
        assert!(!has_platform_blocks("No platform blocks here"));
    }

    #[test]
    fn test_extract_platform_blocks() {
        let content = r#"
<!-- platform:macos -->
Mac content
<!-- end-platform -->

<!-- platform:windows -->
Win content
<!-- end-platform -->
"#;

        let blocks = extract_platform_blocks(content);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, Platform::MacOS);
        assert!(blocks[0].1.contains("Mac content"));
        assert_eq!(blocks[1].0, Platform::Windows);
        assert!(blocks[1].1.contains("Win content"));
    }

    #[test]
    fn test_cleanup_blank_lines() {
        let content = "line1\n\n\n\n\nline2";
        let cleaned = cleanup_blank_lines(content);
        assert_eq!(cleaned, "line1\n\nline2");
    }

    #[test]
    fn test_merge_claude_md_preserves_target_platform() {
        // Source from Mac with macos block
        let source = r#"# Common Content

## Rules
- Rule 1
- Rule 2

<!-- platform:macos -->
- Use fnm for node
<!-- end-platform -->
"#;

        // Target on Windows with windows block
        let target = r#"# Old Content

<!-- platform:windows -->
- Use nvm-windows for node
<!-- end-platform -->
"#;

        // Merge on Windows platform
        let merged = merge_claude_md(source, target, Platform::Windows);

        // Should contain common content from source
        assert!(merged.contains("# Common Content"));
        assert!(merged.contains("- Rule 1"));
        assert!(merged.contains("- Rule 2"));

        // Should NOT contain Mac-specific content
        assert!(!merged.contains("fnm"));

        // Should preserve Windows block from target (with tags)
        assert!(merged.contains("<!-- platform:windows -->"));
        assert!(merged.contains("nvm-windows"));
        assert!(merged.contains("<!-- end-platform -->"));
    }

    #[test]
    fn test_merge_claude_md_no_target_platform_block() {
        // Source with macos block
        let source = r#"# Common
<!-- platform:macos -->
Mac content
<!-- end-platform -->
"#;

        // Target has no platform blocks
        let target = "# Old content";

        // Merge on Windows - no Windows block to preserve
        let merged = merge_claude_md(source, target, Platform::Windows);

        // Should contain common content only
        assert!(merged.contains("# Common"));
        assert!(!merged.contains("Mac content"));
        assert!(!merged.contains("Old content")); // Target content is replaced
    }

    #[test]
    fn test_extract_current_platform_block() {
        let content = r#"
<!-- platform:macos -->
Mac content
<!-- end-platform -->

<!-- platform:windows -->
Windows content
<!-- end-platform -->
"#;

        let mac_block = extract_current_platform_block(content, Platform::MacOS);
        assert!(mac_block.is_some());
        assert!(mac_block.as_ref().unwrap().contains("Mac content"));
        assert!(mac_block
            .as_ref()
            .unwrap()
            .contains("<!-- platform:macos -->"));

        let win_block = extract_current_platform_block(content, Platform::Windows);
        assert!(win_block.is_some());
        assert!(win_block.as_ref().unwrap().contains("Windows content"));

        let linux_block = extract_current_platform_block(content, Platform::Linux);
        assert!(linux_block.is_none());
    }
}
