//! Project trust gate (non-interactive paths for T10, UI wiring in W4/T14).
//!
//! Port of `packages/coding-agent/src/core/trust-manager.ts` and
//! `project-trust.ts` @ pi 0.82.1 (2efa728). Interactive prompting reaches
//! [`resolve_project_trusted`] through [`ProjectTrustContext`]: headless
//! callers pass [`ProjectTrustContext::headless`], which resolves to "not
//! trusted" past the stored/default decisions (project-trust.ts:82-85).

use std::path::{Path, PathBuf};

use crate::config::CONFIG_DIR_NAME;
use crate::core::extensions::{ProjectTrustEventDecision, ProjectTrustEventResult};
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

/// `ProjectTrustUpdate` (trust-manager.ts:15-18): `decision: None` deletes
/// the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTrustUpdate {
    pub path: PathBuf,
    pub decision: Option<bool>,
}

/// `ProjectTrustOption` (trust-manager.ts:20-25): one selectable trust
/// decision with the store updates it persists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTrustOption {
    pub label: String,
    pub trusted: bool,
    pub updates: Vec<ProjectTrustUpdate>,
    pub saved_path: Option<PathBuf>,
}

/// `getProjectTrustParentPath` (trust-manager.ts:59-63).
pub fn get_project_trust_parent_path(cwd: &Path) -> Option<PathBuf> {
    let trust_path = normalize_cwd(cwd);
    let parent = trust_path.parent()?;
    (parent != trust_path).then(|| parent.to_path_buf())
}

/// `getProjectTrustOptions` (trust-manager.ts:65-95).
pub fn get_project_trust_options(
    cwd: &Path,
    include_session_only: bool,
) -> Vec<ProjectTrustOption> {
    let trust_path = normalize_cwd(cwd);
    let mut options = vec![ProjectTrustOption {
        label: "Trust".to_string(),
        trusted: true,
        updates: vec![ProjectTrustUpdate {
            path: trust_path.clone(),
            decision: Some(true),
        }],
        saved_path: Some(trust_path.clone()),
    }];
    if let Some(parent_path) = get_project_trust_parent_path(cwd) {
        options.push(ProjectTrustOption {
            label: format!("Trust parent folder ({})", parent_path.display()),
            trusted: true,
            updates: vec![
                ProjectTrustUpdate {
                    path: parent_path.clone(),
                    decision: Some(true),
                },
                ProjectTrustUpdate {
                    path: trust_path.clone(),
                    decision: None,
                },
            ],
            saved_path: Some(parent_path),
        });
    }
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Trust (this session only)".to_string(),
            trusted: true,
            updates: Vec::new(),
            saved_path: None,
        });
    }
    options.push(ProjectTrustOption {
        label: "Do not trust".to_string(),
        trusted: false,
        updates: vec![ProjectTrustUpdate {
            path: trust_path.clone(),
            decision: Some(false),
        }],
        saved_path: Some(trust_path),
    });
    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Do not trust (this session only)".to_string(),
            trusted: false,
            updates: Vec::new(),
            saved_path: None,
        });
    }
    options
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
    // (trust-manager.ts:124-134). JS `sort()` orders strings by UTF-16
    // code units (T14 review m5); Rust's `str` ordering is code-point
    // based and diverges only for supplementary-plane characters (emoji
    // in paths) — compare the UTF-16 encodings for parity.
    let mut sorted = serde_json::Map::new();
    let mut keys: Vec<&String> = data.keys().collect();
    keys.sort_by(|a, b| utf16_code_unit_cmp(a, b));
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
    // Atomic write (temp + rename; T14 review — upstream `writeFileSync`
    // can truncate the store on crash mid-write).
    crate::config::atomic_write(path, &text)?;
    Ok(())
}

