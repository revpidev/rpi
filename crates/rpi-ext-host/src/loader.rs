//! Extension loader — factories, discovery, load order, cache @ pi 0.82.1
//! (2efa728).
//!
//! Port of `packages/coding-agent/src/core/extensions/loader.ts`:
//! - `ExtensionFactory` / `InlineExtension` (types.ts:1488-1499)
//! - module cache keyed by cwd + generation (loader.ts:142-164, 395-428)
//! - `loadExtension` error isolation (loader.ts:454-480)
//! - `loadExtensionFromFactory` (loader.ts:485-498) + inline naming /
//!   `hidden` (resource-loader.ts:892-913)
//! - one-level discovery (loader.ts:581-668)
//! - `discoverAndLoadExtensions` path assembly with canonical dedupe
//!   (loader.ts:673-721), reordered to the resource-loader load order
//!   (`loadCurrentExtensionSet`, resource-loader.ts:494-514): CLI `-e`
//!   first, then discovered + package/settings paths, inline factories last
//!
//! Intentional differences:
//! - Native (L0) extensions are registered as boxed Rust factories, not
//!   loaded from files. Path-based discovery recognizes `.wasm` files and
//!   `rpi-extension.json` manifests (design §13 open item 2) instead of
//!   `*.ts` / `*.js` / `package.json` (loader.ts:581-583, 594-624); loading
//!   them fails with a W6 placeholder error that still flows through the
//!   same error-isolation path.
//! - The upstream cache is module-global (loader.ts:142-144); here it is
//!   owned by the loader so concurrent agents do not share factories. The
//!   cwd + generation invalidation semantics are unchanged.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::api::{BoxFuture, ExtensionApi, ExtensionRuntime, LoadedExtension};
use crate::types::ExtensionLoadError;

/// `ExtensionFactory` (types.ts:1489): receives the `pi` object, performs
/// registration, may be async. `Err(message)` mirrors a throw during
/// factory execution.
pub type ExtensionFactory =
    Arc<dyn Fn(ExtensionApi) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// `InlineExtension` (types.ts:1491-1499): anonymous factory, or named with
/// an optional `hidden` flag (named only — resource-loader.ts:900-906).
/// `Clone` (the factory is an `Arc`) so the host can replay the set on
/// `/reload` (T15 W5).
#[derive(Clone)]
pub enum InlineExtension {
    /// Bare factory; gets path `<inline:N>` (1-based load index).
    Anonymous(ExtensionFactory),
    /// `{ name, factory, hidden? }`; gets path `<inline:name>`.
    Named {
        name: String,
        factory: ExtensionFactory,
        hidden: bool,
    },
}

impl InlineExtension {
    /// Path assigned at load time (resource-loader.ts:902).
    fn extension_path(&self, index: usize) -> String {
        match self {
            InlineExtension::Anonymous(_) => format!("<inline:{}>", index + 1),
            InlineExtension::Named { name, .. } => format!("<inline:{name}>"),
        }
    }

    fn factory(&self) -> &ExtensionFactory {
        match self {
            InlineExtension::Anonymous(factory) => factory,
            InlineExtension::Named { factory, .. } => factory,
        }
    }

    fn hidden(&self) -> bool {
        match self {
            InlineExtension::Anonymous(_) => false,
            InlineExtension::Named { hidden, .. } => *hidden,
        }
    }
}

/// `LoadExtensionsResult` minus the runtime (the caller owns it)
/// (types.ts:1677-1683).
#[derive(Default)]
pub struct LoadExtensionsResult {
    pub extensions: Vec<Arc<LoadedExtension>>,
    pub errors: Vec<ExtensionLoadError>,
}

// ============================================================================
// Factory cache (loader.ts:142-164, 395-428)
// ============================================================================

/// Cache token: the (cwd, generation) pair a load was started with
/// (`ExtensionCacheToken`, loader.ts:146-149).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheToken {
    pub cwd: String,
    pub generation: u64,
}

/// Module cache keyed by canonical path, scoped to (cwd, generation)
/// (loader.ts:142-164). A cwd switch or an explicit [`FactoryCache::clear`]
/// (reload) invalidates every entry. Interior-mutable + `Clone` (shared
/// inner) so the owning host can hold it behind a lock (T15 W5 reload).
#[derive(Clone, Default)]
pub struct FactoryCache {
    inner: Arc<FactoryCacheInner>,
}

