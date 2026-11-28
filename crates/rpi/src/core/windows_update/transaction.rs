//! The startup transaction (design §11.5, "P1").
//!
//! Runs at the earliest point of a start, before CLI dispatch and extension
//! loading. With no pending update it costs exactly one existence query.
//! With one it takes the update lock, replays any uncommitted journal from a
//! start that died mid-way, re-verifies the staged files, and swaps them in
//! one journaled step at a time: extensions → pointers → the executable
//! (two back-to-back renames) → the install manifest. Before the first
//! change it persists the pre-image (`undo.json`) a first-boot rollback
//! needs.
//!
//! An applied update that still waits for its first confirmation is judged
//! here too: a crashed first-boot record rolls the update back, and the
//! calling process (which then runs the new image) must re-exec the
//! restored previous one.
//!
//! Everything runs over [`UpdateFs`] with scripted process liveness, so the
//! whole state machine — including a failure injected at any step — is
//! tested on Linux.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::boot::{
    FirstBootVerdict, ProcessProbe, clear_boot_records, judge_first_boot, own_boot_record,
    read_boot_records, write_boot_record,
};
use super::fs::{FsOp, UpdateFs, rename_with_retry};
use super::journal::{self, UndoContext};
use super::layout::UpdatePaths;
use super::lock::{self, LockError};
use super::pins::live_pins;
use super::records::{
    FileDigest, JournalStep, PendingUpdate, PinRecord, STATE_SCHEMA_VERSION, UndoData,
    UpdateResult, UpdateState, read_record, to_json, write_record,
};
use super::rollback::{RollbackReason, rollback_applied_update};

/// Which process is starting, read from the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupEnv {
    /// `RPI_SUBAGENT_CHILD`: a subagent's rpi child — no transaction, no
    /// first-boot participation, pins only (R7.3.8).
    pub subagent_child: bool,
    /// `RPI_UPDATE_VERIFY_CHILD`: the `--verify-extension` preflight child.
    pub verify_child: bool,
    /// `RPI_UPDATE_TRAMPOLINE_PID`: set when a trampoline launched this
    /// process (the first boot of an update).
    pub trampoline_pid: Option<u32>,
}

impl StartupEnv {
    pub fn from_env() -> Self {
        let flag = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        StartupEnv {
            subagent_child: flag("RPI_SUBAGENT_CHILD"),
            verify_child: flag(super::ENV_VERIFY_CHILD),
            trampoline_pid: std::env::var(super::ENV_TRAMPOLINE_PID)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok()),
        }
    }

    /// A plain interactive/one-shot start.
    pub fn plain() -> Self {
        StartupEnv::default()
    }
}

/// Tunables a test may shorten.
#[derive(Debug, Clone, Copy)]
pub struct StartupOptions {
    pub lock_wait: Duration,
}

impl Default for StartupOptions {
    fn default() -> Self {
        StartupOptions {
            lock_wait: super::LOCK_WAIT,
        }
    }
}

/// What the start does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionOutcome {
    /// No pending update: nothing was touched.
    NoPending,
    /// Another instance held the update lock for the whole wait: start
    /// as-is and judge again on the next start (R7.3.2).
    LockBusy,
    /// This process committed the transaction: start the new executable
    /// (the trampoline does) instead of continuing.
    Applied { first_boot_protocol: bool },
    /// An applied update waits for its first confirmation. `target_version`
    /// names the version that confirms; a process of another version only
    /// observed the state.
    FirstBootOpen { target_version: String },
    /// The pending was discarded or the transaction failed; the previous
    /// version continues. `manual` is the guidance when a human has to act.
    Degraded {
        message: String,
        manual: Option<String>,
    },
    /// A crashed first boot was rolled back; this process runs the new
    /// image and must re-exec the restored previous executable.
    RolledBack { message: String },
}

/// Run the transaction for this start. `running_version` is the caller's
/// `VERSION`. The lock is held only inside this call.
pub fn run_startup_transaction(
    fs: &dyn UpdateFs,
    paths: &UpdatePaths,
    probe: &dyn ProcessProbe,
    env: &StartupEnv,
    running_version: &str,
    options: &StartupOptions,
    log: &dyn Fn(&str),
) -> TransactionOutcome {
    if env.subagent_child || env.verify_child {
        return TransactionOutcome::NoPending;
    }
    // The fast path: one existence query, nothing else (R7.3.1, §4.13).
    if !fs.exists(&paths.pending_path()).unwrap_or(false) {
        return TransactionOutcome::NoPending;
    }
    let _lock = match lock::acquire_exclusive(&paths.lock_path(), options.lock_wait) {
        Ok(lock) => lock,
        Err(LockError::Timeout) => {
            log("transaction: the update lock stayed held by another instance; starting as-is");
            return TransactionOutcome::LockBusy;
        }
        Err(LockError::Io(error)) => {
            log(&format!(
                "transaction: could not take the update lock ({error}); starting as-is"
            ));
            return TransactionOutcome::LockBusy;
        }
    };
    // Re-read under the lock: the pending may have been applied or dropped
    // while this start waited for it.
    let Some(pending) = read_record::<PendingUpdate>(fs, &paths.pending_path()) else {
        if fs.exists(&paths.pending_path()).unwrap_or(false) {
            log("transaction: pending.json is unreadable; discarding it");
            let _ = fs.remove_file(&paths.pending_path());
            return TransactionOutcome::Degraded {
                message: format!(
                    "The staged update could not be read and was discarded. {} is unchanged; run \
                     \"{} update --self\" again.",
                    crate::config::APP_NAME,
                    crate::config::APP_NAME
                ),
                manual: None,
            };
        }
        return TransactionOutcome::NoPending;
    };
    if pending.applied {
        return judge_open_first_boot(fs, paths, probe, &pending, running_version, log);
    }
    log(&format!(
        "transaction: applying staged update {} -> {}",
        pending.from_version, pending.target_version
    ));
    // A start that died mid-transaction left its journal behind.
    let context = UndoContext { paths, fs, log };
    journal::rollback_uncommitted(&context);
    if let Err(reason) = reverify(fs, paths, &pending, running_version) {
        log(&format!("transaction: pending invalidated ({reason})"));
        discard_pending(fs, paths, &pending);
        let mut result = UpdateResult::new(
            UpdateState::Degraded,
            Some(pending.from_version.clone()),
            Some(pending.target_version.clone()),
        );
        result.reason = Some(format!("staged update discarded: {reason}"));
        let _ = write_record(fs, &paths.result_path(), Some(&result));
        return TransactionOutcome::Degraded {
            message: format!(
                "The staged update to rpi {} was discarded ({}). rpi is still on {}.",
                pending.target_version, reason, pending.from_version
            ),
            manual: None,
        };
    }
    let pins = live_pins(fs, &paths.pins_dir(), probe);
    match apply(fs, paths, &pending, running_version, &pins, log) {
        Ok(()) => TransactionOutcome::Applied {
            first_boot_protocol: pending.first_boot_protocol,
        },
        Err(failure) => {
            let failures = journal::rollback_uncommitted(&context);
            discard_pending(fs, paths, &pending);
            let mut result = UpdateResult::new(
                UpdateState::Degraded,
                Some(pending.from_version.clone()),
                Some(pending.target_version.clone()),
            );
            result.reason = Some(format!("transaction failed: {failure}"));
            let _ = write_record(fs, &paths.result_path(), Some(&result));
            if !failures.is_empty() {
                log(&format!(
                    "transaction: undo failures after a failed transaction: {}",
                    failures.join("; ")
                ));
            }
            TransactionOutcome::Degraded {
                message: format!(
                    "The update to rpi {} could not be applied ({}). rpi is still on {}.",
                    pending.target_version, failure, pending.from_version
                ),
                manual: Some(
                    crate::core::self_update::windows_manual_update_instructions(
                        &pending.target_version,
                        &paths.exe_path,
                    ),
                ),
            }
        }
    }
}

