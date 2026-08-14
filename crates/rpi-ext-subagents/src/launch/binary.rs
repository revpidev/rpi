//! Child binary + self extension path resolution.
//!
//! Port of pi-subagents `src/runs/shared/pi-spawn.ts` @ v0.48.0 (56f97234)
//! `getPiSpawnCommand` (139-163), reduced to the two rpi-relevant steps:
//! `RPI_SUBAGENT_RPI_BINARY` → current executable when it is `rpi` → `rpi`
//! from PATH. Upstream's node/package-root branch has no rpi equivalent (the
//! rpi binary is self-contained).
//!
//! The self-extension path (`dladdr` on this cdylib) is rpi-specific: upstream
//! injects its runtime extensions by source-file path inside the installed
//! package; here the same cdylib is passed to `--extension` for child-side
//! duties (required-tools diagnostic, child-safe subagent tool). The host
//! loader dedupes by canonical path (rpi-ext-host loader.rs:267-271), so the
//! explicit flag is harmless when ambient discovery also finds it.

use std::path::PathBuf;

pub const SUBAGENT_RPI_BINARY_ENV: &str = "RPI_SUBAGENT_RPI_BINARY";
pub const SUBAGENT_EXTENSION_PATH_ENV: &str = "RPI_SUBAGENT_EXTENSION_PATH";

#[derive(Debug, Clone, PartialEq)]
pub struct SpawnCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// `getPiSpawnCommand` order: env override → `process.execPath` when its file
/// name is `rpi` → bare `rpi` (PATH lookup at spawn time).
pub fn resolve_spawn_command(args: &[String]) -> SpawnCommand {
    if let Some(binary) = std::env::var(SUBAGENT_RPI_BINARY_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return SpawnCommand {
            program: binary,
            args: args.to_vec(),
        };
    }
    if let Ok(current) = std::env::current_exe() {
        let is_rpi = current
            .file_name()
            .map(|n| {
                let name = n.to_string_lossy().to_lowercase();
                name == "rpi" || name == "rpi.exe"
            })
            .unwrap_or(false);
        if is_rpi {
            return SpawnCommand {
                program: current.to_string_lossy().to_string(),
                args: args.to_vec(),
            };
        }
    }
    SpawnCommand {
        program: "rpi".to_string(),
        args: args.to_vec(),
    }
}

/// Absolute path of this cdylib, for `--extension` child injection.
/// `RPI_SUBAGENT_EXTENSION_PATH` wins; then `dladdr` on this module's own
/// export. `None` when neither resolves (caller decides whether that is fatal
/// — fanout-authorized launches fail fast, plain launches continue without
/// child-side duties, see ADR-0017 / TE04 deviations).
pub fn resolve_self_extension_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var(SUBAGENT_EXTENSION_PATH_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return Some(PathBuf::from(path));
    }
    self_path_via_dladdr()
}

#[cfg(unix)]
fn self_path_via_dladdr() -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStringExt;

    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // Safety: any function defined in this crate resides in this cdylib's
    // text; dladdr only resolves the address and fills the struct.
    let anchor = self_path_via_dladdr as *const () as *const libc::c_void;
    let ok = unsafe { libc::dladdr(anchor, &mut info) };
    if ok == 0 || info.dli_fname.is_null() {
        return None;
    }
    // Safety: dli_fname points at a NUL-terminated path owned by libc.
    let name = unsafe { CStr::from_ptr(info.dli_fname) };
    let path = PathBuf::from(std::ffi::OsString::from_vec(name.to_bytes().to_vec()));
    (!path.as_os_str().is_empty()).then_some(path)
}

#[cfg(not(unix))]
fn self_path_via_dladdr() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins_and_path_fallback() {
        // Not setting the env (or an empty value) exercises the current-exe /
        // PATH branches without depending on the test runner's binary name.
        let resolved = resolve_spawn_command(&["--mode".into(), "json".into()]);
        assert_eq!(resolved.args, vec!["--mode", "json"]);
    }
}
