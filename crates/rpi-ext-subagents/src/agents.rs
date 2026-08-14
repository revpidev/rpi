//! Agent definition discovery (multi-level, overrides, aliases).

pub mod builtin;
pub mod discover;
pub mod frontmatter;
pub mod skills;

use std::path::{Path, PathBuf};

use crate::paths;

/// Project-level `settings.json` path for subagent settings reads
/// (`getProjectAgentSettingsPath`, agents.ts:680-683): `<projectRoot>/.rpi/settings.json`.
pub fn project_settings_path(cwd: &Path) -> Option<PathBuf> {
    let root = discover::find_configured_project_root(cwd)?;
    Some(paths::get_project_config_dir(&root).join("settings.json"))
}