/// Step 2 of the design: an applied pending means the first boot is open.
fn judge_open_first_boot(
    fs: &dyn UpdateFs,
    paths: &UpdatePaths,
    probe: &dyn ProcessProbe,
    pending: &PendingUpdate,
    running_version: &str,
    log: &dyn Fn(&str),
) -> TransactionOutcome {
    if !pending.first_boot_protocol {
        // A target without the protocol is confirmed at commit; this
        // pending was left behind by an interrupted commit.
        log("transaction: applied pending without the first-boot protocol; confirming");
        confirm_update(fs, paths, pending);
        return TransactionOutcome::FirstBootOpen {
            target_version: pending.target_version.clone(),
        };
    }
    match judge_first_boot(fs, &paths.boot_dir(), probe, &pending.target_version) {
        FirstBootVerdict::Crashed(path, record) => {
            log(&format!(
                "transaction: first boot of {} crashed (pid {}, record {}); rolling back",
                pending.target_version,
                record.pid,
                path.display()
            ));
            let outcome =
                rollback_applied_update(fs, paths, pending, RollbackReason::FirstBootCrash, log);
            let message = super::rollback::rollback_message(
                &pending.from_version,
                &pending.target_version,
                &paths.log_path,
            );
            if outcome.exe_restored {
                TransactionOutcome::RolledBack { message }
            } else {
                log(&format!(
                    "transaction: rollback incomplete: {}",
                    outcome.failures.join("; ")
                ));
                TransactionOutcome::Degraded {
                    message: format!(
                        "{message} The rollback is incomplete ({}).",
                        outcome.failures.join("; ")
                    ),
                    manual: Some(format!(
                        "Re-run install.ps1 to repair the installation of {}.",
                        crate::config::APP_NAME
                    )),
                }
            }
        }
        FirstBootVerdict::AllExitedNormally => {
            // Failed one-shot commands: nothing to judge, the records go.
            for (path, _) in read_boot_records(fs, &paths.boot_dir()) {
                let _ = fs.remove_file(&path);
            }
            register_own_boot_record(fs, paths, probe, pending, running_version, log)
        }
        FirstBootVerdict::Live | FirstBootVerdict::NoRecords => {
            register_own_boot_record(fs, paths, probe, pending, running_version, log)
        }
    }
}

/// A process of the target version records itself for the judgement; a
/// process of any other version (an old instance that started while the
/// update was already applied) takes no part in it.
fn register_own_boot_record(
    fs: &dyn UpdateFs,
    paths: &UpdatePaths,
    probe: &dyn ProcessProbe,
    pending: &PendingUpdate,
    running_version: &str,
    log: &dyn Fn(&str),
) -> TransactionOutcome {
    if running_version == pending.target_version {
        let record = own_boot_record(probe, running_version);
        if let Err(error) = write_boot_record(fs, &paths.boot_record_path(record.pid), &record) {
            log(&format!(
                "transaction: could not write the boot record: {error}"
            ));
        }
    }
    TransactionOutcome::FirstBootOpen {
        target_version: pending.target_version.clone(),
    }
}

/// Write `confirmed` and drop the first-boot state (`undo.json`, the
/// pending, every boot record). The caller holds the lock and has checked
/// that the state is still applied.
pub fn confirm_update(fs: &dyn UpdateFs, paths: &UpdatePaths, pending: &PendingUpdate) {
    let mut result = read_record::<UpdateResult>(fs, &paths.result_path()).unwrap_or_else(|| {
        UpdateResult::new(
            UpdateState::Confirmed,
            Some(pending.from_version.clone()),
            Some(pending.target_version.clone()),
        )
    });
    result.schema_version = STATE_SCHEMA_VERSION;
    result.state = UpdateState::Confirmed;
    result.from_version = Some(pending.from_version.clone());
    result.to_version = Some(pending.target_version.clone());
    result.first_boot_protocol = pending.first_boot_protocol;
    let _ = write_record(fs, &paths.result_path(), Some(&result));
    let _ = write_record::<UndoData>(fs, &paths.undo_path(), None);
    let _ = write_record::<PendingUpdate>(fs, &paths.pending_path(), None);
    clear_boot_records(fs, &paths.boot_dir());
}

/// What the trampoline concludes after its new instance exited
/// (design §11.7 "跳板在场"). Holds the lock while it decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrampolineVerdict {
    /// Exit with the child's code: confirmed, a target without the
    /// protocol, or a normal exit before any confirmation.
    PassThrough,
    /// The child crashed its first boot: the rollback is done, relaunch the
    /// previous executable.
    RolledBack,
    /// The lock stayed busy: do not guess, exit with the child's code and
    /// let the next start judge.
    Deferred,
}

pub fn judge_trampoline_child(
    fs: &dyn UpdateFs,
    paths: &UpdatePaths,
    probe: &dyn ProcessProbe,
    child_pid: u32,
    options: &StartupOptions,
    log: &dyn Fn(&str),
) -> TrampolineVerdict {
    if !fs.exists(&paths.pending_path()).unwrap_or(false) {
        return TrampolineVerdict::PassThrough;
    }
    let _lock = match lock::acquire_exclusive(&paths.lock_path(), options.lock_wait) {
        Ok(lock) => lock,
        Err(error) => {
            log(&format!(
                "trampoline: update lock unavailable ({error}); deferring the judgement"
            ));
            return TrampolineVerdict::Deferred;
        }
    };
    let Some(pending) = read_record::<PendingUpdate>(fs, &paths.pending_path()) else {
        log(&format!(
            "trampoline: child {child_pid} exited; no pending update"
        ));
        return TrampolineVerdict::PassThrough;
    };
    if !pending.applied || !pending.first_boot_protocol {
        // Confirmed, or a target without the protocol: the code stands.
        return TrampolineVerdict::PassThrough;
    }
    log(&format!(
        "trampoline: child {child_pid} exited with code pending judgement"
    ));
    match judge_first_boot(fs, &paths.boot_dir(), probe, &pending.target_version) {
        FirstBootVerdict::Crashed(..) => {
            log(&format!(
                "trampoline: first boot of {} crashed; rolling back",
                pending.target_version
            ));
            let outcome =
                rollback_applied_update(fs, paths, &pending, RollbackReason::FirstBootCrash, log);
            if outcome.exe_restored {
                TrampolineVerdict::RolledBack
            } else {
                log(&format!(
                    "trampoline: rollback incomplete: {}",
                    outcome.failures.join("; ")
                ));
                // Nothing to relaunch: keep the child's code.
                TrampolineVerdict::PassThrough
            }
        }
        // Confirmed (no pending), exited normally, or another instance is
        // still in its first boot: the child's code stands.
        FirstBootVerdict::AllExitedNormally => {
            for (path, _) in read_boot_records(fs, &paths.boot_dir()) {
                let _ = fs.remove_file(&path);
            }
            TrampolineVerdict::PassThrough
        }
        // Another instance is still in its first boot: not this child's
        // call. A child that exited without any record never reached P1:
        // an unmarked termination is a crash (design §11.7).
        FirstBootVerdict::Live => TrampolineVerdict::PassThrough,
        FirstBootVerdict::NoRecords => {
            log(&format!(
                "trampoline: child {child_pid} exited without a boot record; rolling back"
            ));
            let outcome =
                rollback_applied_update(fs, paths, &pending, RollbackReason::FirstBootCrash, log);
            if outcome.exe_restored {
                TrampolineVerdict::RolledBack
            } else {
                log(&format!(
                    "trampoline: rollback incomplete: {}",
                    outcome.failures.join("; ")
                ));
                TrampolineVerdict::PassThrough
            }
        }
    }
}

/// The version the installation reports: the manifest's, else the running
/// one (a manual binary without a manifest).
pub fn installed_version(fs: &dyn UpdateFs, paths: &UpdatePaths, running_version: &str) -> String {
    read_record::<crate::config::InstallManifest>(fs, &paths.manifest_path)
        .map(|manifest| manifest.version)
        .unwrap_or_else(|| running_version.to_owned())
}

