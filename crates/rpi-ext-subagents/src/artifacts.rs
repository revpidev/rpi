//! Artifact directory selection, file naming, metadata persistence and the
//! 24h-throttled age cleanup (FR-P0-09).
//!
//! Port of pi-subagents `src/shared/artifacts.ts` @ v0.48.0 (56f97234) plus
//! `DEFAULT_ARTIFACT_CONFIG` / `resolveTempScopeId` from types.ts. The npm
//! packaging warning (`getProjectArtifactPackagingWarning`) is not ported: it
//! inspects package.json/.npmignore for npm publish leakage, which has no
//! rpi equivalent (deviation TE-D18).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::paths;

pub const CLEANUP_MARKER_FILE: &str = ".last-cleanup";
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactDirPreference {
    Project,
    Session,
    Temp,
}

impl ArtifactDirPreference {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            Some("project") => Ok(Self::Project),
            Some("session") => Ok(Self::Session),
            Some("temp") => Ok(Self::Temp),
            other => Err(format!(
                "Unsupported artifactDir {:?}; expected \"project\", \"session\", or \"temp\".",
                other
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactPaths {
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub jsonl_path: PathBuf,
    pub transcript_path: PathBuf,
    pub metadata_path: PathBuf,
}

impl ArtifactPaths {
    pub fn to_json(&self) -> Value {
        json!({
            "inputPath": self.input_path.to_string_lossy(),
            "outputPath": self.output_path.to_string_lossy(),
            "jsonlPath": self.jsonl_path.to_string_lossy(),
            "transcriptPath": self.transcript_path.to_string_lossy(),
            "metadataPath": self.metadata_path.to_string_lossy(),
        })
    }
}

/// `getArtifactsDir` (artifacts.ts:160-184).
pub fn get_artifacts_dir(
    session_file: Option<&Path>,
    project_cwd: Option<&Path>,
    preference: ArtifactDirPreference,
) -> PathBuf {
    let session_artifacts = |session_file: Option<&Path>| match session_file {
        Some(file) => file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("subagent-artifacts"),
        None => paths::temp_artifacts_dir(),
    };
    match preference {
        ArtifactDirPreference::Session => session_artifacts(session_file),
        ArtifactDirPreference::Temp => paths::temp_artifacts_dir(),
        ArtifactDirPreference::Project => match project_cwd {
            Some(cwd) => paths::get_project_artifacts_dir(cwd),
            None => session_artifacts(session_file),
        },
    }
}

/// `getArtifactPaths` (artifacts.ts:186-197). `safe_agent` keeps `[^\w.-]` → `_`.
pub fn get_artifact_paths(
    artifacts_dir: &Path,
    run_id: &str,
    agent: &str,
    index: Option<u32>,
) -> ArtifactPaths {
    let safe_agent: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let base = match index {
        Some(index) => format!("{run_id}_{safe_agent}_{index}"),
        None => format!("{run_id}_{safe_agent}"),
    };
    ArtifactPaths {
        input_path: artifacts_dir.join(format!("{base}_input.md")),
        output_path: artifacts_dir.join(format!("{base}_output.md")),
        jsonl_path: artifacts_dir.join(format!("{base}.jsonl")),
        transcript_path: artifacts_dir.join(format!("{base}_transcript.jsonl")),
        metadata_path: artifacts_dir.join(format!("{base}_meta.json")),
    }
}

pub fn ensure_artifacts_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// `writeArtifact` (artifacts.ts:203-206) — creates parents, plain write.
pub fn write_artifact(file_path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file_path, content)
}

/// `formatOutputArtifactContent` (artifacts.ts:208-219).
pub fn format_output_artifact_content(
    output: &str,
    error: Option<&str>,
    transcript_path: Option<&Path>,
    metadata_path: Option<&Path>,
) -> String {
    if !output.trim().is_empty() || error.is_none() {
        return output.to_string();
    }
    let mut lines = vec![
        "Subagent run failed before producing output.".to_string(),
        String::new(),
        format!("Error:{}", error.unwrap_or("")),
    ];
    if let Some(transcript) = transcript_path {
        lines.push(String::new());
        lines.push(format!("Transcript: {}", transcript.to_string_lossy()));
    }
    if let Some(metadata) = metadata_path {
        lines.push(format!("Metadata: {}", metadata.to_string_lossy()));
    }
    lines.join("\n")
}

/// `writeMetadata` (artifacts.ts:221-224) — two-space indented JSON, written
/// atomically (tmp file + rename, upstream `writeAtomicJson`): concurrent
/// readers never observe a torn document and multi-writer races resolve to
/// one whole file.
pub fn write_metadata(file_path: &Path, metadata: &Value) -> std::io::Result<()> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(metadata).unwrap_or_default();
    let tmp = file_path.with_extension(format!("tmp-{}", std::process::id() as u64 ^ nanos_now()));
    std::fs::write(&tmp, body)?;
    // Same-directory rename is atomic on unix; propagate its error after
    // cleaning the leftover tmp file.
    let result = std::fs::rename(&tmp, file_path);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn nanos_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub fn append_jsonl(file_path: &Path, line: &str) {
    use std::io::Write;
    if let Some(parent) = file_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// `cleanupOldArtifacts` (artifacts.ts:230-259): 24h-throttled via the
/// `.last-cleanup` marker, first directory level only, best-effort unlinks.
pub fn cleanup_old_artifacts(dir: &Path, max_age_days: u64) {
    if max_age_days == 0 || !dir.exists() {
        return;
    }
    let marker_path = dir.join(CLEANUP_MARKER_FILE);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if let Ok(meta) = std::fs::metadata(&marker_path) {
        if let Ok(modified) = meta.modified() {
            let mtime = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if (now as u128) < mtime + DAY_MS as u128 {
                return;
            }
        }
    }
    let cutoff = now.saturating_sub(max_age_days * DAY_MS);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy() == CLEANUP_MARKER_FILE {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let mtime = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if mtime < cutoff {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let _ = std::fs::write(&marker_path, now.to_string());
}

/// `cleanupAllArtifactDirs` (artifacts.ts:261-285): temp artifacts + every
/// `<agentDir>/sessions/*/subagent-artifacts`.
pub fn cleanup_all_artifact_dirs(max_age_days: u64) {
    cleanup_old_artifacts(&paths::temp_artifacts_dir(), max_age_days);
    // Chain scratch dirs: user-scoped temp roots get the 24h sweep
    // (settings.ts cleanupOldChainDirs L197-215); project-local roots are
    // never age-scanned, matching upstream.
    crate::p1::chain::cleanup_old_chain_dirs(
        &paths::temp_root_dir().join("chain-runs"),
        crate::p1::chain::CHAIN_DIR_MAX_AGE_MS,
    );
    let sessions_base = paths::get_agent_dir().join("sessions");
    let Ok(dirs) = std::fs::read_dir(&sessions_base) else {
        return;
    };
    for dir in dirs.flatten() {
        let artifacts_dir = dir.path().join("subagent-artifacts");
        cleanup_old_artifacts(&artifacts_dir, max_age_days);
    }
}

/// RFC3339 UTC timestamp with milliseconds, the rpi session/`timestamp`
/// convention (`new Date().toISOString()` upstream).
pub fn format_iso8601(unix_millis: u64) -> String {
    let seconds = unix_millis / 1000;
    let millis = unix_millis % 1000;
    let days = seconds / 86_400;
    let time_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Days-since-epoch to civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u64, d as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_paths_naming_and_sanitization() {
        let dir = Path::new("/work/.rpi/subagents/artifacts");
        let paths = get_artifact_paths(dir, "ab12cd34", "scout", Some(0));
        assert_eq!(
            paths.input_path.file_name().unwrap(),
            "ab12cd34_scout_0_input.md"
        );
        assert_eq!(
            paths.output_path.file_name().unwrap(),
            "ab12cd34_scout_0_output.md"
        );
        assert_eq!(
            paths.jsonl_path.file_name().unwrap(),
            "ab12cd34_scout_0.jsonl"
        );
        assert_eq!(
            paths.transcript_path.file_name().unwrap(),
            "ab12cd34_scout_0_transcript.jsonl"
        );
        assert_eq!(
            paths.metadata_path.file_name().unwrap(),
            "ab12cd34_scout_0_meta.json"
        );
        let paths = get_artifact_paths(dir, "ab12cd34", "weird/name!", None);
        assert_eq!(
            paths.output_path.file_name().unwrap(),
            "ab12cd34_weird_name__output.md"
        );
    }

    #[test]
    fn dir_preference_fallbacks() {
        // project without cwd falls back to session, then temp
        assert_eq!(
            get_artifacts_dir(
                Some(Path::new("/s/x.jsonl")),
                None,
                ArtifactDirPreference::Project
            ),
            Path::new("/s/subagent-artifacts")
        );
        assert_eq!(
            get_artifacts_dir(None, None, ArtifactDirPreference::Project),
            paths::temp_artifacts_dir()
        );
        assert_eq!(
            get_artifacts_dir(
                None,
                Some(Path::new("/repo")),
                ArtifactDirPreference::Project
            ),
            Path::new("/repo/.rpi/subagents/artifacts")
        );
    }

    #[test]
    fn cleanup_respects_marker_throttle_and_age() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.jsonl");
        std::fs::write(&old, "x").unwrap();
        // Backdate beyond cleanupDays and the marker window.
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86_400);
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(stale).unwrap();
        drop(file);
        cleanup_old_artifacts(&dir, 7);
        assert!(!old.exists(), "aged artifact removed");
        assert!(dir.join(CLEANUP_MARKER_FILE).exists(), "marker written");
        // Second call inside 24h is a no-op — a fresh aged file survives.
        let old2 = dir.join("old2.jsonl");
        std::fs::write(&old2, "x").unwrap();
        let file = std::fs::File::options().write(true).open(&old2).unwrap();
        file.set_modified(stale).unwrap();
        drop(file);
        cleanup_old_artifacts(&dir, 7);
        assert!(old2.exists(), "throttled by 24h marker");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn iso_format_matches_rfc3339() {
        assert_eq!(format_iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_iso8601(1_755_168_000_123),
            "2025-08-14T10:40:00.123Z"
        );
    }
}
