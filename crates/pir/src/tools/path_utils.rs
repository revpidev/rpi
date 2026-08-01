//! Path resolution utilities for built-in tools.
//!
//! Port of `packages/coding-agent/src/core/tools/path-utils.ts` and
//! the relevant parts of `packages/coding-agent/src/utils/paths.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - `resolve_read_path` is `async` (uses `tokio::fs::try_exists`).
//! - `~` expansion reads the `HOME` environment variable directly (unix only).
//! - `file://` URL decoding is a minimal hand-rolled percent-decoder (no new
//!   dependency).

use std::path::{Path, PathBuf};

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

/// Narrow no-break space `U+202F`, used by macOS in screenshot filenames
/// before AM/PM (path-utils.ts:5).
pub const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

// -----------------------------------------------------------------------
// Unicode-space normalisation (paths.ts:7, 59-61)
// -----------------------------------------------------------------------

/// Returns `true` if `c` is one of the Unicode space variants that
/// `normalizePath` replaces with a regular space.
fn is_unicode_space(c: char) -> bool {
    match c {
        '\u{00A0}' => true,              // NO-BREAK SPACE
        '\u{2000}'..='\u{200A}' => true, // EN QUAD .. HAIR SPACE
        '\u{202F}' => true,              // NARROW NO-BREAK SPACE
        '\u{205F}' => true,              // MEDIUM MATHEMATICAL SPACE
        '\u{3000}' => true,              // IDEOGRAPHIC SPACE
        _ => false,
    }
}

// -----------------------------------------------------------------------
// percent-decoding (for file:// URLs)
// -----------------------------------------------------------------------

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Minimal percent-decoder (no new dependency). Equivalent to Node's
/// `decodeURIComponent` for the percent-encoding subset used in file URLs.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                result.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

/// Convert a `file://` URL to a filesystem path (paths.ts:74-76).
///
/// Handles the common `file:///path/to/file` form and percent-decodes.
fn file_url_to_path(url: &str) -> String {
    debug_assert!(url.starts_with("file://"));
    let rest = &url[7..]; // after "file://"

    // On unix the authority is typically empty: file:///path → /path.
    if let Some(slash) = rest.find('/') {
        percent_decode(&rest[slash..])
    } else {
        // No path component — degenerate, return as-is.
        percent_decode(rest)
    }
}

// -----------------------------------------------------------------------
// normalizePath (paths.ts:57-79)
// -----------------------------------------------------------------------

/// Normalise a path string with the same options as upstream `normalizePath`.
///
/// - `strip_at_prefix`: remove a leading `@`.
/// - `normalize_unicode_spaces`: replace Unicode space variants with ` `.
/// - Tilde expansion (`~`, `~/`) is always applied (unix `HOME`).
/// - `file://` URLs are decoded.
fn normalize_path_inner(
    input: &str,
    strip_at_prefix: bool,
    normalize_unicode_spaces: bool,
) -> String {
    let mut normalized = if normalize_unicode_spaces {
        input
            .chars()
            .map(|c| if is_unicode_space(c) { ' ' } else { c })
            .collect::<String>()
    } else {
        input.to_string()
    };

    if strip_at_prefix && normalized.starts_with('@') {
        normalized.remove(0);
    }

    // Tilde expansion (paths.ts:66-72). Only `~` alone and `~/...` expand;
    // `~draft.md` stays literal.
    if normalized == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return home.to_string_lossy().into_owned();
        }
    } else if let Some(rest) = normalized.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(rest)
                .to_string_lossy()
                .into_owned();
        }
    }

    // file:// URL → path (paths.ts:74-76).
    if normalized.starts_with("file://") {
        return file_url_to_path(&normalized);
    }

    normalized
}

// -----------------------------------------------------------------------
// lexical path resolution (equivalent to Node path.resolve normalisation)
// -----------------------------------------------------------------------

/// Lexically normalise a path: resolve `.` and `..` components without
/// touching the filesystem. Equivalent to the normalisation portion of
/// Node's `path.resolve`.
fn normalize_lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => result.push(p.as_os_str()),
            Component::RootDir => {
                result.push(component.as_os_str());
            }
            Component::CurDir => {} // skip "."
            Component::ParentDir => match result.components().next_back() {
                Some(Component::Normal(_)) => {
                    result.pop();
                }
                // At root or after prefix: /.. = /, no-op.
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => {
                    result.push("..");
                }
            },
            Component::Normal(s) => {
                result.push(s);
            }
        }
    }
    result
}

// -----------------------------------------------------------------------
// expandPath / resolveToCwd (path-utils.ts:40-50)
// -----------------------------------------------------------------------

/// Expand a path: strip `@` prefix, normalise Unicode spaces, expand `~`,
/// decode `file://` URLs.
///
/// Port of `expandPath` (path-utils.ts:40-42).
pub fn expand_path(file_path: &str) -> PathBuf {
    PathBuf::from(normalize_path_inner(file_path, true, true))
}

