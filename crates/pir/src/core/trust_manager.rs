//! Project trust gate (non-interactive paths for T10).
//!
//! Port of `packages/coding-agent/src/core/trust-manager.ts` and the
//! non-UI half of `project-trust.ts` @ pi 0.82.1 (2efa728). Interactive
//! prompting (selector UI) is T12; headless callers get `has_ui: false`,
//! which resolves to "not trusted" past the stored/default decisions
//! (project-trust.ts:82-85).

use std::path::{Path, PathBuf};

use crate::config::CONFIG_DIR_NAME;
use crate::error::PirError;
use crate::tools::path_utils::resolve_path;

/// `TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES` (trust-manager.ts:29-37).
const TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES: [&str; 7] = [
    "settings.json",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
];

/// `canonicalizePath` (utils/paths.ts:28-34): realpath, else the input.
pub fn canonicalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn normalize_cwd(cwd: &Path) -> PathBuf {
    canonicalize_path(&resolve_path(
        &cwd.to_string_lossy(),
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    ))
}

/// `hasTrustRequiringProjectResources` (trust-manager.ts:184-206).
pub fn has_trust_requiring_project_resources(cwd: &Path) -> bool {
    let home = crate::config::user_home_dir().unwrap_or_default();
    let user_agents_skills_dir = canonicalize_path(&home)
        .join(crate::config::AGENTS_DIR_NAME)
        .join("skills");
    let mut current_dir = normalize_cwd(cwd);

    let config_dir = current_dir.join(CONFIG_DIR_NAME);
    if TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES
        .iter()
        .any(|entry| config_dir.join(entry).exists())
    {
        return true;
    }

    loop {
        let agents_skills_dir = current_dir
            .join(crate::config::AGENTS_DIR_NAME)
            .join("skills");
        if agents_skills_dir != user_agents_skills_dir && agents_skills_dir.exists() {
            return true;
        }
        if !current_dir.pop() {
            return false;
        }
    }
}

/// `TrustFile` = `Record<string, boolean | null>` (trust-manager.ts:27).
type TrustFile = serde_json::Map<String, serde_json::Value>;

fn read_trust_file(path: &Path) -> Result<TrustFile, PirError> {
    if !path.exists() {
        return Ok(TrustFile::new());
    }
    let content = std::fs::read_to_string(path).map_err(|e| {
        PirError::Settings(format!(
            "Failed to read trust store {}: {e}",
            path.display()
        ))
    })?;
    let parsed: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
        PirError::Settings(format!(
            "Failed to read trust store {}: {e}",
            path.display()
        ))
    })?;
    let Some(object) = parsed.as_object() else {
        return Err(PirError::Settings(format!(
            "Invalid trust store {}: expected an object",
            path.display()
        )));
    };
    for (key, value) in object {
        if !(value.is_boolean() || value.is_null()) {
            return Err(PirError::Settings(format!(
                "Invalid trust store {}: value for {key:?} must be true, false, or null",
                path.display()
            )));
        }
    }
    Ok(object.clone())
}