fn reverify(
    fs: &dyn UpdateFs,
    paths: &UpdatePaths,
    pending: &PendingUpdate,
    running_version: &str,
) -> Result<(), String> {
    if !fs
        .sha256_file(&pending.binary.staged_path)
        .is_ok_and(|sha| sha.eq_ignore_ascii_case(&pending.binary.sha256))
    {
        return Err("the staged executable is missing or changed".to_owned());
    }
    for extension in &pending.extensions {
        // A pending without per-file digests (written by an older build)
        // is verified by the executable alone.
        if !extension.files.is_empty() {
            verify_staged_tree(fs, &extension.staged_path, &extension.files)?;
        }
    }
    if let Some(manifest) = read_record::<crate::config::InstallManifest>(fs, &paths.manifest_path)
        && super::records::is_later(&manifest.installed_at, &pending.written_at)
    {
        return Err(
            "the installation changed after the update was staged (the installer was re-run)"
                .to_owned(),
        );
    }
    if !pending.allow_non_monotonic {
        let installed = installed_version(fs, paths, running_version);
        if crate::core::version_check::compare_package_versions(&pending.target_version, &installed)
            != Some(std::cmp::Ordering::Greater)
        {
            return Err(format!(
                "rpi {installed} is not older than the staged rpi {}",
                pending.target_version
            ));
        }
    }
    Ok(())
}

/// Check that a staged directory holds exactly the recorded files.
pub fn verify_staged_tree(
    fs: &dyn UpdateFs,
    dir: &Path,
    expected: &[FileDigest],
) -> Result<(), String> {
    let actual =
        collect_file_digests(fs, dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    if actual.len() != expected.len() {
        return Err(format!(
            "{} holds {} file(s), the update recorded {}",
            dir.display(),
            actual.len(),
            expected.len()
        ));
    }
    for digest in expected {
        let Some(found) = actual.iter().find(|file| file.path == digest.path) else {
            return Err(format!("{} lacks {}", dir.display(), digest.path.display()));
        };
        if !found.sha256.eq_ignore_ascii_case(&digest.sha256) {
            return Err(format!("{} changed", dir.join(&digest.path).display()));
        }
    }
    Ok(())
}

/// sha256 of every file under `dir`, paths relative to it, sorted.
pub fn collect_file_digests(fs: &dyn UpdateFs, dir: &Path) -> io::Result<Vec<FileDigest>> {
    let mut out = Vec::new();
    walk_digests(fs, dir, Path::new(""), &mut out)?;
    out.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(out)
}

fn walk_digests(
    fs: &dyn UpdateFs,
    root: &Path,
    relative: &Path,
    out: &mut Vec<FileDigest>,
) -> io::Result<()> {
    for entry in fs.read_dir(&root.join(relative))? {
        let next = relative.join(entry.path.file_name().unwrap_or_default());
        if entry.is_dir {
            walk_digests(fs, root, &next, out)?;
        } else {
            out.push(FileDigest {
                sha256: fs.sha256_file(&entry.path)?,
                path: next,
            });
        }
    }
    Ok(())
}

/// Remove a discarded pending and everything it staged.
pub fn discard_pending(fs: &dyn UpdateFs, paths: &UpdatePaths, pending: &PendingUpdate) {
    let _ = write_record::<PendingUpdate>(fs, &paths.pending_path(), None);
    // A pre-image belongs to an applied update only.
    let _ = write_record::<UndoData>(fs, &paths.undo_path(), None);
    if let Some(dir) = pending.binary.staged_path.parent() {
        let _ = fs.remove_dir_all(dir);
    }
    for extension in &pending.extensions {
        let _ = fs.remove_dir_all(&extension.staged_path);
    }
}

/// Where a failing step reports itself.
struct Failure(String);

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Failure(error.to_string())
    }
}

fn apply(
    fs: &dyn UpdateFs,
    paths: &UpdatePaths,
    pending: &PendingUpdate,
    running_version: &str,
    pins: &[PinRecord],
    log: &dyn Fn(&str),
) -> Result<(), String> {
    let result = apply_inner(fs, paths, pending, running_version, pins, log);
    if let Err(failure) = &result {
        log(&format!("transaction: step failed: {}", failure.0));
    }
    result.map_err(|failure| failure.0)
}

fn apply_inner(
    fs: &dyn UpdateFs,
    paths: &UpdatePaths,
    pending: &PendingUpdate,
    running_version: &str,
    pins: &[PinRecord],
    log: &dyn Fn(&str),
) -> Result<(), Failure> {
    let installed = installed_version(fs, paths, running_version);
    journal::create(fs, &paths.journal_path())?;
    // --- the pre-image, before the first change ---
    let old_manifest_text = fs
        .read(&paths.manifest_path)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    let mut pointers = Vec::new();
    for extension in &pending.extensions {
        let container = extension.container();
        let pointer_path = container.join(rpi_ext_host::install_layout::GENERATION_POINTER_FILE);
        pointers.push(super::records::PointerUndo {
            container,
            previous_text: fs
                .read(&pointer_path)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok()),
            new_text: String::new(),
        });
    }
    let preferred_old = paths.old_exe_path(&installed);
    let old_exe_path = if fs.exists(&preferred_old)? {
        match fs.remove_file(&preferred_old) {
            Ok(()) => preferred_old,
            Err(_) => paths.unused_sibling(preferred_old.clone()),
        }
    } else {
        preferred_old
    };
    let old_exe_name = old_exe_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let undo = UndoData {
        schema_version: STATE_SCHEMA_VERSION,
        old_manifest_text: old_manifest_text.clone(),
        old_exe_name: old_exe_name.clone(),
        pointers: pointers.clone(),
    };
    fs.write_atomic(&paths.undo_path(), to_json(&undo).as_bytes())?;
    log("transaction: pre-image (undo.json) written");

    // --- extensions: place every generation, then flip every pointer ---
    for extension in &pending.extensions {
        journal::append(
            fs,
            &paths.journal_path(),
            JournalStep::PlaceExtension {
                name: extension.name.clone(),
                staged_path: extension.staged_path.clone(),
                generation_path: extension.generation_path(),
            },
        )?;
        place_staged_extension(fs, extension, pins).map_err(Failure)?;
        log(&format!(
            "transaction: extension {} {} placed",
            extension.name, extension.version
        ));
    }
    let mut new_pointers = Vec::new();
    for extension in &pending.extensions {
        let container = extension.container();
        let previous_text = pointers
            .iter()
            .find(|pointer| pointer.container == container)
            .and_then(|pointer| pointer.previous_text.clone());
        let previous = previous_text
            .as_deref()
            .and_then(|text| {
                serde_json::from_str::<rpi_ext_host::install_layout::GenerationPointer>(text).ok()
            })
            .map(|pointer| pointer.version);
        let pointer = super::generations::generation_pointer(
            fs,
            &extension.generation_path(),
            &extension.version,
            previous.as_deref(),
        )?;
        let new_text = to_json(&pointer);
        journal::append(
            fs,
            &paths.journal_path(),
            JournalStep::FlipPointer {
                container: container.clone(),
                previous_text: previous_text.clone(),
                new_text: new_text.clone(),
            },
        )?;
        flip_pointer(fs, &container, new_text.as_bytes())?;
        new_pointers.push(super::records::PointerUndo {
            container: container.clone(),
            previous_text,
            new_text,
        });
        rpi_ext_host::install_layout::refresh_install_resolution(&container);
        log(&format!(
            "transaction: pointer of {} flipped",
            extension.name
        ));
    }
    // The undo record now carries the texts a rollback compares against.
    let undo = UndoData {
        schema_version: STATE_SCHEMA_VERSION,
        old_manifest_text: old_manifest_text.clone(),
        old_exe_name: old_exe_name.clone(),
        pointers: new_pointers,
    };
    fs.write_atomic(&paths.undo_path(), to_json(&undo).as_bytes())?;

    // --- the executable: two back-to-back renames ---
    journal::append(
        fs,
        &paths.journal_path(),
        JournalStep::ExeSwap {
            old_name: old_exe_name.clone(),
        },
    )?;
    // Opening the three paths for DELETE first surfaces a sharing violation
    // before anything has moved.
    super::fs::preopen_for_swap(&[&paths.exe_path, &pending.binary.staged_path, &old_exe_path])?;
    rename(fs, &paths.exe_path, &old_exe_path)?;
    rename(fs, &pending.binary.staged_path, &paths.exe_path)?;
    log("transaction: executable swapped");

    // --- the install manifest (schema unchanged) ---
    journal::append(
        fs,
        &paths.journal_path(),
        JournalStep::WriteManifest {
            previous_text: old_manifest_text.clone(),
        },
    )?;
    let manifest = crate::config::InstallManifest {
        version: pending.target_version.clone(),
        target: pending.binary.target.clone(),
        installed_at: fs.now_iso8601(),
        source_url: pending.binary.source_url.clone(),
        sha256: pending.binary.asset_sha256.clone(),
        install_path: paths.exe_path.to_string_lossy().into_owned(),
        method: crate::config::InstallManifest::METHOD_BINARY.to_owned(),
    };
    fs.write_atomic(&paths.manifest_path, to_json(&manifest).as_bytes())?;

    // --- commit ---
    journal::commit(fs, &paths.journal_path())?;
    let mut committed = pending.clone();
    committed.applied = true;
    write_record(fs, &paths.pending_path(), Some(&committed))?;
    let mut result = UpdateResult::new(
        UpdateState::Applied,
        Some(pending.from_version.clone()),
        Some(pending.target_version.clone()),
    );
    result.first_boot_protocol = pending.first_boot_protocol;
    write_record(fs, &paths.result_path(), Some(&result))?;
    clear_boot_records(fs, &paths.boot_dir());
    log("transaction: committed");
    Ok(())
}