#[derive(Default)]
struct FactoryCacheInner {
    cwd: std::sync::RwLock<Option<String>>,
    generation: std::sync::atomic::AtomicU64,
    factories: std::sync::RwLock<HashMap<PathBuf, ExtensionFactory>>,
}

impl FactoryCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn read<T>(m: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
        m.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write<T>(m: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
        m.write().unwrap_or_else(|e| e.into_inner())
    }

    /// `clearExtensionCache` (loader.ts:151-155).
    pub fn clear(&self) {
        Self::write(&self.inner.factories).clear();
        *Self::write(&self.inner.cwd) = None;
        self.inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// `useExtensionCacheCwd` (loader.ts:157-164): a cwd change clears the
    /// cache; returns the token for this load pass.
    pub fn use_cwd(&self, cwd: &str) -> CacheToken {
        if Self::read(&self.inner.cwd)
            .as_deref()
            .is_some_and(|cached| cached != cwd)
        {
            self.clear();
        }
        *Self::write(&self.inner.cwd) = Some(cwd.to_owned());
        CacheToken {
            cwd: cwd.to_owned(),
            generation: self
                .inner
                .generation
                .load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// `isCurrentCacheToken` (loader.ts:395-401).
    pub fn is_current(&self, token: &CacheToken) -> bool {
        Self::read(&self.inner.cwd).as_deref() == Some(token.cwd.as_str())
            && self
                .inner
                .generation
                .load(std::sync::atomic::Ordering::SeqCst)
                == token.generation
    }

    /// Cached factory lookup, valid only under a current token
    /// (loader.ts:404-409).
    pub fn get(&self, path: &Path, token: &CacheToken) -> Option<ExtensionFactory> {
        if self.is_current(token) {
            Self::read(&self.inner.factories).get(path).cloned()
        } else {
            None
        }
    }

    /// Cache insert, valid only under a current token (loader.ts:424-426).
    pub fn insert(&self, path: PathBuf, factory: ExtensionFactory, token: &CacheToken) {
        if self.is_current(token) {
            Self::write(&self.inner.factories).insert(path, factory);
        }
    }
}

// ============================================================================
// Discovery (loader.ts:581-668, adapted to wasm entries)
// ============================================================================

/// `isExtensionFile` (loader.ts:581-583) adapted: bare `.wasm` files load
/// with `capabilities = []` (design §13 open item 2).
fn is_extension_file(name: &str) -> bool {
    name.ends_with(".wasm")
}

/// `resolveExtensionEntries` (loader.ts:594-624) adapted: a subdirectory is
/// an extension when it has a `rpi-extension.json` manifest naming a `wasm`
/// file that exists, or an `index.wasm`.
fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let manifest_path = dir.join("rpi-extension.json");
    if manifest_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                // `wasm` wins over `native` when both are present
                // (docs/extension-abi.md §5).
                for field in ["wasm", "native"] {
                    if let Some(entry) = manifest.get(field).and_then(|w| w.as_str()) {
                        let resolved = dir.join(entry);
                        if resolved.is_file() {
                            return Some(vec![resolved]);
                        }
                    }
                }
            }
        }
    }

    let index_wasm = dir.join("index.wasm");
    if index_wasm.is_file() {
        return Some(vec![index_wasm]);
    }

    None
}

/// `discoverExtensionsInDir` (loader.ts:636-668): one level only — direct
/// `*.wasm` files, and subdirectories resolved via
/// [`resolve_extension_entries`]. Unreadable directories yield no entries
/// (loader.ts:662-665).
pub fn discover_extensions_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut discovered = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return discovered,
    };

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let entry_path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // `isFile() || isSymbolicLink()` (loader.ts:650).
        let is_file_like = file_type.is_file() || file_type.is_symlink();
        let is_dir_like = file_type.is_dir() || file_type.is_symlink();

        let name = entry.file_name();
        let name = name.to_string_lossy();

        if is_file_like && is_extension_file(&name) {
            discovered.push(entry_path);
            continue;
        }

        if is_dir_like && entry_path.is_dir() {
            if let Some(entries) = resolve_extension_entries(&entry_path) {
                discovered.extend(entries);
            }
        }
    }

    discovered
}

