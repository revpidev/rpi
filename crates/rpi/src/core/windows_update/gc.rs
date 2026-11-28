//! The startup cleanup scan (design §11.9, R7.5.1).
//!
//! Every start (but a subagent or preflight child) scans for old-version
//! leftovers and classifies each one without ever terminating a process:
//!
//! - **retained** — kept by policy: the old executable of an update that is
//!   applied or confirmed but has not seen its first normal exit after
//!   confirmation (the rollback pre-image and the manual-recovery window).
//! - **live** — in use: an old executable of a version a running instance
//!   still has in its pin, an extension directory a pin names, a staging
//!   directory its live owner is still filling.
//! - **garbage** — referenced by nobody: deleted.
//! - **stuck** — garbage that could not be deleted; the reason category is
//!   reported.
//!
//! The scan reads no file content except pointers, and only renames or
//! deletes things nobody references. It runs after the startup transaction
//! and does not take the update lock: two scans racing on the same garbage
//! merely race to the same result.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rpi_ext_host::install_layout::{
    GENERATION_POINTER_FILE, GenerationPointer, RETIRED_MARKER_FILE, is_reserved_container_entry,
};

use super::boot::ProcessProbe;
use super::fs::UpdateFs;
use super::layout::{OLD_EXE_SUFFIX, UpdatePaths, version_from_exe_leftover_name};
use super::pins::pin_references;
use super::records::{PendingUpdate, PinRecord, UpdateResult, UpdateState, read_record};

/// What a leftover is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftoverKind {
    /// `rpi.exe.old-<version>`: the image an update parked.
    OldExe,
    /// `rpi.exe.bad-<version>`: the image a rollback set aside.
    NewExe,
    /// A generation directory nobody keeps.
    Generation,
    /// Legacy flat files left in a migrated container root.
    FlatFiles,
    /// An extension extraction directory under `.staging/` or a legacy
    /// `<name>.tmp-<pid>/` / `<name>.old-<pid>/`.
    Staging,
    /// A container retired by `rpi remove`.
    RetiredContainer,
    /// Staged binary files of a discarded update.
    BinaryStaging,
    /// Boot records outside the `applied` state.
    BootRecord,
    /// Temporary files of interrupted state writes.
    TempFile,
}

/// A leftover in use, with the pids (excluding this process) that use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveLeftover {
    pub kind: LeftoverKind,
    pub path: PathBuf,
    pub holders: Vec<u32>,
}

/// A leftover that could not be deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckLeftover {
    pub kind: LeftoverKind,
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub removed: Vec<(LeftoverKind, PathBuf)>,
    /// Garbage found by a classify-only scan ([`cleanup_scan_with`] with
    /// `remove == false`): would be deleted, nothing was touched.
    pub garbage: Vec<(LeftoverKind, PathBuf)>,
    pub live: Vec<LiveLeftover>,
    pub retained: Vec<PathBuf>,
    pub stuck: Vec<StuckLeftover>,
    /// The instances that run the retained `.old-<version>` image (by
    /// image identity): the reminder's "other instances still on the
    /// previous version".
    pub old_exe_holders: Vec<u32>,
    /// Staging directories skipped because a live process may own them.
    pub skipped_staging: usize,
    /// Whether the old executable of the pending update is gone (removed
    /// now, or never there).
    pub old_exe_gone: bool,
}

