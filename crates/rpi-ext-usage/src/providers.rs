//! Built-in provider scripts and their registration (TE44 FR-B/FR-C;
//! TE53 FR-B/FR-C).
//!
//! The eight provider scripts ship inside the cdylib (`include_str!`) so the
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

/// The eight first-party providers (plugin 01 §3; TE53 §5.1).
///
/// Registration order is stable so `/usage list` and `/usage all` are
/// deterministic; the host sorts the registered ids anyway.
pub const BUILTIN_SCRIPTS: [BuiltinScript; 8] = [
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
    BuiltinScript {
        provider: "opencode-go",
        file: "opencode_go.py",
        source: include_str!("../scripts/opencode_go.py"),
    },
    BuiltinScript {
        provider: "anthropic",
        file: "anthropic.py",
        source: include_str!("../scripts/anthropic.py"),
    },
    BuiltinScript {
        provider: "openai-codex",
        file: "openai_codex.py",
        source: include_str!("../scripts/openai_codex.py"),
    },
    BuiltinScript {
        provider: "xai",
        file: "xai.py",
        source: include_str!("../scripts/xai.py"),
    },
];

/// Model provider id → usage provider key (FR-A "current provider"
/// resolution; TE53 §5.4).
///
/// A model provider whose id is already a registered usage key resolves to
/// itself. `opencode` is deliberately **not** mapped to `opencode-go`; a
/// user-registered legacy provider named after the model keeps working
/// through the exact-match branch, and the `/usage` direct aliases
/// (`claude`/`gpt`/`codex`/`grok`) are command-level only.
pub fn alias_for(model_provider: &str) -> Option<&'static str> {
    if let Some(script) = BUILTIN_SCRIPTS
        .iter()
        .find(|script| script.provider == model_provider)
    {
        return Some(script.provider);
    }
    match model_provider {
        "zai" | "zai-coding-cn" => Some("glm-coding-plan"),
        "minimax" | "minimax-cn" => Some("minimax-token-plan"),
        "kimi-coding" => Some("kimi-code"),
        _ => None,
    }
}

/// `/usage` direct-position aliases (TE53 §5.4). `--provider` never expands
/// these; it always consumes the raw registry key.
pub const COMMAND_ALIASES: [(&str, &str); 4] = [
    ("claude", "anthropic"),
    ("gpt", "openai-codex"),
    ("codex", "openai-codex"),
    ("grok", "xai"),
];

/// Expand a direct-position provider token (alias or registry key).
pub fn expand_command_alias(token: &str) -> &str {
    COMMAND_ALIASES
        .iter()
        .find(|(alias, _)| *alias == token)
        .map_or(token, |(_, provider)| *provider)
}

/// Providers whose automatic (footer) refresh is forbidden: only an explicit
/// non-cached user command may run the script (TE53 §5.7, xAI).
pub fn is_explicit_only(provider: &str) -> bool {
    provider == "xai"
}

/// Resolve a current model provider to a registered usage provider: the
/// first-party alias first, then an exact registry key (a user-registered
/// legacy provider named after the model provider). `opencode` never maps
/// to `opencode-go`; only an exact registration matches.
pub fn current_provider(model_provider: &str, known: &[String]) -> Option<String> {
    if model_provider.trim().is_empty() {
        return None;
    }
    if let Some(provider) = alias_for(model_provider)
        && known.iter().any(|entry| entry == provider)
    {
        return Some(provider.to_owned());
    }
    known
        .iter()
        .find(|entry| entry.as_str() == model_provider)
        .cloned()
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
        let register_path = register_path(&dir, &script)?;
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
/// directly). A shim write failure is a hard error (round-2): silently
/// falling back to the `.py` path produced a registration that could never
/// start on Windows.
fn register_path(dir: &Path, script: &BuiltinScript) -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        let shim = dir.join(windows_shim_name(script.file));
        std::fs::write(&shim, windows_shim_body(script.file)).map_err(|error| {
            format!(
                "could not write the Windows launcher {}: {error}",
                shim.display()
            )
        })?;
        return Ok(shim);
    }
    #[cfg(not(windows))]
    Ok(dir.join(script.file))
}