/// JS `String` ordering: UTF-16 code-unit comparison (see
/// `write_trust_file`).
fn utf16_code_unit_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
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
    let lock_path = {
        // Upstream appends: `${path}.lock` (trust-manager.ts:145) —
        // `trust.json.lock`, not `trust.lock`.
        let mut os = path.as_os_str().to_owned();
        os.push(".lock");
        PathBuf::from(os)
    };
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
            // Retry only on contention (proper-lockfile retries on
            // `ELOCKED` and fails on anything else; T14 review m3).
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && attempt < MAX_ATTEMPTS =>
            {
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
    // proper-lockfile removes the lockfile on release (trust-manager.ts
    // wraps proper-lockfile); best-effort, recreated on the next acquire.
    let _ = std::fs::remove_file(&lock_path);
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

    /// `get(cwd)`: nearest ancestor decision, `None` when unset. Lock/read
    /// failures propagate (upstream `getEntry` throws, trust-manager.ts:219-224).
    pub fn get(&self, cwd: &Path) -> Result<Option<bool>, PirError> {
        Ok(self.get_entry(cwd)?.map(|(_, decision)| decision))
    }

    pub fn get_entry(&self, cwd: &Path) -> Result<Option<(PathBuf, bool)>, PirError> {
        with_trust_file_lock(&self.trust_path, || {
            let data = read_trust_file(&self.trust_path)?;
            Ok(find_nearest_trust_entry(&data, cwd))
        })
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

/// Selector callback behind [`ProjectTrustContext::select`]: shows `title`
/// with `options` (labels) and returns the chosen label, `None` on cancel
/// (`ctx.ui.select`, extensions/types.ts). Synchronous like the trust store
/// itself (trust-manager.ts:156-159 keeps callers sync); UI implementations
/// run their own pump loop inside.
pub type ProjectTrustSelect = Box<dyn FnMut(&str, &[String]) -> Option<String> + Send>;

/// Async selector callback behind [`ProjectTrustContext::select_async`]
/// (T15 W7, ADR-0006): the TUI trust prompt runs on the event loop and must
/// be awaited, so it cannot use the blocking [`ProjectTrustSelect`].
pub type ProjectTrustSelectAsync = std::sync::Arc<
    dyn Fn(String, Vec<String>) -> futures::future::BoxFuture<'static, Option<String>>
        + Send
        + Sync,
>;

/// `projectTrustContextFactory` (interactive-mode.ts:4816/4830): builds the
/// trust prompt context for a target cwd (T15 W7, `switch_session`).
pub type ProjectTrustContextFactory =
    std::sync::Arc<dyn Fn(&std::path::Path) -> ProjectTrustContext + Send + Sync>;

/// `ProjectTrustContext` (extensions/types.ts:525-530), minus the
/// confirm/input/notify surface the trust flow never uses: `has_ui` gates
/// the interactive ask branch, `select` renders the prompt.
pub struct ProjectTrustContext {
    pub has_ui: bool,
    pub select: Option<ProjectTrustSelect>,
    /// Async selector variant; takes precedence when present.
    pub select_async: Option<ProjectTrustSelectAsync>,
}

impl ProjectTrustContext {
    /// Headless context: the ask branch resolves to `false`
    /// (project-trust.ts:86-88).
    pub fn headless() -> Self {
        ProjectTrustContext {
            has_ui: false,
            select: None,
            select_async: None,
        }
    }
}

/// `formatProjectTrustPrompt` (project-trust.ts:24-26), rebranded to rpi.
pub fn format_project_trust_prompt(cwd: &Path) -> String {
    format!(
        "Trust project folder?\n{}\n\nThis allows rpi to load {CONFIG_DIR_NAME} settings and resources, install missing project packages, and execute project extensions.",
        cwd.display()
    )
}

/// `resolveProjectTrusted` (project-trust.ts:46-96).
///
/// `extension_event` is the pre-emitted `project_trust` result
/// ([`crate::core::extensions::ExtensionRunner::emit_project_trust`]);
/// callers with an extension runner emit first (async), then resolve here.
/// `None` = no extensions (the case until the T15 host lands).
pub fn resolve_project_trusted(
    cwd: &Path,
    trust_store: &ProjectTrustStore,
    trust_override: Option<bool>,
    default_project_trust: DefaultProjectTrust,
    extension_event: Option<ProjectTrustEventResult>,
    context: &mut ProjectTrustContext,
) -> Result<bool, PirError> {
    if let Some(trust_override) = trust_override {
        return Ok(trust_override);
    }
    if !has_trust_requiring_project_resources(cwd) {
        return Ok(true);
    }

    // The `project_trust` extension event (project-trust.ts:54-70): the
    // first yes/no wins; `remember: true` persists the decision.
    if let Some(event) = extension_event {
        if event.trusted != ProjectTrustEventDecision::Undecided {
            let trusted = event.trusted == ProjectTrustEventDecision::Yes;
            if event.remember == Some(true) {
                trust_store.set(cwd, Some(trusted))?;
            }
            return Ok(trusted);
        }
    }

    if let Some(decision) = trust_store.get(cwd)? {
        return Ok(decision);
    }

    match default_project_trust {
        DefaultProjectTrust::Always => return Ok(true),
        DefaultProjectTrust::Never => return Ok(false),
        DefaultProjectTrust::Ask => {}
    }

    // `!options.projectTrustContext.hasUI` → false (project-trust.ts:86-88).
    if !context.has_ui {
        return Ok(false);
    }

    // `selectProjectTrustOption` + `saveProjectTrustPromptResult`
    // (project-trust.ts:28-44, 90-95).
    if let Some(select) = context.select.as_mut() {
        let options = get_project_trust_options(cwd, true);
        let labels: Vec<String> = options.iter().map(|option| option.label.clone()).collect();
        let selected = select(&format_project_trust_prompt(cwd), &labels);
        return apply_trust_selection(cwd, &options, selected, trust_store);
    }
    Ok(false)
}

/// Shared tail: apply a chosen label (project-trust.ts:36-44, 90-95).
fn apply_trust_selection(
    _cwd: &Path,
    options: &[ProjectTrustOption],
    selected: Option<String>,
    trust_store: &ProjectTrustStore,
) -> Result<bool, PirError> {
    if let Some(selected) = selected {
        if let Some(option) = options.iter().find(|option| option.label == selected) {
            if !option.updates.is_empty() {
                let updates: Vec<(PathBuf, Option<bool>)> = option
                    .updates
                    .iter()
                    .map(|update| (update.path.clone(), update.decision))
                    .collect();
                trust_store.set_many(&updates)?;
            }
            return Ok(option.trusted);
        }
    }
    Ok(false)
}

/// Async variant for contexts carrying `select_async` (T15 W7, ADR-0006):
/// identical decision chain, but the selector is awaited on the event loop.
pub async fn resolve_project_trusted_async(
    cwd: &Path,
    trust_store: &ProjectTrustStore,
    trust_override: Option<bool>,
    default_project_trust: DefaultProjectTrust,
    extension_event: Option<ProjectTrustEventResult>,
    context: &mut ProjectTrustContext,
) -> Result<bool, PirError> {
    if context.select_async.is_none() {
        return resolve_project_trusted(
            cwd,
            trust_store,
            trust_override,
            default_project_trust,
            extension_event,
            context,
        );
    }
    if let Some(trust_override) = trust_override {
        return Ok(trust_override);
    }
    if !has_trust_requiring_project_resources(cwd) {
        return Ok(true);
    }
    if let Some(event) = extension_event {
        if event.trusted != ProjectTrustEventDecision::Undecided {
            let trusted = event.trusted == ProjectTrustEventDecision::Yes;
            if event.remember == Some(true) {
                trust_store.set(cwd, Some(trusted))?;
            }
            return Ok(trusted);
        }
    }
    if let Some(decision) = trust_store.get(cwd)? {
        return Ok(decision);
    }
    match default_project_trust {
        DefaultProjectTrust::Always => return Ok(true),
        DefaultProjectTrust::Never => return Ok(false),
        DefaultProjectTrust::Ask => {}
    }
    if !context.has_ui {
        return Ok(false);
    }
    if let Some(select) = context.select_async.clone() {
        let options = get_project_trust_options(cwd, true);
        let labels: Vec<String> = options.iter().map(|option| option.label.clone()).collect();
        let selected = select(format_project_trust_prompt(cwd), labels).await;
        return apply_trust_selection(cwd, &options, selected, trust_store);
    }
    Ok(false)
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

#[cfg(test)]
mod tests {
    //! Port of `packages/coding-agent/test/trust-manager.test.ts` plus
    //! priority-chain coverage for `resolveProjectTrusted` (project-trust.ts)
    //! and the `getProjectTrustOptions` shapes (trust-manager.ts:65-95).
    //! HOME-mutating tests are serialized through `ENV_LOCK`.
    use std::sync::{Mutex, MutexGuard};

    use super::*;
    use crate::tools::test_helpers::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    struct TestDirs {
        _tmp: TempDir,
        agent_dir: PathBuf,
        cwd: PathBuf,
    }

    fn test_dirs() -> TestDirs {
        let tmp = TempDir::new();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        TestDirs {
            _tmp: tmp,
            agent_dir,
            cwd,
        }
    }

    /// A cwd that requires trust (`.rpi/settings.json` present).
    fn trust_requiring_dirs() -> TestDirs {
        let dirs = test_dirs();
        std::fs::create_dir_all(dirs.cwd.join(CONFIG_DIR_NAME)).unwrap();
        std::fs::write(dirs.cwd.join(CONFIG_DIR_NAME).join("settings.json"), "{}").unwrap();
        dirs
    }

    fn select_returning(label: Option<&str>) -> ProjectTrustSelect {
        let label = label.map(str::to_owned);
        Box::new(move |_title, _options| label.clone())
    }

    // --- ProjectTrustStore (trust-manager.test.ts:24-37) -------------------

    #[test]
    fn store_decisions_inherit_from_parent_directories() {
        let dirs = test_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        let parent = dirs.cwd.parent().unwrap().join("trusted-parent");
        let child = parent.join("project");
        std::fs::create_dir_all(&child).unwrap();

        assert_eq!(store.get(&child).unwrap(), None);
        store.set(&parent, Some(true)).unwrap();
        assert_eq!(store.get(&child).unwrap(), Some(true));
        store.set(&child, Some(false)).unwrap();
        assert_eq!(store.get(&child).unwrap(), Some(false));
        store.set(&child, None).unwrap();
        assert_eq!(store.get(&child).unwrap(), Some(true));
    }

    #[test]
    fn store_nearest_ancestor_entry_wins() {
        let dirs = test_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        let grandparent = dirs.cwd.parent().unwrap().to_path_buf();
        store.set(&grandparent, Some(false)).unwrap();
        store.set(&dirs.cwd, Some(true)).unwrap();
        // The cwd entry is nearer than the grandparent entry.
        assert_eq!(store.get(&dirs.cwd).unwrap(), Some(true));
        let (path, decision) = store.get_entry(&dirs.cwd).unwrap().unwrap();
        assert_eq!(path, canonicalize_path(&dirs.cwd));
        assert!(decision);
    }

    #[test]
    fn store_writes_sorted_keys_with_trailing_newline_and_lockfile() {
        let dirs = test_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        let z_dir = dirs.cwd.join("zeta");
        let a_dir = dirs.cwd.join("alpha");
        std::fs::create_dir_all(&z_dir).unwrap();
        std::fs::create_dir_all(&a_dir).unwrap();
        store.set(&z_dir, Some(true)).unwrap();
        store.set(&a_dir, Some(false)).unwrap();

        let trust_path = dirs.agent_dir.join("trust.json");
        let text = std::fs::read_to_string(&trust_path).unwrap();
        assert!(text.ends_with("}\n"), "pretty JSON + trailing newline");
        let a_key = canonicalize_path(&a_dir).to_string_lossy().into_owned();
        let z_key = canonicalize_path(&z_dir).to_string_lossy().into_owned();
        assert!(
            text.find(&a_key).unwrap() < text.find(&z_key).unwrap(),
            "keys sorted ascending: {text}"
        );
        // Lockfile shape: `${trustPath}.lock` (trust-manager.ts:145), and
        // it is removed on release (proper-lockfile semantics; T14 review
        // m3 — the previous implementation left it behind).
        let lock_path = dirs.agent_dir.join("trust.json.lock");
        assert!(!lock_path.exists(), "lockfile removed on release");
        // …but it is created while a write is in flight (acquired before
        // the store mutation, then released).
        store.set(&a_dir, Some(true)).unwrap();
        assert!(!lock_path.exists());
    }

    #[test]
    fn store_rejects_invalid_trust_files() {
        let dirs = test_dirs();
        let trust_path = dirs.agent_dir.join("trust.json");
        std::fs::write(&trust_path, "not json").unwrap();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        assert!(store.get(&dirs.cwd).is_err());

        std::fs::write(&trust_path, "[1,2]").unwrap();
        assert!(store.get(&dirs.cwd).is_err());

        std::fs::write(
            &trust_path,
            format!("{{\"{}\": \"yes\"}}", dirs.cwd.display()),
        )
        .unwrap();
        assert!(store.get(&dirs.cwd).is_err());
    }

    #[test]
    fn store_skips_null_entries_in_parent_chain() {
        let dirs = test_dirs();
        // A hand-written null at cwd must not shadow the parent decision
        // (trust-manager.ts:47 only matches true/false).
        let parent = dirs.cwd.parent().unwrap().to_path_buf();
        let trust_path = dirs.agent_dir.join("trust.json");
        std::fs::write(
            &trust_path,
            format!(
                "{{\"{}\": null, \"{}\": true}}",
                canonicalize_path(&dirs.cwd).display(),
                canonicalize_path(&parent).display()
            ),
        )
        .unwrap();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        assert_eq!(store.get(&dirs.cwd).unwrap(), Some(true));
    }

    // --- hasTrustRequiringProjectResources (trust-manager.test.ts:39-66) ---

    #[test]
    fn bare_config_dir_does_not_require_trust() {
        let dirs = test_dirs();
        std::fs::create_dir_all(dirs.cwd.join(CONFIG_DIR_NAME)).unwrap();
        assert!(!has_trust_requiring_project_resources(&dirs.cwd));
    }

    #[test]
    fn each_trust_requiring_resource_triggers() {
        for entry in TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES {
            let dirs = test_dirs();
            let path = dirs.cwd.join(CONFIG_DIR_NAME).join(entry);
            if entry.ends_with(".json") || entry.ends_with(".md") {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, "{}").unwrap();
            } else {
                std::fs::create_dir_all(&path).unwrap();
            }
            assert!(
                has_trust_requiring_project_resources(&dirs.cwd),
                "{entry} should require trust"
            );
        }
    }

    #[test]
    fn ancestor_agents_skills_requires_trust() {
        let dirs = test_dirs();
        // .agents/skills in the cwd itself.
        std::fs::create_dir_all(dirs.cwd.join(".agents").join("skills")).unwrap();
        assert!(has_trust_requiring_project_resources(&dirs.cwd));

        // .agents/skills only in an ancestor of the cwd.
        let dirs = test_dirs();
        let ancestor = dirs.cwd.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(ancestor.join(".agents").join("skills")).unwrap();
        assert!(has_trust_requiring_project_resources(&dirs.cwd));
    }

    #[test]
    fn user_agents_skills_is_exempt() {
        let tmp = TempDir::new();
        let (_lock, _env) = EnvGuard::set(&[("HOME", Some(&tmp.path().to_string_lossy()))]);
        // ~/.agents/skills exists, cwd == HOME: the user's own skills dir is
        // always trusted (trust-manager.ts:177-183).
        std::fs::create_dir_all(tmp.path().join(".agents").join("skills")).unwrap();
        assert!(!has_trust_requiring_project_resources(tmp.path()));
        // A child of HOME walks up to the exempt dir only → still false.
        let child = tmp.path().join("project");
        std::fs::create_dir_all(&child).unwrap();
        assert!(!has_trust_requiring_project_resources(&child));
    }

    // --- getProjectTrustOptions (trust-manager.ts:59-95) --------------------

    #[test]
    fn trust_options_include_session_only_variants() {
        let dirs = test_dirs();
        let options = get_project_trust_options(&dirs.cwd, true);
        let labels: Vec<&str> = options.iter().map(|option| option.label.as_str()).collect();
        let parent = normalize_cwd(&dirs.cwd);
        let parent_label = format!(
            "Trust parent folder ({})",
            parent.parent().unwrap().display()
        );
        assert_eq!(
            labels,
            vec![
                "Trust",
                parent_label.as_str(),
                "Trust (this session only)",
                "Do not trust",
                "Do not trust (this session only)",
            ]
        );
        // Session-only options persist nothing (trust-manager.ts:83, 92).
        assert!(options[2].updates.is_empty());
        assert!(options[4].updates.is_empty());
        // Trust-parent clears the child entry (trust-manager.ts:74-79).
        let parent_updates = &options[1].updates;
        assert_eq!(parent_updates[0].decision, Some(true));
        assert_eq!(parent_updates[1].decision, None);
    }

    #[test]
    fn trust_options_omit_session_only_and_root_parent() {
        let dirs = test_dirs();
        let options = get_project_trust_options(&dirs.cwd, false);
        assert_eq!(options.len(), 3);
        // The filesystem root has no parent option (trust-manager.ts:70-71).
        let root_options = get_project_trust_options(Path::new("/"), false);
        assert_eq!(root_options.len(), 2);
        assert_eq!(root_options[0].label, "Trust");
        assert_eq!(root_options[1].label, "Do not trust");
    }

    // --- resolveProjectTrusted priority chain (project-trust.ts:46-96) -----

    #[test]
    fn cli_override_short_circuits_everything() {
        let dirs = test_dirs(); // no trust-requiring resources at all
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            Some(false),
            DefaultProjectTrust::Always,
            None,
            &mut ProjectTrustContext::headless(),
        )
        .unwrap();
        assert!(!trusted);
    }

    #[test]
    fn no_trust_requiring_resources_is_trusted() {
        let dirs = test_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            None,
            DefaultProjectTrust::Never,
            None,
            &mut ProjectTrustContext::headless(),
        )
        .unwrap();
        assert!(trusted);
    }

    #[test]
    fn extension_event_beats_store_and_remember_persists() {
        let dirs = trust_requiring_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        store.set(&dirs.cwd, Some(false)).unwrap();
        let event = ProjectTrustEventResult {
            trusted: ProjectTrustEventDecision::Yes,
            remember: Some(true),
        };
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            None,
            DefaultProjectTrust::Never,
            Some(event),
            &mut ProjectTrustContext::headless(),
        )
        .unwrap();
        assert!(trusted);
        assert_eq!(
            store.get(&dirs.cwd).unwrap(),
            Some(true),
            "remember persisted"
        );
    }

    #[test]
    fn extension_undecided_falls_through_to_store() {
        let dirs = trust_requiring_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        store.set(&dirs.cwd, Some(true)).unwrap();
        let event = ProjectTrustEventResult {
            trusted: ProjectTrustEventDecision::Undecided,
            remember: None,
        };
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            None,
            DefaultProjectTrust::Never,
            Some(event),
            &mut ProjectTrustContext::headless(),
        )
        .unwrap();
        assert!(trusted);
    }

    #[test]
    fn stored_decision_beats_default() {
        let dirs = trust_requiring_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        store.set(&dirs.cwd, Some(false)).unwrap();
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            None,
            DefaultProjectTrust::Always,
            None,
            &mut ProjectTrustContext::headless(),
        )
        .unwrap();
        assert!(!trusted);
    }

    #[test]
    fn default_always_and_never_apply_without_stored_decision() {
        let dirs = trust_requiring_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        for (default, expected) in [
            (DefaultProjectTrust::Always, true),
            (DefaultProjectTrust::Never, false),
        ] {
            let trusted = resolve_project_trusted(
                &dirs.cwd,
                &store,
                None,
                default,
                None,
                &mut ProjectTrustContext::headless(),
            )
            .unwrap();
            assert_eq!(trusted, expected, "{default:?}");
        }
    }

    #[test]
    fn ask_without_ui_is_not_trusted() {
        let dirs = trust_requiring_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            None,
            DefaultProjectTrust::Ask,
            None,
            &mut ProjectTrustContext::headless(),
        )
        .unwrap();
        assert!(!trusted);
    }

    #[test]
    fn ask_with_ui_persists_selected_option() {
        let dirs = trust_requiring_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);

        // "Trust" → trusted + persisted.
        let mut context = ProjectTrustContext {
            has_ui: true,
            select: Some(select_returning(Some("Trust"))),
            select_async: None,
        };
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            None,
            DefaultProjectTrust::Ask,
            None,
            &mut context,
        )
        .unwrap();
        assert!(trusted);
        assert_eq!(store.get(&dirs.cwd).unwrap(), Some(true));

        // "Trust parent folder" → parent persisted, cwd entry cleared.
        let dirs = trust_requiring_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        let parent = normalize_cwd(&dirs.cwd);
        let parent = parent.parent().unwrap().to_path_buf();
        let label = format!("Trust parent folder ({})", parent.display());
        let mut context = ProjectTrustContext {
            has_ui: true,
            select: Some(select_returning(Some(&label))),
            select_async: None,
        };
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            None,
            DefaultProjectTrust::Ask,
            None,
            &mut context,
        )
        .unwrap();
        assert!(trusted);
        assert_eq!(store.get(&parent).unwrap(), Some(true));
        assert_eq!(
            store.get_entry(&dirs.cwd).unwrap().unwrap().0,
            canonicalize_path(&parent)
        );
        // The cwd key itself was cleared, not set (trust-manager.ts:77).
        let text = std::fs::read_to_string(dirs.agent_dir.join("trust.json")).unwrap();
        assert!(!text.contains(&canonicalize_path(&dirs.cwd).to_string_lossy().into_owned()));

        // "Trust (this session only)" → trusted, nothing persisted.
        let dirs = trust_requiring_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        let mut context = ProjectTrustContext {
            has_ui: true,
            select: Some(select_returning(Some("Trust (this session only)"))),
            select_async: None,
        };
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            None,
            DefaultProjectTrust::Ask,
            None,
            &mut context,
        )
        .unwrap();
        assert!(trusted);
        assert!(!dirs.agent_dir.join("trust.json").exists());

        // "Do not trust" → untrusted + persisted.
        let dirs = trust_requiring_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        let mut context = ProjectTrustContext {
            has_ui: true,
            select: Some(select_returning(Some("Do not trust"))),
            select_async: None,
        };
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            None,
            DefaultProjectTrust::Ask,
            None,
            &mut context,
        )
        .unwrap();
        assert!(!trusted);
        assert_eq!(store.get(&dirs.cwd).unwrap(), Some(false));

        // Cancel → untrusted, nothing persisted (project-trust.ts:94-95).
        let dirs = trust_requiring_dirs();
        let store = ProjectTrustStore::new(&dirs.agent_dir);
        let mut context = ProjectTrustContext {
            has_ui: true,
            select: Some(select_returning(None)),
            select_async: None,
        };
        let trusted = resolve_project_trusted(
            &dirs.cwd,
            &store,
            None,
            DefaultProjectTrust::Ask,
            None,
            &mut context,
        )
        .unwrap();
        assert!(!trusted);
        assert!(!dirs.agent_dir.join("trust.json").exists());
    }

    #[test]
    fn trust_prompt_text_mentions_config_dir() {
        let text = format_project_trust_prompt(Path::new("/some/project"));
        assert!(text.starts_with("Trust project folder?\n/some/project\n"));
        assert!(text.contains(CONFIG_DIR_NAME));
    }
}
