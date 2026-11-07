//! Port of `packages/coding-agent/src/utils/changelog.ts` @ pi 0.82.1
//! (2efa728) — the `## [x.y.z]` section parser feeding
//! `getChangelogForDisplay` (interactive-mode.ts:1171-1196) and the
//! `/changelog` command.
//!
//! Intentional differences:
//! - Upstream reads `getChangelogPath()` (a CHANGELOG.md shipped next to the
//!   package); rpi is a single binary, so the asset is embedded at compile
//!   time ([`CHANGELOG_MD`]).
//! - `normalizeChangelogLinks` (relative-link rewriting against the GitHub
//!   source tree) is not ported: the embedded CHANGELOG.md carries no
//!   relative links (repo convention: plain text + absolute URLs only).
//! - Telemetry/recording side effects of `getChangelogForDisplay` live in
//!   [`crate::core::telemetry::prepare_install_report`] (D-046); this module
//!   is the pure display half — callers must read the PREVIOUS
//!   `lastChangelogVersion` before `prepare_install_report` overwrites it.

/// The embedded changelog asset (`getChangelogPath()`, config.ts:442-444 —
/// a file next to the package in upstream; single-binary embed here).
pub const CHANGELOG_MD: &str = include_str!("../../../../CHANGELOG.md");

/// `ChangelogEntry` (changelog.ts:5-10) — a `## [major.minor.patch]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    /// The section content, trimmed (heading line included, upstream keeps
    /// `currentLines = [line]` seeded with the `## ` header).
    pub content: String,
}

impl ChangelogEntry {
    fn version_tuple(&self) -> (u32, u32, u32) {
        (self.major, self.minor, self.patch)
    }
}

/// `parseChangelog` (changelog.ts:111-168): scan `## ` lines, keep sections
/// whose heading parses as `## [x.y.z]` (brackets optional), collect until
/// the next `## ` or EOF.
pub fn parse_changelog(content: &str) -> Vec<ChangelogEntry> {
    let mut entries: Vec<ChangelogEntry> = Vec::new();
    let mut current_version: Option<(u32, u32, u32)> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for line in content.split('\n') {
        if let Some(rest) = line.strip_prefix("## ") {
            // Save the previous entry FIRST, unconditionally (changelog.ts
            // :128-133): an unparsable `## ` heading resets the section
            // AFTER the save, so it must not drop the collected entry.
            if let (Some((major, minor, patch)), false) =
                (current_version.take(), current_lines.is_empty())
            {
                entries.push(finish_entry(major, minor, patch, &current_lines));
            }
            if let Some(version) = parse_version_heading(rest) {
                current_version = Some(version);
                current_lines = vec![line];
            } else {
                // Unparsable `## ` heading resets the section (upstream
                // sets currentVersion = null and clears the lines).
                current_version = None;
                current_lines.clear();
            }
        } else if current_version.is_some() {
            current_lines.push(line);
        }
    }
    if let (Some((major, minor, patch)), false) = (current_version, current_lines.is_empty()) {
        entries.push(finish_entry(major, minor, patch, &current_lines));
    }
    entries
}

/// `##\s+\[?(\d+)\.(\d+)\.(\d+)\]?` (changelog.ts:136).
fn parse_version_heading(rest: &str) -> Option<(u32, u32, u32)> {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('[').unwrap_or(rest);
    let mut parts = rest.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()?
        .trim_end_matches(']')
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn finish_entry(major: u32, minor: u32, patch: u32, lines: &[&str]) -> ChangelogEntry {
    ChangelogEntry {
        major,
        minor,
        patch,
        content: lines.join("\n").trim().to_owned(),
    }
}

/// Normalize a release version for section comparisons (V18-12 FR-E):
/// strip an optional `v` prefix, the `-<prerelease>` suffix and the
/// `+<build>` metadata, then parse the first three dot-separated segments.
///
/// `lastChangelogVersion` stores the exact build version (for example
/// `0.1.8-rc.3`), but CHANGELOG.md headings only carry `x.y.z`; comparing
/// the normalized tuple keeps RC/RC and RC/stable folds on the same base.
pub fn normalize_version(version: &str) -> Option<(u32, u32, u32)> {
    let version = version.trim();
    let version = version.strip_prefix('v').unwrap_or(version);
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let tuple = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    if parts.next().is_some() {
        return None;
    }
    Some(tuple)
}

/// `getNewEntries` (changelog.ts:182-198): entries strictly newer than
/// `last_version`, compared on the normalized `x.y.z` tuple. An unparsable
/// `last_version` yields no entries (upstream's `parts.some(Number.isNaN)`
/// guard).
pub fn get_new_entries(entries: &[ChangelogEntry], last_version: &str) -> Vec<ChangelogEntry> {
    let Some(last) = normalize_version(last_version) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|entry| entry.version_tuple() > last)
        .cloned()
        .collect()
}