/// Move one staged extension to its generation directory, honouring the
/// three rules for an existing directory (design §11.5).
fn place_staged_extension(
    fs: &dyn UpdateFs,
    extension: &super::records::StagedExtension,
    pins: &[PinRecord],
) -> Result<(), String> {
    crate::core::extension_registry::ensure_safe_extension_name(&extension.name)?;
    let container = extension.container();
    fs.create_dir_all(&container)
        .map_err(|error| error.to_string())?;
    let retired = container.join(rpi_ext_host::install_layout::RETIRED_MARKER_FILE);
    if fs.exists(&retired).unwrap_or(false) {
        fs.remove_file(&retired)
            .map_err(|error| error.to_string())?;
    }
    let generation = extension.generation_path();
    if fs.exists(&generation).unwrap_or(false) {
        let pinned = super::pins::count_references(pins, &generation) > 0;
        let replaced = !pinned && fs.remove_dir_all(&generation).is_ok();
        if !replaced {
            let existing = fs.sha256_file(&generation.join("rpi-extension.json")).ok();
            if existing.as_deref() != Some(extension.manifest_sha256.as_str()) {
                return Err(format!(
                    "{} {} is already installed as a different build and is in use",
                    extension.name, extension.version
                ));
            }
            let _ = fs.remove_dir_all(&extension.staged_path);
            return Ok(());
        }
    }
    fs.rename(&extension.staged_path, &generation)
        .map_err(|error| {
            format!(
                "could not move {} into place: {error}",
                extension.staged_path.display()
            )
        })
}

fn flip_pointer(fs: &dyn UpdateFs, container: &Path, text: &[u8]) -> Result<(), Failure> {
    let target = container.join(rpi_ext_host::install_layout::GENERATION_POINTER_FILE);
    let mut temp = target.as_os_str().to_owned();
    temp.push(format!(".tmp-{}", std::process::id()));
    let temp = PathBuf::from(temp);
    fs.write_atomic(&temp, text)?;
    rename(fs, &temp, &target)
}

fn rename(fs: &dyn UpdateFs, from: &Path, to: &Path) -> Result<(), Failure> {
    rename_with_retry(fs, from, to, super::RENAME_RETRIES, &|duration| {
        fs.retry_sleep(duration)
    })?;
    Ok(())
}

