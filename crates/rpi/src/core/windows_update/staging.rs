//! P0: staging an update while rpi runs (design §11.4, R7.2).
//!
//! On Windows `rpi update --self` / `--all` never touches the installed
//! executable or an installed extension. It downloads and verifies the
//! release asset (the shared downloader), stages the executable under
//! `.update/staging/<version>/`, records the extensions the same batch
//! already extracted under `<root>/.staging/`, and writes `pending.json`
//! for the next start. Any failure removes what this run staged; the system
//! is then exactly as it was.

use std::io;
use std::path::Path;

use super::fs::UpdateFs;
use super::layout::{UpdatePaths, safe_version_component};
use super::records::{
    BlockedVersions, FileDigest, PendingUpdate, STATE_SCHEMA_VERSION, StagedBinary,
    StagedExtension, UpdateResult, UpdateState, read_record, write_record,
};
use super::transaction::collect_file_digests;
use crate::core::version_check::UpdateChannel;

/// Everything a staged update records.
#[derive(Debug, Clone)]
pub struct StageRequest<'a> {
    pub target_version: &'a str,
    pub channel: UpdateChannel,
    /// `--force`: reinstall or downgrade; also re-enables a blocked version.
    pub force: bool,
    /// `--version <ver>`: an exact pin (any direction).
    pub pinned: bool,
    pub running_version: &'a str,
    /// The release asset (URL and archive sha256 go into the manifest).
    pub release: &'a crate::core::self_update::VerifiedRelease,
    /// The executable extracted from the release asset.
    pub exe: &'a [u8],
    pub target: &'a str,
    /// Extensions of the same `--all` batch, already extracted and verified.
    pub extensions: Vec<StagedExtension>,
}

/// How staging ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    /// `pending.json` is written; the update takes effect on the next start.
    Staged {
        version: String,
        extensions: Vec<String>,
        /// An unapplied pending for another target (or another build) was
        /// replaced.
        replaced_previous: bool,
    },
    /// The same update was already staged: nothing changed.
    AlreadyStaged {
        version: String,
        extensions: Vec<String>,
    },
    /// An update is applied and waits for its first start: no new staging
    /// is started on top of it (R7.2.6).
    AwaitingFirstBoot { version: String },
}

/// The success line of `rpi update` on Windows (stdout).
pub fn staged_success_line(version: &str, extensions: &[String]) -> String {
    if extensions.is_empty() {
        format!("Downloaded rpi {version}. It takes effect the next time rpi starts.")
    } else {
        format!(
            "Downloaded rpi {version} (with extensions: {}). It takes effect the next time rpi starts.",
            extensions.join(", ")
        )
    }
}

/// The line printed while an applied update waits for its first start.
pub fn awaiting_first_boot_line(version: &str) -> String {
    format!(
        "rpi {version} is already downloaded and waits for its first start to be confirmed. \
         Start rpi once to switch to it."
    )
}

/// The executable file name the staged asset carries (`rpi.exe`).
pub fn staged_exe_name() -> &'static str {
    "rpi.exe"
}

/// Describe an extracted, verified extension directory for the pending.
pub fn describe_staged_extension(
    fs: &dyn UpdateFs,
    install_root: &Path,
    name: &str,
    version: &str,
    staged_path: &Path,
) -> io::Result<StagedExtension> {
    Ok(StagedExtension {
        name: name.to_owned(),
        version: version.to_owned(),
        install_root: install_root.to_path_buf(),
        staged_path: staged_path.to_path_buf(),
        generation: safe_version_component(version),
        manifest_sha256: fs.sha256_file(&staged_path.join("rpi-extension.json"))?,
        files: collect_file_digests(fs, staged_path)?,
    })
}

fn installed_version(fs: &dyn UpdateFs, paths: &UpdatePaths, running_version: &str) -> String {
    super::transaction::installed_version(fs, paths, running_version)
}

/// Whether everything a pending names is still in place and intact: the
/// staged executable by its digest and every staged extension tree by its
/// recorded file list. The one idempotency criterion of staging, shared
/// with the `already downloaded` fast path of `update --self` (round-2
/// review: a broken extension staging must not pass as "downloaded").
pub fn staged_update_is_complete(fs: &dyn UpdateFs, pending: &PendingUpdate) -> bool {
    fs.sha256_file(&pending.binary.staged_path)
        .is_ok_and(|sha| sha.eq_ignore_ascii_case(&pending.binary.sha256))
        && pending.extensions.iter().all(|old| {
            old.files.is_empty()
                || super::transaction::verify_staged_tree(fs, &old.staged_path, &old.files).is_ok()
        })
}

