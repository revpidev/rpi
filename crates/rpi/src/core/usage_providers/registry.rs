//! Provider resolution, governance, and the fetch pipeline (V16-05 FR-A
//! R1/R3/R4/R5).
//!
//! Resolution priority is explicit `usage.providers` settings > user
//! directory (`<agentDir>/usage-providers/<provider>.py`) > plugin
//! registration. Fetches are serialized (concurrency 1), bounded by the
//! configured timeout, and cached; any failure keeps the last success and
//! never surfaces an error to the UI.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::UsageEnvelope;
use super::cache::{DEFAULT_USAGE_CACHE_TTL_MS, UsageCache};
use super::script::{UsageScriptContext, UsageScriptEnv, execute_usage_script, usage_api_key_env};

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
    registrations: Mutex<BTreeMap<String, PathBuf>>,
    cache: UsageCache,
    /// Serializes script execution (concurrency 1, R6.1.3).
    execution_lock: tokio::sync::Mutex<()>,
}

impl UsageProviderRegistry {
    pub fn new(agent_dir: PathBuf, cwd: String, settings: SettingsFn) -> Self {
        Self {
            agent_dir,
            cwd,
            settings,
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
    /// resolves against the session cwd.
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
        Ok(())
    }

    /// Resolve one provider against the priority chain.
    pub fn resolve(&self, provider: &str) -> Option<UsageProviderSpec> {
        let settings = (self.settings)();
        if let Some(config) = settings.providers.get(provider) {
            return Some(UsageProviderSpec {
                provider: provider.to_owned(),
                script_path: self.resolve_script_path(&config.script),
                source: UsageProviderSource::Explicit,
                base_url: config.base_url.clone(),
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

    /// Fetch one provider's usage envelope JSON (V16-05 FR-A R4/R5).
    ///
    /// - a TTL-fresh cache hit returns without running the script when
    ///   `force` is false;
    /// - an unknown provider answers the last cached success (or `None`);
    /// - failures answer the last successful envelope (or `None`) — never an
    ///   error.
    pub async fn fetch(&self, provider: &str, force: bool) -> Option<Value> {
        if !force && let Some(cached) = self.cache.get_fresh(provider) {
            return Some(cached.to_json());
        }
        let Some(spec) = self.resolve(provider) else {
            return self
                .cache
                .get_last(provider)
                .map(|envelope| envelope.to_json());
        };
        let _guard = self.execution_lock.lock().await;
        // A queued fetch for the same provider may have refreshed the cache
        // while this one waited for its turn.
        if !force && let Some(cached) = self.cache.get_fresh(provider) {
            return Some(cached.to_json());
        }
        let timeout_ms = clamp_usage_timeout((self.settings)().timeout_ms);
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
        };
        match execute_usage_script(&spec.script_path, &self.cwd, &context, timeout_ms, &env).await {
            Ok(envelope) => {
                self.cache.store(provider, envelope.clone());
                Some(envelope.to_json())
            }
            Err(reason) => {
                // Silent degrade (R6.1.3): keep the last success; the reason
                // is debug-level diagnostics only and never carries stdout,
                // stderr, or environment values.
                tracing::debug!(provider = %provider, reason = %reason, "usage provider fetch failed");
                self.cache
                    .get_last(provider)
                    .map(|envelope| envelope.to_json())
            }
        }
    }

    /// The last successful envelope (cache read for diagnostics/tests).
    pub fn cached(&self, provider: &str) -> Option<UsageEnvelope> {
        self.cache.get_last(provider)
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
        registry_with_settings(temp, settings)
    }

    fn registry_with_settings(
        temp: &TempDir,
        settings: SettingsFn,
    ) -> (UsageProviderRegistry, PathBuf) {
        let agent_dir = temp.path().join("agent");
        let cwd = temp.path().join("cwd");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::create_dir_all(agent_dir.join("usage-providers")).expect("user dir");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let registry =
            UsageProviderRegistry::new(agent_dir.clone(), cwd.display().to_string(), settings);
        (registry, agent_dir)
    }

    fn explicit(path: &str) -> UsageProviderConfig {
        UsageProviderConfig {
            script: path.to_owned(),
            ..UsageProviderConfig::default()
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
                ("shared".to_owned(), explicit("/tmp/shared.py")),
            ]),
            timeout_ms: None,
        };
        // The helper creates the agent dir (and `usage-providers/`), so
        // scripts are written after it runs.
        let (registry, _agent) = registry(&temp, settings);
        std::fs::write(
            temp.path().join("agent/usage-providers/user-dir.py"),
            "#!/bin/sh\n",
        )
        .unwrap();
        // A user-dir script named `shared` must lose to the explicit entry.
        std::fs::write(
            temp.path().join("agent/usage-providers/shared.py"),
            "#!/bin/sh\n",
        )
        .unwrap();
        let plugin_script = temp.path().join("plugin.py");
        std::fs::write(&plugin_script, "#!/bin/sh\n").unwrap();
        registry
            .register("plugin", plugin_script.to_str().unwrap())
            .unwrap();
        registry
            .register("shared", plugin_script.to_str().unwrap())
            .unwrap();

        assert_eq!(
            registry.list_providers(),
            vec!["explicit", "plugin", "shared", "user-dir"]
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
            registry.resolve("shared").unwrap().source,
            UsageProviderSource::Explicit
        );
        assert!(registry.resolve("missing").is_none());
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
    async fn failure_matrix_keeps_last_success() {
        let temp = TempDir::new();
        let script = temp.path().join("flaky.py");
        let mode = temp.path().join("mode");
        write_script(
            &script,
            &format!(
                "#!/bin/sh\nmode=$(cat {mode} 2>/dev/null || echo ok)\ncase \"$mode\" in\n  ok) printf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\",\"displayText\":\"p: good\"}}' ;;\n  exit) echo boom >&2; exit 3 ;;\n  json) printf '%s' 'not json' ;;\n  shape) printf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\"}}' ;;\n  type) printf '%s' '{{\"schemaVersion\":1,\"provider\":\"p\",\"displayText\":\"x\",\"used\":\"many\"}}' ;;\n  schema) printf '%s' '{{\"schemaVersion\":9,\"provider\":\"p\",\"displayText\":\"x\"}}' ;;\nesac\n",
                mode = mode.display()
            ),
        );
        let (registry, _agent) = registry(&temp, settings_for("p", &script));
        let ok = registry.fetch("p", false).await.expect("initial success");
        assert_eq!(ok.get("displayText"), Some(&Value::from("p: good")));

        for failure in ["exit", "json", "shape", "type", "schema"] {
            std::fs::write(&mode, failure).unwrap();
            let result = registry
                .fetch("p", true)
                .await
                .unwrap_or_else(|| panic!("{failure}: last success must be kept"));
            assert_eq!(
                result.get("displayText"),
                Some(&Value::from("p: good")),
                "{failure}: stale success returned"
            );
            assert!(registry.cached("p").is_some(), "{failure}: cache kept");
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn timeout_and_oversize_fail_but_keep_last_success() {
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

        // A mutable settings slot lets the test swap the script without
        // re-registering (which would be a different resolution tier).
        let slot = Arc::new(Mutex::new(UsageSettings {
            providers: BTreeMap::from([("p".to_owned(), explicit(ok_script.to_str().unwrap()))]),
            // 500ms lower bound keeps the timeout test fast.
            timeout_ms: Some(500),
        }));
        let settings_fn: SettingsFn = {
            let slot = slot.clone();
            Arc::new(move || {
                slot.lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
            })
        };
        let (registry, _agent) = registry_with_settings(&temp, settings_fn);
        let ok = registry.fetch("p", false).await.expect("success first");
        assert_eq!(ok.get("displayText"), Some(&Value::from("p: good")));

        // Timeout: the stale success is kept for both forced and non-forced
        // fetches.
        *slot.lock().unwrap_or_else(|error| error.into_inner()) = UsageSettings {
            providers: BTreeMap::from([("p".to_owned(), explicit(slow_script.to_str().unwrap()))]),
            timeout_ms: Some(500),
        };
        let result = registry.fetch("p", true).await.expect("kept success");
        assert_eq!(result.get("displayText"), Some(&Value::from("p: good")));

        // Oversize stdout: same keep-last-success behavior.
        *slot.lock().unwrap_or_else(|error| error.into_inner()) = UsageSettings {
            providers: BTreeMap::from([("p".to_owned(), explicit(big_script.to_str().unwrap()))]),
            timeout_ms: Some(500),
        };
        let result = registry.fetch("p", true).await.expect("kept success");
        assert_eq!(result.get("displayText"), Some(&Value::from("p: good")));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn timeout_without_a_prior_success_answers_none() {
        let temp = TempDir::new();
        let slow_script = temp.path().join("slow.py");
        write_script(&slow_script, "#!/bin/sh\nsleep 5\n");
        let mut settings = settings_for("p", &slow_script);
        settings.timeout_ms = Some(500);
        let (registry, _agent) = registry(&temp, settings);
        assert!(registry.fetch("p", true).await.is_none());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn unknown_provider_answers_cache_or_none() {
        let temp = TempDir::new();
        let (registry, _agent) = registry(&temp, UsageSettings::default());
        assert!(registry.fetch("missing", false).await.is_none());
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
        // The env var is present in the host process (the child inherits it;
        // the framework also injects it explicitly when the name resolves).
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
        // The script exited 0 only because the env carried the value.
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
}