impl ScanReport {
    /// Distinct pids (other than the caller) holding a live leftover.
    pub fn holders(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = self
            .live
            .iter()
            .flat_map(|item| item.holders.iter().copied())
            .collect();
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

/// What the scan needs to know about the running installation.
pub struct ScanContext<'a> {
    pub paths: &'a UpdatePaths,
    pub probe: &'a dyn ProcessProbe,
    /// Pins of live processes, this one included.
    pub live_pins: Vec<PinRecord>,
    pub self_pid: u32,
    pub state: UpdateState,
    pub pending: Option<&'a PendingUpdate>,
    pub result: Option<&'a UpdateResult>,
    pub now: SystemTime,
}

/// The reason category of a failed removal (R7.5.2).
pub fn stuck_reason(error: &std::io::Error, kind: LeftoverKind) -> String {
    let raw = error.raw_os_error();
    match error.kind() {
        std::io::ErrorKind::NotFound => "already gone".to_owned(),
        std::io::ErrorKind::PermissionDenied
            if matches!(kind, LeftoverKind::OldExe | LeftoverKind::NewExe) =>
        {
            // A mapped image always refuses deletion: an instance runs it.
            "still in use by a running instance".to_owned()
        }
        std::io::ErrorKind::PermissionDenied => "access denied".to_owned(),
        _ if raw == Some(5) && matches!(kind, LeftoverKind::OldExe | LeftoverKind::NewExe) => {
            "still in use by a running instance".to_owned()
        }
        _ if raw == Some(5) => "access denied".to_owned(),
        _ if raw == Some(32) => "in use".to_owned(),
        _ => "I/O error".to_owned(),
    }
}

/// The state files that legitimately live in `.update/`.
const STATE_FILES: [&str; 6] = [
    super::layout::PENDING_FILE,
    super::layout::JOURNAL_FILE,
    super::layout::UNDO_FILE,
    super::layout::RESULT_FILE,
    super::layout::BLOCKED_FILE,
    super::layout::LOCK_FILE,
];

/// Run the scan, removing what nobody uses. Failures of individual removals
/// become `stuck` entries; the scan itself cannot fail.
pub fn cleanup_scan(fs: &dyn UpdateFs, context: &ScanContext<'_>) -> ScanReport {
    cleanup_scan_with(fs, context, true)
}

/// [`cleanup_scan`] with the removal switched off: a detection-only pass
/// classifies everything (garbage, live, retained, stuck) and deletes
/// nothing — `rpi self-uninstall` uses it to refuse while files are in use.
pub fn cleanup_scan_with(fs: &dyn UpdateFs, context: &ScanContext<'_>, remove: bool) -> ScanReport {
    let mut report = ScanReport::default();
    scan_executable_leftovers(fs, context, &mut report, remove);
    scan_update_dir(fs, context, &mut report, remove);
    for root in &context.paths.extension_roots {
        scan_extension_root(fs, context, root, &mut report, remove);
    }
    report.old_exe_gone = old_exe_is_gone(fs, context, &report);
    report
}

/// The instances that run the image at `path` (`rpi.exe.old-<version>` or
/// `.bad-<version>`): by image identity (length and modification time, the
/// marks a pin records at start) when both the file and the pin carry
/// them, by version string otherwise. The version alone would count every
/// instance of a same-version reinstall — the one that replaced the
/// parked image included — as a holder of the parked image.
fn holders_of_exe(
    fs: &dyn UpdateFs,
    context: &ScanContext<'_>,
    path: &Path,
    version: &str,
) -> Vec<u32> {
    let identity = fs.exe_identity(path).ok();
    let mut pids: Vec<u32> = context
        .live_pins
        .iter()
        .filter(|pin| match (identity, pin.exe_identity) {
            (Some(file), Some(pin_image)) => file == pin_image,
            _ => pin.exe_version == version,
        })
        .map(|pin| pin.pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Pids whose pin names `dir` or anything below it.
fn holders_within(context: &ScanContext<'_>, dir: &Path) -> Vec<u32> {
    let key = super::pins::path_key(dir);
    let prefix = format!("{key}{}", std::path::MAIN_SEPARATOR);
    let mut pids: Vec<u32> = context
        .live_pins
        .iter()
        .filter(|pin| {
            pin.dirs
                .iter()
                .any(|listed| super::pins::path_key(listed).starts_with(&prefix))
        })
        .map(|pin| pin.pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn holders_of_dir(context: &ScanContext<'_>, dir: &Path) -> Vec<u32> {
    let mut pids: Vec<u32> = context
        .live_pins
        .iter()
        .filter(|pin| pin_references(pin, dir))
        .map(|pin| pin.pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Delete one leftover (or, in a classify-only scan, only record it).
/// The DELETE-access probe runs first (design §11.9): a path some process
/// holds open without share-delete is *live* and is never touched — a
/// loaded library would otherwise keep a half-deleted tree. The probe
/// cannot see an image that is mapped (Windows grants DELETE access to a
/// running executable) nor a library loaded from inside a directory, so
/// a directory is additionally renamed aside before it is deleted: the
/// rename fails as a whole where the recursive delete would stop halfway,
/// and that failure classifies the directory as live. `NotFound` means
/// the path vanished meanwhile and counts as removed.
fn remove_leftover(
    fs: &dyn UpdateFs,
    context: &ScanContext<'_>,
    kind: LeftoverKind,
    path: &Path,
    report: &mut ScanReport,
    directory: bool,
    remove: bool,
) {
    match fs.probe_deletable(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if remove {
                report.removed.push((kind, path.to_path_buf()));
            } else {
                report.garbage.push((kind, path.to_path_buf()));
            }
            return;
        }
        Err(_) => {
            report.live.push(LiveLeftover {
                kind,
                path: path.to_path_buf(),
                holders: holders_for(fs, context, kind, path),
            });
            return;
        }
    }
    if !remove {
        report.garbage.push((kind, path.to_path_buf()));
        return;
    }
    // A directory is renamed aside first; `where` is what is left to
    // delete when the deletion does not finish (the aside name, so the
    // warning points at what exists).
    let mut left = path.to_path_buf();
    let result = if directory {
        match rename_aside(fs, context, path) {
            Ok(aside) => {
                left = aside;
                fs.remove_dir_all(&left)
            }
            Err(_) => {
                report.live.push(LiveLeftover {
                    kind,
                    path: path.to_path_buf(),
                    holders: holders_for(fs, context, kind, path),
                });
                return;
            }
        }
    } else {
        fs.remove_file(path)
    };
    match result {
        Ok(()) => report.removed.push((kind, path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.removed.push((kind, path.to_path_buf()));
        }
        Err(error) => report.stuck.push(StuckLeftover {
            kind,
            path: left,
            reason: stuck_reason(&error, kind),
        }),
    }
}

/// Move a leftover directory aside (`<name>.garbage-<pid>`) so that its
/// deletion is all-or-nothing with respect to loaded libraries. The aside
/// name stays garbage for the next scan — [`is_garbage_name`] directories
/// are leftovers whatever a half-finished deletion left inside them —
/// when the deletion does not finish this time.
fn rename_aside(
    fs: &dyn UpdateFs,
    context: &ScanContext<'_>,
    path: &Path,
) -> std::io::Result<PathBuf> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no parent",
        ));
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut aside = parent.join(format!("{name}.garbage-{}", context.self_pid));
    if fs.exists(&aside).unwrap_or(false) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);
        aside = parent.join(format!("{name}.garbage-{}.t{stamp}", context.self_pid));
    }
    fs.rename(path, &aside)?;
    Ok(aside)
}

/// Whether a directory name is the aside name of a leftover (`<name>
/// .garbage-<pid>`): a half-deleted tree that no longer carries the marks
/// the ordinary classification looks for (a generation's manifest, a
/// retired container's marker).
fn is_garbage_name(name: &str) -> bool {
    name.contains(".garbage-")
}

/// The instances that hold a leftover: old/new images by their version,
/// everything else by the directories the pins name.
fn holders_for(
    fs: &dyn UpdateFs,
    context: &ScanContext<'_>,
    kind: LeftoverKind,
    path: &Path,
) -> Vec<u32> {
    let exe_name = context.paths.exe_file_name();
    if matches!(kind, LeftoverKind::OldExe | LeftoverKind::NewExe)
        && let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        && let Some(version) = version_from_exe_leftover_name(&exe_name, &name)
    {
        return holders_of_exe(fs, context, path, &version);
    }
    holders_of_dir(context, path)
}

/// Whether the old executable of an applied or confirmed update is kept by
/// policy: until the first normal exit after the confirmation (design
/// §11.7). Judged by the update result, which outlives the pending the
/// confirmation deletes.
fn old_exe_retained(context: &ScanContext<'_>, version: &str) -> bool {
    if !context.state.keeps_old_exe() {
        return false;
    }
    let Some(result) = context.result else {
        return false;
    };
    // A target without the first-boot protocol has no exit marking: it
    // would never release the image, and that target cannot read the
    // pointer layout anyway. Its `.old` is an ordinary leftover.
    if !result.first_boot_protocol {
        return false;
    }
    if result.from_version.as_deref() != Some(version) {
        return false;
    }
    !result.normal_exit_after_confirm
}

fn scan_executable_leftovers(
    fs: &dyn UpdateFs,
    context: &ScanContext<'_>,
    report: &mut ScanReport,
    remove: bool,
) {
    let Some(prefix) = context.paths.exe_path.parent() else {
        return;
    };
    let exe_name = context.paths.exe_file_name();
    for entry in fs.read_dir(prefix).unwrap_or_default() {
        if entry.is_dir {
            continue;
        }
        let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(version) = version_from_exe_leftover_name(&exe_name, name) else {
            continue;
        };
        let kind = if name.contains(OLD_EXE_SUFFIX) {
            LeftoverKind::OldExe
        } else {
            LeftoverKind::NewExe
        };
        if kind == LeftoverKind::OldExe && old_exe_retained(context, &version) {
            report.old_exe_holders = holders_of_exe(fs, context, &entry.path, &version);
            report.retained.push(entry.path);
            continue;
        }
        let holders = holders_of_exe(fs, context, &entry.path, &version);
        if !holders.is_empty() {
            report.live.push(LiveLeftover {
                kind,
                path: entry.path,
                holders,
            });
            continue;
        }
        remove_leftover(fs, context, kind, &entry.path, report, false, remove);
    }
}

fn scan_update_dir(
    fs: &dyn UpdateFs,
    context: &ScanContext<'_>,
    report: &mut ScanReport,
    remove: bool,
) {
    let update_dir = &context.paths.update_dir;
    // A staged (not yet applied) executable waits for the transaction; once
    // applied it was moved out and only an empty directory remains.
    let staged_binary_dir = context
        .pending
        .filter(|pending| !pending.applied)
        .and_then(|pending| pending.binary.staged_path.parent().map(Path::to_path_buf));
    let entries = fs.read_dir(update_dir).unwrap_or_default();
    for entry in entries {
        let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == super::layout::STAGING_DIR_NAME {
            for staged in fs.read_dir(&entry.path).unwrap_or_default() {
                if staged.is_dir && Some(staged.path.clone()) == staged_binary_dir {
                    continue;
                }
                remove_leftover(
                    fs,
                    context,
                    LeftoverKind::BinaryStaging,
                    &staged.path,
                    report,
                    true,
                    remove,
                );
            }
        } else if name == super::layout::BOOT_DIR_NAME {
            if context.state != UpdateState::Applied {
                for record in fs.read_dir(&entry.path).unwrap_or_default() {
                    if !record.is_dir {
                        remove_leftover(
                            fs,
                            context,
                            LeftoverKind::BootRecord,
                            &record.path,
                            report,
                            false,
                            remove,
                        );
                    }
                }
            }
        } else if entry.is_dir {
            // Only `staging/` and `boot/` are expected.
            remove_leftover(
                fs,
                context,
                LeftoverKind::TempFile,
                &entry.path,
                report,
                true,
                remove,
            );
        } else if name == super::layout::UNDO_FILE {
            // The pre-image outlives its update only while the old
            // executable is kept (`applied` / `confirmed`).
            if !context.state.keeps_old_exe() {
                remove_leftover(
                    fs,
                    context,
                    LeftoverKind::TempFile,
                    &entry.path,
                    report,
                    false,
                    remove,
                );
            }
        } else if !STATE_FILES.contains(&name) {
            remove_leftover(
                fs,
                context,
                LeftoverKind::TempFile,
                &entry.path,
                report,
                false,
                remove,
            );
        }
    }
}

/// The pid at the end of a staging or legacy temporary name
/// (`rpi-todo-0.2.0-4242`, `rpi-todo.tmp-4242`).
fn owner_pid(name: &str) -> Option<u32> {
    name.rsplit_once('-')?.1.parse().ok()
}

fn scan_staging_dir(
    fs: &dyn UpdateFs,
    context: &ScanContext<'_>,
    staging: &Path,
    report: &mut ScanReport,
    remove: bool,
) {
    let pending_paths: Vec<&Path> = context
        .pending
        .map(|pending| {
            pending
                .extensions
                .iter()
                .map(|extension| extension.staged_path.as_path())
                .collect()
        })
        .unwrap_or_default();
    for entry in fs.read_dir(staging).unwrap_or_default() {
        if !entry.is_dir || pending_paths.contains(&entry.path.as_path()) {
            continue;
        }
        let name = entry
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        // An aside name ends in the renaming process's pid: that is no
        // owner, the directory is garbage as soon as it exists.
        let owner_alive = !is_garbage_name(name)
            && owner_pid(name).is_some_and(|pid| context.probe.process_start(pid).is_some());
        let age = fs
            .modified(&entry.path)
            .ok()
            .and_then(|modified| context.now.duration_since(modified).ok());
        if owner_alive && age.is_some_and(|age| age < super::STAGING_MAX_AGE) {
            report.skipped_staging += 1;
            continue;
        }
        remove_leftover(
            fs,
            context,
            LeftoverKind::Staging,
            &entry.path,
            report,
            true,
            remove,
        );
    }
}

fn read_pointer(fs: &dyn UpdateFs, container: &Path) -> Option<GenerationPointer> {
    read_record::<GenerationPointer>(fs, &container.join(GENERATION_POINTER_FILE))
        .filter(|pointer| rpi_ext_host::install_layout::is_safe_generation_path(&pointer.path))
}

fn scan_extension_root(
    fs: &dyn UpdateFs,
    context: &ScanContext<'_>,
    root: &Path,
    report: &mut ScanReport,
    remove: bool,
) {
    if !fs.exists(root).unwrap_or(false) {
        return;
    }
    for entry in fs.read_dir(root).unwrap_or_default() {
        let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == super::layout::EXTENSION_STAGING_DIR {
            scan_staging_dir(fs, context, &entry.path, report, remove);
            continue;
        }
        if name == super::layout::EXTENSION_PINS_DIR {
            continue;
        }
        if !entry.is_dir {
            continue;
        }
        if is_garbage_name(name) {
            // The aside name of a leftover whose deletion did not finish
            // (round-3 review): a leftover whatever is left inside.
            let kind = if fs
                .exists(&entry.path.join(RETIRED_MARKER_FILE))
                .unwrap_or(false)
            {
                LeftoverKind::RetiredContainer
            } else {
                LeftoverKind::Staging
            };
            remove_leftover(fs, context, kind, &entry.path, report, true, remove);
            continue;
        }
        if name.contains(".tmp-") || name.contains(".old-") {
            // Legacy activation leftovers of the in-place swap (the package
            // root itself, so it carries a manifest like an install).
            let owner_alive =
                owner_pid(name).is_some_and(|pid| context.probe.process_start(pid).is_some());
            if owner_alive {
                report.skipped_staging += 1;
            } else {
                remove_leftover(
                    fs,
                    context,
                    LeftoverKind::Staging,
                    &entry.path,
                    report,
                    true,
                    remove,
                );
            }
            continue;
        }
        if is_reserved_container_entry(name) {
            continue;
        }
        let container = entry.path;
        if fs
            .exists(&container.join(RETIRED_MARKER_FILE))
            .unwrap_or(false)
        {
            // A pin on any directory inside the container keeps it too.
            let holders = holders_within(context, &container);
            if holders.is_empty() {
                remove_leftover(
                    fs,
                    context,
                    LeftoverKind::RetiredContainer,
                    &container,
                    report,
                    true,
                    remove,
                );
            } else {
                report.live.push(LiveLeftover {
                    kind: LeftoverKind::RetiredContainer,
                    path: container.clone(),
                    holders,
                });
            }
            continue;
        }
        let pointer = read_pointer(fs, &container);
        for sub in fs.read_dir(&container).unwrap_or_default() {
            if !sub.is_dir {
                continue;
            }
            let Some(sub_name) = sub.path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if is_garbage_name(sub_name) {
                // The aside name of a generation whose deletion did not
                // finish — a leftover even without its manifest.
                remove_leftover(
                    fs,
                    context,
                    LeftoverKind::Generation,
                    &sub.path,
                    report,
                    true,
                    remove,
                );
                continue;
            }
            if is_reserved_container_entry(sub_name)
                || !sub.path.join("rpi-extension.json").is_file()
            {
                // Package subdirectories (`dist/`, `lib/`) are not generations.
                continue;
            }
            let kept = pointer.as_ref().is_some_and(|pointer| {
                sub_name == pointer.path || pointer.previous.as_deref() == Some(sub_name)
            });
            if kept {
                continue;
            }
            let holders = holders_of_dir(context, &sub.path);
            if !holders.is_empty() {
                report.live.push(LiveLeftover {
                    kind: LeftoverKind::Generation,
                    path: sub.path,
                    holders,
                });
                continue;
            }
            remove_leftover(
                fs,
                context,
                LeftoverKind::Generation,
                &sub.path,
                report,
                true,
                remove,
            );
        }
        if pointer.is_some() {
            scan_flat_leftovers(fs, context, &container, report, remove);
        }
    }
}

/// Files of a legacy flat install left in the container root after the
/// install moved to a generation. Only unreferenced ones go: an instance
/// that started before the migration still loads them. While an update
/// waits for its first start (`applied`) nothing is reclaimed at all —
/// instances older than the pins (`0.1.9-rc.4`) write none, so a flat
/// install they still use cannot be told from an unreferenced one until
/// the first start confirmed (known limit, release notes).
fn scan_flat_leftovers(
    fs: &dyn UpdateFs,
    context: &ScanContext<'_>,
    container: &Path,
    report: &mut ScanReport,
    remove: bool,
) {
    if context.state == UpdateState::Applied {
        return;
    }
    let keep = [
        GENERATION_POINTER_FILE,
        RETIRED_MARKER_FILE,
        crate::core::extension_registry::GITHUB_INSTALL_MARKER_FILE,
    ];
    let holders = holders_of_dir(context, container);
    for entry in fs.read_dir(container).unwrap_or_default() {
        if entry.is_dir {
            continue;
        }
        let Some(name) = entry.path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if keep.contains(&name) {
            continue;
        }
        if !holders.is_empty() {
            report.live.push(LiveLeftover {
                kind: LeftoverKind::FlatFiles,
                path: container.to_path_buf(),
                holders: holders.clone(),
            });
            return;
        }
        remove_leftover(
            fs,
            context,
            LeftoverKind::FlatFiles,
            &entry.path,
            report,
            false,
            remove,
        );
    }
}

/// Whether nothing of the old version is left in the executable prefix
/// (the first-boot reminder then says the previous version was removed).
fn old_exe_is_gone(fs: &dyn UpdateFs, context: &ScanContext<'_>, report: &ScanReport) -> bool {
    let Some(result) = context.result.filter(|result| result.state.keeps_old_exe()) else {
        return false;
    };
    let Some(from_version) = result.from_version.as_deref() else {
        return false;
    };
    if old_exe_retained(context, from_version) {
        return false;
    }
    if report
        .live
        .iter()
        .any(|item| item.kind == LeftoverKind::OldExe)
        || report
            .stuck
            .iter()
            .any(|item| item.kind == LeftoverKind::OldExe)
    {
        return false;
    }
    let Some(prefix) = context.paths.exe_path.parent() else {
        return true;
    };
    let exe_name = context.paths.exe_file_name();
    !fs.read_dir(prefix).unwrap_or_default().iter().any(|entry| {
        let name = entry
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        version_from_exe_leftover_name(&exe_name, &name)
            .is_some_and(|version| version == from_version)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::windows_update::boot::FakeProcessProbe;
    use crate::core::windows_update::fs::{FsOp, SpyFs};
    use crate::core::windows_update::layout::{UpdatePaths as Layout, safe_version_component};
    use crate::core::windows_update::pins::PinHandle;
    use crate::core::windows_update::records::{FileDigest, StagedBinary, to_json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        dir: PathBuf,
        fs: Arc<SpyFs>,
        paths: UpdatePaths,
        probe: FakeProcessProbe,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rpi-wu-gc-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(dir.join("prefix")).unwrap();
            std::fs::create_dir_all(dir.join("agent/extensions")).unwrap();
            let fs = Arc::new(SpyFs::new());
            let paths = UpdatePaths::new(
                &dir.join("prefix/rpi.exe"),
                dir.join("logs/update.log"),
                &dir.join("agent"),
                None,
            );
            Fixture {
                dir,
                fs,
                paths,
                probe: FakeProcessProbe::new(1000, 1),
            }
        }

        fn root(&self) -> PathBuf {
            self.dir.join("agent/extensions")
        }

        fn write(&self, path: &Path, text: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn generation(&self, name: &str, version: &str) -> PathBuf {
            let dir = self.root().join(name).join(safe_version_component(version));
            self.write(&dir.join("rpi-extension.json"), "{}");
            dir
        }

        /// A registration of another live instance (left on disk).
        fn pin(&self, pid: u32, exe_version: &str, dirs: Vec<PathBuf>) {
            self.pin_image(pid, exe_version, &self.paths.exe_path.clone(), dirs);
        }

        /// Pin `pid` as an instance of `exe_version` that started from
        /// `image` (its identity is what the holder judgement compares).
        fn pin_image(&self, pid: u32, exe_version: &str, image: &Path, dirs: Vec<PathBuf>) {
            let mut record =
                PinRecord::new(pid, pid as u64 * 10, format!("s{pid}"), exe_version, image);
            record.dirs = dirs;
            self.fs
                .write_atomic(
                    &self.paths.pins_dir().join(format!("{pid}.json")),
                    super::super::records::to_json(&record).as_bytes(),
                )
                .unwrap();
            self.probe.spawn(pid, pid as u64 * 10);
        }

        fn context<'a>(&'a self, state: UpdateState) -> ScanContext<'a> {
            self.context_with(state, None, None)
        }

        fn context_with<'a>(
            &'a self,
            state: UpdateState,
            pending: Option<&'a PendingUpdate>,
            result: Option<&'a UpdateResult>,
        ) -> ScanContext<'a> {
            let live =
                super::super::pins::live_pins(&*self.fs, &self.paths.pins_dir(), &self.probe);
            ScanContext {
                paths: &self.paths,
                probe: &self.probe,
                live_pins: live,
                self_pid: 1000,
                state,
                pending,
                result,
                now: SystemTime::now(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn applied_pending(from: &str, to: &str) -> PendingUpdate {
        PendingUpdate {
            schema_version: 1,
            target_version: to.to_owned(),
            channel: "stable".to_owned(),
            written_at: "2026-11-28T00:00:00Z".to_owned(),
            from_version: from.to_owned(),
            allow_non_monotonic: false,
            first_boot_protocol: true,
            applied: true,
            binary: StagedBinary {
                staged_path: PathBuf::from("/nowhere/staging/rpi.exe"),
                sha256: "0".repeat(64),
                source_url: String::new(),
                asset_sha256: "0".repeat(64),
                target: "t".to_owned(),
            },
            extensions: Vec::new(),
        }
    }

    #[test]
    fn an_empty_installation_has_nothing_to_do() {
        let f = Fixture::new();
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert_eq!(report, ScanReport::default());
        assert_eq!(f.fs.count(FsOp::Read), 0, "the scan reads no file content");
    }

    #[test]
    fn the_old_exe_of_an_applied_update_is_retained_until_a_normal_exit() {
        let f = Fixture::new();
        let old = f.paths.old_exe_path("0.1.9-rc.3");
        std::fs::write(&old, b"old").unwrap();
        let pending = applied_pending("0.1.9-rc.3", "0.1.9-rc.4");

        // Applied, first boot open: kept, silently. The update result
        // names the version the old image belongs to (the pending is still
        // there too) and carries the protocol flag of the commit.
        let mut applied = UpdateResult::new(
            UpdateState::Applied,
            Some("0.1.9-rc.3".to_owned()),
            Some("0.1.9-rc.4".to_owned()),
        );
        applied.first_boot_protocol = true;
        let report = cleanup_scan(
            &*f.fs,
            &f.context_with(UpdateState::Applied, Some(&pending), Some(&applied)),
        );
        assert_eq!(report.retained, vec![old.clone()]);
        assert!(report.removed.is_empty());
        assert!(!report.old_exe_gone);
        assert!(old.exists());

        // Confirmed but no normal exit since: still kept. The confirmation
        // deleted the pending; the result alone carries the retention.
        let mut confirmed = UpdateResult::new(
            UpdateState::Confirmed,
            Some("0.1.9-rc.3".to_owned()),
            Some("0.1.9-rc.4".to_owned()),
        );
        confirmed.first_boot_protocol = true;
        let report = cleanup_scan(
            &*f.fs,
            &f.context_with(UpdateState::Confirmed, None, Some(&confirmed)),
        );
        assert_eq!(report.retained, vec![old.clone()]);

        // First normal exit after confirmation recorded: removed.
        let mut exited = confirmed.clone();
        exited.normal_exit_after_confirm = true;
        let report = cleanup_scan(
            &*f.fs,
            &f.context_with(UpdateState::Confirmed, None, Some(&exited)),
        );
        assert_eq!(report.removed, vec![(LeftoverKind::OldExe, old.clone())]);
        assert!(report.old_exe_gone);
        assert!(!old.exists());
    }

    /// A target without the first-boot protocol has no exit marking, so
    /// its previous image is not retained: an ordinary leftover, removed
    /// as soon as nothing runs it (round-2 review).
    #[test]
    fn the_old_exe_of_a_target_without_the_protocol_is_not_retained() {
        let f = Fixture::new();
        let old = f.paths.old_exe_path("0.1.9-rc.3");
        std::fs::write(&old, b"old").unwrap();
        let mut result = UpdateResult::new(
            UpdateState::Confirmed,
            Some("0.1.9-rc.3".to_owned()),
            Some("0.1.9-rc.2".to_owned()),
        );
        result.first_boot_protocol = false;
        let report = cleanup_scan(
            &*f.fs,
            &f.context_with(UpdateState::Confirmed, None, Some(&result)),
        );
        assert!(report.retained.is_empty(), "{report:?}");
        assert_eq!(report.removed, vec![(LeftoverKind::OldExe, old.clone())]);
        assert!(!old.exists());
    }

    #[test]
    fn an_old_exe_another_instance_still_runs_is_live_not_deleted() {
        let f = Fixture::new();
        let old = f.paths.old_exe_path("0.1.9-rc.3");
        std::fs::write(&old, b"old").unwrap();
        f.pin(2000, "0.1.9-rc.3", vec![]);
        f.pin(2001, "0.1.9-rc.3", vec![]);
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Confirmed));
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(
            report.live,
            vec![LiveLeftover {
                kind: LeftoverKind::OldExe,
                path: old.clone(),
                holders: vec![2000, 2001],
            }]
        );
        assert_eq!(report.holders(), vec![2000, 2001]);
        assert!(old.exists());
        // Both instances exit: the next start removes it.
        f.probe.kill(2000);
        f.probe.kill(2001);
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Confirmed));
        assert_eq!(report.removed, vec![(LeftoverKind::OldExe, old.clone())]);
    }