/// Whether the file system did only the fast-path query (the budget of
/// design §11.12, asserted through [`super::fs::SpyFs`]).
pub fn touched_only_pending_query(fs: &super::fs::SpyFs) -> bool {
    let calls = fs.calls();
    calls.len() == 1 && calls[0].op == FsOp::Exists
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::windows_update::boot::FakeProcessProbe;
    use crate::core::windows_update::fs::{FsOp, SpyFs};
    use crate::core::windows_update::records::StagedBinary;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        dir: PathBuf,
        fs: Arc<SpyFs>,
        paths: UpdatePaths,
        probe: Arc<FakeProcessProbe>,
        pending: PendingUpdate,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Fixture {
        /// An installation of `from` with a staged update to `to`, the
        /// way `rpi update --all` leaves it.
        fn staged(with_extension: bool) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rpi-wu-txn-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            let fs = Arc::new(SpyFs::new());
            let paths = UpdatePaths::new(
                &dir.join("prefix/rpi.exe"),
                dir.join("logs/update.log"),
                &dir.join("agent"),
                None,
            );
            let prefix = dir.join("prefix");
            std::fs::create_dir_all(prefix.join(".update/staging/0.1.9-rc.4")).unwrap();
            std::fs::create_dir_all(dir.join("agent/extensions")).unwrap();
            std::fs::write(prefix.join("rpi.exe"), b"old exe").unwrap();
            std::fs::write(
                &paths.manifest_path,
                br#"{"version":"0.1.9-rc.3","target":"t","installedAt":"2026-11-27T00:00:00.000Z","sourceUrl":"u","sha256":"s","installPath":"p","method":"binary"}"#,
            )
            .unwrap();
            std::fs::write(
                prefix.join(".update/staging/0.1.9-rc.4/rpi.exe"),
                b"new exe",
            )
            .unwrap();
            let mut extensions = Vec::new();
            if with_extension {
                let root = dir.join("agent/extensions");
                // Legacy flat install of the extension.
                std::fs::create_dir_all(root.join("rpi-todo")).unwrap();
                std::fs::write(
                    root.join("rpi-todo/rpi-extension.json"),
                    r#"{"name":"rpi-todo","version":"0.1.9-rc.3"}"#,
                )
                .unwrap();
                let staged =
                    UpdatePaths::extension_staging_path(&root, "rpi-todo", "0.1.9-rc.4", 7);
                std::fs::create_dir_all(staged.join("lib")).unwrap();
                std::fs::write(staged.join("rpi-extension.json"), r#"{"name":"rpi-todo","version":"0.1.9-rc.4","native":"lib/lib.dll","rpiAbi":1}"#).unwrap();
                std::fs::write(staged.join("lib/lib.dll"), b"dll").unwrap();
                let files = collect_file_digests(&*fs, &staged).unwrap();
                extensions.push(super::super::records::StagedExtension {
                    name: "rpi-todo".to_owned(),
                    version: "0.1.9-rc.4".to_owned(),
                    install_root: root,
                    staged_path: staged,
                    generation: "0.1.9-rc.4".to_owned(),
                    manifest_sha256: crate::core::windows_update::fs::sha256_bytes(
                        br#"{"name":"rpi-todo","version":"0.1.9-rc.4","native":"lib/lib.dll","rpiAbi":1}"#,
                    ),
                    files,
                });
            }
            let pending = PendingUpdate {
                schema_version: 1,
                target_version: "0.1.9-rc.4".to_owned(),
                channel: "stable".to_owned(),
                written_at: "2026-11-28T00:00:00.000Z".to_owned(),
                from_version: "0.1.9-rc.3".to_owned(),
                allow_non_monotonic: false,
                first_boot_protocol: true,
                applied: false,
                binary: StagedBinary {
                    staged_path: prefix.join(".update/staging/0.1.9-rc.4/rpi.exe"),
                    sha256: crate::core::windows_update::fs::sha256_bytes(b"new exe"),
                    source_url: "https://example.invalid/rpi-0.1.9-rc.4-t.zip".to_owned(),
                    asset_sha256: "ab".repeat(32),
                    target: "x86_64-pc-windows-msvc".to_owned(),
                },
                extensions,
            };
            fs.write_atomic(&paths.pending_path(), to_json(&pending).as_bytes())
                .unwrap();
            let probe = Arc::new(FakeProcessProbe::new(4242, 42));
            Fixture {
                dir,
                fs,
                paths,
                probe,
                pending,
                log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn run(&self, running_version: &str) -> TransactionOutcome {
            self.run_env(&StartupEnv::plain(), running_version)
        }

        fn run_env(&self, env: &StartupEnv, running_version: &str) -> TransactionOutcome {
            let log = Arc::clone(&self.log);
            run_startup_transaction(
                &*self.fs,
                &self.paths,
                &*self.probe,
                env,
                running_version,
                &StartupOptions {
                    lock_wait: Duration::from_millis(300),
                },
                &move |message: &str| log.lock().unwrap().push(message.to_owned()),
            )
        }

        fn prefix(&self) -> PathBuf {
            self.dir.join("prefix")
        }

        fn logs(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const RUNNING_OLD: &str = "0.1.9-rc.3";
    const RUNNING_NEW: &str = "0.1.9-rc.4";

    // ---- the fast path -------------------------------------------------

    #[test]
    fn no_pending_costs_exactly_one_existence_query() {
        let f = Fixture::staged(false);
        f.fs.remove_file(&f.paths.pending_path()).unwrap();
        f.fs.reset();
        assert_eq!(f.run(RUNNING_OLD), TransactionOutcome::NoPending);
        assert!(touched_only_pending_query(&f.fs), "{:?}", f.fs.calls());
        assert!(f.fs.calls()[0].path == f.paths.pending_path());
        assert!(f.logs().is_empty());
    }

    #[test]
    fn subagent_and_verify_children_never_run_the_transaction() {
        let f = Fixture::staged(true);
        f.fs.reset();
        let child = StartupEnv {
            subagent_child: true,
            ..StartupEnv::default()
        };
        assert_eq!(
            f.run_env(&child, RUNNING_OLD),
            TransactionOutcome::NoPending
        );
        let verify = StartupEnv {
            verify_child: true,
            ..StartupEnv::default()
        };
        assert_eq!(
            f.run_env(&verify, RUNNING_OLD),
            TransactionOutcome::NoPending
        );
        assert!(f.fs.calls().is_empty(), "not even the existence query");
        assert!(f.paths.pending_path().exists());
    }

    // ---- the happy path -------------------------------------------------

    #[test]
    fn a_staged_update_applies_and_persists_its_pre_image() {
        let f = Fixture::staged(true);
        assert_eq!(
            f.run(RUNNING_OLD),
            TransactionOutcome::Applied {
                first_boot_protocol: true
            }
        );
        let prefix = f.prefix();
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"new exe");
        assert_eq!(
            std::fs::read(prefix.join("rpi.exe.old-0.1.9-rc.3")).unwrap(),
            b"old exe"
        );
        assert!(!prefix.join(".update/staging/0.1.9-rc.4/rpi.exe").exists());
        let manifest: crate::config::InstallManifest =
            serde_json::from_slice(&std::fs::read(&f.paths.manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.version, "0.1.9-rc.4");
        assert_eq!(manifest.method, "binary");
        assert_eq!(manifest.sha256, "ab".repeat(32));
        assert_eq!(manifest.target, "x86_64-pc-windows-msvc");
        assert_eq!(manifest.install_path, f.paths.exe_path.to_string_lossy());
        // Extensions: generation placed, pointer flipped, flat files kept.
        let container = f.dir.join("agent/extensions/rpi-todo");
        assert!(container.join("0.1.9-rc.4/lib/lib.dll").exists());
        assert!(
            container.join("rpi-extension.json").exists(),
            "legacy flat files wait for the cleanup scan"
        );
        let pointer: rpi_ext_host::install_layout::GenerationPointer =
            serde_json::from_slice(&std::fs::read(container.join("current.json")).unwrap())
                .unwrap();
        assert_eq!(pointer.version, "0.1.9-rc.4");
        assert!(pointer.previous.is_none());
        // State after commit.
        let pending = read_record::<PendingUpdate>(&*f.fs, &f.paths.pending_path()).unwrap();
        assert!(pending.applied);
        assert!(
            read_record::<super::super::records::Journal>(&*f.fs, &f.paths.journal_path())
                .is_none()
        );
        let result = read_record::<UpdateResult>(&*f.fs, &f.paths.result_path()).unwrap();
        assert_eq!(result.state, UpdateState::Applied);
        assert!(!result.read);
        let undo = read_record::<UndoData>(&*f.fs, &f.paths.undo_path()).unwrap();
        assert_eq!(undo.old_exe_name, "rpi.exe.old-0.1.9-rc.3");
        assert!(
            undo.old_manifest_text
                .as_deref()
                .is_some_and(|text| text.contains("0.1.9-rc.3"))
        );
        assert_eq!(undo.pointers.len(), 1);
        assert!(undo.pointers[0].previous_text.is_none());
        assert_eq!(
            undo.pointers[0].new_text,
            String::from_utf8(std::fs::read(container.join("current.json")).unwrap()).unwrap()
        );
        assert!(
            f.logs().iter().any(|line| line.contains("committed")),
            "{:?}",
            f.logs()
        );
    }

    #[test]
    fn an_existing_old_exe_name_is_replaced_or_suffix() {
        let f = Fixture::staged(false);
        // A leftover of an earlier cycle with the same version.
        std::fs::write(f.prefix().join("rpi.exe.old-0.1.9-rc.3"), b"stale").unwrap();
        assert!(matches!(
            f.run(RUNNING_OLD),
            TransactionOutcome::Applied { .. }
        ));
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe.old-0.1.9-rc.3")).unwrap(),
            b"old exe"
        );
    }

    #[test]
    fn an_existing_old_exe_that_cannot_be_deleted_gets_a_stamped_name() {
        let f = Fixture::staged(false);
        std::fs::write(f.prefix().join("rpi.exe.old-0.1.9-rc.3"), b"held").unwrap();
        f.fs.fail(FsOp::RemoveFile, "rpi.exe.old-0.1.9-rc.3", 1);
        assert!(matches!(
            f.run(RUNNING_OLD),
            TransactionOutcome::Applied { .. }
        ));
        let undo = read_record::<UndoData>(&*f.fs, &f.paths.undo_path()).unwrap();
        assert!(
            undo.old_exe_name.starts_with("rpi.exe.old-0.1.9-rc.3.t"),
            "{}",
            undo.old_exe_name
        );
        assert_eq!(
            std::fs::read(f.prefix().join(&undo.old_exe_name)).unwrap(),
            b"old exe"
        );
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe.old-0.1.9-rc.3")).unwrap(),
            b"held"
        );
    }

    // ---- startup re-verification ---------------------------------------

    #[test]
    fn a_tampered_staged_executable_invalidates_the_pending() {
        let f = Fixture::staged(false);
        std::fs::write(
            f.prefix().join(".update/staging/0.1.9-rc.4/rpi.exe"),
            b"tampered",
        )
        .unwrap();
        let outcome = f.run(RUNNING_OLD);
        assert!(matches!(
            outcome,
            TransactionOutcome::Degraded { manual: None, .. }
        ));
        let TransactionOutcome::Degraded { message, .. } = outcome else {
            unreachable!()
        };
        assert!(message.contains("discarded"), "{message}");
        assert!(message.contains("rpi is still on 0.1.9-rc.3"), "{message}");
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"old exe"
        );
        assert!(!f.paths.pending_path().exists());
        assert!(!f.prefix().join(".update/staging/0.1.9-rc.4").exists());
        let result = read_record::<UpdateResult>(&*f.fs, &f.paths.result_path()).unwrap();
        assert_eq!(result.state, UpdateState::Degraded);
    }

    #[test]
    fn a_tampered_staged_extension_file_invalidates_the_pending() {
        let f = Fixture::staged(true);
        let staged = &f.pending.extensions[0].staged_path;
        std::fs::write(staged.join("lib/lib.dll"), b"other").unwrap();
        assert!(matches!(
            f.run(RUNNING_OLD),
            TransactionOutcome::Degraded { .. }
        ));
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"old exe"
        );
        assert!(
            !staged.exists(),
            "the staged extension is dropped with the pending"
        );
        let container = f.dir.join("agent/extensions/rpi-todo");
        assert!(!container.join("current.json").exists());
    }

    #[test]
    fn an_installation_changed_after_staging_invalidates_the_pending() {
        let f = Fixture::staged(false);
        std::fs::write(&f.paths.manifest_path,
            br#"{"version":"0.1.9-rc.3","target":"t","installedAt":"2026-11-29T00:00:00Z","sourceUrl":"u","sha256":"s","installPath":"p","method":"binary"}"#,
        )
        .unwrap();
        let TransactionOutcome::Degraded { message, .. } = f.run(RUNNING_OLD) else {
            panic!("expected a degraded outcome")
        };
        assert!(message.contains("installation changed"), "{message}");
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"old exe"
        );
    }

    #[test]
    fn a_non_monotonic_target_needs_the_flag() {
        let f = Fixture::staged(false);
        // The installation reports a newer version than the target.
        std::fs::write(&f.paths.manifest_path,
            br#"{"version":"0.1.9-rc.5","target":"t","installedAt":"2026-11-27T00:00:00Z","sourceUrl":"u","sha256":"s","installPath":"p","method":"binary"}"#,
        )
        .unwrap();
        let TransactionOutcome::Degraded { message, .. } = f.run(RUNNING_OLD) else {
            panic!("expected a degraded outcome")
        };
        assert!(message.contains("not older"), "{message}");
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"old exe"
        );

        // `--force` / `--version` recorded the flag: the downgrade applies.
        let f = Fixture::staged(false);
        std::fs::write(&f.paths.manifest_path,
            br#"{"version":"0.1.9-rc.5","target":"t","installedAt":"2026-11-27T00:00:00Z","sourceUrl":"u","sha256":"s","installPath":"p","method":"binary"}"#,
        )
        .unwrap();
        let mut pending = f.pending.clone();
        pending.allow_non_monotonic = true;
        f.fs.write_atomic(&f.paths.pending_path(), to_json(&pending).as_bytes())
            .unwrap();
        assert!(matches!(
            f.run(RUNNING_OLD),
            TransactionOutcome::Applied { .. }
        ));
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"new exe"
        );
    }

    // ---- per-step fault injection ----------------------------------------

    /// Every step between the pre-image and the commit, each named by the
    /// path its rename or write touches.
    fn every_step_failure() -> Vec<(&'static str, FsOp, &'static str)> {
        vec![
            ("undo write", FsOp::WriteAtomic, "undo.json"),
            ("extension placement", FsOp::Rename, "rpi-todo/0.1.9-rc.4"),
            ("pointer flip", FsOp::Rename, "rpi-todo/current.json"),
            ("exe park", FsOp::Rename, "rpi.exe.old-0.1.9-rc.3"),
            ("exe place", FsOp::Rename, "staging/0.1.9-rc.4/rpi.exe"),
            ("manifest write", FsOp::WriteAtomic, "rpi.install.json"),
        ]
    }

    #[test]
    fn a_failure_at_any_step_rolls_back_to_a_startable_state() {
        for (step, op, path) in every_step_failure() {
            let f = Fixture::staged(true);
            f.fs.fail(op, path, 1);
            let outcome = f.run(RUNNING_OLD);
            let TransactionOutcome::Degraded { message, manual } = &outcome else {
                panic!("{step}: expected a degraded outcome, got {outcome:?}")
            };
            assert!(manual.is_some(), "{step}: manual guidance is given");
            assert!(
                message.contains("could not be applied"),
                "{step}: {message}"
            );
            let prefix = f.prefix();
            // The executable is back and startable in every case.
            assert_eq!(
                std::fs::read(prefix.join("rpi.exe")).unwrap_or_default(),
                b"old exe",
                "{step}: executable"
            );
            assert!(
                !prefix.join("rpi.exe.old-0.1.9-rc.3").exists(),
                "{step}: no parked image remains"
            );
            // The manifest is the previous one.
            let manifest: crate::config::InstallManifest =
                serde_json::from_slice(&std::fs::read(&f.paths.manifest_path).unwrap()).unwrap();
            assert_eq!(manifest.version, "0.1.9-rc.3", "{step}: manifest");
            // No pointer was flipped and no generation was left behind.
            let container = f.dir.join("agent/extensions/rpi-todo");
            assert!(!container.join("current.json").exists(), "{step}: pointer");
            assert!(
                !f.pending.extensions[0].staged_path.exists(),
                "{step}: the staged extension is dropped with the pending"
            );
            // Pending, staging, journal and pre-image are gone.
            assert!(!f.paths.pending_path().exists(), "{step}: pending");
            assert!(!f.paths.journal_path().exists(), "{step}: journal");
            assert!(!f.paths.undo_path().exists(), "{step}: undo");
            assert!(
                !prefix.join(".update/staging/0.1.9-rc.4").exists(),
                "{step}: staging"
            );
            let result = read_record::<UpdateResult>(&*f.fs, &f.paths.result_path()).unwrap();
            assert_eq!(result.state, UpdateState::Degraded, "{step}");
            assert!(
                result
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("transaction failed")),
                "{step}: {:?}",
                result.reason
            );
        }
    }

    #[test]
    fn a_transient_rename_failure_at_the_exe_swap_is_retried() {
        let f = Fixture::staged(false);
        f.fs.fail_with(
            FsOp::Rename,
            "rpi.exe.old-0.1.9-rc.3",
            2,
            io::Error::from_raw_os_error(32),
        );
        assert!(matches!(
            f.run(RUNNING_OLD),
            TransactionOutcome::Applied { .. }
        ));
        assert_eq!(f.fs.sleeps().len(), 2);
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"new exe"
        );
    }

    #[test]
    fn an_uncommitted_journal_is_undone_before_the_pending_is_judged() {
        let f = Fixture::staged(true);
        // A previous start died after parking the executable and placing
        // the generation.
        let prefix = f.prefix();
        std::fs::rename(
            prefix.join("rpi.exe"),
            prefix.join("rpi.exe.old-0.1.9-rc.3"),
        )
        .unwrap();
        let staged = &f.pending.extensions[0].staged_path.clone();
        std::fs::rename(staged, f.dir.join("agent/extensions/rpi-todo/0.1.9-rc.4")).unwrap();
        journal::create(&*f.fs, &f.paths.journal_path()).unwrap();
        journal::append(
            &*f.fs,
            &f.paths.journal_path(),
            JournalStep::PlaceExtension {
                name: "rpi-todo".to_owned(),
                staged_path: staged.clone(),
                generation_path: f.dir.join("agent/extensions/rpi-todo/0.1.9-rc.4"),
            },
        )
        .unwrap();
        journal::append(
            &*f.fs,
            &f.paths.journal_path(),
            JournalStep::ExeSwap {
                old_name: "rpi.exe.old-0.1.9-rc.3".to_owned(),
            },
        )
        .unwrap();
        // The transaction undoes it first: the executable is back and the
        // placed generation removed. The pending's staged extension is gone
        // with it, so the pending is invalidated and discarded.
        let outcome = f.run(RUNNING_OLD);
        assert!(
            matches!(outcome, TransactionOutcome::Degraded { .. }),
            "{outcome:?}"
        );
        assert!(!f.paths.journal_path().exists());
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"old exe");
        assert!(!prefix.join("rpi.exe.old-0.1.9-rc.3").exists());
        assert!(!f.dir.join("agent/extensions/rpi-todo/0.1.9-rc.4").exists());
        assert!(!staged.exists());
        assert!(
            f.logs()
                .iter()
                .any(|line| line.contains("uncommitted transaction with 2 step(s)")),
            "{:?}",
            f.logs()
        );
    }

    #[test]
    fn an_uncommitted_journal_with_a_complete_pending_applies_cleanly() {
        // The journal was written before a step that never ran (crash right
        // after the append): undo is a no-op and the transaction proceeds.
        let f = Fixture::staged(false);
        journal::create(&*f.fs, &f.paths.journal_path()).unwrap();
        journal::append(
            &*f.fs,
            &f.paths.journal_path(),
            JournalStep::WriteManifest {
                previous_text: Some("{}".to_owned()),
            },
        )
        .unwrap();
        assert!(matches!(
            f.run(RUNNING_OLD),
            TransactionOutcome::Applied { .. }
        ));
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"new exe"
        );
    }

    #[test]
    fn the_lock_serializes_two_starts() {
        let f = Fixture::staged(false);
        let held =
            lock::acquire_exclusive(&f.paths.lock_path(), Duration::from_millis(100)).unwrap();
        assert_eq!(f.run(RUNNING_OLD), TransactionOutcome::LockBusy);
        drop(held);
        assert!(matches!(
            f.run(RUNNING_OLD),
            TransactionOutcome::Applied { .. }
        ));
        assert!(f.logs().iter().any(|line| line.contains("stayed held")));
    }

    #[test]
    fn an_unreadable_pending_is_discarded_with_a_message() {
        let f = Fixture::staged(false);
        f.fs.write_atomic(&f.paths.pending_path(), b"{broken")
            .unwrap();
        let TransactionOutcome::Degraded { message, manual } = f.run(RUNNING_OLD) else {
            panic!("expected a degraded outcome")
        };
        assert!(manual.is_none());
        assert!(message.contains("could not be read"), "{message}");
        assert!(!f.paths.pending_path().exists());
    }

    // ---- the first boot ---------------------------------------------------

    fn applied(with_extension: bool) -> Fixture {
        let f = Fixture::staged(with_extension);
        assert!(matches!(
            f.run(RUNNING_OLD),
            TransactionOutcome::Applied { .. }
        ));
        f.fs.reset();
        f.log.lock().unwrap().clear();
        f
    }

    #[test]
    fn the_new_version_registers_for_the_first_boot_and_confirms() {
        let f = applied(false);
        assert_eq!(
            f.run(RUNNING_NEW),
            TransactionOutcome::FirstBootOpen {
                target_version: "0.1.9-rc.4".to_owned()
            }
        );
        let record = read_record::<super::super::records::BootRecord>(
            &*f.fs,
            &f.paths.boot_record_path(4242),
        )
        .unwrap();
        assert_eq!(record.version, RUNNING_NEW);
        assert!(!record.normal_exit);

        confirm_update(&*f.fs, &f.paths, &f.pending);
        assert!(read_record::<PendingUpdate>(&*f.fs, &f.paths.pending_path()).is_none());
        assert!(read_record::<UndoData>(&*f.fs, &f.paths.undo_path()).is_none());
        assert!(!f.paths.boot_record_path(4242).exists());
        let result = read_record::<UpdateResult>(&*f.fs, &f.paths.result_path()).unwrap();
        assert_eq!(result.state, UpdateState::Confirmed);
        assert!(result.first_boot_protocol);
        assert_eq!(result.from_version.as_deref(), Some("0.1.9-rc.3"));

        // Confirmed: nothing is open any more.
        f.fs.reset();
        assert_eq!(f.run(RUNNING_NEW), TransactionOutcome::NoPending);
        assert!(touched_only_pending_query(&f.fs));
    }

    #[test]
    fn an_old_version_process_takes_no_part_in_the_first_boot() {
        let f = applied(false);
        assert_eq!(
            f.run(RUNNING_OLD),
            TransactionOutcome::FirstBootOpen {
                target_version: "0.1.9-rc.4".to_owned()
            }
        );
        assert!(
            !f.paths.boot_record_path(4242).exists(),
            "no boot record for another version"
        );
    }

    #[test]
    fn a_first_boot_that_ran_non_user_commands_exited_normally_and_did_not_confirm() {
        let f = applied(false);
        // The new version ran `rpi --help`… which confirms; a failed
        // one-shot command exits normally without confirming: not a crash.
        let probe = Arc::new(FakeProcessProbe::new(9000, 1));
        write_boot_record(
            &*f.fs,
            &f.paths.boot_record_path(5000),
            &super::super::records::BootRecord {
                schema_version: 1,
                pid: 5000,
                process_start: 50,
                version: RUNNING_NEW.to_owned(),
                normal_exit: true,
            },
        )
        .unwrap();
        assert_eq!(
            run_startup_transaction(
                &*f.fs,
                &f.paths,
                &*probe,
                &StartupEnv::plain(),
                RUNNING_NEW,
                &StartupOptions::default(),
                &|_: &str| {}
            ),
            TransactionOutcome::FirstBootOpen {
                target_version: "0.1.9-rc.4".to_owned()
            }
        );
        assert!(
            !f.paths.boot_record_path(5000).exists(),
            "the settled record is removed"
        );
        assert!(
            f.paths.pending_path().exists(),
            "the update is neither confirmed nor rolled back"
        );
    }

    #[test]
    fn a_crashed_first_boot_rolls_back_and_the_new_image_must_reexec_the_old_one() {
        let f = applied(true);
        let probe = Arc::new(FakeProcessProbe::new(9000, 1));
        write_boot_record(
            &*f.fs,
            &f.paths.boot_record_path(5000),
            &super::super::records::BootRecord {
                schema_version: 1,
                pid: 5000,
                process_start: 50,
                version: RUNNING_NEW.to_owned(),
                normal_exit: false,
            },
        )
        .unwrap();
        let outcome = run_startup_transaction(
            &*f.fs,
            &f.paths,
            &*probe,
            &StartupEnv {
                trampoline_pid: Some(4242),
                ..StartupEnv::default()
            },
            RUNNING_NEW,
            &StartupOptions::default(),
            &|_: &str| {},
        );
        let TransactionOutcome::RolledBack { message } = outcome else {
            panic!("expected a rollback")
        };
        assert!(
            message.contains("rolled back to rpi 0.1.9-rc.3"),
            "{message}"
        );
        let prefix = f.prefix();
        assert_eq!(std::fs::read(prefix.join("rpi.exe")).unwrap(), b"old exe");
        assert!(!prefix.join("rpi.exe.old-0.1.9-rc.3").exists());
        let container = f.dir.join("agent/extensions/rpi-todo");
        assert!(
            !container.join("current.json").exists(),
            "the pointer that did not exist is removed"
        );
        let blocked =
            read_record::<super::super::records::BlockedVersions>(&*f.fs, &f.paths.blocked_path())
                .unwrap();
        assert!(blocked.contains("0.1.9-rc.4"));
        assert!(!f.paths.boot_record_path(5000).exists());
        assert!(read_record::<PendingUpdate>(&*f.fs, &f.paths.pending_path()).is_none());
    }

    #[test]
    fn a_live_first_boot_of_another_instance_is_left_alone() {
        let f = applied(false);
        let probe = Arc::new(FakeProcessProbe::new(9000, 1));
        probe.spawn(5000, 50);
        write_boot_record(
            &*f.fs,
            &f.paths.boot_record_path(5000),
            &super::super::records::BootRecord {
                schema_version: 1,
                pid: 5000,
                process_start: 50,
                version: RUNNING_NEW.to_owned(),
                normal_exit: false,
            },
        )
        .unwrap();
        assert_eq!(
            run_startup_transaction(
                &*f.fs,
                &f.paths,
                &*probe,
                &StartupEnv::plain(),
                RUNNING_NEW,
                &StartupOptions::default(),
                &|_: &str| {}
            ),
            TransactionOutcome::FirstBootOpen {
                target_version: "0.1.9-rc.4".to_owned()
            }
        );
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"new exe"
        );
        assert!(f.paths.boot_record_path(5000).exists());
        assert!(
            f.paths.boot_record_path(9000).exists(),
            "this instance registered itself"
        );
    }

    #[test]
    fn an_applied_pending_without_the_protocol_is_confirmed_on_the_spot() {
        let f = applied(false);
        let mut pending = f.pending.clone();
        pending.applied = true;
        pending.first_boot_protocol = false;
        f.fs.write_atomic(&f.paths.pending_path(), to_json(&pending).as_bytes())
            .unwrap();
        assert_eq!(
            f.run(RUNNING_NEW),
            TransactionOutcome::FirstBootOpen {
                target_version: "0.1.9-rc.4".to_owned()
            }
        );
        assert!(!f.paths.pending_path().exists());
        assert!(!f.paths.undo_path().exists());
        assert!(!f.paths.boot_record_path(4242).exists());
    }

    // ---- the trampoline's judgement ----------------------------------------

    /// Judge a child `pid` whose boot record (`normal_exit` `None`: no
    /// record was written) is registered under a separate probe.
    fn judge_child(f: &Fixture, pid: u32, normal_exit: Option<bool>) -> TrampolineVerdict {
        let probe = FakeProcessProbe::new(1, 1);
        if let Some(normal_exit) = normal_exit {
            write_boot_record(
                &*f.fs,
                &f.paths.boot_record_path(pid),
                &super::super::records::BootRecord {
                    schema_version: 1,
                    pid,
                    process_start: pid as u64,
                    version: RUNNING_NEW.to_owned(),
                    normal_exit,
                },
            )
            .unwrap();
        }
        judge_trampoline_child(
            &*f.fs,
            &f.paths,
            &probe,
            pid,
            &StartupOptions::default(),
            &|_: &str| {},
        )
    }

    #[test]
    fn the_trampoline_passes_a_confirmed_or_normal_exit_through_and_rolls_a_crash_back() {
        // Still applied and no record at all (the child died before P1):
        // an unmarked termination is a crash.
        let f = applied(false);
        assert_eq!(judge_child(&f, 5100, None), TrampolineVerdict::RolledBack);
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"old exe"
        );
        assert!(
            read_record::<super::super::records::BlockedVersions>(&*f.fs, &f.paths.blocked_path())
                .is_some_and(|blocked| blocked.contains(RUNNING_NEW))
        );

        // A normal exit before any confirmation is not a crash: the update
        // keeps waiting for a later confirmation.
        let f = applied(false);
        assert_eq!(
            judge_child(&f, 5101, Some(true)),
            TrampolineVerdict::PassThrough,
            "normal exit without a confirmation"
        );
        assert!(
            !f.paths.boot_record_path(5101).exists(),
            "settled records are removed"
        );
        assert!(
            f.paths.pending_path().exists(),
            "still waiting for a confirmation"
        );
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"new exe"
        );

        // Exited without the normal-exit mark: a crash.
        let f = applied(false);
        assert_eq!(
            judge_child(&f, 5102, Some(false)),
            TrampolineVerdict::RolledBack,
            "no normal-exit mark"
        );

        // Confirmed: nothing is judged.
        let f = applied(false);
        confirm_update(&*f.fs, &f.paths, &f.pending);
        assert_eq!(
            judge_child(&f, 5103, None),
            TrampolineVerdict::PassThrough,
            "confirmed"
        );

        // Another instance is still in its first boot: not this child's call.
        let f = applied(false);
        let probe = Arc::new(FakeProcessProbe::new(1, 1));
        probe.spawn(6000, 1);
        write_boot_record(
            &*f.fs,
            &f.paths.boot_record_path(6000),
            &super::super::records::BootRecord {
                schema_version: 1,
                pid: 6000,
                process_start: 1,
                version: RUNNING_NEW.to_owned(),
                normal_exit: false,
            },
        )
        .unwrap();
        assert_eq!(
            judge_trampoline_child(
                &*f.fs,
                &f.paths,
                &*probe,
                6001,
                &StartupOptions::default(),
                &|_: &str| {}
            ),
            TrampolineVerdict::PassThrough,
            "a live first-boot instance keeps the judgement open"
        );
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"new exe"
        );
    }

    #[test]
    fn the_trampoline_defers_when_it_cannot_take_the_lock() {
        let f = applied(false);
        let probe = Arc::new(FakeProcessProbe::new(1, 1));
        let _held =
            lock::acquire_exclusive(&f.paths.lock_path(), Duration::from_millis(100)).unwrap();
        assert_eq!(
            judge_trampoline_child(
                &*f.fs,
                &f.paths,
                &*probe,
                5100,
                &StartupOptions {
                    lock_wait: Duration::from_millis(50)
                },
                &|_: &str| {}
            ),
            TrampolineVerdict::Deferred
        );
        assert_eq!(
            std::fs::read(f.prefix().join("rpi.exe")).unwrap(),
            b"new exe"
        );
    }

    #[test]
    fn staged_tree_verification_reports_added_missing_and_changed_files() {
        let f = Fixture::staged(true);
        let staged = &f.pending.extensions[0].staged_path;
        let expected = collect_file_digests(&*f.fs, staged).unwrap();
        assert!(verify_staged_tree(&*f.fs, staged, &expected).is_ok());
        std::fs::write(staged.join("extra.txt"), b"x").unwrap();
        let error = verify_staged_tree(&*f.fs, staged, &expected).unwrap_err();
        assert!(error.contains("3 file(s)"), "{error}");
        std::fs::remove_file(staged.join("extra.txt")).unwrap();
        let error = verify_staged_tree(&*f.fs, staged, &expected[..1]).unwrap_err();
        assert!(
            error.contains("2 file(s), the update recorded 1"),
            "{error}"
        );
        let error = verify_staged_tree(&*f.fs, staged, &[]).unwrap_err();
        assert!(
            error.contains("2 file(s), the update recorded 0"),
            "{error}"
        );
        // Same count, one recorded file missing from the directory.
        let mut missing = expected.clone();
        missing[0].path = PathBuf::from("missing.txt");
        let error = verify_staged_tree(&*f.fs, staged, &missing).unwrap_err();
        assert!(error.contains("lacks missing.txt"), "{error}");
        let mut changed = expected.clone();
        changed[0].sha256 = "0".repeat(64);
        let error = verify_staged_tree(&*f.fs, staged, &changed).unwrap_err();
        assert!(error.contains("changed"), "{error}");
    }

    #[test]
    fn env_flags_are_read_from_the_process_environment() {
        // Only the trampoline pid is parsed strictly; flags are "set and
        // non-empty".
        assert_eq!(StartupEnv::plain(), StartupEnv::default());
        rpi_test_env::set_var(super::super::ENV_TRAMPOLINE_PID, "77");
        rpi_test_env::set_var("RPI_SUBAGENT_CHILD", "");
        let env = StartupEnv::from_env();
        rpi_test_env::remove_var(super::super::ENV_TRAMPOLINE_PID);
        rpi_test_env::remove_var("RPI_SUBAGENT_CHILD");
        assert_eq!(env.trampoline_pid, Some(77));
        assert!(!env.subagent_child, "an empty flag is not set");
        rpi_test_env::set_var("RPI_SUBAGENT_CHILD", "1");
        let env = StartupEnv::from_env();
        rpi_test_env::remove_var("RPI_SUBAGENT_CHILD");
        assert!(env.subagent_child);
    }
}