/// Canonical path for dedupe (`path.resolve` + `seen` set,
/// loader.ts:684-692): canonicalize when possible, else fall back to the
/// absolute-ish input so missing paths still dedupe consistently.
pub(crate) fn canonical_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

// ============================================================================
// Loader
// ============================================================================

/// Inputs for a full discovery + load pass
/// (`discoverAndLoadExtensions` + `loadCurrentExtensionSet` merge,
/// loader.ts:673-721, resource-loader.ts:494-514).
pub struct DiscoverConfig {
    /// Working directory (`.rpi/extensions` discovery root).
    pub cwd: PathBuf,
    /// Agent dir (`~/.rpi/agent`; its `extensions/` subdirectory is
    /// discovered).
    pub agent_dir: PathBuf,
    /// CLI `-e` paths (temporary scope) — loaded first, even with
    /// `no_extensions` (resource-loader.ts:500-504).
    pub cli_paths: Vec<String>,
    /// Package/settings-resolved extension paths — after discovery.
    pub package_paths: Vec<String>,
    /// Inline factories — loaded last (resource-loader.ts:509-512).
    pub inline: Vec<InlineExtension>,
    /// Include project-local `.rpi/extensions` discovery. `false` under an
    /// untrusted project (resource-loader.ts:327-335 forces untrusted
    /// settings for the bootstrap pass, which excludes project-local
    /// extensions/packages).
    pub include_project_local: bool,
    /// `--no-extensions`: only CLI `-e` paths load
    /// (resource-loader.ts:500-504).
    pub no_extensions: bool,
}

/// State carried from the pre-trust bootstrap load into the final pass
/// (resource-loader.ts:520-571 `loadFinalExtensionSet`): already-loaded
/// extensions are reused, already-attempted paths are skipped, and inline
/// extensions trail in their original order.
pub struct PreTrustRecord {
    /// Extensions loaded pre-trust (inline factories, until path loading
    /// lands in W6).
    pub extensions: Vec<Arc<LoadedExtension>>,
    /// Load errors from the pre-trust pass (carried into the final result,
    /// resource-loader.ts:564-568).
    pub errors: Vec<ExtensionLoadError>,
}

/// Loads native extension factories and (W6) wasm modules into
/// [`LoadedExtension`]s sharing one [`ExtensionRuntime`]. `Clone` (both
/// fields are shared handles).
#[derive(Clone)]
pub struct ExtensionLoader {
    runtime: ExtensionRuntime,
    cache: FactoryCache,
}

impl ExtensionLoader {
    pub fn new(runtime: ExtensionRuntime) -> Self {
        ExtensionLoader {
            runtime,
            cache: FactoryCache::new(),
        }
    }

    pub fn runtime(&self) -> ExtensionRuntime {
        self.runtime.clone()
    }

    pub fn cache(&self) -> &FactoryCache {
        &self.cache
    }

    /// Replace the runtime handle (T15 W5 reload: factories re-run against
    /// the fresh runtime).
    pub fn set_runtime(&mut self, runtime: ExtensionRuntime) {
        self.runtime = runtime;
    }

    /// `loadExtensionFromFactory` (loader.ts:485-498) + inline naming /
    /// hidden (resource-loader.ts:900-910). Factory errors are isolated:
    /// the extension is dropped and the error recorded (loader.ts:476-479).
    pub async fn load_inline(
        &self,
        inline: &[InlineExtension],
        cwd: &Path,
    ) -> LoadExtensionsResult {
        let mut result = LoadExtensionsResult::default();

        for (index, input) in inline.iter().enumerate() {
            let extension_path = input.extension_path(index);
            let extension = Arc::new(LoadedExtension::new(&extension_path, &extension_path));
            let api = ExtensionApi::new(
                extension.clone(),
                self.runtime.clone(),
                cwd.to_string_lossy().into_owned(),
            );
            match (input.factory())(api).await {
                Ok(()) => {
                    // `extension.hidden` post-load (resource-loader.ts:905).
                    extension.set_hidden(input.hidden());
                    result.extensions.push(extension);
                }
                Err(error) => result.errors.push(ExtensionLoadError {
                    path: extension_path,
                    error,
                }),
            }
        }

        result
    }

