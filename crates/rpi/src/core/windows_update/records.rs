//! The on-disk state records (design §11.2). Every record carries a
//! `schemaVersion`; unknown fields are ignored so an additive revision still
//! loads. Reads are total: a missing, unreadable or malformed record is
//! `None`, and the caller decides what that means (a corrupt pending is
//! discarded, a corrupt result file is `idle`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::fs::UpdateFs;

pub const STATE_SCHEMA_VERSION: u32 = 1;

/// `update-result.json` `state`; the file's absence is [`UpdateState::Idle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    Staged,
    Applied,
    Confirmed,
    RolledBack,
    Degraded,
    /// Only in memory: no `update-result.json`.
    Idle,
}

impl UpdateState {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateState::Staged => "staged",
            UpdateState::Applied => "applied",
            UpdateState::Confirmed => "confirmed",
            UpdateState::RolledBack => "rolled_back",
            UpdateState::Degraded => "degraded",
            UpdateState::Idle => "idle",
        }
    }

    /// The states in which the old executable is kept by policy until the
    /// first normal exit after confirmation (design §11.9 "retained").
    pub fn keeps_old_exe(self) -> bool {
        matches!(self, UpdateState::Applied | UpdateState::Confirmed)
    }
}

/// `pending.json`: what `rpi update` staged and what the next start applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingUpdate {
    pub schema_version: u32,
    pub target_version: String,
    /// `stable` or `rc`: the channel `--rc` / `--version` selected.
    pub channel: String,
    /// ISO 8601 UTC; the startup re-verification compares it with the
    /// install manifest's `installedAt`.
    pub written_at: String,
    /// Version of the install the update was staged from.
    pub from_version: String,
    /// Staged by `--force` (downgrade) or `--version` (pin): the target need
    /// not be newer than the installed version.
    pub allow_non_monotonic: bool,
    /// The target implements the first-boot confirmation protocol.
    pub first_boot_protocol: bool,
    /// Set when the startup transaction committed; the record then stays
    /// until the first boot is confirmed or rolled back.
    #[serde(default)]
    pub applied: bool,
    pub binary: StagedBinary,
    #[serde(default)]
    pub extensions: Vec<StagedExtension>,
}

/// The staged executable and what the manifest says about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedBinary {
    /// Absolute path of the staged executable.
    pub staged_path: PathBuf,
    pub sha256: String,
    /// The release asset the executable was extracted from.
    pub source_url: String,
    /// sha256 of the release asset (the manifest's integrity value).
    pub asset_sha256: String,
    /// Build target triple recorded in the manifest.
    pub target: String,
}

/// One staged extension of an `--all` batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedExtension {
    pub name: String,
    pub version: String,
    /// The extensions root the container lives under.
    pub install_root: PathBuf,
    /// Absolute path of the extracted, verified staging directory.
    pub staged_path: PathBuf,
    /// Generation directory name under `<install_root>/<name>/`.
    pub generation: String,
    /// sha256 of the staged `rpi-extension.json`.
    pub manifest_sha256: String,
    /// Every staged file with its sha256, relative to `staged_path`.
    pub files: Vec<FileDigest>,
}

impl StagedExtension {
    /// The container `<install_root>/<name>/`.
    pub fn container(&self) -> PathBuf {
        self.install_root.join(&self.name)
    }

    /// The generation directory the staging directory is moved to.
    pub fn generation_path(&self) -> PathBuf {
        self.container().join(&self.generation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDigest {
    /// Relative path (platform separators).
    pub path: PathBuf,
    pub sha256: String,
}

/// `update-result.json`: the single state source plus the "read" marks of
/// the startup notices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateResult {
    pub schema_version: u32,
    pub state: UpdateState,
    /// Shown once in an interactive start, then marked read.
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub from_version: Option<String>,
    #[serde(default)]
    pub to_version: Option<String>,
    /// Why the update degraded or rolled back (log detail).
    #[serde(default)]
    pub reason: Option<String>,
    /// A normal exit happened after confirmation: the old executable may be
    /// removed by the cleanup scan.
    #[serde(default)]
    pub normal_exit_after_confirm: bool,
    /// Whether the applied update implemented the first-boot protocol.
    #[serde(default)]
    pub first_boot_protocol: bool,
}

impl UpdateResult {
    pub fn new(
        state: UpdateState,
        from_version: Option<String>,
        to_version: Option<String>,
    ) -> Self {
        UpdateResult {
            schema_version: STATE_SCHEMA_VERSION,
            state,
            read: false,
            from_version,
            to_version,
            reason: None,
            normal_exit_after_confirm: false,
            first_boot_protocol: false,
        }
    }
}

/// `blocked.json`: versions whose first boot crashed; only `--force` stages
/// them again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedVersions {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub versions: Vec<String>,
}

impl BlockedVersions {
    pub fn contains(&self, version: &str) -> bool {
        self.versions.iter().any(|blocked| blocked == version)
    }
}

/// `.update/boot/<pid>.json`: written by a new-version process while the
/// first boot is open; `normalExit` is set by the unified exit outlet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootRecord {
    pub schema_version: u32,
    pub pid: u32,
    /// Process start time in platform ticks (FILETIME on Windows), to tell
    /// a reused pid from the recorded process.
    pub process_start: u64,
    pub version: String,
    #[serde(default)]
    pub normal_exit: bool,
}

