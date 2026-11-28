//! The rollback of an applied update (design §11.7 "rollback flow").
//!
//! Three paths share it: the trampoline saw its new instance crash, the
//! next start found a crashed first-boot record (the trampoline was not
//! around any more), or the trampoline could not start the new executable
//! at all. It relies on `undo.json` — the pre-image persisted before the
//! transaction's first change — and is idempotent by target state, so a
//! rollback that died half-way is simply finished by the next start.
//!
//! The caller holds the update lock. Nothing here terminates a process: a
//! new-version instance that still runs keeps running from the image that
//! was set aside for it.

use std::path::{Path, PathBuf};

use super::fs::UpdateFs;
use super::journal::restore_text;
use super::layout::UpdatePaths;
use super::records::{
    BlockedVersions, PendingUpdate, STATE_SCHEMA_VERSION, UndoData, UpdateResult, UpdateState,
    read_record, to_json, write_record,
};

/// Why an applied update is being rolled back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackReason {
    /// The new version terminated before its first confirmation.
    FirstBootCrash,
    /// The trampoline could not start the new executable (the version is
    /// not blocked for that).
    LaunchFailed,
}

impl RollbackReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RollbackReason::FirstBootCrash => "first boot crashed",
            RollbackReason::LaunchFailed => "the new executable could not be started",
        }
    }
}

/// What the rollback achieved.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RollbackOutcome {
    /// The previous executable is back at its path.
    pub exe_restored: bool,
    /// Where the new image was set aside (it is deleted when nobody runs
    /// it; the cleanup scan finishes the job later).
    pub parked_new_exe: Option<PathBuf>,
    /// Steps that failed (already logged by the caller through `log`).
    pub failures: Vec<String>,
}

impl RollbackOutcome {
    pub fn ok(&self) -> bool {
        self.exe_restored && self.failures.is_empty()
    }
}

/// The first-boot reminder text after a rollback: the reason line shown on
/// stderr and in the next interactive start.
pub fn rollback_message(from_version: &str, to_version: &str, log_path: &Path) -> String {
    format!(
        "The update to rpi {to_version} failed and was rolled back to rpi {from_version}. See {}.",
        log_path.display()
    )
}