    /// `loadExtension` for filesystem paths (loader.ts:454-480): `.wasm`
    /// guest modules. Errors are isolated per path — one bad extension
    /// never blocks the rest.
    pub async fn load_paths(&self, paths: &[PathBuf], cwd: &Path) -> LoadExtensionsResult {
        let mut result = LoadExtensionsResult::default();
        let token = self.cache.use_cwd(&cwd.to_string_lossy());
        // Directory entries resolve to their declared entry points first,
        // then one-level discovery (loader.ts:705-716).
        let mut expanded: Vec<PathBuf> = Vec::new();
        for path in paths {
            if path.is_dir() {
                if let Some(entries) = resolve_extension_entries(path) {
                    expanded.extend(entries);
                } else {
                    expanded.extend(discover_extensions_in_dir(path));
                }
            } else {
                expanded.push(path.clone());
            }
        }
        for path in &expanded {
            match self.load_one_path(path, cwd, &token).await {
                Ok(extension) => result.extensions.push(extension),
                Err(error) => result.errors.push(ExtensionLoadError {
                    path: path.to_string_lossy().into_owned(),
                    error,
                }),
            }
        }
        result
    }

    /// Load one `.wasm` entry (loader.ts:454-480 `loadExtension`): resolve
    /// capabilities (manifest vs bare), compile via the cwd+generation
    /// cache, run `rpi_extension_init`.
    async fn load_one_path(
        &self,
        path: &Path,
        cwd: &Path,
        token: &CacheToken,
    ) -> Result<Arc<LoadedExtension>, String> {
        let capabilities = wasm_capabilities_for(path)?;
        // Native (abi_stable) plugins dispatch by extension; no compile
        // cache (the OS caches the dylib).
        if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("so" | "dll" | "dylib")
        ) {
            return self.load_native_path(path, cwd, capabilities).await;
        }
        let path_str = path.to_string_lossy().into_owned();
        let extension = Arc::new(LoadedExtension::new(&path_str, &path_str));
        let api = ExtensionApi::new(
            extension.clone(),
            self.runtime.clone(),
            cwd.to_string_lossy().into_owned(),
        );