/// `CreateProcess` cannot start a `.py` directly; the registered path is a
/// `.cmd` shim next to the script.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_shim_name(file: &str) -> String {
    file.replace(".py", ".cmd")
}

/// The Windows launcher body: try `python3`, then `py -3`, then `python`
/// (TE53 FR-A R6), forwarding the exit code and all script arguments.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_shim_body(file: &str) -> String {
    // Labels (not an `if (...)` block) keep `%errorlevel%` expansion after
    // the interpreter has run; a block would expand it at parse time and
    // swallow a non-zero script exit code.
    format!(
        "@echo off\r\n\
         where python3 >nul 2>nul\r\n\
         if errorlevel 1 goto rpi_usage_py\r\n\
         python3 \"%~dp0{file}\" %*\r\n\
         exit /b %errorlevel%\r\n\
         :rpi_usage_py\r\n\
         where py >nul 2>nul\r\n\
         if errorlevel 1 goto rpi_usage_python\r\n\
         py -3 \"%~dp0{file}\" %*\r\n\
         exit /b %errorlevel%\r\n\
         :rpi_usage_python\r\n\
         python \"%~dp0{file}\" %*\r\n"
    )
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
        assert_eq!(alias_for("opencode-go"), Some("opencode-go"));
        assert_eq!(alias_for("anthropic"), Some("anthropic"));
        assert_eq!(alias_for("openai-codex"), Some("openai-codex"));
        assert_eq!(alias_for("xai"), Some("xai"));
        // Registry keys are identities (a settings provider may be current).
        for script in BUILTIN_SCRIPTS {
            assert_eq!(alias_for(script.provider), Some(script.provider));
        }
        assert_eq!(alias_for("dgx-spark"), None);
        assert_eq!(alias_for("moonshotai"), None);
        assert_eq!(alias_for("opencode"), None, "no opencode -> opencode-go");
    }

    #[test]
    fn command_aliases_are_direct_position_only() {
        assert_eq!(expand_command_alias("claude"), "anthropic");
        assert_eq!(expand_command_alias("gpt"), "openai-codex");
        assert_eq!(expand_command_alias("codex"), "openai-codex");
        assert_eq!(expand_command_alias("grok"), "xai");
        assert_eq!(expand_command_alias("opencode"), "opencode");
        assert_eq!(expand_command_alias("deepseek"), "deepseek");
        assert!(is_explicit_only("xai"));
        for provider in [
            "deepseek",
            "glm-coding-plan",
            "minimax-token-plan",
            "kimi-code",
            "opencode-go",
            "anthropic",
            "openai-codex",
        ] {
            assert!(!is_explicit_only(provider), "{provider}");
        }
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
    fn windows_shim_prefers_python3_then_py3_then_python() {
        assert_eq!(windows_shim_name("xai.py"), "xai.cmd");
        let body = windows_shim_body("xai.py");
        assert!(body.starts_with("@echo off\r\n"), "{body}");
        let python3 = body.find("python3 \"%~dp0xai.py\" %*").expect("python3");
        let py3 = body.find("py -3 \"%~dp0xai.py\" %*").expect("py -3");
        let python = body.find("\r\npython \"%~dp0xai.py\" %*").expect("python");
        assert!(python3 < py3 && py3 < python, "{body}");
        assert_eq!(body.matches("exit /b %errorlevel%").count(), 2, "{body}");
        // Labels keep the exit code expansion after the interpreter runs;
        // an `if (...)` block would expand it at parse time.
        assert!(!body.contains("%errorlevel%==0"), "{body}");
        assert!(body.contains("if errorlevel 1 goto rpi_usage_py"), "{body}");
        assert!(body.contains(":rpi_usage_py"), "{body}");
        assert!(
            body.contains("if errorlevel 1 goto rpi_usage_python"),
            "{body}"
        );
        assert!(body.contains(":rpi_usage_python"), "{body}");
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
