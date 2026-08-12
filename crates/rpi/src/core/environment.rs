//! Process-level `RPI_*` environment variables (requirements §3.3).
//!
//! Port of the process-level `PI_*` surface of
//! `packages/coding-agent/docs/environment-variables.md` and its read/write
//! points — `config.ts:369,494-496,505-508` (`PI_PACKAGE_DIR`,
//! `PI_SHARE_VIEWER_URL`), `main.ts:95,476-479,805` (`isTruthyEnvFlag`,
//! `--offline` linkage, `PI_STARTUP_BENCHMARK`), `core/telemetry.ts:3-12`
//! (`PI_TELEMETRY`), `core/experimental.ts:2` (`PI_EXPERIMENTAL`),
//! `core/timings.ts:6` (`PI_TIMING`), `utils/version-check.ts:71`
//! (`PI_SKIP_VERSION_CHECK`), `core/settings-manager.ts:1098,1182`
//! (`PI_CLEAR_ON_SHRINK`, `PI_HARDWARE_CURSOR`), `tui/src/terminal.ts:112`
//! (`PI_TUI_WRITE_LOG`), `tui/src/tui.ts:313,1331` (`PI_CLEAR_ON_SHRINK`,
//! `PI_DEBUG_REDRAW`), `cli.ts:13` / `rpc-entry.ts:7` (`PI_CODING_AGENT`),
//! and `packages/ai/src/api/*` (`PI_CACHE_RETENTION`)
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences (ADR-0001, requirements §1.4):
//! - The env prefix is `RPI_` (upstream `PI_`).
//! - `RPI_CODING_AGENT_DIR` / `RPI_CODING_AGENT_SESSION_DIR` are owned by
//!   [`crate::config`] (`ENV_AGENT_DIR` / `ENV_SESSION_DIR`, single point of
//!   path resolution, coding-standards §10.1) and are not redefined here.
//! - The bash-tool session injection variables (`RPI_SESSION_ID`,
//!   `RPI_SESSION_FILE`, `RPI_PROVIDER`, `RPI_MODEL`, `RPI_REASONING_LEVEL`)
//!   are implemented in `crate::tools::bash` (T06); only their constants live
//!   here so the naming surface is complete in one place.
//! - Provider API-key variables (requirements §5.6) belong to `rpi-ai` auth
//!   and are out of scope for this module.

use std::path::PathBuf;

use crate::tools::path_utils::normalize_path;

// ---------------------------------------------------------------------------
// Process marker (docs/environment-variables.md §Process Marker)
// ---------------------------------------------------------------------------

/// `PI_CODING_AGENT` (cli.ts:13, rpc-entry.ts:7) — Rpi rename (ADR-0001).
///
/// The CLI and RPC entry points set this to `"true"` so child processes can
/// detect that they run inside Rpi. It is **not** set automatically when Rpi
/// is embedded through the SDK.
pub const ENV_CODING_AGENT: &str = "RPI_CODING_AGENT";

/// `process.env.PI_CODING_AGENT = "true"` (cli.ts:13, rpc-entry.ts:7).
///
/// Called by the CLI/RPC entry points only (T10 wiring); SDK embedding must
/// not call this.
pub fn set_coding_agent_marker() {
    std::env::set_var(ENV_CODING_AGENT, "true");
}