/// The display half of `getChangelogForDisplay` (interactive-mode.ts
/// :1171-1196) WITHOUT the recording/telemetry side effects (those live in
/// `prepare_install_report`, D-046): given the PREVIOUS `last_version` and
/// the running `current_version`, returns the markdown to show, or `None`
/// when there is nothing new.
///
/// V18-12 FR-E: sections are compared on the normalized tuple. When the
/// normalized version did not move (RC→RC on the same base, or the RC→stable
/// fold), the exact version still changed, so the current version's section
/// is shown once instead of an empty block.
pub fn changelog_for_display(last_version: Option<&str>, current_version: &str) -> Option<String> {
    changelog_for_display_from(
        &parse_changelog(CHANGELOG_MD),
        last_version,
        current_version,
    )
}

fn changelog_for_display_from(
    entries: &[ChangelogEntry],
    last_version: Option<&str>,
    current_version: &str,
) -> Option<String> {
    let last_version = last_version?;
    let last = normalize_version(last_version)?;
    let current = normalize_version(current_version)?;
    // A downgrade never shows a changelog block.
    if current < last {
        return None;
    }
    // Exact `lastChangelogVersion` is the dedupe key: the same build (or a
    // `+build`-only change) shows nothing, even when the embedded changelog
    // already contains newer unreleased sections.
    if last_version.trim() == current_version.trim() {
        return None;
    }
    let new_entries: Vec<&ChangelogEntry> = entries
        .iter()
        .filter(|entry| entry.version_tuple() > last)
        .collect();
    if !new_entries.is_empty() {
        return Some(join_entries(&new_entries));
    }
    // A different exact version on the same normalized base (rc.1→rc.3,
    // rc.3→stable, `+build`) falls back to the current section once.
    entries
        .iter()
        .find(|entry| entry.version_tuple() == current)
        .map(|entry| entry.content.clone())
}

