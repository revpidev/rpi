//! Built-in provider scripts and their registration (TE44 FR-B/FR-C).
//!
//! The four provider scripts ship inside the cdylib (`include_str!`) so the
//! plugin is self-sufficient regardless of how it was installed (source
//! checkout, `.rpix`, project-local). At install they are materialized under
//! `<agentDir>/rpi-usage/scripts/` and registered through
//! `ctx.usage.register(provider, scriptPath)`; the host's resolution
//! priority keeps explicit `usage.providers` settings and the user script
//! directory (`<agentDir>/usage-providers/*.py`) ahead of plugin
//! registrations (V16-05 §7.2), so user overrides win without extra work.
//!
//! Writes are idempotent (only a changed body is rewritten, via a temp file +
//! rename) and fail-soft (a read-only agent dir leaves the framework
//! untouched; `/usage` still explains how to configure a provider).

use std::path::{Path, PathBuf};

use crate::HostCall;

/// One embedded provider script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinScript {
    /// Registry key (`/usage <provider>`).
    pub provider: &'static str,
    /// Source file name under `scripts/`.
    pub file: &'static str,
    /// Embedded body (the canonical `scripts/<file>` content).
    pub source: &'static str,
}

/// The four first-party providers (plugin 01 §3).
pub const BUILTIN_SCRIPTS: [BuiltinScript; 4] = [
    BuiltinScript {
        provider: "deepseek",
        file: "deepseek.py",
        source: include_str!("../scripts/deepseek.py"),
    },
    BuiltinScript {
        provider: "glm-coding-plan",
        file: "glm_coding_plan.py",
        source: include_str!("../scripts/glm_coding_plan.py"),
    },
    BuiltinScript {
        provider: "minimax-token-plan",
        file: "minimax_token_plan.py",
        source: include_str!("../scripts/minimax_token_plan.py"),
    },
    BuiltinScript {
        provider: "kimi-code",
        file: "kimi_code.py",
        source: include_str!("../scripts/kimi_code.py"),
    },
];

/// Model provider id → usage provider key (FR-A "current provider"
/// resolution). Unknown ids have no script; `/usage` explains how to add one.
pub fn alias_for(model_provider: &str) -> Option<&'static str> {
    if let Some(script) = BUILTIN_SCRIPTS
        .iter()
        .find(|script| script.provider == model_provider)
    {
        return Some(script.provider);
    }
    match model_provider {
        "deepseek" => Some("deepseek"),
        "zai" | "zai-coding-cn" => Some("glm-coding-plan"),
        "minimax" | "minimax-cn" => Some("minimax-token-plan"),
        "kimi-coding" => Some("kimi-code"),
        _ => None,
    }
}

/// The script directory inside the agent dir.
pub fn scripts_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("rpi-usage").join("scripts")
}

/// Materialize every built-in script and answer `(provider, path)` pairs to
/// register. Idempotent; a body change rewrites the file atomically.
pub fn materialize(agent_dir: &Path) -> Result<Vec<(&'static str, PathBuf)>, String> {
    let dir = scripts_dir(agent_dir);
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let mut written = Vec::new();
    for script in BUILTIN_SCRIPTS {
        let path = dir.join(script.file);
        write_if_changed(&path, script.source)?;
        let register_path = register_path(&dir, &script);
        written.push((script.provider, register_path));
    }
    Ok(written)
}

/// Materialize + register the four built-ins; answers the registration count.
///
/// A materialization failure is a hard error (nothing can register); a
/// single registration failure is logged and the remaining providers still
/// register (the framework surface stays usable for the rest).
pub fn register_builtin(host: &dyn HostCall, agent_dir: &Path) -> Result<usize, String> {
    let scripts = materialize(agent_dir)?;
    let mut registered = 0;
    for (provider, path) in scripts {
        match crate::host::usage_register(host, provider, &path.to_string_lossy()) {
            Ok(()) => registered += 1,
            Err(error) => {
                tracing::warn!(provider, %error, "rpi-usage: provider registration rejected");
            }
        }
    }
    Ok(registered)
}

/// The path handed to `ctx.usage.register`: the script itself on Unix
/// (shebang), a `cmd` shim on Windows (CreateProcess cannot start a `.py`
/// directly).
fn register_path(dir: &Path, script: &BuiltinScript) -> PathBuf {
    let script_path = dir.join(script.file);
    #[cfg(windows)]
    {
        let shim = dir.join(script.file.replace(".py", ".cmd"));
        let body = format!(
            "@echo off\r\nwhere python >nul 2>nul\r\nif %errorlevel%==0 (python \"%~dp0{}\" %*) else (py \"%~dp0{}\" %*)\r\n",
            script.file, script.file
        );
        if std::fs::write(&shim, body).is_ok() {
            return shim;
        }
    }
    script_path
}

/// Write `body` to `path` when missing or different; chmod 0755 on Unix.
fn write_if_changed(path: &Path, body: &str) -> Result<(), String> {
    let current = std::fs::read_to_string(path).ok();
    if current.as_deref() == Some(body) {
        set_executable(path)?;
        return Ok(());
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).map_err(|error| format!("{}: {error}", tmp.display()))?;
    set_executable(&tmp)?;
    std::fs::rename(&tmp, path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_cover_the_builtin_model_providers() {
        assert_eq!(alias_for("deepseek"), Some("deepseek"));
        assert_eq!(alias_for("zai"), Some("glm-coding-plan"));
        assert_eq!(alias_for("zai-coding-cn"), Some("glm-coding-plan"));
        assert_eq!(alias_for("minimax"), Some("minimax-token-plan"));
        assert_eq!(alias_for("minimax-cn"), Some("minimax-token-plan"));
        assert_eq!(alias_for("kimi-coding"), Some("kimi-code"));
        // Registry keys are identities (a settings provider may be current).
        for script in BUILTIN_SCRIPTS {
            assert_eq!(alias_for(script.provider), Some(script.provider));
        }
        assert_eq!(alias_for("dgx-spark"), None);
        assert_eq!(alias_for("moonshotai"), None);
    }

    #[test]
    fn materialize_writes_executable_idempotent_scripts() {
        let dir = crate::test_host::TestDir::new("usage-materialize");
        let agent = dir.path();
        let first = materialize(agent).expect("materialize");
        assert_eq!(first.len(), BUILTIN_SCRIPTS.len());
        for (provider, path) in &first {
            let body = std::fs::read_to_string(path).expect("script body");
            assert!(body.starts_with("#!/usr/bin/env python3"), "{provider}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&first[0].1)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "script is executable");
        }
        // A second run keeps the same bytes (no rewrite churn is observable).
        let second = materialize(agent).expect("second materialize");
        assert_eq!(first, second);
        // A tampered body is restored.
        std::fs::write(&first[0].1, "broken").expect("tamper");
        materialize(agent).expect("heal");
        assert_eq!(
            std::fs::read_to_string(&first[0].1).expect("body"),
            BUILTIN_SCRIPTS[0].source
        );
    }

    #[test]
    fn embedded_sources_match_the_source_tree_scripts() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts");
        for script in BUILTIN_SCRIPTS {
            let on_disk = std::fs::read_to_string(root.join(script.file))
                .unwrap_or_else(|error| panic!("{}: {error}", script.file));
            assert_eq!(
                on_disk, script.source,
                "{} must be embedded verbatim",
                script.file
            );
        }
    }
}