        let factory = match self.cache.get(path, token) {
            Some(factory) => factory,
            None => {
                let bytes = std::fs::read(path).map_err(|e| {
                    format!("Failed to load extension: read {}: {e}", path.display())
                })?;
                let module = crate::wasm::compile_module(&bytes)
                    .map_err(|e| format!("Failed to load extension: compile: {e}"))?;
                let factory: ExtensionFactory = Arc::new(move |api| {
                    let module = module.clone();
                    let capabilities = capabilities.clone();
                    Box::pin(async move {
                        let guest =
                            crate::wasm::instantiate_and_init(&module, api.clone(), capabilities)
                                .await?;
                        // The guest thread lives on the extension object.
                        api.extension().set_wasm_guest(guest);
                        Ok(())
                    })
                });
                self.cache
                    .insert(path.to_path_buf(), factory.clone(), token);
                factory
            }
        };
        factory(api)
            .await
            .map_err(|e| format!("Failed to load extension: {e}"))?;
        Ok(extension)
    }

    /// Load a native (abi_stable) plugin path (T15 W7).
    async fn load_native_path(
        &self,
        path: &Path,
        cwd: &Path,
        capabilities: std::collections::HashSet<crate::wasm::Capability>,
    ) -> Result<Arc<LoadedExtension>, String> {
        let path_str = path.to_string_lossy().into_owned();
        let extension = Arc::new(LoadedExtension::new(&path_str, &path_str));
        let api = ExtensionApi::new(
            extension.clone(),
            self.runtime.clone(),
            cwd.to_string_lossy().into_owned(),
        );
        crate::native::load_native_plugin(path, api, capabilities)
            .await
            .map_err(|e| format!("Failed to load extension: {e}"))?;
        Ok(extension)
    }

    /// Assemble the ordered, canonically deduped path list
    /// (loader.ts:673-721 reordered to the resource-loader load order,
    /// resource-loader.ts:494-514):
    ///
    /// 1. CLI `-e` paths (always, even with `no_extensions`)
    /// 2. project-local `.rpi/extensions/` (when included)
    /// 3. global `<agent_dir>/extensions/`
    /// 4. package/settings paths
    ///
    /// Sources 2-4 are skipped with `no_extensions`
    /// (resource-loader.ts:500-504). First occurrence wins on canonical
    /// dedupe (loader.ts:684-692).
    pub fn resolve_paths(config: &DiscoverConfig) -> Vec<PathBuf> {
        fn add_paths(paths: Vec<PathBuf>, all: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
            for path in paths {
                let key = canonical_key(&path);
                if seen.insert(key) {
                    all.push(path);
                }
            }
        }

        let mut all_paths: Vec<PathBuf> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();

        // 1. CLI `-e` (resolved relative to cwd, loader.ts:703-718).
        let cli: Vec<PathBuf> = config
            .cli_paths
            .iter()
            .map(|p| expand_cli_path(p, &config.cwd))
            .collect();
        add_paths(cli, &mut all_paths, &mut seen);

        if !config.no_extensions {
            // 2. project-local (loader.ts:694-696).
            if config.include_project_local {
                let local_dir = config.cwd.join(".rpi").join("extensions");
                add_paths(
                    discover_extensions_in_dir(&local_dir),
                    &mut all_paths,
                    &mut seen,
                );
            }

            // 3. global (loader.ts:698-700).
            let global_dir = config.agent_dir.join("extensions");
            add_paths(
                discover_extensions_in_dir(&global_dir),
                &mut all_paths,
                &mut seen,
            );

            // 4. package/settings paths.
            let package: Vec<PathBuf> = config
                .package_paths
                .iter()
                .map(|p| expand_cli_path(p, &config.cwd))
                .collect();
            add_paths(package, &mut all_paths, &mut seen);
        }

        all_paths
    }

    /// Full discovery + load pass: paths in [`Self::resolve_paths`] order,
    /// then inline factories (resource-loader.ts:509-512).
    pub async fn discover_and_load(&self, config: &DiscoverConfig) -> LoadExtensionsResult {
        let all_paths = Self::resolve_paths(config);
        let mut result = self.load_paths(&all_paths, &config.cwd).await;

        // Inline factories tail.
        let mut inline_result = self.load_inline(&config.inline, &config.cwd).await;
        result.extensions.append(&mut inline_result.extensions);
        result.errors.append(&mut inline_result.errors);

        result
    }

    /// Final-pass load after a pre-trust bootstrap
    /// (`loadFinalExtensionSet`, resource-loader.ts:520-571): paths already
    /// loaded pre-trust are reused, pre-trust failures are not retried,
    /// pre-trust extensions are ordered by the FINAL path list (a pre-trust
    /// load outside the final path set — e.g. `--no-extensions` dropping
    /// global discovery — is discarded, :543-564), inline factories are NOT
    /// re-run and trail in their pre-trust order (:556-558), and pre-trust
    /// errors carry into the result (:564-568).
    pub async fn discover_and_load_reuse(
        &self,
        config: &DiscoverConfig,
        pre_trust: &PreTrustRecord,
    ) -> LoadExtensionsResult {
        let all_paths = Self::resolve_paths(config);

        // `preloadedByPath` (:543-548): pre-trust PATH extensions keyed by
        // canonical path; inline extensions are excluded (they trail the
        // final order, :556-558).
        let mut loaded: HashMap<PathBuf, Arc<LoadedExtension>> = pre_trust
            .extensions
            .iter()
            .filter(|ext| !ext.path.starts_with("<inline:"))
            .map(|ext| (canonical_key(Path::new(&ext.resolved_path)), ext.clone()))
            .collect();

        // `failedPreloadPaths` (:549-553): pre-trust attempts that errored
        // are not retried in the final pass.
        let failed: HashSet<PathBuf> = pre_trust
            .errors
            .iter()
            .map(|error| canonical_key(Path::new(&error.path)))
            .collect();

        let remaining: Vec<PathBuf> = all_paths
            .iter()
            .filter(|path| {
                let key = canonical_key(path);
                !loaded.contains_key(&key) && !failed.contains(&key)
            })
            .cloned()
            .collect();
        let result = self.load_paths(&remaining, &config.cwd).await;

        // `loadedByPath` (:554-555): pre-trust reuse + fresh loads.
        for ext in &result.extensions {
            loaded.insert(canonical_key(Path::new(&ext.resolved_path)), ext.clone());
        }

        // `orderedExtensions` (:560-564): the FINAL path order decides the
        // extension order (a pre-trust extension whose path is not in the
        // final set is dropped); pre-trust inline extensions trail.
        let mut extensions: Vec<Arc<LoadedExtension>> = all_paths
            .iter()
            .filter_map(|path| loaded.get(&canonical_key(path)).cloned())
            .collect();
        extensions.extend(
            pre_trust
                .extensions
                .iter()
                .filter(|ext| ext.path.starts_with("<inline:"))
                .cloned(),
        );

        // `errors: [...preTrustExtensions.errors, ...remainingExtensions.errors]`
        let mut errors = pre_trust.errors.clone();
        errors.extend(result.errors);
        LoadExtensionsResult { extensions, errors }
    }
}