/// The identity of an executable image by metadata only (design §11.9):
/// its length and its modification time. A rename keeps both, so an
/// instance keeps matching the image it started from after a startup
/// transaction parked that image. The version string alone cannot tell a
/// parked image from the new image of the same version (a same-version
/// reinstall, R7.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExeIdentity {
    pub len: u64,
    pub modified_unix_ms: u64,
}

impl ExeIdentity {
    pub fn of(metadata: std::fs::Metadata) -> Option<Self> {
        let modified_unix_ms = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis();
        Some(ExeIdentity {
            len: metadata.len(),
            modified_unix_ms: u64::try_from(modified_unix_ms).unwrap_or(u64::MAX),
        })
    }
}

/// `.pins/<pid>.json`: one rpi process and the extension directories it
/// uses (Windows only; design §11.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinRecord {
    pub schema_version: u32,
    pub pid: u32,
    pub process_start: u64,
    /// Session identity (the harness session id when set, else a random id).
    pub session: String,
    pub exe_version: String,
    pub exe_path: PathBuf,
    /// Absolute generation or flat directories this process loads from,
    /// keyed by install root.
    pub dirs: Vec<PathBuf>,
    /// The image this process started from, by length and modification
    /// time; `None` when the metadata could not be read (the version string
    /// is the fallback then).
    #[serde(default)]
    pub exe_identity: Option<ExeIdentity>,
}

impl PinRecord {
    pub fn new(
        pid: u32,
        process_start: u64,
        session: String,
        exe_version: &str,
        exe_path: &Path,
    ) -> Self {
        PinRecord {
            schema_version: STATE_SCHEMA_VERSION,
            pid,
            process_start,
            session,
            exe_version: exe_version.to_owned(),
            exe_path: exe_path.to_path_buf(),
            dirs: Vec::new(),
            exe_identity: std::fs::metadata(exe_path).ok().and_then(ExeIdentity::of),
        }
    }
}

/// `journal.json`: the steps of a running startup transaction, each recorded
/// with its undo before it runs. An existing file is an uncommitted
/// transaction (the previous start died mid-way).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Journal {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub steps: Vec<JournalStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "camelCase")]
pub enum JournalStep {
    /// Move the extension staging directory to its generation directory.
    /// Undo: remove the generation directory (the pointer still names the
    /// previous generation, so nothing references it).
    PlaceExtension {
        name: String,
        staged_path: PathBuf,
        generation_path: PathBuf,
    },
    /// Point `<container>/current.json` at a generation. Undo: restore the
    /// previous text, or delete a pointer that did not exist.
    FlipPointer {
        container: PathBuf,
        previous_text: Option<String>,
        new_text: String,
    },
    /// Park the running executable as `old_name` and move the staged one in
    /// place. Undo: rename `old_name` back over the executable.
    ExeSwap { old_name: String },
    /// Write the new install manifest. Undo: restore the previous text, or
    /// delete a manifest that did not exist.
    WriteManifest { previous_text: Option<String> },
}

