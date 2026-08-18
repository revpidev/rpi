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

/// `getNewEntries` (changelog.ts:182-198): entries strictly newer than
/// `last_version` (`x.y.z` string). An unparsable `last_version` yields no
/// entries (upstream's `parts.some(Number.isNaN)` guard).
pub fn get_new_entries(entries: &[ChangelogEntry], last_version: &str) -> Vec<ChangelogEntry> {
    let Some(last) = parse_version_string(last_version) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|entry| entry.version_tuple() > last)
        .cloned()
        .collect()
}

/// `getNewEntries`'s `lastVersion` parsing (`v.split(".").map(Number)`,
/// NaN-guarded).
fn parse_version_string(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
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

/// The display half of `getChangelogForDisplay` (interactive-mode.ts
/// :1171-1196) WITHOUT the recording/telemetry side effects (those live in
/// `prepare_install_report`, D-046): given the PREVIOUS `last_version`,
/// returns the joined new-entry markdown, or `None` when there is nothing
/// new to show.
pub fn changelog_for_display(last_version: Option<&str>) -> Option<String> {
    let last_version = last_version?;
    let new_entries = get_new_entries(&parse_changelog(CHANGELOG_MD), last_version);
    if new_entries.is_empty() {
        return None;
    }
    Some(
        new_entries
            .iter()
            .map(|entry| entry.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
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

    /// The embedded asset parses and its top section matches VERSION
    /// (the display path is only useful while the asset tracks releases).
    #[test]
    fn embedded_changelog_parses_and_tracks_version() {
        let entries = parse_changelog(CHANGELOG_MD);
        assert!(!entries.is_empty(), "embedded CHANGELOG.md has sections");
        let version = crate::config::VERSION;
        let parsed =
            parse_version_string(version).unwrap_or_else(|| panic!("VERSION is x.y.z: {version}"));
        assert_eq!(
            entries[0].version_tuple(),
            parsed,
            "CHANGELOG.md top section must match VERSION {version}"
        );
    }
}