/// Resolve a CLI/configured path against cwd (`resolvePath(p, cwd)`,
/// loader.ts:704). `~` expansion lands with the config module integration
/// (W7); relative paths resolve against `cwd`.
fn expand_cli_path(path: &str, cwd: &Path) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

// ============================================================================
// Wasm manifest + capabilities (design §13 open item 2)
// ============================================================================

/// `rpi-extension.json` (directory-level manifest).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WasmManifest {
    // name/version/wasm/description are discovery/display metadata; the W6
    // loader only enforces `rpiAbi` + `capabilities` (display lands with
    // the W7 install management).
    #[allow(dead_code)]
    name: Option<String>,
    #[allow(dead_code)]
    version: Option<String>,
    #[allow(dead_code)]
    description: Option<String>,
    #[allow(dead_code)]
    wasm: Option<String>,
    capabilities: Option<Vec<String>>,
    rpi_abi: Option<u32>,
    /// Native (abi_stable) plugin library path — package-relative
    /// (docs/extension-abi.md §5; mutually exclusive with `wasm`, which
    /// wins when both are present). Discovery reads it off the raw
    /// manifest Value (`resolve_extension_entries`); this typed mirror
    /// only documents the schema.
    #[allow(dead_code)]
    native: Option<String>,
}

/// Locate the manifest governing a `.wasm` entry: the package directory
/// containing `rpi-extension.json` within two levels above the file (the
/// manifest's `wasm` field is package-relative, e.g. `dist/x.wasm`).
fn find_manifest(wasm_path: &Path) -> Option<PathBuf> {
    let mut dir = wasm_path.parent();
    for _ in 0..2 {
        let current = dir?;
        let candidate = current.join("rpi-extension.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
}

/// The resolved plugin kind for a discovered entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    Wasm,
    Native,
}

/// Resolve the capability set for a plugin entry: bare files (no manifest)
/// get `[]` (event subscription only, per the W6 spec); manifests validate
/// `rpiAbi` and map capability strings.
fn wasm_capabilities_for(
    wasm_path: &Path,
) -> Result<std::collections::HashSet<crate::wasm::Capability>, String> {
    let Some(manifest_path) = find_manifest(wasm_path) else {
        return Ok(std::collections::HashSet::new());
    };
    let content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Failed to load extension: read manifest: {e}"))?;
    let manifest: WasmManifest = serde_json::from_str(&content).map_err(|e| {
        format!(
            "Failed to load extension: parse {}: {e}",
            manifest_path.display()
        )
    })?;
    match manifest.rpi_abi {
        Some(crate::wasm::RPI_ABI_VERSION) => {}
        other => {
            return Err(format!(
                "Failed to load extension: unsupported rpiAbi {other:?} (host implements {})",
                crate::wasm::RPI_ABI_VERSION
            ))
        }
    }
    let mut capabilities = std::collections::HashSet::new();
    for name in manifest.capabilities.unwrap_or_default() {
        match crate::wasm::Capability::parse(&name) {
            Some(capability) => {
                capabilities.insert(capability);
            }
            None => {
                return Err(format!(
                    "Failed to load extension: unknown capability \"{name}\" in {}",
                    manifest_path.display()
                ))
            }
        }
    }
    Ok(capabilities)
}