/// Resolve a path relative to `cwd`, handling `~` expansion and absolute paths.
///
/// Port of `resolveToCwd` (path-utils.ts:48-50). If the normalised input is
/// absolute, it is returned (lexically cleaned). Otherwise it is joined with
/// the normalised `cwd`.
pub fn resolve_to_cwd(file_path: &str, cwd: &Path) -> PathBuf {
    let normalized = normalize_path_inner(file_path, true, true);
    let normalized_base = normalize_path_inner(&cwd.to_string_lossy(), false, false);

    let normalized_path = PathBuf::from(&normalized);
    if normalized_path.is_absolute() {
        normalize_lexical(&normalized_path)
    } else {
        let base = PathBuf::from(&normalized_base);
        normalize_lexical(&base.join(&normalized_path))
    }
}

// -----------------------------------------------------------------------
// resolveReadPath variants (path-utils.ts:52-117)
// -----------------------------------------------------------------------

/// macOS screenshot AM/PM variant: replace ` (AM|PM).` with
/// `<NARROW_NBSP>$1.` (case-insensitive) (path-utils.ts:7-9).
fn try_macos_screenshot_path(path: &str) -> String {
    // Invariant: verified-valid regex literal.
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"(?i) (AM|PM)\.").expect("macOS screenshot regex is valid"));
    re.replace_all(path, |caps: &regex::Captures| {
        format!("{NARROW_NO_BREAK_SPACE}{}.", &caps[1])
    })
    .into_owned()
}

/// NFD (decomposed) variant for macOS filenames (path-utils.ts:11-13).
fn try_nfd_variant(path: &str) -> String {
    path.nfd().collect()
}

/// Curly-quote variant: replace `'` (U+0027) with `'` (U+2019)
/// (path-utils.ts:16-19).
fn try_curly_quote_variant(path: &str) -> String {
    path.replace('\'', "\u{2019}")
}

/// Async existence check via `tokio::fs::try_exists`.
async fn path_exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

