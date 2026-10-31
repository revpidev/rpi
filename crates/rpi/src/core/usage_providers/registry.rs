//! Provider resolution, governance, identity, and the fetch pipeline
//! (V16-05 FR-A R1/R3/R4/R5; V18-10 FR-B/FR-C).
//!
//! Resolution priority is explicit `usage.providers` settings > user
//! directory (`<agentDir>/usage-providers/<provider>.py`) > plugin
//! registration. Fetches are serialized (concurrency 1), bounded by the
//! configured timeout, and cached per credential identity.
//!
//! `ctx.usage.cached` is call-graph isolated from
//! [`execute_usage_script`] and the network; `ctx.usage.fetchDetailed`
//! reports `source`/`stale`/`error`; the legacy `ctx.usage.fetch` keeps its
//! `Option<Value>` shape and stale fallback for third-party scripts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::cache::{DEFAULT_USAGE_CACHE_TTL_MS, UsageCache, UsageCacheKey};
use super::identity::{UsageCredentialResolver, UsageCredentials, is_builtin_usage_provider};
use super::script::{UsageScriptContext, UsageScriptEnv, execute_usage_script, usage_api_key_env};
use super::{UsageEnvelope, UsageError, UsageErrorKind, UsageFetchResult};

/// Resolved `usage` settings source (read lazily so hot-reloaded settings
/// apply on the next call).
pub type UsageFrameworkSettings = crate::core::settings_manager::UsageSettings;

/// Default script execution timeout (R6.1.3; statusline magnitude).
pub const DEFAULT_USAGE_TIMEOUT_MS: u64 = 3_000;
/// Lower bound for the configurable timeout (`usage.timeoutMs`).
pub const MIN_USAGE_TIMEOUT_MS: u64 = 500;
/// Upper bound for the configurable timeout (`usage.timeoutMs`).
pub const MAX_USAGE_TIMEOUT_MS: u64 = 60_000;
/// stdout cap for one script run; overflow is a failed fetch (V16-05 §7.2).
pub const MAX_USAGE_STDOUT_BYTES: usize = 64 * 1024;

/// Where a resolved provider script came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageProviderSource {
    /// Explicit `usage.providers.<provider>` settings entry.
    Explicit,
    /// `<agentDir>/usage-providers/<provider>.py` auto-discovery.
    UserDir,
    /// `ctx.usage.register(provider, scriptPath)` from an extension.
    Plugin,
}

impl UsageProviderSource {
    pub fn as_str(self) -> &'static str {
        match self {
            UsageProviderSource::Explicit => "explicit",
            UsageProviderSource::UserDir => "user-dir",
            UsageProviderSource::Plugin => "plugin",
        }
    }
}

/// One resolved provider → script binding with the optional context
/// metadata forwarded to the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageProviderSpec {
    pub provider: String,
    pub script_path: PathBuf,
    pub source: UsageProviderSource,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub model: Option<String>,
}

type SettingsFn = Arc<dyn Fn() -> UsageFrameworkSettings + Send + Sync>;

/// Session-scoped usage-provider registry (V16-05 FR-A). Cheap to share via
/// `Arc`; the cache and registrations live for the session.
pub struct UsageProviderRegistry {
    agent_dir: PathBuf,
    cwd: String,
    settings: SettingsFn,
    credentials: Arc<dyn UsageCredentialResolver>,
    offline: Arc<dyn Fn() -> bool + Send + Sync>,
    registrations: Mutex<BTreeMap<String, PathBuf>>,
    cache: UsageCache,
    /// Serializes script execution (concurrency 1, R6.1.3).
    execution_lock: tokio::sync::Mutex<()>,
}

impl UsageProviderRegistry {
    pub fn new(
        agent_dir: PathBuf,
        cwd: String,
        settings: SettingsFn,
        credentials: Arc<dyn UsageCredentialResolver>,
    ) -> Self {
        Self::with_offline(
            agent_dir,
            cwd,
            settings,
            credentials,
            Arc::new(crate::core::environment::is_offline),
        )
    }