/// Roll back the applied `pending`. `log` receives one line per event.
pub fn rollback_applied_update(
    fs: &dyn UpdateFs,
    paths: &UpdatePaths,
    pending: &PendingUpdate,
    reason: RollbackReason,
    log: &dyn Fn(&str),
) -> RollbackOutcome {
    let mut outcome = RollbackOutcome::default();
    let undo: Option<UndoData> = read_record(fs, &paths.undo_path());
    if undo.is_none() {
        log("rollback: undo.json is missing; restoring the executable by its parked name only");
        outcome.failures.push("undo.json is missing".to_owned());
    }
    let prefix = paths
        .exe_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let old_name = undo
        .as_ref()
        .map(|undo| undo.old_exe_name.clone())
        .unwrap_or_else(|| {
            paths
                .old_exe_path(&pending.from_version)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let old_path = prefix.join(&old_name);

    // 1. The executable at its path is still the new one → set it aside.
    // The undo record is the proof that the swap ever happened: without
    // it a matching digest proves nothing for a same-version reinstall
    // (R7.2.3), whose previous executable is the very image the update
    // would have installed, so the executable is not touched then.
    let image_is_new = fs
        .sha256_file(&paths.exe_path)
        .is_ok_and(|sha| sha.eq_ignore_ascii_case(&pending.binary.sha256));
    let at_path_is_new = undo.is_some() && image_is_new;
    if at_path_is_new {
        let parked = paths.unused_sibling(paths.bad_exe_path(&pending.target_version));
        match rename(fs, &paths.exe_path, &parked) {
            Ok(()) => {
                outcome.parked_new_exe = Some(parked.clone());
                log(&format!(
                    "rollback: new executable set aside as {}",
                    parked.display()
                ));
                // Removed when no new-version instance runs it; a failure
                // is expected then and is not a rollback failure.
                if fs.remove_file(&parked).is_ok() {
                    log("rollback: new executable deleted");
                    outcome.parked_new_exe = None;
                }
            }
            Err(error) => {
                outcome.failures.push(format!(
                    "could not set the new executable aside ({}): {error}",
                    paths.exe_path.display()
                ));
            }
        }
    } else if undo.is_none() {
        log("rollback: no undo record; the executable at its path is left untouched");
    } else {
        log("rollback: the executable at its path is already the previous version");
    }
    // Nothing of the update replaced the executable: no undo record, and
    // either the image in place is not the new one or the update would
    // have installed the same version (a same-version reinstall whose
    // transaction never ran). A missing executable at its path is never
    // "nothing landed": that is a broken installation to report, not a
    // restored one.
    let nothing_landed = undo.is_none()
        && fs.exists(&paths.exe_path).unwrap_or(false)
        && (!image_is_new || pending.from_version == pending.target_version);

    // 2. The previous executable back to its path.
    let parked_old_exists = fs.exists(&old_path).unwrap_or(false);
    if parked_old_exists {
        match rename(fs, &old_path, &paths.exe_path) {
            Ok(()) => {
                outcome.exe_restored = true;
                log("rollback: previous executable restored");
            }
            Err(error) => outcome.failures.push(format!(
                "could not restore the previous executable ({}): {error}",
                old_path.display()
            )),
        }
    } else if nothing_landed {
        log("rollback: nothing of the update landed; the executable in place is the previous one");
        outcome.exe_restored = true;
    } else if image_is_new || !fs.exists(&paths.exe_path).unwrap_or(false) {
        outcome.failures.push(format!(
            "the previous executable {} is missing; reinstall with install.ps1",
            old_path.display()
        ));
    } else {
        // Neither the parked image exists nor the path holds the new image:
        // someone (install.ps1 preflight, a manual rename) already restored it.
        outcome.exe_restored = true;
    }

    // 3. Pointers, only those still carrying the text the transaction wrote.
    for pointer in undo.iter().flat_map(|undo| undo.pointers.iter()) {
        let path = pointer
            .container
            .join(rpi_ext_host::install_layout::GENERATION_POINTER_FILE);
        match fs.read(&path) {
            Ok(current) if current == pointer.new_text.as_bytes() => {
                if let Err(error) = restore_text(fs, &path, pointer.previous_text.as_deref()) {
                    outcome.failures.push(format!(
                        "could not restore the pointer of {}: {error}",
                        pointer.container.display()
                    ));
                }
            }
            _ => {}
        }
        rpi_ext_host::install_layout::refresh_install_resolution(&pointer.container);
    }

    // 4. The install manifest.
    if let Some(undo) = &undo
        && let Err(error) =
            restore_text(fs, &paths.manifest_path, undo.old_manifest_text.as_deref())
    {
        outcome
            .failures
            .push(format!("could not restore the install manifest: {error}"));
    }

    // 5. Block the version (a crash only), record the result, drop the
    //    first-boot state.
    if reason == RollbackReason::FirstBootCrash {
        let mut blocked =
            read_record::<BlockedVersions>(fs, &paths.blocked_path()).unwrap_or(BlockedVersions {
                schema_version: STATE_SCHEMA_VERSION,
                versions: Vec::new(),
            });
        blocked.schema_version = STATE_SCHEMA_VERSION;
        if !blocked.contains(&pending.target_version) {
            blocked.versions.push(pending.target_version.clone());
        }
        let _ = write_record(fs, &paths.blocked_path(), Some(&blocked));
    }
    let mut result = UpdateResult::new(
        UpdateState::RolledBack,
        Some(pending.from_version.clone()),
        Some(pending.target_version.clone()),
    );
    result.reason = Some(reason.as_str().to_owned());
    result.first_boot_protocol = pending.first_boot_protocol;
    let _ = write_record(fs, &paths.result_path(), Some(&result));
    let _ = write_record::<UndoData>(fs, &paths.undo_path(), None);
    let _ = write_record::<PendingUpdate>(fs, &paths.pending_path(), None);
    super::boot::clear_boot_records(fs, &paths.boot_dir());
    outcome
}

fn rename(fs: &dyn UpdateFs, from: &Path, to: &Path) -> std::io::Result<()> {
    super::fs::rename_with_retry(fs, from, to, super::RENAME_RETRIES, &|duration| {
        fs.retry_sleep(duration)
    })
}

/// Convenience: the serialized form of a record, for assertions.
pub fn serialized<T: serde::Serialize>(record: &T) -> String {
    to_json(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::windows_update::boot::FakeProcessProbe;
    use crate::core::windows_update::fs::SpyFs;
    use crate::core::windows_update::records::StagedBinary;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        dir: PathBuf,
        fs: SpyFs,
        paths: UpdatePaths,
        pending: PendingUpdate,
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rpi-wu-rollback-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            let fs = SpyFs::new();
            let paths = UpdatePaths::new(
                &dir.join("prefix/rpi.exe"),
                dir.join("logs/update.log"),
                &dir.join("agent"),
                None,
            );
            let pending = PendingUpdate {
                schema_version: 1,
                target_version: "0.1.9-rc.4".to_owned(),
                channel: "stable".to_owned(),
                written_at: "2026-11-28T00:00:00Z".to_owned(),
                from_version: "0.1.9-rc.3".to_owned(),
                allow_non_monotonic: false,
                first_boot_protocol: true,
                applied: true,
                binary: StagedBinary {
                    staged_path: dir.join("prefix/.update/staging/0.1.9-rc.4/rpi.exe"),
                    sha256: crate::core::windows_update::fs::sha256_bytes(b"new exe"),
                    source_url: String::new(),
                    asset_sha256: "0".repeat(64),
                    target: "t".to_owned(),
                },
                extensions: Vec::new(),
            };
            let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let _ = FakeProcessProbe::new(1, 1);
            Fixture {
                dir,
                fs,
                paths,
                pending,
                log,
            }
        }

        fn applied_state(&self) {
            let prefix = self.dir.join("prefix");
            std::fs::create_dir_all(prefix.join(".update/boot")).unwrap();
            std::fs::write(prefix.join("rpi.exe"), b"new exe").unwrap();
            std::fs::write(prefix.join("rpi.exe.old-0.1.9-rc.3"), b"old exe").unwrap();
            std::fs::write(&self.paths.manifest_path, br#"{"version":"0.1.9-rc.4"}"#).unwrap();
            std::fs::write(self.paths.undo_path(), "{{").unwrap();
            std::fs::write(self.paths.pending_path(), serialized(&self.pending)).unwrap();
            let container = self.dir.join("agent/extensions/rpi-todo");
            std::fs::create_dir_all(container.join("0.1.9-rc.4")).unwrap();
            std::fs::write(container.join("current.json"), r#"{"new":"pointer"}"#).unwrap();
            let undo = UndoData {
                schema_version: 1,
                old_manifest_text: Some(r#"{"version":"0.1.9-rc.3"}"#.to_owned()),
                old_exe_name: "rpi.exe.old-0.1.9-rc.3".to_owned(),
                pointers: vec![super::super::records::PointerUndo {
                    container: container.clone(),
                    previous_text: Some(r#"{"old":"pointer"}"#.to_owned()),
                    new_text: r#"{"new":"pointer"}"#.to_owned(),
                }],
            };
            self.fs
                .write_atomic(&self.paths.undo_path(), serialized(&undo).as_bytes())
                .unwrap();
            self.fs
                .write_atomic(
                    &self.paths.boot_dir().join("77.json"),
                    br#"{"schemaVersion":1,"pid":77,"processStart":5,"version":"0.1.9-rc.4","normalExit":false}"#,
                )
                .unwrap();
            self.fs
                .write_atomic(
                    &self.paths.result_path(),
                    br#"{"schemaVersion":1,"state":"applied","read":true}"#,
                )
                .unwrap();
        }

        fn sink(&self) -> impl Fn(&str) + '_ {
            move |message: &str| self.log.lock().unwrap().push(message.to_owned())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_crash_rollback_restores_everything_and_blocks_the_version() {
        let f = Fixture::new();
        f.applied_state();
        let outcome = rollback_applied_update(
            &f.fs,
            &f.paths,
            &f.pending,
            RollbackReason::FirstBootCrash,
            &f.sink(),
        );
        assert!(outcome.ok(), "{outcome:?}");
        assert!(outcome.exe_restored);
        let prefix = f.dir.join("prefix");
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"old exe");
        assert!(!prefix.join("rpi.exe.old-0.1.9-rc.3").exists());
        assert!(
            !prefix.join("rpi.exe.bad-0.1.9-rc.4").exists(),
            "the new image nobody runs is deleted"
        );
        assert_eq!(
            std::fs::read_to_string(&f.paths.manifest_path).unwrap(),
            r#"{"version":"0.1.9-rc.3"}"#
        );
        let container = f.dir.join("agent/extensions/rpi-todo");
        assert_eq!(
            std::fs::read_to_string(container.join("current.json")).unwrap(),
            r#"{"old":"pointer"}"#
        );
        let blocked = read_record::<BlockedVersions>(&f.fs, &f.paths.blocked_path()).unwrap();
        assert!(blocked.contains("0.1.9-rc.4"));
        let result = read_record::<UpdateResult>(&f.fs, &f.paths.result_path()).unwrap();
        assert_eq!(result.state, UpdateState::RolledBack);
        assert_eq!(result.from_version.as_deref(), Some("0.1.9-rc.3"));
        assert_eq!(result.to_version.as_deref(), Some("0.1.9-rc.4"));
        assert_eq!(result.reason.as_deref(), Some("first boot crashed"));
        assert!(!result.read);
        assert!(read_record::<PendingUpdate>(&f.fs, &f.paths.pending_path()).is_none());
        assert!(read_record::<UndoData>(&f.fs, &f.paths.undo_path()).is_none());
        assert!(!f.paths.boot_dir().join("77.json").exists());
        assert!(
            f.log
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("previous executable restored")),
            "{:?}",
            f.log.lock().unwrap()
        );

        // Running it again (a rollback that was interrupted after the exe
        // step) finds the target state and reports no failure.
        let again = rollback_applied_update(
            &f.fs,
            &f.paths,
            &f.pending,
            RollbackReason::FirstBootCrash,
            &f.sink(),
        );
        assert!(again.exe_restored, "{again:?}");
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"old exe");
    }

    #[test]
    fn a_launch_failure_rollback_does_not_block_the_version() {
        let f = Fixture::new();
        f.applied_state();
        rollback_applied_update(
            &f.fs,
            &f.paths,
            &f.pending,
            RollbackReason::LaunchFailed,
            &f.sink(),
        );
        assert!(read_record::<BlockedVersions>(&f.fs, &f.paths.blocked_path()).is_none());
        let result = read_record::<UpdateResult>(&f.fs, &f.paths.result_path()).unwrap();
        assert_eq!(
            result.reason.as_deref(),
            Some("the new executable could not be started")
        );
    }

    #[test]
    fn a_pointer_changed_after_the_update_is_not_undone() {
        let f = Fixture::new();
        f.applied_state();
        let container = f.dir.join("agent/extensions/rpi-todo");
        std::fs::write(container.join("current.json"), r#"{"later":"update"}"#).unwrap();
        let outcome = rollback_applied_update(
            &f.fs,
            &f.paths,
            &f.pending,
            RollbackReason::FirstBootCrash,
            &f.sink(),
        );
        assert!(outcome.ok(), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(container.join("current.json")).unwrap(),
            r#"{"later":"update"}"#
        );
    }

    #[test]
    fn a_new_image_that_cannot_be_deleted_is_parked_for_the_cleanup_scan() {
        let f = Fixture::new();
        f.applied_state();
        f.fs.fail(
            crate::core::windows_update::fs::FsOp::RemoveFile,
            "rpi.exe.bad-",
            1,
        );
        let outcome = rollback_applied_update(
            &f.fs,
            &f.paths,
            &f.pending,
            RollbackReason::FirstBootCrash,
            &f.sink(),
        );
        assert!(outcome.ok(), "{outcome:?}");
        assert!(outcome.parked_new_exe.is_some());
        assert!(f.dir.join("prefix/rpi.exe.bad-0.1.9-rc.4").exists());
    }

    #[test]
    fn a_missing_previous_executable_is_reported_not_hidden() {
        let f = Fixture::new();
        f.applied_state();
        std::fs::remove_file(f.dir.join("prefix/rpi.exe.old-0.1.9-rc.3")).unwrap();
        let outcome = rollback_applied_update(
            &f.fs,
            &f.paths,
            &f.pending,
            RollbackReason::FirstBootCrash,
            &f.sink(),
        );
        assert!(!outcome.exe_restored);
        assert!(
            outcome
                .failures
                .iter()
                .any(|failure| failure.contains("reinstall")),
            "{outcome:?}"
        );
        assert!(
            !f.dir.join("prefix/rpi.exe").exists(),
            "the new image was still set aside"
        );
    }

    /// A pending that was marked applied without its transaction ever
    /// running (round-2 review: a same-version reinstall taken for a
    /// landed commit) has no undo record and no parked previous image,
    /// and the image in place carries the update's own digest. A rollback
    /// must leave it alone: no `.bad-*` parking, no deletion.
    #[test]
    fn a_rollback_without_a_pre_image_never_parks_the_executable() {
        let f = Fixture::new();
        f.applied_state();
        let prefix = f.dir.join("prefix");
        std::fs::remove_file(f.paths.undo_path()).unwrap();
        std::fs::remove_file(prefix.join("rpi.exe.old-0.1.9-rc.3")).unwrap();
        // The "update" would have installed the version already in place.
        let mut same_version = f.pending.clone();
        same_version.from_version = same_version.target_version.clone();
        let outcome = rollback_applied_update(
            &f.fs,
            &f.paths,
            &same_version,
            RollbackReason::FirstBootCrash,
            &f.sink(),
        );
        assert!(outcome.exe_restored, "{outcome:?}");
        assert!(outcome.parked_new_exe.is_none(), "{outcome:?}");
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"new exe");
        let parked: Vec<_> = std::fs::read_dir(&prefix)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("rpi.exe."))
            .collect();
        assert!(parked.is_empty(), "{parked:?}");
        // With no undo record at all the rollback is still reported as
        // incomplete: the missing record is a finding, not a success.
        assert!(!outcome.ok(), "{outcome:?}");
        // A missing executable at its path is never "nothing landed": a
        // broken installation is reported, not restored (round-3 review).
        let f = Fixture::new();
        f.applied_state();
        let prefix = f.dir.join("prefix");
        std::fs::remove_file(f.paths.undo_path()).unwrap();
        std::fs::remove_file(prefix.join("rpi.exe.old-0.1.9-rc.3")).unwrap();
        std::fs::remove_file(prefix.join("rpi.exe")).unwrap();
        let outcome = rollback_applied_update(
            &f.fs,
            &f.paths,
            &same_version,
            RollbackReason::FirstBootCrash,
            &f.sink(),
        );
        assert!(!outcome.exe_restored, "{outcome:?}");
        assert!(
            outcome
                .failures
                .iter()
                .any(|failure| failure.contains("reinstall")),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_restore_done_by_hand_is_recognized() {
        let f = Fixture::new();
        f.applied_state();
        let prefix = f.dir.join("prefix");
        // install.ps1 preflight already renamed the old executable back.
        std::fs::remove_file(prefix.join("rpi.exe")).unwrap();
        std::fs::rename(
            prefix.join("rpi.exe.old-0.1.9-rc.3"),
            prefix.join("rpi.exe"),
        )
        .unwrap();
        let outcome = rollback_applied_update(
            &f.fs,
            &f.paths,
            &f.pending,
            RollbackReason::FirstBootCrash,
            &f.sink(),
        );
        assert!(outcome.ok(), "{outcome:?}");
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"old exe");
        assert_eq!(
            rollback_message("1.0.0", "1.1.0", Path::new("/log/update.log")),
            "The update to rpi 1.1.0 failed and was rolled back to rpi 1.0.0. See /log/update.log."
        );
    }
}