    #[test]
    fn a_bad_exe_left_by_a_rollback_is_garbage_and_a_failure_is_stuck_with_a_reason() {
        let f = Fixture::new();
        let bad = f.paths.bad_exe_path("0.1.9-rc.4");
        std::fs::write(&bad, b"new").unwrap();
        f.fs.fail_with(
            FsOp::RemoveFile,
            "rpi.exe.bad-0.1.9-rc.4",
            1,
            std::io::Error::from_raw_os_error(5),
        );
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::RolledBack));
        assert_eq!(
            report.stuck,
            vec![StuckLeftover {
                kind: LeftoverKind::NewExe,
                path: bad.clone(),
                reason: "still in use by a running instance".to_owned(),
            }]
        );
        assert!(bad.exists());
    }

    #[test]
    fn unreferenced_generations_go_and_current_previous_and_pinned_ones_stay() {
        let f = Fixture::new();
        let container = f.root().join("rpi-todo");
        let v1 = f.generation("rpi-todo", "0.1.0");
        let v2 = f.generation("rpi-todo", "0.2.0");
        let v3 = f.generation("rpi-todo", "0.3.0");
        let v4 = f.generation("rpi-todo", "0.4.0");
        let v5 = f.generation("rpi-todo", "0.5.0");
        f.write(
            &container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.4.0","path":"0.4.0","previous":"0.3.0"}"#,
        );
        // 0.2.0 is pinned by a running instance; 0.1.0 and 0.5.0 are garbage.
        f.pin(3000, "0.1.9-rc.4", vec![v2.clone()]);
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        let mut removed: Vec<PathBuf> = report
            .removed
            .iter()
            .map(|(_, path)| path.clone())
            .collect();
        removed.sort();
        assert_eq!(removed, vec![v1.clone(), v5.clone()]);
        assert!(v2.exists() && v3.exists() && v4.exists());
        // The pinned generation is in use by a running instance: kept, and
        // reported as an old file still in use.
        assert_eq!(
            report.live,
            vec![LiveLeftover {
                kind: LeftoverKind::Generation,
                path: v2.clone(),
                holders: vec![3000],
            }]
        );
    }

    #[test]
    fn a_generation_only_a_live_pin_keeps_is_reported_live_once_the_pointer_moved_past_it() {
        let f = Fixture::new();
        let container = f.root().join("rpi-todo");
        let v2 = f.generation("rpi-todo", "0.2.0");
        let v3 = f.generation("rpi-todo", "0.3.0");
        let v4 = f.generation("rpi-todo", "0.4.0");
        f.write(
            &container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.4.0","path":"0.4.0","previous":"0.3.0"}"#,
        );
        // 0.2.0 was pinned at startup by an instance that is still running.
        f.pin(3000, "0.1.9-rc.4", vec![v2.clone()]);
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert!(v2.exists() && v3.exists() && v4.exists());
        assert!(report.removed.is_empty());
        // Its pin lists 0.2.0 under the *self* pid: still kept.
        let self_pin = PinRecord {
            dirs: vec![v2.clone()],
            ..PinRecord::new(1000, 1, "self".to_owned(), "0.1.9-rc.4", &f.paths.exe_path)
        };
        PinHandle::register(
            f.fs.clone() as Arc<dyn UpdateFs>,
            &f.paths.pins_dir(),
            self_pin,
        )
        .unwrap()
        .release();
        let live = super::super::pins::live_pins(&*f.fs, &f.paths.pins_dir(), &f.probe);
        let mut context = f.context(UpdateState::Idle);
        context.live_pins = live;
        let report = cleanup_scan(&*f.fs, &context);
        assert!(
            v2.exists(),
            "this process's own pin keeps its generation too"
        );
        assert!(report.removed.is_empty(), "{report:?}");
    }

    #[test]
    fn flat_leftovers_of_a_migrated_container_are_reclaimed_when_unreferenced() {
        let f = Fixture::new();
        let container = f.root().join("rpi-todo");
        f.generation("rpi-todo", "0.2.0");
        f.write(
            &container.join("rpi-extension.json"),
            r#"{"name":"rpi-todo","version":"0.1.0"}"#,
        );
        f.write(&container.join("libtodo.dll"), "old");
        f.write(&container.join(".rpi-install-source"), "github:acme/todo\n");
        f.write(
            &container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        );
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        let mut removed: Vec<PathBuf> = report
            .removed
            .iter()
            .map(|(_, path)| path.clone())
            .collect();
        removed.sort();
        assert_eq!(
            removed,
            vec![
                container.join("libtodo.dll"),
                container.join("rpi-extension.json")
            ]
        );
        assert!(
            container.join(".rpi-install-source").exists(),
            "the github marker stays"
        );
        assert!(container.join("current.json").exists());

        // An instance that started on the flat layout keeps the files:
        // a second migrated container.
        let other = f.root().join("rpi-plan-mode");
        f.generation("rpi-plan-mode", "0.2.0");
        f.write(&other.join("rpi-extension.json"), "{}");
        f.write(
            &other.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        );
        f.pin(4000, "0.1.9-rc.3", vec![other.clone()]);
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert!(other.join("rpi-extension.json").exists());
        assert_eq!(
            report.live,
            vec![LiveLeftover {
                kind: LeftoverKind::FlatFiles,
                path: other.clone(),
                holders: vec![4000],
            }]
        );
    }

    #[test]
    fn package_subdirectories_are_not_generations() {
        let f = Fixture::new();
        let container = f.root().join("rpi-wasm");
        f.generation("rpi-wasm", "0.2.0");
        f.write(&container.join("0.2.0/dist/x.wasm"), "wasm");
        f.write(
            &container.join("0.2.0/rpi-extension.json"),
            r#"{"wasm":"dist/x.wasm"}"#,
        );
        f.write(
            &container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        );
        // An unrelated directory tree inside a flat container.
        let flat = f.root().join("rpi-flat");
        f.write(&flat.join("rpi-extension.json"), "{}");
        f.write(&flat.join("dist/y.wasm"), "wasm");
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(container.join("0.2.0/dist/x.wasm").exists());
        assert!(flat.join("dist/y.wasm").exists());
    }

    /// While an update waits for its first start, legacy flat files stay:
    /// a pre-`0.1.9-rc.4` instance still running has no pin to protect them.
    #[test]
    fn flat_leftovers_stay_while_an_update_waits_for_its_first_start() {
        let f = Fixture::new();
        let container = f.root().join("rpi-todo");
        f.generation("rpi-todo", "0.2.0");
        f.write(&container.join("rpi-extension.json"), "{}");
        f.write(&container.join("libtodo.dll"), "x");
        f.write(
            &container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        );
        let applied = UpdateResult::new(UpdateState::Applied, None, None);
        let report = cleanup_scan(
            &*f.fs,
            &f.context_with(UpdateState::Applied, None, Some(&applied)),
        );
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(container.join("libtodo.dll").exists());
        // Confirmed: the old instances are gone by the pin registry's
        // account, the leftovers go.
        let confirmed = UpdateResult::new(UpdateState::Confirmed, None, None);
        let report = cleanup_scan(
            &*f.fs,
            &f.context_with(UpdateState::Confirmed, None, Some(&confirmed)),
        );
        assert_eq!(report.removed.len(), 2, "{report:?}");
        assert!(!container.join("libtodo.dll").exists());
    }

    #[test]
    fn a_flat_container_without_a_pointer_is_never_touched() {
        let f = Fixture::new();
        let container = f.root().join("rpi-todo");
        f.write(&container.join("rpi-extension.json"), "{}");
        f.write(&container.join("libtodo.dll"), "x");
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert!(report.removed.is_empty());
        assert!(container.join("libtodo.dll").exists());
    }

    #[test]
    fn retired_containers_are_removed_once_unused() {
        let f = Fixture::new();
        let container = f.root().join("rpi-todo");
        f.generation("rpi-todo", "0.2.0");
        f.write(
            &container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        );
        f.write(&container.join(".retired"), "");
        f.pin(5000, "0.1.9-rc.4", vec![container.join("0.2.0")]);
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert!(report.removed.is_empty());
        assert_eq!(report.live.len(), 1);
        f.probe.kill(5000);
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert_eq!(
            report.removed,
            vec![(LeftoverKind::RetiredContainer, container.clone())]
        );
        assert!(!container.exists());
    }

    #[test]
    fn staging_directories_follow_their_owner_and_the_pending() {
        let f = Fixture::new();
        f.probe.spawn(6000, 1);
        let mine = Layout::extension_staging_path(&f.root(), "rpi-todo", "0.2.0", 1000);
        let live_owner = Layout::extension_staging_path(&f.root(), "rpi-todo", "0.3.0", 6000);
        let dead_owner = Layout::extension_staging_path(&f.root(), "rpi-todo", "0.4.0", 6999);
        let legacy = f.root().join("rpi-plan-mode.tmp-6000");
        let legacy_dead = f.root().join("rpi-plan-mode.old-6999");
        for dir in [&mine, &live_owner, &dead_owner, &legacy, &legacy_dead] {
            f.write(&dir.join("rpi-extension.json"), "{}");
        }
        // A pending `--all` batch keeps its own staged extension.
        let mut pending = applied_pending("0.1.9-rc.3", "0.1.9-rc.4");
        pending.applied = false;
        pending
            .extensions
            .push(super::super::records::StagedExtension {
                name: "rpi-todo".to_owned(),
                version: "0.2.0".to_owned(),
                install_root: f.root(),
                staged_path: mine.clone(),
                generation: "0.2.0".to_owned(),
                manifest_sha256: "0".repeat(64),
                files: vec![FileDigest {
                    path: PathBuf::from("rpi-extension.json"),
                    sha256: "0".repeat(64),
                }],
            });
        let report = cleanup_scan(
            &*f.fs,
            &f.context_with(UpdateState::Staged, Some(&pending), None),
        );
        let mut removed: Vec<PathBuf> = report
            .removed
            .iter()
            .map(|(_, path)| path.clone())
            .collect();
        removed.sort();
        assert_eq!(removed, vec![dead_owner.clone(), legacy_dead.clone()]);
        assert_eq!(
            report.skipped_staging, 2,
            "live owners keep their staging directories"
        );
        assert!(mine.exists() && live_owner.exists() && legacy.exists());
        // Without the pending, our own leftover is garbage too (its owner,
        // this pid, is alive but the directory is fresh only for a while).
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert!(
            mine.exists(),
            "a fresh staging directory of a live process is left alone"
        );
        assert_eq!(report.skipped_staging, 3);
    }

    #[test]
    fn update_state_files_are_left_alone_and_stray_files_are_removed() {
        let f = Fixture::new();
        let update = &f.paths.update_dir;
        for name in STATE_FILES {
            f.write(&update.join(name), "{}");
        }
        f.write(&update.join("pending.json.tmp-1-2"), "{}");
        f.write(&update.join("boot/11.json"), "{}");
        f.write(&update.join("staging/0.9.9/rpi.exe"), "x");
        f.write(&update.join("stray-dir/x"), "x");
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        let mut removed: Vec<PathBuf> = report
            .removed
            .iter()
            .map(|(_, path)| path.clone())
            .collect();
        removed.sort();
        assert_eq!(
            removed,
            vec![
                update.join("boot/11.json"),
                update.join("pending.json.tmp-1-2"),
                update.join("staging/0.9.9"),
                update.join("stray-dir"),
                update.join("undo.json"),
            ],
            "a stale undo.json belongs to no applied update"
        );
        for name in STATE_FILES {
            if name != "undo.json" {
                assert!(update.join(name).exists(), "{name}");
            }
        }
        // In the applied state boot records and `undo.json` stay.
        f.write(&update.join("undo.json"), "{}");
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Applied));
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(update.join("undo.json").exists());
    }

    /// Design §11.9: a path the DELETE-access probe finds held open (no
    /// share-delete) is live — never touched — even when no pin names it
    /// (a scanner, or an instance older than the pins). The in-use warning
    /// then names no instance count.
    #[test]
    fn a_refused_delete_probe_keeps_the_path_live_without_a_pin() {
        let f = Fixture::new();
        let container = f.root().join("rpi-todo");
        let v1 = f.generation("rpi-todo", "0.1.0");
        let v2 = f.generation("rpi-todo", "0.2.0");
        f.write(
            &container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        );
        f.fs.fail_with(
            FsOp::ProbeDeletable,
            "rpi-todo/0.1.0",
            1,
            std::io::Error::from_raw_os_error(32),
        );
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(report.stuck.is_empty());
        assert_eq!(
            report.live,
            vec![LiveLeftover {
                kind: LeftoverKind::Generation,
                path: v1.clone(),
                holders: vec![],
            }]
        );
        assert!(v1.exists() && v2.exists());
        assert_eq!(
            crate::core::windows_update::notices::warning_text(
                &report,
                Path::new("/log/update.log")
            )
            .as_deref(),
            Some(
                "Old version files are still in use by another running process. rpi removes them on a later start; run \"rpi update --cleanup\" to retry."
            )
        );
    }

    /// The aside name of a leftover stays a leftover for the next scan
    /// (round-3 review): a half-deleted generation that lost its manifest
    /// and a half-deleted container that lost its marker are removed
    /// anyway, and a deletion that fails again is reported at the aside
    /// path — what actually exists.
    #[test]
    fn an_aside_directory_stays_a_leftover_whatever_a_half_deletion_left() {
        let f = Fixture::new();
        let container = f.root().join("rpi-todo");
        let current = f.generation("rpi-todo", "0.2.0");
        f.write(
            &container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        );
        // The aside of a generation: its manifest is already gone.
        let aside = container.join("0.1.0.garbage-77");
        f.write(&aside.join("lib/lib.dll"), "dll");
        // The aside of a retired container at the root: no marker inside.
        let aside_root = f.root().join("rpi-gone.retired-77.garbage-78");
        f.write(&aside_root.join("rpi-extension.json"), "{}");
        // The aside of a staging directory: its name ends in a live pid.
        let aside_staging = f.root().join(".staging/rpi-todo-0.3.0-5.garbage-1000");
        f.write(&aside_staging.join("rpi-extension.json"), "{}");
        f.probe.spawn(1000, 1);
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        let mut removed: Vec<String> = report
            .removed
            .iter()
            .map(|(kind, path)| format!("{kind:?} {}", path.display()))
            .collect();
        removed.sort();
        let mut expected: Vec<String> = [
            (LeftoverKind::Generation, &aside),
            (LeftoverKind::Staging, &aside_root),
            (LeftoverKind::Staging, &aside_staging),
        ]
        .iter()
        .map(|(kind, path)| format!("{kind:?} {}", path.display()))
        .collect();
        expected.sort();
        assert_eq!(removed, expected, "{report:?}");
        assert!(!aside.exists() && !aside_root.exists() && !aside_staging.exists());
        assert!(current.exists());

        // A deletion that fails again is stuck at the aside path.
        let f = Fixture::new();
        let container = f.root().join("rpi-todo");
        f.generation("rpi-todo", "0.2.0");
        f.write(
            &container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        );
        let old = f.generation("rpi-todo", "0.1.0");
        f.fs.fail_with(
            FsOp::RemoveDirAll,
            "0.1.0.garbage-",
            1,
            std::io::Error::from_raw_os_error(5),
        );
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(report.stuck.len(), 1, "{report:?}");
        assert!(
            report.stuck[0]
                .path
                .to_string_lossy()
                .contains("0.1.0.garbage-"),
            "{}",
            report.stuck[0].path.display()
        );
        assert!(!old.exists(), "the original name is gone");
    }

    /// A directory is renamed aside before it is deleted: a loaded library
    /// inside makes the rename fail as a whole (Windows refuses it for a
    /// mapped image's directory entry), which classifies the directory as
    /// live and leaves it whole — where a direct recursive delete would
    /// have removed the files sorted before the library (round-2 review).
    #[test]
    fn a_generation_that_cannot_be_renamed_aside_is_live_and_stays_whole() {
        let f = Fixture::new();
        let container = f.root().join("rpi-todo");
        let v1 = f.generation("rpi-todo", "0.1.0");
        let v2 = f.generation("rpi-todo", "0.2.0");
        f.write(
            &container.join("current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        );
        f.fs.fail_with(
            FsOp::Rename,
            "rpi-todo/0.1.0",
            1,
            std::io::Error::from_raw_os_error(5),
        );
        let report = cleanup_scan(&*f.fs, &f.context(UpdateState::Idle));
        assert!(report.removed.is_empty(), "{report:?}");
        assert!(report.stuck.is_empty(), "{report:?}");
        assert_eq!(
            report.live,
            vec![LiveLeftover {
                kind: LeftoverKind::Generation,
                path: v1.clone(),
                holders: vec![],
            }]
        );
        assert!(
            v1.join("rpi-extension.json").is_file(),
            "nothing inside the directory was deleted"
        );
        assert!(v2.exists());
        // Nothing was left under an aside name.
        let names: Vec<_> = std::fs::read_dir(&container)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.contains(".garbage-")),
            "{names:?}"
        );
    }

    /// A same-version reinstall (R7.2.3) parks an image whose version
    /// string every later instance of that version shares (round-3 review
    /// R3-B1). The holders of a parked image are the instances that
    /// started from it — by image identity — not every instance of the
    /// version: the parked image goes once nobody runs it, even while
    /// same-version instances (the scanner included) are up.
    #[test]
    fn a_same_version_reinstall_parks_an_image_only_its_own_instances_hold() {
        let f = Fixture::new();
        let old = f.paths.old_exe_path("0.1.9-rc.4");
        std::fs::write(&old, b"old build").unwrap();
        std::fs::write(&f.paths.exe_path, b"new build!").unwrap();
        let mut confirmed = UpdateResult::new(
            UpdateState::Confirmed,
            Some("0.1.9-rc.4".to_owned()),
            Some("0.1.9-rc.4".to_owned()),
        );
        confirmed.first_boot_protocol = true;
        confirmed.normal_exit_after_confirm = true;
        // An instance of the same version that started from the new image.
        f.pin_image(2000, "0.1.9-rc.4", &f.paths.exe_path, vec![]);
        let report = cleanup_scan(
            &*f.fs,
            &f.context_with(UpdateState::Confirmed, None, Some(&confirmed)),
        );
        assert!(report.live.is_empty(), "{report:?}");
        assert_eq!(report.removed, vec![(LeftoverKind::OldExe, old.clone())]);
        assert!(!old.exists());
        assert!(report.old_exe_gone);

        // An instance that started from the parked image keeps it live.
        let f = Fixture::new();
        let old = f.paths.old_exe_path("0.1.9-rc.4");
        std::fs::write(&old, b"old build").unwrap();
        std::fs::write(&f.paths.exe_path, b"new build!").unwrap();
        f.pin_image(2001, "0.1.9-rc.4", &old, vec![]);
        f.pin_image(2002, "0.1.9-rc.4", &f.paths.exe_path, vec![]);
        let report = cleanup_scan(
            &*f.fs,
            &f.context_with(UpdateState::Confirmed, None, Some(&confirmed)),
        );
        assert_eq!(
            report.live,
            vec![LiveLeftover {
                kind: LeftoverKind::OldExe,
                path: old.clone(),
                holders: vec![2001],
            }]
        );
        assert!(old.exists());

        // A pin without an image identity (metadata unavailable at its
        // start) falls back to the version string.
        let f = Fixture::new();
        let old = f.paths.old_exe_path("0.1.9-rc.4");
        std::fs::write(&old, b"old build").unwrap();
        std::fs::write(&f.paths.exe_path, b"new build!").unwrap();
        f.pin(2003, "0.1.9-rc.4", vec![]);
        let mut no_identity =
            read_record::<PinRecord>(&*f.fs, &f.paths.pins_dir().join("2003.json")).unwrap();
        no_identity.exe_identity = None;
        f.fs.write_atomic(
            &f.paths.pins_dir().join("2003.json"),
            to_json(&no_identity).as_bytes(),
        )
        .unwrap();
        let report = cleanup_scan(
            &*f.fs,
            &f.context_with(UpdateState::Confirmed, None, Some(&confirmed)),
        );
        assert_eq!(
            report.live,
            vec![LiveLeftover {
                kind: LeftoverKind::OldExe,
                path: old.clone(),
                holders: vec![2003],
            }]
        );
    }

    /// A classify-only scan deletes nothing and lists the garbage.
    #[test]
    fn a_classify_only_scan_touches_nothing() {
        let f = Fixture::new();
        let v1 = f.generation("rpi-todo", "0.1.0");
        let v2 = f.generation("rpi-todo", "0.2.0");
        f.write(
            &f.root().join("rpi-todo/current.json"),
            r#"{"schemaVersion":1,"version":"0.2.0","path":"0.2.0"}"#,
        );
        let report = cleanup_scan_with(&*f.fs, &f.context(UpdateState::Idle), false);
        assert!(report.removed.is_empty());
        assert_eq!(report.garbage, vec![(LeftoverKind::Generation, v1.clone())]);
        assert!(v1.exists() && v2.exists());
        assert!(!f.fs.called(FsOp::RemoveDirAll, "rpi-todo/0.1.0"));
    }

    #[test]
    fn stuck_reasons_name_the_category() {
        use LeftoverKind::*;
        assert_eq!(
            stuck_reason(&std::io::Error::from_raw_os_error(5), OldExe),
            "still in use by a running instance"
        );
        assert_eq!(
            stuck_reason(
                &std::io::Error::new(std::io::ErrorKind::PermissionDenied, "x"),
                NewExe
            ),
            "still in use by a running instance"
        );
        assert_eq!(
            stuck_reason(&std::io::Error::from_raw_os_error(5), Generation),
            "access denied"
        );
        assert_eq!(
            stuck_reason(&std::io::Error::from_raw_os_error(32), Generation),
            "in use"
        );
        assert_eq!(
            stuck_reason(&std::io::Error::other("x"), FlatFiles),
            "I/O error"
        );
        assert_eq!(
            stuck_reason(
                &std::io::Error::new(std::io::ErrorKind::NotFound, "x"),
                Staging
            ),
            "already gone"
        );
    }
}
