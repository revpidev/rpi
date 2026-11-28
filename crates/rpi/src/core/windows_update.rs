//! Windows startup self-update and generational extension installs
//! (ADR-0050, V19-09; requirements v0.1.9 §8 "R7", design §11).
//!
//! rpi-specific; upstream Pi has no Windows self-update. Three Windows
//! rules make the Unix flows unusable there: a running executable can be
//! renamed but not replaced or deleted; a directory holding a loaded DLL
//! can be neither renamed nor deleted; a shell only waits for the child it
//! created. The mechanism therefore splits the update in two:
//!
//! 1. **P0 staging** (`rpi update --self` / `--all`, [`staging`]): while rpi
//!    runs, only download, verify and stage the new executable and the
//!    extensions of the same batch. Nothing in place is touched.
//! 2. **P1 startup transaction** ([`transaction`]): the next start — before
//!    CLI dispatch and extension loading — re-verifies the staged files and
//!    swaps them in under an OS-level exclusive lock, one journaled step at
//!    a time, each step recording its undo before it runs.
//! 3. **P2 trampoline** (Windows only, [`platform`]): the old process starts
//!    the new executable with the original command line and waits for it, so
//!    the shell prompt returns only when the new instance exits.
//! 4. **P3 first boot** ([`boot`], [`rollback`]): the new instance confirms
//!    itself at known-good points; a crash before the first confirmation
//!    rolls the update back from the persisted pre-image and blocks the
//!    version.
//!
//! Extensions move to a generational layout so running instances keep the
//! libraries they loaded ([`generations`]; the read side lives in
//! `rpi_ext_host::install_layout`). Every start also runs a cleanup scan
//! ([`gc`]) that removes old-version leftovers nobody uses and reports what
//! is still in use or stuck ([`notices`]).
//!
//! Layering: the state machine, journal, rollback, boot records, pins, GC
//! and pointer logic are platform independent and run over an injectable
//! file-system trait ([`fs`]), so they are unit-tested on Linux with
//! per-step fault injection. Only the Win32 calls (process launch, console
//! control handler, process start times, DELETE-access probes, the log
//! directory) sit behind `#[cfg(windows)]`. On non-Windows targets none of
//! this runs: no pointer file is ever written there and the startup path is
//! byte-for-byte the pre-V19-09 one (R7.1.1).

pub mod boot;
pub mod fs;
pub mod gc;
pub mod generations;
pub mod journal;
pub mod layout;
pub mod lock;
pub mod notices;
pub mod pins;
pub mod preflight;
pub mod records;
pub mod rollback;
pub mod staging;
pub mod transaction;
pub mod update_log;
pub mod zip_archive;

#[cfg(windows)]
pub mod platform;
#[cfg(windows)]
pub mod startup;

use std::path::Path;

/// The first released version that implements the first-boot confirmation
/// protocol. A staged target below it switches over without boot records,
/// confirmation or automatic rollback (`firstBootProtocol=false`).
pub const FIRST_BOOT_PROTOCOL_VERSION: &str = "0.1.9-rc.4";

/// Set by the trampoline in the new instance's environment: the pid of the
/// old process that launched it and is waiting for it.
pub const ENV_TRAMPOLINE_PID: &str = "RPI_UPDATE_TRAMPOLINE_PID";
/// Marks the short-lived `--verify-extension` preflight child, which skips
/// the startup transaction, the first-boot confirmation and the cleanup
/// scan.
pub const ENV_VERIFY_CHILD: &str = "RPI_UPDATE_VERIFY_CHILD";
/// Hidden flag: `rpi --verify-extension <path>` loads the extension at
/// `<path>` in a throwaway process and exits 0 when it loads.
pub const VERIFY_EXTENSION_FLAG: &str = "--verify-extension";

/// How long a start waits for the update lock another instance holds
/// (design §11.5).
pub const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// Poll interval while waiting for the update lock.
pub const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(50);
/// Retries of a rename that fails with a transient sharing violation or
/// access denial (antivirus scanners, search indexers), design §11.5.
pub const RENAME_RETRIES: u32 = 5;
/// Timeout of the `--verify-extension` preflight child.
pub const VERIFY_EXTENSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Age after which a leftover extension staging directory counts as garbage.
pub const STAGING_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Whether the running process may run the startup transaction at all.
/// Subagent children and preflight children skip it (R7.3.8).
pub fn startup_transaction_enabled(env: &transaction::StartupEnv) -> bool {
    !env.subagent_child && !env.verify_child
}

/// Whether `version` implements the first-boot confirmation protocol.
pub fn supports_first_boot_protocol(version: &str) -> bool {
    supports_first_boot_protocol_since(version, FIRST_BOOT_PROTOCOL_VERSION)
}

/// [`supports_first_boot_protocol`] against an explicit floor (tests).
pub fn supports_first_boot_protocol_since(version: &str, floor: &str) -> bool {
    matches!(
        crate::core::version_check::compare_package_versions(version, floor),
        Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
    )
}

/// Classify an extension container entry under an install root: a retired
/// container, a live install (flat or generational) or not an install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    /// `rpi remove` retired it.
    Retired,
    /// Resolves to `path`.
    Installed { path: std::path::PathBuf },
}

/// Container state by the shared resolver.
pub fn container_state(container: &Path) -> Option<ContainerState> {
    if rpi_ext_host::install_layout::is_retired(container) {
        return Some(ContainerState::Retired);
    }
    rpi_ext_host::install_layout::resolve_install_dir_uncached(container)
        .map(|path| ContainerState::Installed { path })
}

#[cfg(test)]
mod tests;