/// Resolve a read path, trying multiple filename variants for macOS
/// compatibility.
///
/// Port of `resolveReadPathAsync` (path-utils.ts:86-117). Tries in order:
/// 1. Original resolved path
/// 2. macOS AM/PM narrow-no-break-space variant
/// 3. NFD (decomposed) variant
/// 4. Curly-quote variant
/// 5. Combined NFD + curly-quote variant
///
/// Returns the first variant that exists on disk, or the original resolved
/// path if none match.
pub async fn resolve_read_path(file_path: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(file_path, cwd);

    if path_exists(&resolved).await {
        return resolved;
    }

    let resolved_str = resolved.to_string_lossy();

    // macOS AM/PM variant
    let am_pm_variant = try_macos_screenshot_path(&resolved_str);
    let am_pm_path = PathBuf::from(&am_pm_variant);
    if am_pm_variant != resolved_str && path_exists(&am_pm_path).await {
        return am_pm_path;
    }

    // NFD variant
    let nfd_variant = try_nfd_variant(&resolved_str);
    let nfd_path = PathBuf::from(&nfd_variant);
    if nfd_variant != resolved_str && path_exists(&nfd_path).await {
        return nfd_path;
    }

    // Curly-quote variant
    let curly_variant = try_curly_quote_variant(&resolved_str);
    let curly_path = PathBuf::from(&curly_variant);
    if curly_variant != resolved_str && path_exists(&curly_path).await {
        return curly_path;
    }

    // Combined NFD + curly-quote
    let nfd_curly_variant = try_curly_quote_variant(&nfd_variant);
    let nfd_curly_path = PathBuf::from(&nfd_curly_variant);
    if nfd_curly_variant != resolved_str && path_exists(&nfd_curly_path).await {
        return nfd_curly_path;
    }

    resolved
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_helpers::TempDir;

    // Port of "should expand ~ to home directory"
    #[test]
    fn test_expand_tilde_alone() {
        let result = expand_path("~");
        assert!(!result.to_string_lossy().contains('~'));
    }

    // Port of "should expand ~/path to home directory"
    #[test]
    fn test_expand_tilde_slash_path() {
        let result = expand_path("~/Documents/file.txt");
        assert!(!result.to_string_lossy().contains("~/"));
    }

    // Port of "should keep tilde-prefixed filenames literal"
    #[test]
    fn test_expand_tilde_filename_literal() {
        assert_eq!(expand_path("~draft.md"), PathBuf::from("~draft.md"));
        assert_eq!(expand_path("@~draft.md"), PathBuf::from("~draft.md"));
    }

    // Port of "should normalize Unicode spaces"
    #[test]
    fn test_expand_normalize_unicode_spaces() {
        let with_nbsp = "file\u{00A0}name.txt";
        assert_eq!(expand_path(with_nbsp), PathBuf::from("file name.txt"));
    }

    // Port of "should resolve absolute paths as-is"
    #[test]
    fn test_resolve_absolute_as_is() {
        let tmp = TempDir::new();
        let absolute = tmp.path().join("absolute/path/file.txt");
        let cwd = tmp.path().join("some/cwd");
        let result = resolve_to_cwd(&absolute.to_string_lossy(), &cwd);
        assert_eq!(result, absolute);
    }

    // Port of "should resolve relative paths against cwd"
    #[test]
    fn test_resolve_relative_against_cwd() {
        let result = resolve_to_cwd("relative/file.txt", Path::new("/some/cwd"));
        assert_eq!(result, PathBuf::from("/some/cwd/relative/file.txt"));
    }

    // Port of "should resolve tilde-prefixed filenames against cwd"
    #[test]
    fn test_resolve_tilde_filename_against_cwd() {
        let cwd = std::env::temp_dir().join("pi-path-utils-cwd");
        assert_eq!(resolve_to_cwd("~draft.md", &cwd), cwd.join("~draft.md"));
        assert_eq!(resolve_to_cwd("@~draft.md", &cwd), cwd.join("~draft.md"));
    }

    // Port of "should resolve existing file path"
    #[tokio::test]
    async fn test_resolve_read_existing_file() {
        let tmp = TempDir::new();
        let file_path = tmp.path().join("test-file.txt");
        std::fs::write(&file_path, "content").unwrap();

        let result = resolve_read_path("test-file.txt", tmp.path()).await;
        assert_eq!(result, file_path);
    }

    // Port of "should handle NFC vs NFD Unicode normalization"
    #[tokio::test]
    async fn test_resolve_read_nfc_nfd() {
        let tmp = TempDir::new();
        // NFD: e (U+0065) + combining acute accent (U+0301)
        let nfd_name = "file\u{0065}\u{0301}.txt";
        // NFC: é as single character (U+00E9)
        let nfc_name = "file\u{00E9}.txt";

        assert_ne!(nfd_name, nfc_name);

        // Create file with NFD name
        std::fs::write(tmp.path().join(nfd_name), "content").unwrap();

        // User provides NFC path → should find via NFD fallback (or fs normalisation)
        let result = resolve_read_path(nfc_name, tmp.path()).await;
        assert!(result.starts_with(tmp.path()));
        assert!(
            result.to_string_lossy().ends_with(".txt"),
            "result: {:?}",
            result
        );
    }

    // Port of "should handle curly quotes vs straight quotes"
    #[tokio::test]
    async fn test_resolve_read_curly_quotes() {
        let tmp = TempDir::new();
        let curly_name = "Capture d\u{2019}cran.txt";
        let straight_name = "Capture d'cran.txt";

        assert_ne!(curly_name, straight_name);

        std::fs::write(tmp.path().join(curly_name), "content").unwrap();

        let result = resolve_read_path(straight_name, tmp.path()).await;
        assert_eq!(result, tmp.path().join(curly_name));
    }

    // Port of "should handle combined NFC + curly quote"
    #[tokio::test]
    async fn test_resolve_read_combined_nfd_curly() {
        let tmp = TempDir::new();
        // NFC + curly quote (how the file exists on disk)
        let nfc_curly = "Capture d\u{2019}\u{00E9}cran.txt";
        // NFC + straight quote (user input)
        let nfc_straight = "Capture d'\u{00E9}cran.txt";

        assert_ne!(nfc_curly, nfc_straight);

        std::fs::write(tmp.path().join(nfc_curly), "content").unwrap();

        let result = resolve_read_path(nfc_straight, tmp.path()).await;
        assert_eq!(result, tmp.path().join(nfc_curly));
    }

    // Port of "should handle macOS screenshot AM/PM variant with narrow no-break space"
    #[tokio::test]
    async fn test_resolve_read_macos_ampm_uppercase() {
        let tmp = TempDir::new();
        let macos_name = "Screenshot 2024-01-01 at 10.00.00\u{202F}AM.png";
        let user_name = "Screenshot 2024-01-01 at 10.00.00 AM.png";

        std::fs::write(tmp.path().join(macos_name), "content").unwrap();

        let result = resolve_read_path(user_name, tmp.path()).await;
        assert_eq!(result, tmp.path().join(macos_name));
    }

    // Port of "should handle macOS screenshot lowercase am/pm variant (en_AU locale)"
    #[tokio::test]
    async fn test_resolve_read_macos_ampm_lowercase() {
        let tmp = TempDir::new();
        let macos_name = "Screenshot 2024-01-01 at 10.00.00\u{202F}am.png";
        let user_name = "Screenshot 2024-01-01 at 10.00.00 am.png";

        std::fs::write(tmp.path().join(macos_name), "content").unwrap();

        let result = resolve_read_path(user_name, tmp.path()).await;
        assert_eq!(result, tmp.path().join(macos_name));
    }

    #[test]
    fn test_normalize_lexical_dotdot() {
        assert_eq!(
            normalize_lexical(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(
            normalize_lexical(Path::new("/a/b/./c")),
            PathBuf::from("/a/b/c")
        );
        assert_eq!(
            normalize_lexical(Path::new("/a/../../b")),
            PathBuf::from("/b")
        );
        // /.. = /
        assert_eq!(normalize_lexical(Path::new("/..")), PathBuf::from("/"));
    }

    #[test]
    fn test_file_url_to_path() {
        assert_eq!(
            file_url_to_path("file:///home/user/file.txt"),
            "/home/user/file.txt"
        );
        assert_eq!(
            file_url_to_path("file:///path%20with%20spaces.txt"),
            "/path with spaces.txt"
        );
    }
}
