//! Builtin agent definitions embedded in the cdylib.
//!
//! The six upstream agents ship byte-for-byte from the pinned submodule
//! (pi-subagents `agents/*.md` @ v0.48.0, 56f97234) via `include_str!`; the
//! packaged `agents/` directory override copies (design §4) land with the
//! packaging ADR — discovery accepts an explicit builtin dir override the same
//! way upstream resolves `BUILTIN_AGENTS_DIR` from the package root.

use std::path::{Path, PathBuf};

use super::discover::{self, AgentConfig};

/// Load the embedded builtin agents (lowest discovery priority). `dir`
/// overrides the embedded set (packaged copy) when it exists.
#[allow(dead_code)] // backward-compatible API surface (A-R2); exercised in tests
pub fn load_builtin_agents(dir: Option<&Path>) -> Vec<AgentConfig> {
    load_builtin_agents_with_diagnostics(dir).0
}

/// Diagnostics-aware variant of [`load_builtin_agents`]: an explicit packaged
/// `dir` reports per-file failures through the discovery diagnostics instead
/// of dropping them silently (R7.1.3.1).
pub fn load_builtin_agents_with_diagnostics(
    dir: Option<&Path>,
) -> (Vec<AgentConfig>, Vec<discover::DiscoverDiagnostic>) {
    let embedded: &[(&str, &str)] = &[
        ("delegate", include_str!("../../assets/agents/delegate.md")),
        ("oracle", include_str!("../../assets/agents/oracle.md")),
        (
            "researcher",
            include_str!("../../assets/agents/researcher.md"),
        ),
        ("reviewer", include_str!("../../assets/agents/reviewer.md")),
        ("scout", include_str!("../../assets/agents/scout.md")),
        ("worker", include_str!("../../assets/agents/worker.md")),
    ];
    if let Some(dir) = dir.filter(|d| d.is_dir()) {
        let (agents, diagnostics) = discover::load_agents_from_dir_with_diagnostics(dir, "builtin");
        if !agents.is_empty() {
            return (agents, diagnostics);
        }
        return (embedded_agents(embedded), diagnostics);
    }
    (embedded_agents(embedded), Vec::new())
}

fn embedded_agents(embedded: &[(&str, &str)]) -> Vec<AgentConfig> {
    embedded
        .iter()
        .filter_map(|(name, content)| {
            discover::agent_from_content(content, Path::new(name), discover::AgentSource::Builtin)
                .ok()
                .flatten()
                .map(|mut agent| {
                    // Embedded assets are identified by name; keep a stable
                    // pseudo-path for detail rendering.
                    agent.file_path = PathBuf::from(format!("builtin:{name}"));
                    agent
                })
        })
        .collect()
}