    /// Testable constructor with an injected offline gate. Production builds
    /// use [`Self::new`] (live `RPI_OFFLINE`).
    pub fn with_offline(
        agent_dir: PathBuf,
        cwd: String,
        settings: SettingsFn,
        credentials: Arc<dyn UsageCredentialResolver>,
        offline: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            agent_dir,
            cwd,
            settings,
            credentials,
            offline,
            registrations: Mutex::new(BTreeMap::new()),
            cache: UsageCache::new(Duration::from_millis(DEFAULT_USAGE_CACHE_TTL_MS)),
            execution_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The user script directory (`<agentDir>/usage-providers`).
    pub fn user_dir(&self) -> PathBuf {
        self.agent_dir.join("usage-providers")
    }

    /// Every known provider id (explicit ∪ user dir ∪ plugin), sorted and
    /// deduplicated.
    pub fn list_providers(&self) -> Vec<String> {
        let mut ids: BTreeSet<String> = BTreeSet::new();
        ids.extend((self.settings)().providers.keys().cloned());
        ids.extend(self.discover_user_dir().into_keys());
        ids.extend(
            self.registrations
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .keys()
                .cloned(),
        );
        ids.into_iter().collect()
    }

    /// Register (or replace) a provider's script path
    /// (`ctx.usage.register`). Empty values are rejected; a relative path
    /// resolves against the session cwd. Re-registering clears every cached
    /// identity of that provider.
    pub fn register(&self, provider: &str, script_path: &str) -> Result<(), String> {
        let provider = provider.trim();
        let script_path = script_path.trim();
        if provider.is_empty() {
            return Err("provider must not be empty".to_owned());
        }
        if script_path.is_empty() {
            return Err("scriptPath must not be empty".to_owned());
        }
        self.registrations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(provider.to_owned(), self.resolve_script_path(script_path));
        self.cache.remove_provider(provider);
        Ok(())
    }

    /// Resolve one provider against the priority chain. First-party providers
    /// never receive a settings `baseUrl`: production usage endpoints are
    /// fixed and a custom/proxy URL must not become a usage target.
    pub fn resolve(&self, provider: &str) -> Option<UsageProviderSpec> {
        let settings = (self.settings)();
        if let Some(config) = settings.providers.get(provider) {
            return Some(UsageProviderSpec {
                provider: provider.to_owned(),
                script_path: self.resolve_script_path(&config.script),
                source: UsageProviderSource::Explicit,
                base_url: if is_builtin_usage_provider(provider) {
                    None
                } else {
                    config.base_url.clone()
                },
                api_key_env: config.api_key_env.clone(),
                model: config.model.clone(),
            });
        }
        if let Some(path) = self.discover_user_dir().remove(provider) {
            return Some(UsageProviderSpec {
                provider: provider.to_owned(),
                script_path: path,
                source: UsageProviderSource::UserDir,
                base_url: None,
                api_key_env: None,
                model: None,
            });
        }
        let path = self
            .registrations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(provider)
            .cloned();
        path.map(|script_path| UsageProviderSpec {
            provider: provider.to_owned(),
            script_path,
            source: UsageProviderSource::Plugin,
            base_url: None,
            api_key_env: None,
            model: None,
        })
    }

    /// Cache-only read (zero scripts, zero network). The credential identity
    /// is verified read-only; a first-party provider whose identity cannot be
    /// proven answers `auth-unavailable` instead of another account's quota.
    pub async fn cached_result(&self, provider: &str) -> UsageFetchResult {
        let spec = self.resolve(provider);
        let identity = match self
            .resolve_identity(provider, spec.as_ref().and_then(identity_env))
            .await
        {
            Ok(identity) => identity,
            Err(kind) => return UsageFetchResult::failure(UsageError::new(kind)),
        };
        let key = cache_key(provider, identity.as_ref());
        match self.cache.get_last(&key) {
            Some(envelope) => {
                let stale = !self.cache.is_fresh(&key);
                UsageFetchResult::cache(envelope, stale)
            }
            None => UsageFetchResult::failure(UsageError::new(UsageErrorKind::CacheMiss)),
        }
    }

    /// Detailed fetch (FR-B/FR-C): identity/origin/offline → fresh cache
    /// (non-forced) → script → success cache / classified error + optional
    /// same-identity stale value.
    pub async fn fetch_detailed(&self, provider: &str, force: bool) -> UsageFetchResult {
        let Some(spec) = self.resolve(provider) else {
            return UsageFetchResult::failure(UsageError::new(UsageErrorKind::NoData));
        };
        let offline = (self.offline)();
        let identity = match self
            .resolve_identity(provider, spec.api_key_env.as_deref())
            .await
        {
            Ok(identity) => identity,
            // Offline is the primary condition: no script or network can
            // run, so an unprovable identity is reported as `offline` rather
            // than a login problem.
            Err(_) if offline => {
                return UsageFetchResult::failure(UsageError::new(UsageErrorKind::Offline));
            }
            Err(kind) => return UsageFetchResult::failure(UsageError::new(kind)),
        };
        let key = cache_key(provider, identity.as_ref());

        if offline {
            return match self.cache.get_last(&key) {
                Some(envelope) => UsageFetchResult::cache_with_error(
                    envelope,
                    UsageError::new(UsageErrorKind::Offline),
                ),
                None => UsageFetchResult::failure(UsageError::new(UsageErrorKind::Offline)),
            };
        }

        if !force && let Some(envelope) = self.cache.get_fresh(&key) {
            return UsageFetchResult::cache(envelope, false);
        }

        let _guard = self.execution_lock.lock().await;
        // A queued fetch for the same identity may have refreshed the cache
        // while this one waited for its turn.
        if !force && let Some(envelope) = self.cache.get_fresh(&key) {
            return UsageFetchResult::cache(envelope, false);
        }

        let timeout_ms = clamp_usage_timeout((self.settings)().timeout_ms);
        let (context, env) = self.script_inputs(&spec, identity.as_ref());
        match execute_usage_script(&spec.script_path, &self.cwd, &context, timeout_ms, &env).await {
            Ok(envelope) => {
                self.cache.store(&key, envelope.clone());
                UsageFetchResult::network(envelope)
            }
            Err(failure) => {
                // Never log stdout/stderr or a credential: `detail` is a
                // fixed diagnostic category.
                tracing::debug!(
                    provider = %provider,
                    reason = %failure.detail,
                    "usage provider fetch failed"
                );
                match self.cache.get_last(&key) {
                    Some(envelope) => UsageFetchResult::cache_with_error(envelope, failure.error),
                    None => UsageFetchResult::failure(failure.error),
                }
            }
        }
    }

    /// Legacy `ctx.usage.fetch` surface: the latest successful envelope JSON
    /// (same-identity stale fallback), or `None`. Kept for third-party
    /// compatibility; new consumers use [`Self::fetch_detailed`].
    pub async fn fetch(&self, provider: &str, force: bool) -> Option<Value> {
        self.fetch_detailed(provider, force)
            .await
            .envelope
            .map(|envelope| envelope.to_json())
    }

    /// The last successful envelope for tests/diagnostics (legacy-keyed for
    /// third-party providers).
    pub fn cached_envelope(&self, provider: &str) -> Option<UsageEnvelope> {
        self.cache.get_last(&UsageCacheKey::legacy(provider))
    }

    async fn resolve_identity(
        &self,
        provider: &str,
        api_key_env: Option<&str>,
    ) -> Result<Option<UsageCredentials>, UsageErrorKind> {
        self.credentials.resolve(provider, api_key_env).await
    }

    fn script_inputs(
        &self,
        spec: &UsageProviderSpec,
        identity: Option<&UsageCredentials>,
    ) -> (UsageScriptContext, UsageScriptEnv) {
        match identity {
            Some(identity) => {
                let context = UsageScriptContext {
                    provider: spec.provider.clone(),
                    // First-party usage endpoints are fixed; never forward a
                    // custom/proxy URL.
                    base_url: None,
                    api_key_env: identity.api_key_env.clone(),
                    model: spec.model.clone(),
                    credential_kind: Some(identity.credential_kind),
                };
                (context, identity.script_env.clone())
            }
            None => {
                let api_key_env = spec
                    .api_key_env
                    .clone()
                    .or_else(|| usage_api_key_env(&spec.provider));
                let env = credential_env(api_key_env.as_deref());
                let context = UsageScriptContext {
                    provider: spec.provider.clone(),
                    base_url: spec.base_url.clone(),
                    api_key_env,
                    model: spec.model.clone(),
                    credential_kind: None,
                };
                (context, env)
            }
        }
    }

    /// Scan the user script directory once (each call re-reads it so newly
    /// dropped scripts are picked up without a restart).
    fn discover_user_dir(&self) -> BTreeMap<String, PathBuf> {
        let mut found = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(self.user_dir()) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("py") {
                continue;
            }
            let Some(provider) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if provider.is_empty() {
                continue;
            }
            found.insert(provider.to_owned(), path);
        }
        found
    }