fn write_trust_file(path: &Path, data: &TrustFile) -> Result<(), PirError> {
    // Keys sorted ascending; values limited to true/false/null
    // (trust-manager.ts:124-134).
    let mut sorted = serde_json::Map::new();
    let mut keys: Vec<&String> = data.keys().collect();
    keys.sort();
    for key in keys {
        let value = &data[key];
        if value.is_boolean() || value.is_null() {
            sorted.insert(key.clone(), value.clone());
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(&serde_json::Value::Object(sorted))?;
    text.push('\n');
    std::fs::write(path, text)?;
    Ok(())
}

/// Acquire the trust-store lock: `{trustPath}.lock` via `fs2`, 10 attempts ×
/// 20 ms on contention (proper-lockfile shape, trust-manager.ts:136-166).
fn with_trust_file_lock<T>(
    path: &Path,
    f: impl FnOnce() -> Result<T, PirError>,
) -> Result<T, PirError> {
    use fs2::FileExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock_path = path.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    const MAX_ATTEMPTS: u32 = 10;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match file.try_lock_exclusive() {
            Ok(()) => break,
            Err(_) if attempt < MAX_ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => {
                return Err(PirError::Settings(format!(
                    "Failed to acquire trust store lock: {error}"
                )));
            }
        }
    }
    let result = f();
    let _ = file.unlock();
    result
}

/// `findNearestTrustEntry` (trust-manager.ts:43-57): walk up from cwd.
fn find_nearest_trust_entry(data: &TrustFile, cwd: &Path) -> Option<(PathBuf, bool)> {
    let mut current_dir = normalize_cwd(cwd);
    loop {
        if let Some(value) = data.get(&current_dir.to_string_lossy().into_owned()) {
            if let Some(decision) = value.as_bool() {
                return Some((current_dir, decision));
            }
        }
        if !current_dir.pop() {
            return None;
        }
    }
}

/// `ProjectTrustStore` (trust-manager.ts:208-244).
pub struct ProjectTrustStore {
    trust_path: PathBuf,
}

impl ProjectTrustStore {
    pub fn new(agent_dir: &Path) -> Self {
        ProjectTrustStore {
            trust_path: resolve_path(
                &agent_dir.to_string_lossy(),
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            )
            .join("trust.json"),
        }
    }

    /// `get(cwd)`: nearest ancestor decision, `None` when unset.
    pub fn get(&self, cwd: &Path) -> Option<bool> {
        self.get_entry(cwd).map(|(_, decision)| decision)
    }

    pub fn get_entry(&self, cwd: &Path) -> Option<(PathBuf, bool)> {
        with_trust_file_lock(&self.trust_path, || {
            let data = read_trust_file(&self.trust_path)?;
            Ok(find_nearest_trust_entry(&data, cwd))
        })
        .ok()
        .flatten()
    }

    /// `set(cwd, decision)`: `None` clears the entry.
    pub fn set(&self, cwd: &Path, decision: Option<bool>) -> Result<(), PirError> {
        self.set_many(&[(cwd.to_path_buf(), decision)])
    }

    pub fn set_many(&self, decisions: &[(PathBuf, Option<bool>)]) -> Result<(), PirError> {
        with_trust_file_lock(&self.trust_path, || {
            let mut data = read_trust_file(&self.trust_path)?;
            for (path, decision) in decisions {
                let key = normalize_cwd(path).to_string_lossy().into_owned();
                match decision {
                    None => {
                        data.remove(&key);
                    }
                    Some(decision) => {
                        data.insert(key, serde_json::Value::from(*decision));
                    }
                }
            }
            write_trust_file(&self.trust_path, &data)
        })
    }
}

/// `defaultProjectTrust` setting values (settings-manager.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultProjectTrust {
    Ask,
    Always,
    Never,
}

/// `resolveProjectTrusted` (project-trust.ts:52-96), headless shape:
/// `has_ui` is always false in T10 (no selector UI until T12), so the
/// interactive branch at the end always resolves to `false`.
pub fn resolve_project_trusted(
    cwd: &Path,
    trust_store: &ProjectTrustStore,
    trust_override: Option<bool>,
    default_project_trust: DefaultProjectTrust,
) -> bool {
    if let Some(trust_override) = trust_override {
        return trust_override;
    }
    if !has_trust_requiring_project_resources(cwd) {
        return true;
    }

    // The `project_trust` extension event goes here (project-trust.ts:60-75)
    // — no extensions until T15.

    if let Some(decision) = trust_store.get(cwd) {
        return decision;
    }

    match default_project_trust {
        DefaultProjectTrust::Always => return true,
        DefaultProjectTrust::Never => return false,
        DefaultProjectTrust::Ask => {}
    }

    // `!options.projectTrustContext.hasUI` → false (project-trust.ts:82-85).
    false
}

/// Map the settings enum (`settings_manager::DefaultProjectTrust`) to the
/// trust-gate enum.
pub fn default_project_trust_from_settings(
    value: crate::core::settings_manager::DefaultProjectTrust,
) -> DefaultProjectTrust {
    match value {
        crate::core::settings_manager::DefaultProjectTrust::Ask => DefaultProjectTrust::Ask,
        crate::core::settings_manager::DefaultProjectTrust::Always => DefaultProjectTrust::Always,
        crate::core::settings_manager::DefaultProjectTrust::Never => DefaultProjectTrust::Never,
    }
}