/// `undo.json`: the pre-image of an applied update, kept until the first
/// boot is confirmed or rolled back (design §11.7 "rollback flow").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoData {
    pub schema_version: u32,
    /// The install manifest text before the update (`None`: no manifest).
    pub old_manifest_text: Option<String>,
    /// The file the previous executable was parked as
    /// (`rpi.exe.old-<version>` or a timestamped variant).
    pub old_exe_name: String,
    /// Pointers as they were before the flip; a pointer that did not exist
    /// has `None`.
    pub pointers: Vec<PointerUndo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PointerUndo {
    pub container: PathBuf,
    pub previous_text: Option<String>,
    /// The text the transaction wrote; a rollback only restores a pointer
    /// that still carries it (a later `rpi update --extensions` is not
    /// undone by an unrelated rollback).
    pub new_text: String,
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

/// Serialize a record as pretty JSON with a trailing newline.
pub fn to_json<T: Serialize>(record: &T) -> String {
    let mut text = serde_json::to_string_pretty(record).unwrap_or_default();
    text.push('\n');
    text
}

/// Atomically write a record (`None` removes the file).
pub fn write_record<T: Serialize>(
    fs: &dyn UpdateFs,
    path: &Path,
    record: Option<&T>,
) -> std::io::Result<()> {
    match record {
        Some(record) => fs.write_atomic(path, to_json(record).as_bytes()),
        None => fs.remove_file(path),
    }
}

/// Read and parse a record; `None` when missing or malformed.
pub fn read_record<T: for<'de> Deserialize<'de>>(fs: &dyn UpdateFs, path: &Path) -> Option<T> {
    let bytes = fs.read(path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(text).ok()
}

/// Parse an ISO 8601 UTC timestamp with or without fractional seconds
/// (`now_iso8601()` writes milliseconds; install.ps1 writes whole seconds).
pub fn parse_iso8601(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|time| time.with_timezone(&chrono::Utc))
}

/// Whether `later` is strictly after `earlier`; unparseable inputs are
/// never "later" (a corrupt manifest does not invalidate a pending).
pub fn is_later(later: &str, earlier: &str) -> bool {
    match (parse_iso8601(later), parse_iso8601(earlier)) {
        (Some(later), Some(earlier)) => later > earlier,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::windows_update::fs::{RealFs, SpyFs};

    fn pending() -> PendingUpdate {
        PendingUpdate {
            schema_version: STATE_SCHEMA_VERSION,
            target_version: "0.1.9-rc.4".to_owned(),
            channel: "rc".to_owned(),
            written_at: "2026-11-28T00:00:00.000Z".to_owned(),
            from_version: "0.1.9-rc.3".to_owned(),
            allow_non_monotonic: false,
            first_boot_protocol: true,
            applied: false,
            binary: StagedBinary {
                staged_path: PathBuf::from("/p/.update/staging/0.1.9-rc.4/rpi.exe"),
                sha256: "a".repeat(64),
                source_url: "https://example.invalid/r.zip".to_owned(),
                asset_sha256: "b".repeat(64),
                target: "x86_64-pc-windows-msvc".to_owned(),
            },
            extensions: vec![StagedExtension {
                name: "rpi-todo".to_owned(),
                version: "0.1.9-rc.4".to_owned(),
                install_root: PathBuf::from("/r/extensions"),
                staged_path: PathBuf::from("/r/extensions/.staging/rpi-todo-0.1.9-rc.4-7"),
                generation: "0.1.9-rc.4".to_owned(),
                manifest_sha256: "c".repeat(64),
                files: vec![FileDigest {
                    path: PathBuf::from("rpi-extension.json"),
                    sha256: "c".repeat(64),
                }],
            }],
        }
    }

    #[test]
    fn pending_round_trips_with_camel_case_fields() {
        let text = to_json(&pending());
        assert!(text.contains("\"targetVersion\": \"0.1.9-rc.4\""), "{text}");
        assert!(text.contains("\"firstBootProtocol\": true"), "{text}");
        assert!(text.contains("\"allowNonMonotonic\": false"), "{text}");
        let parsed: PendingUpdate = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, pending());
        assert_eq!(
            parsed.extensions[0].generation_path(),
            PathBuf::from("/r/extensions/rpi-todo/0.1.9-rc.4")
        );
    }

    #[test]
    fn records_tolerate_missing_additive_fields_and_reject_garbage() {
        // `applied` and `extensions` are additive: an older pending loads.
        let legacy: PendingUpdate = serde_json::from_str(
            r#"{"schemaVersion":1,"targetVersion":"1.0.0","channel":"stable","writtenAt":"2026-11-28T00:00:00Z","fromVersion":"0.9.0","allowNonMonotonic":false,"firstBootProtocol":true,"binary":{"stagedPath":"/x/rpi.exe","sha256":"a","sourceUrl":"u","assetSha256":"b","target":"t"}}"#,
        )
        .unwrap();
        assert!(!legacy.applied);
        assert!(legacy.extensions.is_empty());
        let result: UpdateResult =
            serde_json::from_str(r#"{"schemaVersion":1,"state":"applied"}"#).unwrap();
        assert_eq!(result.state, UpdateState::Applied);
        assert!(!result.read);
        let corrupt = serde_json::from_str::<PendingUpdate>(r#"{"schemaVersion":1}"#);
        assert!(corrupt.is_err());
        let unknown_state =
            serde_json::from_str::<UpdateResult>(r#"{"schemaVersion":1,"state":"pending"}"#);
        assert!(
            unknown_state.is_err(),
            "an unknown state is not silently accepted"
        );
    }

    #[test]
    fn state_names_match_the_design() {
        for (state, name) in [
            (UpdateState::Staged, "staged"),
            (UpdateState::Applied, "applied"),
            (UpdateState::Confirmed, "confirmed"),
            (UpdateState::RolledBack, "rolled_back"),
            (UpdateState::Degraded, "degraded"),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), name);
            assert_eq!(state.as_str(), name);
        }
        assert!(UpdateState::Applied.keeps_old_exe());
        assert!(UpdateState::Confirmed.keeps_old_exe());
        assert!(!UpdateState::RolledBack.keeps_old_exe());
    }

    #[test]
    fn journal_steps_are_tagged_and_reversible() {
        let journal = Journal {
            schema_version: STATE_SCHEMA_VERSION,
            steps: vec![
                JournalStep::PlaceExtension {
                    name: "rpi-todo".to_owned(),
                    staged_path: PathBuf::from("/s"),
                    generation_path: PathBuf::from("/g"),
                },
                JournalStep::FlipPointer {
                    container: PathBuf::from("/c"),
                    previous_text: None,
                    new_text: "{}".to_owned(),
                },
                JournalStep::ExeSwap {
                    old_name: "rpi.exe.old-1".to_owned(),
                },
                JournalStep::WriteManifest {
                    previous_text: Some("{}".to_owned()),
                },
            ],
        };
        let text = to_json(&journal);
        assert!(text.contains(r#""step": "placeExtension""#), "{text}");
        assert!(text.contains(r#""step": "flipPointer""#), "{text}");
        assert!(text.contains(r#""step": "exeSwap""#), "{text}");
        assert!(text.contains(r#""step": "writeManifest""#), "{text}");
        assert_eq!(serde_json::from_str::<Journal>(&text).unwrap(), journal);
    }

    #[test]
    fn write_and_read_records_through_the_file_system() {
        let fs = SpyFs::new();
        let dir = std::env::temp_dir().join(format!("rpi-wu-records-{}", std::process::id()));
        let path = dir.join("pending.json");
        write_record(&fs, &path, Some(&pending())).unwrap();
        let loaded: PendingUpdate = read_record(&fs, &path).unwrap();
        assert_eq!(loaded, pending());
        write_record::<PendingUpdate>(&fs, &path, None).unwrap();
        assert!(read_record::<PendingUpdate>(&fs, &path).is_none());
        write_record(&fs, &path, Some(&pending())).unwrap();
        // A BOM is accepted (PowerShell 5.1 rewrites files with one).
        RealFs
            .write_atomic(&path, "\u{feff}".to_string().as_bytes())
            .unwrap();
        assert!(read_record::<PendingUpdate>(&fs, &path).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn timestamps_compare_across_formats() {
        assert!(is_later("2026-11-28T00:00:01Z", "2026-11-28T00:00:00.500Z"));
        assert!(!is_later(
            "2026-11-28T00:00:00.000Z",
            "2026-11-28T00:00:00Z"
        ));
        assert!(is_later("2026-11-28T00:00:00.001Z", "2026-11-28T00:00:00Z"));
        assert!(!is_later("garbage", "2026-11-28T00:00:00Z"));
        assert!(!is_later("2026-11-28T00:00:00Z", "garbage"));
        assert!(parse_iso8601("2026-11-27T21:57:23+08:00").is_some());
    }

    #[test]
    fn blocked_versions_default_to_an_empty_list() {
        let blocked: BlockedVersions = serde_json::from_str("{}").unwrap();
        assert!(blocked.versions.is_empty());
        assert!(!blocked.contains("1.0.0"));
    }
}