    fn resolve_script_path(&self, script_path: &str) -> PathBuf {
        let path = Path::new(script_path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(&self.cwd).join(path)
        }
    }
}

fn identity_env(spec: &UsageProviderSpec) -> Option<&str> {
    spec.api_key_env.as_deref()
}

fn cache_key(provider: &str, identity: Option<&UsageCredentials>) -> UsageCacheKey {
    match identity {
        Some(identity) => UsageCacheKey::new(
            provider,
            identity.generation.clone(),
            identity.effective_origin.clone(),
        ),
        None => UsageCacheKey::legacy(provider),
    }
}

/// Clamp `usage.timeoutMs` into `[MIN, MAX]`, defaulting to 3000ms.
pub fn clamp_usage_timeout(timeout_ms: Option<u64>) -> u64 {
    timeout_ms
        .unwrap_or(DEFAULT_USAGE_TIMEOUT_MS)
        .clamp(MIN_USAGE_TIMEOUT_MS, MAX_USAGE_TIMEOUT_MS)
}

/// Credential injection: the value travels through the child environment
/// only, and only when the host process actually has it. The env-var name is
/// forwarded to the script via the stdin context (never the value).
fn credential_env(api_key_env: Option<&str>) -> UsageScriptEnv {
    match api_key_env {
        Some(name) => match std::env::var(name) {
            Ok(value) => vec![(name.to_owned(), value)],
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings_manager::{UsageProviderConfig, UsageSettings};
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nan = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "rpi-usage-providers-test-{}-{nan}-{id}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn registry(temp: &TempDir, settings: UsageSettings) -> (UsageProviderRegistry, PathBuf) {
        let settings = Arc::new(move || settings.clone());
        registry_with_settings(
            temp,
            settings,
            Arc::new(super::super::LegacyCredentialResolver),
        )
    }

    fn registry_with_settings(
        temp: &TempDir,
        settings: SettingsFn,
        credentials: Arc<dyn UsageCredentialResolver>,
    ) -> (UsageProviderRegistry, PathBuf) {
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("cwd");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::create_dir_all(agent_dir.join("usage-providers")).expect("user dir");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let registry = UsageProviderRegistry::with_offline(
            agent_dir.clone(),
            cwd.display().to_string(),
            settings,
            credentials,
            Arc::new(|| false),
        );
        (registry, agent_dir)
    }

    fn explicit(path: &str) -> UsageProviderConfig {
        UsageProviderConfig {
            script: path.to_owned(),
            ..UsageProviderConfig::default()
        }
    }

    struct FakeResolver {
        result: Result<Option<UsageCredentials>, UsageErrorKind>,
    }

    #[async_trait]
    impl UsageCredentialResolver for FakeResolver {
        async fn resolve(
            &self,
            _provider: &str,
            _api_key_env: Option<&str>,
        ) -> Result<Option<UsageCredentials>, UsageErrorKind> {
            self.result.clone()
        }
    }

    fn fake_identity(generation: &str) -> UsageCredentials {
        UsageCredentials {
            credential_kind: super::super::UsageCredentialKind::ApiKey,
            generation: generation.to_owned(),
            effective_origin: "https://api.example.com".to_owned(),
            script_env: vec![("RPI_TEST_USAGE_KEY".to_owned(), "sk-test".to_owned())],
            api_key_env: Some("RPI_TEST_USAGE_KEY".to_owned()),
        }
    }

    #[test]
    fn timeout_clamping() {
        assert_eq!(clamp_usage_timeout(None), 3_000);
        assert_eq!(clamp_usage_timeout(Some(100)), 500);
        assert_eq!(clamp_usage_timeout(Some(1_500)), 1_500);
        assert_eq!(clamp_usage_timeout(Some(120_000)), 60_000);
    }

    #[test]
    fn resolution_priority_and_listing() {
        let temp = TempDir::new();
        let settings = UsageSettings {
            providers: BTreeMap::from([
                ("explicit".to_owned(), explicit("/tmp/explicit.py")),
                ("deepseek".to_owned(), explicit("/tmp/deepseek.py")),
            ]),
            timeout_ms: None,
        };
        let (registry, _agent) = registry(&temp, settings);
        std::fs::write(
            temp.path().join("agent/usage-providers/user-dir.py"),
            "#!/bin/sh\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("agent/usage-providers/deepseek.py"),
            "#!/bin/sh\n",
        )
        .unwrap();
        let plugin_script = temp.path().join("plugin.py");
        std::fs::write(&plugin_script, "#!/bin/sh\n").unwrap();
        registry
            .register("plugin", plugin_script.to_str().unwrap())
            .unwrap();
        registry
            .register("explicit", plugin_script.to_str().unwrap())
            .unwrap();

        assert_eq!(
            registry.list_providers(),
            vec!["deepseek", "explicit", "plugin", "user-dir"]
        );
        assert_eq!(
            registry.resolve("explicit").unwrap().source,
            UsageProviderSource::Explicit
        );
        assert_eq!(
            registry.resolve("user-dir").unwrap().source,
            UsageProviderSource::UserDir
        );
        assert_eq!(
            registry.resolve("plugin").unwrap().source,
            UsageProviderSource::Plugin
        );
        assert_eq!(
            registry.resolve("deepseek").unwrap().source,
            UsageProviderSource::Explicit
        );
        // First-party providers never forward a settings base URL.
        registry
            .register("x", plugin_script.to_str().unwrap())
            .unwrap();
        assert!(registry.resolve("x").unwrap().base_url.is_none());
    }

    #[test]
    fn first_party_spec_drops_the_settings_base_url() {
        let temp = TempDir::new();
        let settings = UsageSettings {
            providers: BTreeMap::from([(
                "deepseek".to_owned(),
                UsageProviderConfig {
                    script: "/tmp/deepseek.py".to_owned(),
                    base_url: Some("https://proxy.example.com".to_owned()),
                    ..UsageProviderConfig::default()
                },
            )]),
            timeout_ms: None,
        };
        let (registry, _agent) = registry(&temp, settings);
        let spec = registry.resolve("deepseek").expect("spec");
        assert!(spec.base_url.is_none());
    }

    #[test]
    fn register_validates_and_resolves_relative_paths() {
        let temp = TempDir::new();
        let (registry, _agent) = registry(&temp, UsageSettings::default());
        assert!(registry.register("", "script.py").is_err());
        assert!(registry.register("x", "  ").is_err());
        registry.register("x", "scripts/usage.py").unwrap();
        let spec = registry.resolve("x").expect("registered");
        assert!(spec.script_path.is_absolute());
        assert!(spec.script_path.ends_with("cwd/scripts/usage.py"));
    }

    // ------------------------------------------------------------------
    // Script execution matrix (unix-only: fixture scripts with shebangs).
    // ------------------------------------------------------------------

    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).expect("write script");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
    }

    #[cfg(unix)]
    fn settings_for(provider: &str, script: &Path) -> UsageSettings {
        UsageSettings {
            providers: BTreeMap::from([(provider.to_owned(), explicit(script.to_str().unwrap()))]),
            timeout_ms: None,
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn fetch_success_caches_and_force_reruns() {
        let temp = TempDir::new();
        let script = temp.path().join("ok.py");
        let marker = temp.path().join("runs");
        write_script(
            &script,
            &format!(
                "#!/bin/sh\necho run >> {}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\",\"displayText\":\"p: ok\"}}'\n",
                marker.display()
            ),
        );
        let (registry, _agent) = registry(&temp, settings_for("p", &script));
        let first = registry.fetch("p", false).await.expect("first fetch");
        assert_eq!(first.get("displayText"), Some(&Value::from("p: ok")));
        // Fresh cache: the script does not run again.
        let second = registry.fetch("p", false).await.expect("cached fetch");
        assert_eq!(second, first);
        // force=true re-runs.
        registry.fetch("p", true).await.expect("forced fetch");
        let runs = std::fs::read_to_string(&marker).unwrap_or_default();
        assert_eq!(runs.lines().count(), 2, "runs: {runs:?}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn detailed_fetch_reports_network_then_cache() {
        let temp = TempDir::new();
        let script = temp.path().join("ok.py");
        let marker = temp.path().join("runs");
        write_script(
            &script,
            &format!(
                "#!/bin/sh\necho run >> {}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\",\"displayText\":\"p: ok\"}}'\n",
                marker.display()
            ),
        );
        let (registry, _agent) = registry(&temp, settings_for("p", &script));
        let first = registry.fetch_detailed("p", false).await;
        assert_eq!(first.source, Some(super::super::UsageSource::Network));
        assert!(!first.stale);
        assert!(first.error.is_none());
        let second = registry.fetch_detailed("p", false).await;
        assert_eq!(second.source, Some(super::super::UsageSource::Cache));
        assert!(!second.stale);
        assert!(second.error.is_none());
        let runs = std::fs::read_to_string(&marker).unwrap_or_default();
        assert_eq!(runs.lines().count(), 1, "runs: {runs:?}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn cached_result_is_call_graph_isolated() {
        let temp = TempDir::new();
        let script = temp.path().join("ok.py");
        let marker = temp.path().join("runs");
        write_script(
            &script,
            &format!(
                "#!/bin/sh\necho run >> {}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\",\"displayText\":\"p: ok\"}}'\n",
                marker.display()
            ),
        );
        let (registry, _agent) = registry(&temp, settings_for("p", &script));
        let miss = registry.cached_result("p").await;
        assert_eq!(
            miss.error.map(|error| error.kind),
            Some(UsageErrorKind::CacheMiss)
        );
        registry.fetch_detailed("p", false).await;
        let hit = registry.cached_result("p").await;
        assert_eq!(hit.source, Some(super::super::UsageSource::Cache));
        let runs = std::fs::read_to_string(&marker).unwrap_or_default();
        assert_eq!(runs.lines().count(), 1, "cached must not run scripts");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn failure_matrix_keeps_last_success() {
        let temp = TempDir::new();
        let script = temp.path().join("flaky.py");
        let mode = temp.path().join("mode");
        write_script(
            &script,
            &format!(
                "#!/bin/sh\nmode=$(cat {mode} 2>/dev/null || echo ok)\ncase \"$mode\" in\n  ok) printf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\",\"displayText\":\"p: good\"}}' ;;\n  exit) echo boom >&2; exit 3 ;;\n  classified) printf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\",\"error\":{{\"kind\":\"unauthorized\"}}}}'; exit 2 ;;\n  json) printf '%s' 'not json' ;;\n  shape) printf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\"}}' ;;\n  type) printf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\",\"displayText\":\"x\",\"used\":\"many\"}}' ;;\n  schema) printf '%s' '{{\"schemaVersion\":9,\"provider\":\"p\",\"displayText\":\"x\"}}' ;;\nesac\n",
                mode = mode.display()
            ),
        );
        let (registry, _agent) = registry(&temp, settings_for("p", &script));
        let ok = registry.fetch("p", false).await.expect("initial success");
        assert_eq!(ok.get("displayText"), Some(&Value::from("p: good")));

        for (failure, kind) in [
            ("exit", UsageErrorKind::Transport),
            ("classified", UsageErrorKind::Unauthorized),
            ("json", UsageErrorKind::ShapeChanged),
            ("shape", UsageErrorKind::ShapeChanged),
            ("type", UsageErrorKind::ShapeChanged),
            ("schema", UsageErrorKind::ShapeChanged),
        ] {
            std::fs::write(&mode, failure).unwrap();
            let result = registry.fetch_detailed("p", true).await;
            let envelope = result
                .envelope
                .unwrap_or_else(|| panic!("{failure}: last success must be kept"));
            assert_eq!(envelope.display_text, "p: good", "{failure}: stale");
            assert_eq!(result.source, Some(super::super::UsageSource::Cache));
            assert!(result.stale, "{failure}: stale flag");
            assert_eq!(
                result.error.map(|error| error.kind),
                Some(kind),
                "{failure}: classified kind"
            );
            assert!(
                registry.cached_envelope("p").is_some(),
                "{failure}: cache kept"
            );
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn script_error_envelope_wins_over_exit_and_missing_display_text() {
        let temp = TempDir::new();
        let script = temp.path().join("classified.py");
        write_script(
            &script,
            "#!/bin/sh\nprintf '%s' '{\"schemaVersion\":1,\"provider\":\"p\",\"error\":{\"kind\":\"rate-limited\"}}'\nexit 2\n",
        );
        let (registry, _agent) = registry(&temp, settings_for("p", &script));
        let result = registry.fetch_detailed("p", true).await;
        assert_eq!(
            result.error.map(|error| error.kind),
            Some(UsageErrorKind::RateLimited)
        );
        assert!(result.envelope.is_none());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn timeout_and_oversize_are_classified_and_keep_last_success() {
        let temp = TempDir::new();
        let ok_script = temp.path().join("ok.py");
        let slow_script = temp.path().join("slow.py");
        let big_script = temp.path().join("big.py");
        write_script(
            &ok_script,
            "#!/bin/sh\nprintf '%s' '{\"schemaVersion\":1,\"provider\":\"p\",\"displayText\":\"p: good\"}'\n",
        );
        write_script(&slow_script, "#!/bin/sh\nsleep 5\n");
        write_script(
            &big_script,
            "#!/bin/sh\nhead -c 70000 /dev/zero | tr '\\0' 'a'\n",
        );

        let slot = Arc::new(Mutex::new(UsageSettings {
            providers: BTreeMap::from([("p".to_owned(), explicit(ok_script.to_str().unwrap()))]),
            timeout_ms: Some(3000),
        }));
        let settings_fn: SettingsFn = {
            let slot = slot.clone();
            Arc::new(move || {
                slot.lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
            })
        };
        let (registry, _agent) = registry_with_settings(
            &temp,
            settings_fn,
            Arc::new(super::super::LegacyCredentialResolver),
        );
        let ok = registry.fetch("p", false).await.expect("success first");
        assert_eq!(ok.get("displayText"), Some(&Value::from("p: good")));

        // Timeout: the stale success is kept with a classified error.
        *slot.lock().unwrap_or_else(|error| error.into_inner()) = UsageSettings {
            providers: BTreeMap::from([("p".to_owned(), explicit(slow_script.to_str().unwrap()))]),
            timeout_ms: Some(500),
        };
        let result = registry.fetch_detailed("p", true).await;
        assert_eq!(
            result.error.map(|error| error.kind),
            Some(UsageErrorKind::Timeout)
        );
        assert_eq!(result.envelope.expect("stale").display_text, "p: good");

        // Oversize stdout: shape-changed.
        *slot.lock().unwrap_or_else(|error| error.into_inner()) = UsageSettings {
            providers: BTreeMap::from([("p".to_owned(), explicit(big_script.to_str().unwrap()))]),
            timeout_ms: Some(3000),
        };
        let result = registry.fetch_detailed("p", true).await;
        assert_eq!(
            result.error.map(|error| error.kind),
            Some(UsageErrorKind::ShapeChanged)
        );
        assert_eq!(result.envelope.expect("stale").display_text, "p: good");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn timeout_without_a_prior_success_answers_the_classified_error() {
        let temp = TempDir::new();
        let slow_script = temp.path().join("slow.py");
        write_script(&slow_script, "#!/bin/sh\nsleep 5\n");
        let mut settings = settings_for("p", &slow_script);
        settings.timeout_ms = Some(500);
        let (registry, _agent) = registry(&temp, settings);
        let result = registry.fetch_detailed("p", true).await;
        assert!(result.envelope.is_none());
        assert_eq!(
            result.error.map(|error| error.kind),
            Some(UsageErrorKind::Timeout)
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn unknown_provider_answers_cache_or_none() {
        let temp = TempDir::new();
        let (registry, _agent) = registry(&temp, UsageSettings::default());
        assert!(registry.fetch("missing", false).await.is_none());
        let result = registry.fetch_detailed("missing", false).await;
        assert_eq!(
            result.error.map(|error| error.kind),
            Some(UsageErrorKind::NoData)
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn missing_script_or_runtime_is_runtime_unavailable() {
        let temp = TempDir::new();
        let missing = temp.path().join("does-not-exist.py");
        let (registry, _agent) = registry(&temp, settings_for("p", &missing));
        let result = registry.fetch_detailed("p", true).await;
        assert_eq!(
            result.error.map(|error| error.kind),
            Some(UsageErrorKind::RuntimeUnavailable)
        );
        assert!(result.envelope.is_none());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn credential_reaches_env_but_not_stdin_or_envelope() {
        let temp = TempDir::new();
        let script = temp.path().join("secret.py");
        let stdin_capture = temp.path().join("stdin.json");
        let expected = "sk-usage-secret-value";
        write_script(
            &script,
            &format!(
                "#!/bin/sh\ncat > {stdin_capture}\nif [ \"$USAGE_TEST_KEY\" != \"{expected}\" ]; then exit 7; fi\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\",\"displayText\":\"p: ok\"}}'\n",
                stdin_capture = stdin_capture.display()
            ),
        );
        // SAFETY: the test manifest owns this variable for its duration.
        unsafe { std::env::set_var("USAGE_TEST_KEY", expected) };
        let settings = UsageSettings {
            providers: BTreeMap::from([(
                "p".to_owned(),
                UsageProviderConfig {
                    script: script.to_str().unwrap().to_owned(),
                    api_key_env: Some("USAGE_TEST_KEY".to_owned()),
                    ..UsageProviderConfig::default()
                },
            )]),
            timeout_ms: None,
        };
        let (registry, _agent) = registry(&temp, settings);
        let envelope = registry.fetch("p", true).await.expect("fetch with key");
        assert_eq!(envelope.get("displayText"), Some(&Value::from("p: ok")));
        // stdin names the variable but never its value.
        let stdin: Value =
            serde_json::from_str(&std::fs::read_to_string(&stdin_capture).unwrap()).unwrap();
        assert_eq!(stdin.get("apiKeyEnv"), Some(&Value::from("USAGE_TEST_KEY")));
        let stdin_text = std::fs::read_to_string(&stdin_capture).unwrap();
        assert!(
            !stdin_text.contains(expected),
            "stdin must not carry the secret value"
        );
        // The cached/returned envelope never carries the secret either.
        let json_text = serde_json::to_string(&envelope).unwrap();
        assert!(
            !json_text.contains(expected),
            "envelope must not carry the secret value"
        );
        unsafe { std::env::remove_var("USAGE_TEST_KEY") };
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn identity_isolates_cache_and_identity_errors_stop_before_scripts() {
        let temp = TempDir::new();
        let script = temp.path().join("identity.py");
        let marker = temp.path().join("runs");
        write_script(
            &script,
            &format!(
                "#!/bin/sh\necho run >> {}\nprintf '%s' \"{{\\\"schemaVersion\\\":1,\\\"provider\\\":\\\"deepseek\\\",\\\"displayText\\\":\\\"$RPI_TEST_USAGE_KEY\\\"}}\"\n",
                marker.display()
            ),
        );
        let settings = UsageSettings {
            providers: BTreeMap::from([(
                "deepseek".to_owned(),
                explicit(script.to_str().unwrap()),
            )]),
            timeout_ms: None,
        };
        let resolver = Arc::new(FakeResolver {
            result: Ok(Some(fake_identity("generation-one"))),
        });
        let settings_fn = {
            let settings = settings.clone();
            Arc::new(move || settings.clone())
        };
        let (registry, _agent) = registry_with_settings(&temp, settings_fn, resolver.clone());

        let first = registry.fetch_detailed("deepseek", true).await;
        assert_eq!(first.envelope.expect("network").display_text, "sk-test");
        let hit = registry.cached_result("deepseek").await;
        assert!(hit.envelope.is_some());

        // A new account identity is a cache miss and no script runs for
        // cached(); an explicit fetch runs with the new generation.
        let mut identity = fake_identity("generation-two");
        identity.script_env = vec![("RPI_TEST_USAGE_KEY".to_owned(), "sk-two".to_owned())];
        let _ = resolver;
        let failing = Arc::new(FakeResolver {
            result: Ok(Some(identity)),
        });
        let settings_fn = {
            let settings = settings.clone();
            Arc::new(move || settings.clone())
        };
        let (registry, _agent) = registry_with_settings(&temp, settings_fn, failing);
        let miss = registry.cached_result("deepseek").await;
        assert_eq!(
            miss.error.map(|error| error.kind),
            Some(UsageErrorKind::CacheMiss),
            "a different identity must not read the old entry"
        );
        let runs_before = std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .count();
        let result = registry.fetch_detailed("deepseek", true).await;
        assert_eq!(result.envelope.expect("network").display_text, "sk-two");
        let runs_after = std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(runs_after, runs_before + 1);

        // An identity failure stops before the script.
        let auth_failure = Arc::new(FakeResolver {
            result: Err(UsageErrorKind::AuthUnavailable),
        });
        let settings_fn = {
            let settings = settings.clone();
            Arc::new(move || settings.clone())
        };
        let (registry, _agent) = registry_with_settings(&temp, settings_fn, auth_failure);
        let runs_before = std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .count();
        let result = registry.fetch_detailed("deepseek", true).await;
        assert_eq!(
            result.error.map(|error| error.kind),
            Some(UsageErrorKind::AuthUnavailable)
        );
        let misses = registry.cached_result("stale-provider").await;
        assert_eq!(
            misses.error.map(|error| error.kind),
            Some(UsageErrorKind::AuthUnavailable)
        );
        let runs_after = std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(runs_after, runs_before, "identity failures run no script");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn offline_serves_same_identity_cache_and_never_runs_scripts() {
        let temp = TempDir::new();
        let script = temp.path().join("offline.py");
        let marker = temp.path().join("runs");
        write_script(
            &script,
            &format!(
                "#!/bin/sh\necho run >> {}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"deepseek\",\"displayText\":\"cached\"}}'\n",
                marker.display()
            ),
        );
        let settings = UsageSettings {
            providers: BTreeMap::from([(
                "deepseek".to_owned(),
                explicit(script.to_str().unwrap()),
            )]),
            timeout_ms: None,
        };
        let settings_fn = {
            let settings = settings.clone();
            Arc::new(move || settings.clone())
        };
        let resolver = Arc::new(FakeResolver {
            result: Ok(Some(fake_identity("offline-gen"))),
        });
        let offline = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let offline_fn = {
            let offline = offline.clone();
            Arc::new(move || offline.load(Ordering::Relaxed))
        };
        let (registry, _agent) = {
            let agent_dir = temp.path().join("agent");
            let cwd = temp.path().join("cwd");
            std::fs::create_dir_all(&agent_dir).expect("agent dir");
            std::fs::create_dir_all(agent_dir.join("usage-providers")).expect("user dir");
            std::fs::create_dir_all(&cwd).expect("cwd");
            (
                UsageProviderRegistry::with_offline(
                    agent_dir,
                    cwd.display().to_string(),
                    settings_fn,
                    resolver,
                    offline_fn,
                ),
                (),
            )
        };
        registry.fetch_detailed("deepseek", true).await;

        offline.store(true, Ordering::Relaxed);
        let result = registry.fetch_detailed("deepseek", true).await;
        let envelope = result.envelope.expect("cached envelope");
        assert_eq!(envelope.display_text, "cached");
        assert_eq!(result.source, Some(super::super::UsageSource::Cache));
        assert!(result.stale);
        assert_eq!(
            result.error.map(|error| error.kind),
            Some(UsageErrorKind::Offline)
        );
        // cached() stays available offline and runs nothing.
        let hit = registry.cached_result("deepseek").await;
        assert!(hit.envelope.is_some());
        let runs = std::fs::read_to_string(&marker).unwrap_or_default();
        assert_eq!(runs.lines().count(), 1, "offline must not run scripts");

        // A provider with no script answers no-data even offline.
        let miss = registry.fetch_detailed("other", true).await;
        assert_eq!(
            miss.error.map(|error| error.kind),
            Some(UsageErrorKind::NoData)
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn offline_wins_over_an_unprovable_identity_and_runs_no_script() {
        let temp = TempDir::new();
        let script = temp.path().join("offline-identity.py");
        let runs = temp.path().join("runs");
        write_script(
            &script,
            &format!(
                "#!/bin/sh\necho run >> {}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"deepseek\",\"displayText\":\"x\"}}'\n",
                runs.display()
            ),
        );
        let settings = UsageSettings {
            providers: BTreeMap::from([(
                "deepseek".to_owned(),
                explicit(script.to_str().unwrap()),
            )]),
            timeout_ms: None,
        };
        let settings_fn = {
            let settings = settings.clone();
            Arc::new(move || settings.clone())
        };
        let resolver = Arc::new(FakeResolver {
            result: Err(UsageErrorKind::AuthUnavailable),
        });
        let offline = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let offline_fn = {
            let offline = offline.clone();
            Arc::new(move || offline.load(Ordering::Relaxed))
        };
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("cwd");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::create_dir_all(agent_dir.join("usage-providers")).expect("user dir");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let registry = UsageProviderRegistry::with_offline(
            agent_dir,
            cwd.display().to_string(),
            settings_fn,
            resolver,
            offline_fn,
        );
        let result = registry.fetch_detailed("deepseek", true).await;
        assert_eq!(
            result.error.map(|error| error.kind),
            Some(UsageErrorKind::Offline)
        );
        assert!(result.envelope.is_none());
        assert!(!runs.exists(), "offline must never run a script");
        let cached = registry.cached_result("deepseek").await;
        assert_eq!(
            cached.error.map(|error| error.kind),
            Some(UsageErrorKind::AuthUnavailable),
            "explicit --cached still fails closed on an unprovable identity"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn re_registration_clears_every_identity() {
        let temp = TempDir::new();
        let script = temp.path().join("reregister.py");
        write_script(
            &script,
            "#!/bin/sh\nprintf '%s' '{\"schemaVersion\":1,\"provider\":\"deepseek\",\"displayText\":\"ok\"}'\n",
        );
        let settings = UsageSettings {
            providers: BTreeMap::new(),
            timeout_ms: None,
        };
        let settings_fn = {
            let settings = settings.clone();
            Arc::new(move || settings.clone())
        };
        let resolver = Arc::new(FakeResolver {
            result: Ok(Some(fake_identity("re-register-gen"))),
        });
        let (registry, _agent) = registry_with_settings(&temp, settings_fn, resolver);
        registry
            .register("deepseek", script.to_str().unwrap())
            .unwrap();
        registry.fetch_detailed("deepseek", true).await;
        assert!(registry.cached_result("deepseek").await.envelope.is_some());
        registry
            .register("deepseek", script.to_str().unwrap())
            .unwrap();
        let miss = registry.cached_result("deepseek").await;
        assert_eq!(
            miss.error.map(|error| error.kind),
            Some(UsageErrorKind::CacheMiss)
        );
    }
}