/// Stage the update described by `request`. `log` receives one line per
/// event. On `Err` nothing of this run remains staged (the caller removes
/// the extension staging directories it created).
pub fn stage_update(
    fs: &dyn UpdateFs,
    paths: &UpdatePaths,
    request: &StageRequest<'_>,
    log: &dyn Fn(&str),
) -> Result<StageOutcome, String> {
    let target = request.target_version;
    let installed = installed_version(fs, paths, request.running_version);

    // 1. Preflight: the prefix is writable and `.update` can be created.
    if let Err(error) = fs.create_dir_all(&paths.update_dir) {
        return Err(format!(
            "the install directory {} is not writable: {error}",
            paths.update_dir.display()
        ));
    }

    // 2. Rules (design §11.4 step 3).
    if let Some(blocked) = read_record::<BlockedVersions>(fs, &paths.blocked_path())
        && blocked.contains(target)
        && !request.force
    {
        return Err(format!(
            "rpi {target} is on the blocked list because its first start failed. Run \
             \"rpi update --self --force\" to try it again."
        ));
    }
    let prerelease = crate::core::version_check::is_prerelease_version(target);
    if prerelease != (request.channel == UpdateChannel::PreRelease) {
        return Err(format!(
            "rpi {target} is a {} build and does not belong to the selected channel; use {}",
            if prerelease { "pre-release" } else { "stable" },
            if prerelease {
                "--rc"
            } else {
                "the stable channel"
            }
        ));
    }
    let allow_non_monotonic = request.force || request.pinned;
    if !allow_non_monotonic {
        let newer = crate::core::version_check::compare_package_versions(target, &installed)
            == Some(std::cmp::Ordering::Greater);
        if !newer {
            return Err(format!(
                "rpi {target} is not newer than the installed rpi {installed}."
            ));
        }
    }

    // 3. An earlier pending (design §11.4 step 4).
    let mut replaced_previous = false;
    let exe_sha256 = super::fs::sha256_bytes(request.exe);
    if let Some(previous) = read_record::<PendingUpdate>(fs, &paths.pending_path()) {
        if previous.applied {
            log(&format!(
                "staging: rpi {} waits for its first start; no new staging",
                previous.target_version
            ));
            return Ok(StageOutcome::AwaitingFirstBoot {
                version: previous.target_version,
            });
        }
        let same_extensions = previous.extensions.len() == request.extensions.len()
            && request.extensions.iter().all(|extension| {
                previous.extensions.iter().any(|old| {
                    old.name == extension.name
                        && old.version == extension.version
                        && old.install_root == extension.install_root
                        && old.staged_path == extension.staged_path
                })
            });
        let same_target = previous.target_version == target
            && previous.binary.sha256.eq_ignore_ascii_case(&exe_sha256);
        if same_target && same_extensions {
            // Idempotent only when the earlier staging is still complete.
            if staged_update_is_complete(fs, &previous) {
                log("staging: the same update is already staged");
                return Ok(StageOutcome::AlreadyStaged {
                    version: target.to_owned(),
                    extensions: extension_names(&request.extensions),
                });
            }
            log("staging: the earlier staging is incomplete; staging again");
        }
        log(&format!(
            "staging: replacing the pending update to {}",
            previous.target_version
        ));
        super::transaction::discard_pending(fs, paths, &previous);
        replaced_previous = true;
    }

    // 4. Stage the executable (design §11.4 step 5).
    let staged_dir = paths.binary_staging_dir(target);
    let staged_exe = staged_dir.join(staged_exe_name());
    let result = (|| -> io::Result<()> {
        fs.create_dir_all(&staged_dir)?;
        // The same staging directory may hold leftovers of a discarded
        // attempt: rewrite the executable atomically.
        fs.write_atomic(&staged_exe, request.exe)
    })();
    if let Err(error) = result {
        let _ = fs.remove_dir_all(&staged_dir);
        return Err(format!(
            "could not stage the new executable in {}: {error}",
            staged_dir.display()
        ));
    }
    log(&format!(
        "staging: executable written to {}",
        staged_exe.display()
    ));

    // 5. The pending record (design §11.4 step 7).
    let pending = PendingUpdate {
        schema_version: STATE_SCHEMA_VERSION,
        target_version: target.to_owned(),
        channel: match request.channel {
            UpdateChannel::Stable => "stable".to_owned(),
            UpdateChannel::PreRelease => "rc".to_owned(),
        },
        written_at: fs.now_iso8601(),
        from_version: installed.clone(),
        allow_non_monotonic,
        first_boot_protocol: super::supports_first_boot_protocol(target),
        applied: false,
        binary: StagedBinary {
            staged_path: staged_exe,
            sha256: exe_sha256,
            source_url: request.release.asset_url.clone(),
            asset_sha256: request.release.sha256.clone(),
            target: request.target.to_owned(),
        },
        extensions: request.extensions.clone(),
    };
    if let Err(error) = write_record(fs, &paths.pending_path(), Some(&pending)) {
        let _ = fs.remove_dir_all(&staged_dir);
        return Err(format!(
            "could not write {}: {error}",
            paths.pending_path().display()
        ));
    }
    let mut result = UpdateResult::new(
        UpdateState::Staged,
        Some(installed),
        Some(target.to_owned()),
    );
    result.first_boot_protocol = pending.first_boot_protocol;
    let _ = write_record(fs, &paths.result_path(), Some(&result));
    log(&format!(
        "staging: rpi {target} staged ({} extension(s))",
        request.extensions.len()
    ));
    Ok(StageOutcome::Staged {
        version: target.to_owned(),
        extensions: extension_names(&request.extensions),
        replaced_previous,
    })
}