/// Presence check for the process marker. Upstream never reads the variable
/// in-process; this is the documented child-process detection semantic (the
/// entry points only ever write `"true"`).
pub fn is_coding_agent() -> bool {
    std::env::var_os(ENV_CODING_AGENT)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// `AI_AGENT` (cli.ts:14, rpc-entry.ts:8 @ 4f4762f06) — a generic process
/// attribution marker derived from [`APP_NAME`](crate::config::APP_NAME).
///
/// The CLI and RPC entry points set this so generic tooling can attribute
/// child processes (bash tool, etc.) to Rpi. It is **not** set automatically
/// when Rpi is embedded through the SDK.
pub const ENV_AI_AGENT: &str = "AI_AGENT";

/// `process.env.AI_AGENT = APP_NAME` (cli.ts:14, rpc-entry.ts:8 @ 4f4762f06).
///
/// Called by the CLI/RPC entry points only; SDK embedding must not call this.
pub fn set_ai_agent_marker() {
    std::env::set_var(ENV_AI_AGENT, crate::config::APP_NAME);
}

// ---------------------------------------------------------------------------
// Package directory override (config.ts:367-372)
// ---------------------------------------------------------------------------

/// `PI_PACKAGE_DIR` (config.ts:369) — Rpi rename (ADR-0001).
pub const ENV_PACKAGE_DIR: &str = "RPI_PACKAGE_DIR";

/// `getPackageDir` env override (config.ts:369-372): non-empty
/// `RPI_PACKAGE_DIR`, normalized (`normalizePath` = tilde expansion /
/// `file://` decoding). Returns `None` when unset or empty; the caller then
/// falls back to its own package-dir resolution (the upstream Bun/Node
/// `__dirname` walk has no Rust counterpart).
pub fn package_dir_override() -> Option<PathBuf> {
    let value = std::env::var(ENV_PACKAGE_DIR)
        .ok()
        .filter(|v| !v.is_empty())?;
    Some(PathBuf::from(normalize_path(&value)))
}

// ---------------------------------------------------------------------------
// Offline / version check (main.ts:95,476-479; utils/version-check.ts:71)
// ---------------------------------------------------------------------------

/// `PI_OFFLINE` (main.ts:476-479) — Rpi rename (ADR-0001).
pub const ENV_OFFLINE: &str = "RPI_OFFLINE";

/// `PI_SKIP_VERSION_CHECK` (utils/version-check.ts:71) — Rpi rename
/// (ADR-0001).
pub const ENV_SKIP_VERSION_CHECK: &str = "RPI_SKIP_VERSION_CHECK";

/// `isTruthyEnvFlag` (main.ts:95-98, telemetry.ts:3-6): `"1"`, `"true"`, or
/// `"yes"` (case-insensitive); unset and empty are falsy.
pub fn is_truthy_env_flag(value: Option<&str>) -> bool {
    match value {
        None | Some("") => false,
        Some(v) => v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"),
    }
}

/// `isTruthyEnvFlag(process.env.PI_OFFLINE)` (main.ts:476).
pub fn is_offline() -> bool {
    is_truthy_env_flag(std::env::var(ENV_OFFLINE).ok().as_deref())
}

/// `--offline` linkage (main.ts:477-479): offline mode sets both
/// `RPI_OFFLINE=1` and `RPI_SKIP_VERSION_CHECK=1` for child processes.
pub fn set_offline_env() {
    std::env::set_var(ENV_OFFLINE, "1");
    std::env::set_var(ENV_SKIP_VERSION_CHECK, "1");
}

/// `process.env.PI_SKIP_VERSION_CHECK` truthiness (version-check.ts:71):
/// any **non-empty** value disables the version check (note: plain JS
/// truthiness, not `isTruthyEnvFlag` — `"0"` also disables).
pub fn skip_version_check() -> bool {
    std::env::var(ENV_SKIP_VERSION_CHECK)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Telemetry (core/telemetry.ts:3-12)
// ---------------------------------------------------------------------------

/// `PI_TELEMETRY` (telemetry.ts:10) — Rpi rename (ADR-0001).
pub const ENV_TELEMETRY: &str = "RPI_TELEMETRY";

/// `isInstallTelemetryEnabled` env layer (telemetry.ts:10-12): when
/// `RPI_TELEMETRY` is set (even to an empty string), it overrides the
/// `enableInstallTelemetry` setting; `1/true/yes` enable, everything else
/// disables. `None` = unset, the caller falls back to the setting.
pub fn telemetry_enabled_override() -> Option<bool> {
    std::env::var(ENV_TELEMETRY)
        .ok()
        .map(|v| is_truthy_env_flag(Some(&v)))
}

// ---------------------------------------------------------------------------
// Prompt-cache retention (packages/ai/src/api/pi-messages.ts:342 etc.)
// ---------------------------------------------------------------------------

/// `PI_CACHE_RETENTION` — Rpi rename (ADR-0001).
pub const ENV_CACHE_RETENTION: &str = "RPI_CACHE_RETENTION";

/// `getProviderEnvValue("PI_CACHE_RETENTION", env) === "long"` (exact string
/// match, pi-messages.ts:342 / openai-completions.ts:190 /
/// anthropic-messages.ts:53 / openai-responses.ts:61). Provider-scoped env
/// overrides ride `rpi-ai` stream options and take precedence over the
/// process environment at the call site; this helper reads the process-level
/// fallback.
pub fn cache_retention_long() -> bool {
    std::env::var(ENV_CACHE_RETENTION)
        .map(|v| v == "long")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Terminal / TUI toggles
// ---------------------------------------------------------------------------

/// `PI_HARDWARE_CURSOR` (settings-manager.ts:1182) — Rpi rename (ADR-0001).
pub const ENV_HARDWARE_CURSOR: &str = "RPI_HARDWARE_CURSOR";

/// `process.env.PI_HARDWARE_CURSOR === "1"` (settings-manager.ts:1182) —
/// env fallback for the `showHardwareCursor` setting.
pub fn hardware_cursor_enabled() -> bool {
    std::env::var(ENV_HARDWARE_CURSOR)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// `PI_CLEAR_ON_SHRINK` (settings-manager.ts:1098, tui.ts:313) — Rpi rename
/// (ADR-0001).
pub const ENV_CLEAR_ON_SHRINK: &str = "RPI_CLEAR_ON_SHRINK";

/// `process.env.PI_CLEAR_ON_SHRINK === "1"` (settings-manager.ts:1098) —
/// env fallback for the `terminal.clearOnShrink` setting.
pub fn clear_on_shrink_enabled() -> bool {
    std::env::var(ENV_CLEAR_ON_SHRINK)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// `PI_EXPERIMENTAL` (core/experimental.ts:2) — Rpi rename (ADR-0001).
pub const ENV_EXPERIMENTAL: &str = "RPI_EXPERIMENTAL";

/// `process.env.PI_EXPERIMENTAL === "1"` (experimental.ts:2).
pub fn experimental_enabled() -> bool {
    std::env::var(ENV_EXPERIMENTAL)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// `PI_TIMING` (core/timings.ts:6) — Rpi rename (ADR-0001).
pub const ENV_TIMING: &str = "RPI_TIMING";

/// `process.env.PI_TIMING === "1"` (timings.ts:6).
pub fn timing_enabled() -> bool {
    std::env::var(ENV_TIMING).map(|v| v == "1").unwrap_or(false)
}

/// `PI_STARTUP_BENCHMARK` (main.ts:805) — Rpi rename (ADR-0001).
pub const ENV_STARTUP_BENCHMARK: &str = "RPI_STARTUP_BENCHMARK";

/// `isTruthyEnvFlag(process.env.PI_STARTUP_BENCHMARK)` (main.ts:805).
pub fn startup_benchmark_enabled() -> bool {
    is_truthy_env_flag(std::env::var(ENV_STARTUP_BENCHMARK).ok().as_deref())
}

/// `PI_DEBUG_REDRAW` (tui/src/tui.ts:1331) — Rpi rename (ADR-0001).
pub const ENV_DEBUG_REDRAW: &str = "RPI_DEBUG_REDRAW";

/// `process.env.PI_DEBUG_REDRAW === "1"` (tui.ts:1331).
pub fn debug_redraw_enabled() -> bool {
    std::env::var(ENV_DEBUG_REDRAW)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// `PI_TUI_WRITE_LOG` (tui/src/terminal.ts:112) — Rpi rename (ADR-0001).
pub const ENV_TUI_WRITE_LOG: &str = "RPI_TUI_WRITE_LOG";

/// `process.env.PI_TUI_WRITE_LOG || ""` (terminal.ts:112): a file or
/// directory path capturing the raw ANSI stream; unset/empty = disabled.
pub fn tui_write_log_path() -> Option<String> {
    std::env::var(ENV_TUI_WRITE_LOG)
        .ok()
        .filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// External editor fallback (settings-manager.ts:859)
// ---------------------------------------------------------------------------

/// `VISUAL` — external editor fallback, first priority (not renamed).
pub const ENV_VISUAL: &str = "VISUAL";
/// `EDITOR` — external editor fallback, second priority (not renamed).
pub const ENV_EDITOR: &str = "EDITOR";

/// `process.env.VISUAL || process.env.EDITOR` (settings-manager.ts:859):
/// JS `||` semantics — an empty `VISUAL` falls through to `EDITOR`.
pub fn external_editor_from_env() -> Option<String> {
    std::env::var(ENV_VISUAL)
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var(ENV_EDITOR).ok().filter(|v| !v.is_empty()))
}

// ---------------------------------------------------------------------------
// Proxy / git passthrough (documented surface, read elsewhere)
// ---------------------------------------------------------------------------

/// `HTTP_PROXY` — outbound HTTP proxy (not renamed).
pub const ENV_HTTP_PROXY: &str = "HTTP_PROXY";
/// `HTTPS_PROXY` — outbound HTTPS proxy (not renamed).
pub const ENV_HTTPS_PROXY: &str = "HTTPS_PROXY";
/// `no_proxy` — proxy bypass list (not renamed; lowercase per curl
/// convention).
pub const ENV_NO_PROXY: &str = "no_proxy";
/// `GIT_TERMINAL_PROMPT` — git credential prompt toggle (not renamed).
pub const ENV_GIT_TERMINAL_PROMPT: &str = "GIT_TERMINAL_PROMPT";
/// `GIT_SSH_COMMAND` — git SSH command override (not renamed).
pub const ENV_GIT_SSH_COMMAND: &str = "GIT_SSH_COMMAND";

// ---------------------------------------------------------------------------
// Bash-tool session injection constants (implementation: crate::tools::bash)
// ---------------------------------------------------------------------------

/// `PI_SESSION_ID` (bash.ts:168, environment-variables.md) — Rpi rename.
pub const ENV_SESSION_ID: &str = "RPI_SESSION_ID";
/// `PI_SESSION_FILE` (bash.ts:170) — Rpi rename.
pub const ENV_SESSION_FILE: &str = "RPI_SESSION_FILE";
/// `PI_PROVIDER` (bash.ts:172) — Rpi rename.
pub const ENV_PROVIDER: &str = "RPI_PROVIDER";
/// `PI_MODEL` (bash.ts:173) — Rpi rename.
pub const ENV_MODEL: &str = "RPI_MODEL";
/// `PI_REASONING_LEVEL` (bash.ts:174) — Rpi rename.
pub const ENV_REASONING_LEVEL: &str = "RPI_REASONING_LEVEL";

#[cfg(test)]
mod tests {
    //! Env-manipulating tests are serialized through `ENV_LOCK` to avoid
    //! cross-test interference (the process environment is global state).
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Restore the named variables to their prior values on drop.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn set(vars: &[(&'static str, Option<&str>)]) -> (MutexGuard<'static, ()>, Self) {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = vars
                .iter()
                .map(|(name, _)| (*name, std::env::var(name).ok()))
                .collect();
            for (name, value) in vars {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
            (lock, EnvGuard { saved })
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    // Port of isTruthyEnvFlag (main.ts:95-98 / telemetry.ts:3-6).
    #[test]
    fn test_is_truthy_env_flag() {
        assert!(!is_truthy_env_flag(None));
        assert!(!is_truthy_env_flag(Some("")));
        assert!(is_truthy_env_flag(Some("1")));
        assert!(is_truthy_env_flag(Some("true")));
        assert!(is_truthy_env_flag(Some("TRUE")));
        assert!(is_truthy_env_flag(Some("yes")));
        assert!(is_truthy_env_flag(Some("Yes")));
        assert!(!is_truthy_env_flag(Some("0")));
        assert!(!is_truthy_env_flag(Some("false")));
        assert!(!is_truthy_env_flag(Some("no")));
        assert!(!is_truthy_env_flag(Some("2")));
    }

    // Port of "PI_OFFLINE accepts 1/true/yes" (main.ts:476).
    #[test]
    fn test_is_offline_truthy_flag() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_OFFLINE, None)]);
        assert!(!is_offline());
        std::env::set_var(ENV_OFFLINE, "1");
        assert!(is_offline());
        std::env::set_var(ENV_OFFLINE, "true");
        assert!(is_offline());
        std::env::set_var(ENV_OFFLINE, "0");
        assert!(!is_offline());
    }

    // Port of the --offline linkage (main.ts:477-479).
    #[test]
    fn test_set_offline_env_sets_skip_version_check() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_OFFLINE, None), (ENV_SKIP_VERSION_CHECK, None)]);
        set_offline_env();
        assert_eq!(std::env::var(ENV_OFFLINE).ok().as_deref(), Some("1"));
        assert_eq!(
            std::env::var(ENV_SKIP_VERSION_CHECK).ok().as_deref(),
            Some("1")
        );
        assert!(skip_version_check());
    }

    // Port of version-check.ts:71 — plain JS truthiness: any non-empty value
    // (including "0") disables the check.
    #[test]
    fn test_skip_version_check_any_non_empty() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_SKIP_VERSION_CHECK, None)]);
        assert!(!skip_version_check());
        std::env::set_var(ENV_SKIP_VERSION_CHECK, "");
        assert!(!skip_version_check());
        std::env::set_var(ENV_SKIP_VERSION_CHECK, "0");
        assert!(skip_version_check());
    }

    // Port of telemetry.ts:10-12 — set-but-empty overrides the setting with
    // "disabled".
    #[test]
    fn test_telemetry_enabled_override() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_TELEMETRY, None)]);
        assert_eq!(telemetry_enabled_override(), None);
        std::env::set_var(ENV_TELEMETRY, "1");
        assert_eq!(telemetry_enabled_override(), Some(true));
        std::env::set_var(ENV_TELEMETRY, "no");
        assert_eq!(telemetry_enabled_override(), Some(false));
        std::env::set_var(ENV_TELEMETRY, "");
        assert_eq!(telemetry_enabled_override(), Some(false));
    }

    // Port of pi-messages.ts:342 — exact "long" match.
    #[test]
    fn test_cache_retention_long_exact_match() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_CACHE_RETENTION, None)]);
        assert!(!cache_retention_long());
        std::env::set_var(ENV_CACHE_RETENTION, "long");
        assert!(cache_retention_long());
        std::env::set_var(ENV_CACHE_RETENTION, "LONG");
        assert!(!cache_retention_long());
    }

    // Port of experimental.ts:2 / timings.ts:6 / tui.ts:1331 — exact "1".
    #[test]
    fn test_exact_one_flags() {
        let (_lock, _guard) = EnvGuard::set(&[
            (ENV_EXPERIMENTAL, None),
            (ENV_TIMING, None),
            (ENV_DEBUG_REDRAW, None),
            (ENV_HARDWARE_CURSOR, None),
            (ENV_CLEAR_ON_SHRINK, None),
        ]);
        assert!(!experimental_enabled());
        assert!(!timing_enabled());
        assert!(!debug_redraw_enabled());
        assert!(!hardware_cursor_enabled());
        assert!(!clear_on_shrink_enabled());
        for (name, _) in [
            (ENV_EXPERIMENTAL, ()),
            (ENV_TIMING, ()),
            (ENV_DEBUG_REDRAW, ()),
            (ENV_HARDWARE_CURSOR, ()),
            (ENV_CLEAR_ON_SHRINK, ()),
        ] {
            std::env::set_var(name, "true");
        }
        // "true" is NOT accepted for the === "1" flags.
        assert!(!experimental_enabled());
        assert!(!timing_enabled());
        assert!(!debug_redraw_enabled());
        assert!(!hardware_cursor_enabled());
        assert!(!clear_on_shrink_enabled());
        std::env::set_var(ENV_EXPERIMENTAL, "1");
        assert!(experimental_enabled());
    }

    // Port of startup benchmark (main.ts:805) — isTruthyEnvFlag semantics.
    #[test]
    fn test_startup_benchmark_truthy_flag() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_STARTUP_BENCHMARK, None)]);
        assert!(!startup_benchmark_enabled());
        std::env::set_var(ENV_STARTUP_BENCHMARK, "yes");
        assert!(startup_benchmark_enabled());
    }

    // Port of terminal.ts:112 — empty disables.
    #[test]
    fn test_tui_write_log_path() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_TUI_WRITE_LOG, None)]);
        assert_eq!(tui_write_log_path(), None);
        std::env::set_var(ENV_TUI_WRITE_LOG, "");
        assert_eq!(tui_write_log_path(), None);
        std::env::set_var(ENV_TUI_WRITE_LOG, "/tmp/rpi-ansi.log");
        assert_eq!(tui_write_log_path().as_deref(), Some("/tmp/rpi-ansi.log"));
    }

    // Port of settings-manager.ts:859 — JS `||` fall-through on empty.
    #[test]
    fn test_external_editor_from_env_precedence() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_VISUAL, None), (ENV_EDITOR, None)]);
        assert_eq!(external_editor_from_env(), None);
        std::env::set_var(ENV_EDITOR, "emacs");
        assert_eq!(external_editor_from_env().as_deref(), Some("emacs"));
        std::env::set_var(ENV_VISUAL, "vim");
        assert_eq!(external_editor_from_env().as_deref(), Some("vim"));
        // Empty VISUAL falls through to EDITOR (JS `||`).
        std::env::set_var(ENV_VISUAL, "");
        assert_eq!(external_editor_from_env().as_deref(), Some("emacs"));
    }

    // Port of cli.ts:13 / rpc-entry.ts:7 marker write + presence detection.
    #[test]
    fn test_coding_agent_marker() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_CODING_AGENT, None)]);
        assert!(!is_coding_agent());
        set_coding_agent_marker();
        assert!(is_coding_agent());
        assert_eq!(
            std::env::var(ENV_CODING_AGENT).ok().as_deref(),
            Some("true")
        );
    }

    // Port of cli.ts:14 / rpc-entry.ts:8 @ 4f4762f06 — AI_AGENT derived from
    // APP_NAME so child processes (bash tool etc.) can attribute themselves.
    #[test]
    fn test_ai_agent_marker() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_AI_AGENT, None)]);
        assert_ne!(
            std::env::var(ENV_AI_AGENT).ok().as_deref(),
            Some(crate::config::APP_NAME)
        );
        set_ai_agent_marker();
        assert_eq!(
            std::env::var(ENV_AI_AGENT).ok().as_deref(),
            Some(crate::config::APP_NAME)
        );
        assert_eq!(crate::config::APP_NAME, "rpi");
    }

    // Port of config.ts:369-372 — non-empty override, normalized.
    #[test]
    fn test_package_dir_override() {
        let (_lock, _guard) = EnvGuard::set(&[(ENV_PACKAGE_DIR, None)]);
        assert_eq!(package_dir_override(), None);
        std::env::set_var(ENV_PACKAGE_DIR, "");
        assert_eq!(package_dir_override(), None);
        std::env::set_var(ENV_PACKAGE_DIR, "/nix/store/abc-rpi");
        assert_eq!(
            package_dir_override(),
            Some(PathBuf::from("/nix/store/abc-rpi"))
        );
    }
}