fn join_entries(entries: &[&ChangelogEntry]) -> String {
    entries
        .iter()
        .map(|entry| entry.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Section scanning semantics: `## [x.y.z]` headings collect until the
    /// next `## `, unparsable `## ` headings reset, pre-heading content is
    /// ignored (changelog.ts:124-161).
    #[test]
    fn parses_version_sections_and_ignores_unparsable_headings() {
        let entries = parse_changelog(
            "# Changelog\n\nintro ignored\n\n## [1.2.3] - day\n\nfirst\n\n## 0.9.0\n\nsecond\n\n## Notes\n\nnot a version\n\n## [2.0.0]\n\nthird\n",
        );
        let versions: Vec<(u32, u32, u32)> =
            entries.iter().map(ChangelogEntry::version_tuple).collect();
        assert_eq!(versions, vec![(1, 2, 3), (0, 9, 0), (2, 0, 0)]);
        assert!(entries[0].content.contains("## [1.2.3]"));
        assert!(entries[0].content.contains("first"));
        assert!(!entries[0].content.contains("second"));
        assert!(entries[1].content.contains("second"));
        // The unparsable "## Notes" reset dropped the "not a version" lines
        // from every section.
        assert!(!entries.iter().any(|e| e.content.contains("not a version")));
    }

    /// `getNewEntries` keeps strictly newer entries; an unparsable
    /// lastVersion yields nothing (changelog.ts:182-198).
    #[test]
    fn new_entries_compare_versions() {
        let entries = parse_changelog("## [1.0.0]\na\n\n## [1.1.0]\nb\n\n## [2.0.0]\nc\n");
        let new = get_new_entries(&entries, "1.0.0");
        assert_eq!(
            new.iter()
                .map(ChangelogEntry::version_tuple)
                .collect::<Vec<_>>(),
            vec![(1, 1, 0), (2, 0, 0)]
        );
        assert!(get_new_entries(&entries, "2.0.0").is_empty());
        assert!(get_new_entries(&entries, "not-a-version").is_empty());
    }

    /// V18-12 FR-E: prerelease/build suffixes are stripped for section
    /// comparison while `lastChangelogVersion` keeps the exact string.
    #[test]
    fn normalize_version_strips_prerelease_and_build_metadata() {
        assert_eq!(normalize_version("0.1.8-rc.3"), Some((0, 1, 8)));
        assert_eq!(normalize_version("0.1.8+build.7"), Some((0, 1, 8)));
        assert_eq!(normalize_version("v0.1.8"), Some((0, 1, 8)));
        assert_eq!(normalize_version("0.1.8-rc.3+sha.abc"), Some((0, 1, 8)));
        assert_eq!(normalize_version("not-a-version"), None);
        assert_eq!(normalize_version("1.2"), None);
        assert_eq!(normalize_version("1.2.3.4"), None);
    }

    /// V18-12 FR-E migration matrix: every version change that moved the
    /// normalized tuple shows the newer sections; a same-base RC/RC or
    /// RC/stable fold falls back to the current section once; downgrade and
    /// exact-version reruns show nothing.
    #[test]
    fn changelog_display_matrix_covers_rc_and_build_transitions() {
        let entries =
            parse_changelog("## [0.1.7]\n\nold\n\n## [0.1.8]\n\nmid\n\n## [0.1.9]\n\nnewer\n");
        let fold_entries = parse_changelog("## [0.1.7]\n\nold\n\n## [0.1.8]\n\nmid\n");
        let show =
            |last: &str, current: &str| changelog_for_display_from(&entries, Some(last), current);

        // stable→stable: sections above the last stable version.
        let text = show("0.1.7", "0.1.9").expect("new stable sections");
        assert!(text.contains("newer") && text.contains("mid"));
        // stable→RC on the next base shows the new sections.
        let text = show("0.1.7", "0.1.8-rc.2").expect("stable to RC");
        assert!(text.contains("mid"));
        // RC→RC on the same base (current is the top release) falls back to
        // the current section once.
        let text = changelog_for_display_from(&fold_entries, Some("0.1.8-rc.1"), "0.1.8-rc.3")
            .expect("rc to rc fallback");
        assert_eq!(text.matches("## [0.1.8]").count(), 1);
        // RC→stable fold on the same base.
        let text = changelog_for_display_from(&fold_entries, Some("0.1.8-rc.3"), "0.1.8")
            .expect("rc to stable fold");
        assert_eq!(text.matches("## [0.1.8]").count(), 1);
        // Exact-version rerun shows nothing.
        assert_eq!(
            changelog_for_display_from(&fold_entries, Some("0.1.8"), "0.1.8"),
            None
        );
        // A +build-only exact-version change falls back to the current
        // section once.
        let text = changelog_for_display_from(&fold_entries, Some("0.1.8+build.1"), "0.1.8")
            .expect("build suffix fallback");
        assert_eq!(text.matches("## [0.1.8]").count(), 1);
        // Downgrade shows nothing.
        assert_eq!(show("0.1.9", "0.1.8"), None);
        // Same exact version with a prerelease suffix is deduped.
        assert_eq!(show("0.1.8-rc.3", "0.1.8-rc.3"), None);
        // Unparsable last version yields nothing.
        assert_eq!(show("nope", "0.1.9"), None);
    }

    /// RC suffixes no longer break `getNewEntries` (the V18-12 root cause).
    #[test]
    fn new_entries_accept_prerelease_last_version() {
        let entries = parse_changelog("## [0.1.7]\na\n\n## [0.1.8]\nb\n");
        assert_eq!(get_new_entries(&entries, "0.1.7-rc.1").len(), 1);
        assert!(get_new_entries(&entries, "0.1.8-rc.3").is_empty());
    }

    /// The embedded asset parses and its top section matches VERSION
    /// (the display path is only useful while the asset tracks releases).
    /// V14-19：预发布构建（`0.1.4-rc.1`）比对**基线版本**——CHANGELOG.md
    /// 顶段对应候选目标版本，rc 序号不入 changelog。
    #[test]
    fn embedded_changelog_parses_and_tracks_version() {
        let entries = parse_changelog(CHANGELOG_MD);
        assert!(!entries.is_empty(), "embedded CHANGELOG.md has sections");
        let version = crate::config::VERSION;
        let base = version.split('-').next().unwrap_or(version);
        let parsed =
            normalize_version(base).unwrap_or_else(|| panic!("VERSION is x.y.z: {version}"));
        assert_eq!(
            entries[0].version_tuple(),
            parsed,
            "CHANGELOG.md top section must match VERSION {version} (base {base})"
        );
    }
}