/// What an earlier `rpi update` left behind, for the CLI to decide before
/// it downloads anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingState {
    /// No pending update.
    None,
    /// Staged, waiting for the next start.
    Staged { version: String },
    /// Applied, waiting for its first start to be confirmed: no new staging.
    Applied { version: String },
}

pub fn pending_state(fs: &dyn UpdateFs, paths: &UpdatePaths) -> PendingState {
    match read_record::<PendingUpdate>(fs, &paths.pending_path()) {
        Some(pending) if pending.applied => PendingState::Applied {
            version: pending.target_version,
        },
        Some(pending) => PendingState::Staged {
            version: pending.target_version,
        },
        None => PendingState::None,
    }
}

fn extension_names(extensions: &[StagedExtension]) -> Vec<String> {
    extensions
        .iter()
        .map(|extension| extension.name.clone())
        .collect()
}

/// The digests of one file, relative to a root (tests and the CLI).
pub fn file_digests(fs: &dyn UpdateFs, dir: &Path) -> io::Result<Vec<FileDigest>> {
    collect_file_digests(fs, dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::self_update::VerifiedRelease;
    use crate::core::windows_update::fs::SpyFs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    const RUNNING: &str = "0.1.9-rc.3";
    const TARGET: &str = "0.1.9-rc.4";

    struct Fixture {
        dir: PathBuf,
        fs: SpyFs,
        paths: UpdatePaths,
        log: std::cell::RefCell<Vec<String>>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rpi-wu-stage-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(dir.join("prefix")).unwrap();
            let fs = SpyFs::new();
            fs.set_now("2026-11-28T00:00:00Z");
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
                log: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn install_manifest(&self, version: &str) {
            self.fs
                .write_atomic(
                    &self.paths.manifest_path,
                    format!(
                        r#"{{"version":"{version}","target":"t","installedAt":"2026-11-27T00:00:00Z","sourceUrl":"u","sha256":"s","installPath":"p","method":"binary"}}"#
                    )
                    .as_bytes(),
                )
                .unwrap();
        }

        fn release(&self) -> VerifiedRelease {
            VerifiedRelease {
                asset_url: "https://example.invalid/rpi-0.1.9-rc.4-t.zip".to_owned(),
                sha256: "cd".repeat(32),
                archive: Vec::new(),
            }
        }

        fn request<'a>(
            &'a self,
            release: &'a VerifiedRelease,
            force: bool,
            pinned: bool,
        ) -> StageRequest<'a> {
            StageRequest {
                target_version: TARGET,
                channel: UpdateChannel::PreRelease,
                force,
                pinned,
                running_version: RUNNING,
                release,
                exe: b"new exe",
                target: "x86_64-pc-windows-msvc",
                extensions: Vec::new(),
            }
        }

        fn stage(&self, request: &StageRequest<'_>) -> Result<StageOutcome, String> {
            let log = &self.log;
            stage_update(&self.fs, &self.paths, request, &|message: &str| {
                log.borrow_mut().push(message.to_owned())
            })
        }

        fn logs(&self) -> Vec<String> {
            self.log.borrow().clone()
        }

        fn extension(&self, name: &str, version: &str) -> StagedExtension {
            self.extension_at(name, version, 7)
        }

        /// A freshly extracted staging directory owned by `pid`.
        fn extension_at(&self, name: &str, version: &str, pid: u32) -> StagedExtension {
            let root = self.dir.join("agent/extensions");
            let staged = UpdatePaths::extension_staging_path(&root, name, version, pid);
            std::fs::create_dir_all(staged.join("lib")).unwrap();
            std::fs::write(
                staged.join("rpi-extension.json"),
                format!(
                    r#"{{"name":"{name}","version":"{version}","native":"lib/lib.dll","rpiAbi":1}}"#
                ),
            )
            .unwrap();
            std::fs::write(staged.join("lib/lib.dll"), b"dll").unwrap();
            describe_staged_extension(&self.fs, &root, name, version, &staged).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_staged_update_leaves_the_installation_untouched() {
        let f = Fixture::new();
        f.install_manifest(RUNNING);
        std::fs::write(f.dir.join("prefix/rpi.exe"), b"old exe").unwrap();
        let release = f.release();
        let outcome = f.stage(&f.request(&release, false, false)).unwrap();
        assert_eq!(
            outcome,
            StageOutcome::Staged {
                version: TARGET.to_owned(),
                extensions: vec![],
                replaced_previous: false
            }
        );
        // Zero replacement.
        assert_eq!(
            std::fs::read(f.dir.join("prefix/rpi.exe")).unwrap(),
            b"old exe"
        );
        let manifest: crate::config::InstallManifest =
            serde_json::from_slice(&f.fs.read(&f.paths.manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.version, RUNNING);
        assert!(
            !f.fs
                .called(super::super::fs::FsOp::Rename, "prefix/rpi.exe"),
            "no rename touches the executable"
        );
        // The staged executable and the pending.
        let staged_exe = f.paths.binary_staging_dir(TARGET).join("rpi.exe");
        assert_eq!(std::fs::read(&staged_exe).unwrap(), b"new exe");
        let pending = read_record::<PendingUpdate>(&f.fs, &f.paths.pending_path()).unwrap();
        assert_eq!(pending.target_version, TARGET);
        assert_eq!(pending.from_version, RUNNING);
        assert_eq!(pending.channel, "rc");
        assert_eq!(pending.written_at, "2026-11-28T00:00:00Z");
        assert!(!pending.allow_non_monotonic);
        assert!(pending.first_boot_protocol);
        assert!(!pending.applied);
        assert_eq!(pending.binary.staged_path, staged_exe);
        assert_eq!(
            pending.binary.sha256,
            super::super::fs::sha256_bytes(b"new exe")
        );
        assert_eq!(pending.binary.source_url, release.asset_url);
        assert_eq!(pending.binary.asset_sha256, release.sha256);
        assert_eq!(pending.binary.target, "x86_64-pc-windows-msvc");
        let result = read_record::<UpdateResult>(&f.fs, &f.paths.result_path()).unwrap();
        assert_eq!(result.state, UpdateState::Staged);
        assert!(!result.read);
    }

    #[test]
    fn force_and_version_record_the_non_monotonic_flag_and_a_stable_target_its_channel() {
        let f = Fixture::new();
        f.install_manifest("0.1.9-rc.9");
        let release = f.release();
        // Without a flag the downgrade is refused before anything is staged.
        let error = f.stage(&f.request(&release, false, false)).unwrap_err();
        assert!(error.contains("not newer"), "{error}");
        assert!(!f.paths.pending_path().exists());

        let mut request = f.request(&release, false, true);
        request.target_version = "0.1.9-rc.4";
        let outcome = f.stage(&request).unwrap();
        assert!(matches!(outcome, StageOutcome::Staged { .. }));
        let pending = read_record::<PendingUpdate>(&f.fs, &f.paths.pending_path()).unwrap();
        assert!(pending.allow_non_monotonic);
        assert_eq!(pending.from_version, "0.1.9-rc.9");

        // A stable target belongs to the stable channel.
        let f = Fixture::new();
        f.install_manifest("0.1.8");
        let release = f.release();
        let mut request = f.request(&release, true, false);
        request.target_version = "0.1.9";
        request.channel = UpdateChannel::Stable;
        f.stage(&request).unwrap();
        let pending = read_record::<PendingUpdate>(&f.fs, &f.paths.pending_path()).unwrap();
        assert_eq!(pending.channel, "stable");
        assert!(pending.allow_non_monotonic);
        assert!(pending.first_boot_protocol, "0.1.9 >= 0.1.9-rc.4");
    }

    #[test]
    fn the_channel_must_match_the_target() {
        let f = Fixture::new();
        f.install_manifest(RUNNING);
        let release = f.release();
        let mut request = f.request(&release, false, false);
        request.channel = UpdateChannel::Stable;
        let error = f.stage(&request).unwrap_err();
        assert!(
            error.contains("does not belong to the selected channel"),
            "{error}"
        );
        assert!(!f.paths.pending_path().exists());
    }

    #[test]
    fn a_blocked_version_needs_force() {
        let f = Fixture::new();
        f.install_manifest(RUNNING);
        f.fs.write_atomic(
            &f.paths.blocked_path(),
            br#"{"schemaVersion":1,"versions":["0.1.9-rc.4"]}"#,
        )
        .unwrap();
        let release = f.release();
        let error = f.stage(&f.request(&release, false, false)).unwrap_err();
        assert!(error.contains("blocked list"), "{error}");
        assert!(!f.paths.pending_path().exists());
        assert!(matches!(
            f.stage(&f.request(&release, true, false)).unwrap(),
            StageOutcome::Staged { .. }
        ));
    }

    #[test]
    fn a_target_below_the_first_protocol_version_is_staged_without_the_protocol() {
        let f = Fixture::new();
        f.install_manifest("0.1.8");
        let release = f.release();
        let mut request = f.request(&release, true, true);
        request.target_version = "0.1.9-rc.3";
        f.stage(&request).unwrap();
        let pending = read_record::<PendingUpdate>(&f.fs, &f.paths.pending_path()).unwrap();
        assert!(
            !pending.first_boot_protocol,
            "the published rc.3 does not implement the protocol"
        );
    }

    #[test]
    fn an_applied_pending_blocks_new_staging() {
        let f = Fixture::new();
        f.install_manifest(RUNNING);
        let mut pending = PendingUpdate {
            schema_version: 1,
            target_version: TARGET.to_owned(),
            channel: "rc".to_owned(),
            written_at: "2026-11-28T00:00:00Z".to_owned(),
            from_version: RUNNING.to_owned(),
            allow_non_monotonic: false,
            first_boot_protocol: true,
            applied: true,
            binary: StagedBinary {
                staged_path: f.dir.join("x"),
                sha256: String::new(),
                source_url: String::new(),
                asset_sha256: String::new(),
                target: String::new(),
            },
            extensions: Vec::new(),
        };
        f.fs.write_atomic(
            &f.paths.pending_path(),
            super::super::records::to_json(&pending).as_bytes(),
        )
        .unwrap();
        let release = f.release();
        assert_eq!(
            f.stage(&f.request(&release, false, false)).unwrap(),
            StageOutcome::AwaitingFirstBoot {
                version: TARGET.to_owned()
            }
        );
        // Even for a different target.
        pending.target_version = "0.1.9-rc.5".to_owned();
        f.fs.write_atomic(
            &f.paths.pending_path(),
            super::super::records::to_json(&pending).as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            f.stage(&f.request(&release, false, false)).unwrap(),
            StageOutcome::AwaitingFirstBoot { .. }
        ));
        assert!(
            f.logs()
                .iter()
                .any(|line| line.contains("waits for its first start"))
        );
    }

    #[test]
    fn the_same_staged_update_is_idempotent_and_a_different_one_replaces_it() {
        let f = Fixture::new();
        f.install_manifest(RUNNING);
        let release = f.release();
        f.stage(&f.request(&release, false, false)).unwrap();
        // Re-running `rpi update --self` with the same download changes nothing.
        f.log.borrow_mut().clear();
        let outcome = f.stage(&f.request(&release, false, false)).unwrap();
        assert_eq!(
            outcome,
            StageOutcome::AlreadyStaged {
                version: TARGET.to_owned(),
                extensions: vec![]
            }
        );
        // A different build of the same version replaces the pending.
        let mut other = f.request(&release, false, false);
        other.exe = b"other build";
        let outcome = f.stage(&other).unwrap();
        assert!(matches!(
            outcome,
            StageOutcome::Staged {
                replaced_previous: true,
                ..
            }
        ));
        let pending = read_record::<PendingUpdate>(&f.fs, &f.paths.pending_path()).unwrap();
        assert_eq!(
            pending.binary.sha256,
            super::super::fs::sha256_bytes(b"other build")
        );
        assert_eq!(
            std::fs::read(f.paths.binary_staging_dir(TARGET).join("rpi.exe")).unwrap(),
            b"other build"
        );
    }

    /// The one completeness criterion of staging (round-2 review): a
    /// staged extension tree that lost a file makes the pending
    /// incomplete, for the idempotency check of `stage_update` and for
    /// the `already downloaded` fast path alike — neither may pass a
    /// broken staging as complete and leave it for the next start to
    /// discard.
    #[test]
    fn an_incomplete_extension_staging_is_not_complete() {
        let f = Fixture::new();
        f.install_manifest(RUNNING);
        let release = f.release();
        let mut request = f.request(&release, false, false);
        request.extensions.push(f.extension("rpi-todo", TARGET));
        f.stage(&request).unwrap();
        let pending = read_record::<PendingUpdate>(&f.fs, &f.paths.pending_path()).unwrap();
        assert!(staged_update_is_complete(&f.fs, &pending));
        // One recorded file of the staged tree goes missing.
        assert!(!pending.extensions[0].files.is_empty());
        let missing = pending.extensions[0]
            .staged_path
            .join(&pending.extensions[0].files[0].path);
        std::fs::remove_file(&missing).unwrap();
        assert!(!staged_update_is_complete(&f.fs, &pending));
        // The next `update --self` extracts afresh (a staging directory of
        // its own) and stages again instead of answering "already
        // downloaded" over the broken tree.
        let mut again = f.request(&release, false, false);
        again.extensions.push(f.extension_at("rpi-todo", TARGET, 8));
        let outcome = f.stage(&again).unwrap();
        assert!(matches!(
            outcome,
            StageOutcome::Staged {
                replaced_previous: true,
                ..
            }
        ));
        let pending = read_record::<PendingUpdate>(&f.fs, &f.paths.pending_path()).unwrap();
        assert!(staged_update_is_complete(&f.fs, &pending));
    }

    #[test]
    fn extensions_of_the_batch_are_recorded_with_their_digests() {
        let f = Fixture::new();
        f.install_manifest(RUNNING);
        let release = f.release();
        let mut request = f.request(&release, false, false);
        request.extensions.push(f.extension("rpi-todo", TARGET));
        request
            .extensions
            .push(f.extension("rpi-subagents", TARGET));
        let outcome = f.stage(&request).unwrap();
        assert_eq!(
            outcome,
            StageOutcome::Staged {
                version: TARGET.to_owned(),
                extensions: vec!["rpi-todo".to_owned(), "rpi-subagents".to_owned()],
                replaced_previous: false
            }
        );
        let pending = read_record::<PendingUpdate>(&f.fs, &f.paths.pending_path()).unwrap();
        assert_eq!(pending.extensions.len(), 2);
        let todo = &pending.extensions[0];
        assert_eq!(todo.files.len(), 2);
        assert_eq!(
            todo.generation_path(),
            f.dir.join("agent/extensions/rpi-todo/0.1.9-rc.4")
        );
        assert_eq!(todo.manifest_sha256.as_str().len(), 64);
        assert_eq!(
            staged_success_line(TARGET, &["rpi-todo".to_owned(), "rpi-subagents".to_owned()]),
            "Downloaded rpi 0.1.9-rc.4 (with extensions: rpi-todo, rpi-subagents). It takes effect the next time rpi starts."
        );
        assert_eq!(
            staged_success_line(TARGET, &[]),
            "Downloaded rpi 0.1.9-rc.4. It takes effect the next time rpi starts."
        );
        assert!(awaiting_first_boot_line(TARGET).contains("waits for its first start"));
    }

    #[test]
    fn an_unwritable_prefix_fails_before_touching_anything() {
        let f = Fixture::new();
        f.install_manifest(RUNNING);
        f.fs.fail(super::super::fs::FsOp::CreateDirAll, ".update", 1);
        let release = f.release();
        let error = f.stage(&f.request(&release, false, false)).unwrap_err();
        assert!(error.contains("not writable"), "{error}");
        assert!(!f.paths.pending_path().exists());
    }

    #[test]
    fn a_failed_pending_write_removes_the_staged_executable() {
        let f = Fixture::new();
        f.install_manifest(RUNNING);
        f.fs.fail(super::super::fs::FsOp::WriteAtomic, "pending.json", 1);
        let release = f.release();
        let error = f.stage(&f.request(&release, false, false)).unwrap_err();
        assert!(error.contains("could not write"), "{error}");
        assert!(!f.paths.binary_staging_dir(TARGET).exists());
        assert!(!f.paths.pending_path().exists());
    }
}
