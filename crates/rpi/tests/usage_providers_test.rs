//! V16-05 FR-A integration: `ctx.usage.*` over a real host + session +
//! extension pipeline — pre-bind registration queue, resolution priority,
//! fetch/cache/force semantics, and the user script directory.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi_ext_host::api::ExtensionApi;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_test_support::faux::{FauxAiProvider, FauxProvider, FauxProviderOptions};
use serde_json::{Value, json};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rpi-usage-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_script(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn envelope_script(provider: &str, marker: Option<&std::path::Path>) -> String {
    let marker = marker
        .map(|path| format!("echo run >> {}\n", path.display()))
        .unwrap_or_default();
    format!(
        "#!/bin/sh\n{marker}printf '%s' '{{\"schemaVersion\":1,\"provider\":\"{provider}\",\"displayText\":\"{provider}: ok\"}}'\n"
    )
}

struct Fixture {
    api: ExtensionApi,
    session: rpi::core::agent_session::AgentSession,
    agent_dir: PathBuf,
    _tmp: TempDir,
    /// Serializes tests in this file: process-wide provider env vars and the
    /// live `RPI_OFFLINE` gate are shared state.
    _test_lock: tokio::sync::MutexGuard<'static, ()>,
}

static V18_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Extra fixture inputs for the V18-10 identity tests.
#[derive(Default)]
struct FixtureOptions {
    /// Faux model provider id (defaults to `deepseek`).
    model_provider: Option<String>,
    /// Written verbatim to `<agentDir>/auth.json`.
    auth_json: Option<String>,
    /// Written verbatim to `<agentDir>/models.json`.
    models_json: Option<String>,
}

/// Build the host/session pipeline. `pre_register` entries are registered by
/// the extension factory (pre-bind => queued); `settings_json` is written to
/// the global settings before the services are created.
async fn fixture(pre_register: Vec<(String, String)>, settings_json: Option<&str>) -> Fixture {
    fixture_with(pre_register, settings_json, FixtureOptions::default()).await
}

async fn fixture_with(
    pre_register: Vec<(String, String)>,
    settings_json: Option<&str>,
    options: FixtureOptions,
) -> Fixture {
    let test_lock = V18_TEST_LOCK.lock().await;
    let slot: Arc<Mutex<Option<ExtensionApi>>> = Arc::new(Mutex::new(None));
    let slot_for_factory = slot.clone();
    let factory: ExtensionFactory = Arc::new(move |api: ExtensionApi| {
        *slot_for_factory
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(api.clone());
        let pre_register = pre_register.clone();
        Box::pin(async move {
            for (provider, script_path) in pre_register {
                api.usage_register(&provider, &script_path)
                    .expect("pre-bind usage register");
            }
            Ok(())
        })
    });

    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(agent_dir.join("usage-providers")).expect("user dir");
    if let Some(settings) = settings_json {
        std::fs::write(agent_dir.join("settings.json"), settings).expect("settings.json");
    }
    if let Some(auth) = options.auth_json {
        std::fs::write(agent_dir.join("auth.json"), auth).expect("auth.json");
    }
    if let Some(models) = options.models_json {
        std::fs::write(agent_dir.join("models.json"), models).expect("models.json");
    }

    let provider = FauxProvider::new(FauxProviderOptions {
        provider: Some(
            options
                .model_provider
                .unwrap_or_else(|| "deepseek".to_owned()),
        ),
        ..Default::default()
    });
    let model = provider.get_model(None).expect("faux model");
    let model_runtime = rpi::core::model_runtime::ModelRuntime::create(
        rpi::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: rpi::core::model_runtime::ModelsPathInput::Path(
                agent_dir.join("models.json"),
            ),
            ..Default::default()
        },
    )
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider)))
        .await
        .expect("register faux provider");

    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");

    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session"),
    ));

    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    let errors = host
        .load_inline(&[InlineExtension::Anonymous(factory)])
        .await;
    assert!(errors.is_empty(), "load errors: {errors:?}");

    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir.clone()),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(host.clone()),
        ..Default::default()
    })
    .await
    .expect("create session");

    rpi::core::extension_actions::bind_session_actions(&host, &created.session).await;

    let api = slot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
        .expect("factory ran");
    Fixture {
        api,
        session: created.session,
        agent_dir,
        _tmp: tmp,
        _test_lock: test_lock,
    }
}

/// Pre-bind registration flushes on bind: the provider lists and fetches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_bind_registration_flushes_and_fetches() {
    let tmp = TempDir::new();
    let script = tmp.path().join("plugin.py");
    write_script(&script, &envelope_script("plugin-p", None));
    let fixture = fixture(
        vec![("plugin-p".to_owned(), script.display().to_string())],
        None,
    )
    .await;

    assert!(
        fixture
            .api
            .usage_list_providers()
            .unwrap()
            .contains(&"plugin-p".to_owned()),
        "queued registration must flush on bind"
    );
    let envelope = fixture
        .api
        .usage_fetch("plugin-p", false)
        .await
        .unwrap()
        .expect("envelope");
    assert_eq!(envelope.get("displayText"), Some(&json!("plugin-p: ok")));
}

/// Post-bind `usage_register` + cache hit + `force` re-run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_bind_registration_caches_until_forced() {
    let tmp = TempDir::new();
    let script = tmp.path().join("post.py");
    let marker = tmp.path().join("runs");
    write_script(&script, &envelope_script("post-p", Some(&marker)));
    let fixture = fixture(Vec::new(), None).await;

    fixture
        .api
        .usage_register("post-p", &script.display().to_string())
        .unwrap();
    let first = fixture
        .api
        .usage_fetch("post-p", false)
        .await
        .unwrap()
        .expect("first fetch");
    let cached = fixture
        .api
        .usage_fetch("post-p", false)
        .await
        .unwrap()
        .expect("cached fetch");
    assert_eq!(cached, first);
    fixture
        .api
        .usage_fetch("post-p", true)
        .await
        .unwrap()
        .expect("forced fetch");

    let runs = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        runs.lines().count(),
        2,
        "fresh cache must not re-run; force must: {runs:?}"
    );
}

/// Resolution priority through the API: explicit settings > user directory >
/// plugin registration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_settings_and_user_dir_resolve() {
    let tmp = TempDir::new();
    let explicit_script = tmp.path().join("explicit.py");
    write_script(&explicit_script, &envelope_script("explicit-p", None));
    let settings = format!(
        "{{\"usage\":{{\"providers\":{{\"explicit-p\":\"{}\"}},\"timeoutMs\":2000}}}}",
        explicit_script.display()
    );
    let fixture = fixture(Vec::new(), Some(&settings)).await;
    // The typed settings reader exposes the explicit provider map (host
    // consumption face).
    let usage_settings = fixture
        .session
        .settings_manager(|manager| manager.get_usage_settings());
    assert!(usage_settings.providers.contains_key("explicit-p"));
    assert_eq!(usage_settings.timeout_ms, Some(2000));

    // User-directory script dropped after the session started.
    write_script(
        &fixture.agent_dir.join("usage-providers/user-p.py"),
        &envelope_script("user-p", None),
    );

    let providers = fixture.api.usage_list_providers().unwrap();
    assert!(
        providers.contains(&"explicit-p".to_owned()),
        "{providers:?}"
    );
    assert!(providers.contains(&"user-p".to_owned()), "{providers:?}");

    let envelope = fixture
        .api
        .usage_fetch("explicit-p", false)
        .await
        .unwrap()
        .expect("explicit fetch");
    assert_eq!(envelope.get("displayText"), Some(&json!("explicit-p: ok")));
    let envelope = fixture
        .api
        .usage_fetch("user-p", false)
        .await
        .unwrap()
        .expect("user-dir fetch");
    assert_eq!(envelope.get("displayText"), Some(&json!("user-p: ok")));
}

/// Round-2: the host injects the enforced `usage.timeoutMs` into the
/// script environment so a script's own degradation path can finish
/// before the process is killed (the built-in scripts default to 8s).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scripts_receive_the_enforced_timeout_budget() {
    let tmp = TempDir::new();
    let script = tmp.path().join("timeout.sh");
    write_script(
        &script,
        r#"#!/bin/sh
printf '%s' "{\"schemaVersion\":1,\"provider\":\"timeout-p\",\"displayText\":\"$RPI_USAGE_TIMEOUT_MS\"}"
"#,
    );
    let fixture = fixture(
        vec![("timeout-p".to_owned(), script.display().to_string())],
        Some(r#"{"usage":{"timeoutMs":1234}}"#),
    )
    .await;
    let envelope = fixture
        .api
        .usage_fetch("timeout-p", true)
        .await
        .unwrap()
        .expect("envelope");
    assert_eq!(
        envelope.get("displayText"),
        Some(&json!("1234")),
        "the script must see the same budget the host enforces"
    );
}

/// Unknown providers answer `null` (the wire arm maps `None`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_provider_answers_none() {
    let fixture = fixture(Vec::new(), None).await;
    let result = fixture.api.usage_fetch("missing", false).await.unwrap();
    assert!(result.is_none());
    let _: Option<Value> = result;
}

// ---------------------------------------------------------------------------
// V18-10: real credential identity (API key / OAuth), cache isolation,
// origin mismatch and the live offline gate.
// ---------------------------------------------------------------------------

/// Saves and restores process-wide provider env vars. The caller must already
/// hold the fixture's serialisation lock (every test that creates a fixture
/// does), and should declare this guard AFTER the fixture so it restores the
/// variables before the fixture releases the lock.
struct EnvVars {
    previous: Vec<(&'static str, Option<String>)>,
}

impl EnvVars {
    fn set(pairs: &[(&'static str, Option<&str>)]) -> Self {
        let mut previous = Vec::new();
        for (name, value) in pairs {
            previous.push((*name, std::env::var(name).ok()));
            match value {
                Some(value) => rpi_test_env::set_var(name, value),
                None => rpi_test_env::remove_var(name),
            }
        }
        Self { previous }
    }
}

impl Drop for EnvVars {
    fn drop(&mut self) {
        for (name, value) in self.previous.drain(..) {
            match value {
                Some(value) => rpi_test_env::set_var(name, value),
                None => rpi_test_env::remove_var(name),
            }
        }
    }
}

fn usage_settings(providers: &[(&str, &std::path::Path)]) -> String {
    let mut entries = serde_json::Map::new();
    for (provider, script) in providers {
        entries.insert(
            (*provider).to_owned(),
            json!({"script": script.display().to_string()}),
        );
    }
    serde_json::to_string(&json!({"usage": {"providers": entries}})).expect("settings json")
}

fn fetch_result(value: Value) -> rpi_ext_host::usage::UsageFetchResult {
    serde_json::from_value(value).expect("UsageFetchResult json")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_key_identity_reaches_the_script_and_isolates_the_cache() {
    let tmp = TempDir::new();
    let script = tmp.path().join("deepseek.py");
    let stdin_path = tmp.path().join("stdin.json");
    let key_path = tmp.path().join("key.txt");
    let runs = tmp.path().join("runs");
    write_script(
        &script,
        &format!(
            "#!/bin/sh\ncat > {stdin}\nprintf '%s' \"$DEEPSEEK_API_KEY\" > {key}\necho run >> {runs}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"deepseek\",\"displayText\":\"ok\"}}'\n",
            stdin = stdin_path.display(),
            key = key_path.display(),
            runs = runs.display()
        ),
    );
    let settings = usage_settings(&[("deepseek", &script)]);
    let fixture = fixture_with(Vec::new(), Some(&settings), FixtureOptions::default()).await;
    let _env = EnvVars::set(&[
        ("DEEPSEEK_API_KEY", Some("sk-env-one")),
        ("RPI_OFFLINE", None),
    ]);

    let result = fetch_result(
        fixture
            .api
            .usage_fetch_detailed("deepseek", true)
            .await
            .expect("fetchDetailed"),
    );
    assert_eq!(
        result.source,
        Some(rpi_ext_host::usage::UsageSource::Network)
    );
    assert!(!result.stale);
    assert!(result.error.is_none());
    assert_eq!(result.envelope.expect("envelope").display_text, "ok");
    let stdin: Value =
        serde_json::from_str(&std::fs::read_to_string(&stdin_path).expect("stdin capture"))
            .expect("stdin json");
    assert_eq!(stdin["apiKeyEnv"], "DEEPSEEK_API_KEY");
    assert_eq!(stdin["credentialKind"], "api-key");
    assert_eq!(
        std::fs::read_to_string(&key_path).expect("env key capture"),
        "sk-env-one"
    );

    let cached = fetch_result(fixture.api.usage_cached("deepseek").await.expect("cached"));
    assert_eq!(cached.source, Some(rpi_ext_host::usage::UsageSource::Cache));
    assert!(!cached.stale);
    assert!(cached.envelope.is_some());

    // A changed key is a different identity: cached() is a miss and runs no
    // script.
    let runs_before = std::fs::read_to_string(&runs)
        .unwrap_or_default()
        .lines()
        .count();
    rpi_test_env::set_var("DEEPSEEK_API_KEY", "sk-env-two");
    let miss = fetch_result(fixture.api.usage_cached("deepseek").await.expect("cached"));
    assert_eq!(
        miss.error.map(|error| error.kind),
        Some(rpi_ext_host::usage::UsageErrorKind::CacheMiss)
    );
    assert!(miss.envelope.is_none());
    let runs_after = std::fs::read_to_string(&runs)
        .unwrap_or_default()
        .lines()
        .count();
    assert_eq!(runs_after, runs_before, "cached must not run scripts");

    // The live offline gate stops the script and serves the same-identity
    // cache marked stale with an `offline` error.
    rpi_test_env::set_var("DEEPSEEK_API_KEY", "sk-env-one");
    rpi_test_env::set_var("RPI_OFFLINE", "1");
    let result = fetch_result(
        fixture
            .api
            .usage_fetch_detailed("deepseek", true)
            .await
            .expect("fetchDetailed"),
    );
    rpi_test_env::remove_var("RPI_OFFLINE");
    assert_eq!(result.source, Some(rpi_ext_host::usage::UsageSource::Cache));
    assert!(result.stale);
    assert_eq!(
        result.error.map(|error| error.kind),
        Some(rpi_ext_host::usage::UsageErrorKind::Offline)
    );
    assert_eq!(
        std::fs::read_to_string(&runs)
            .unwrap_or_default()
            .lines()
            .count(),
        runs_after,
        "offline must not run scripts"
    );
}

fn oauth_auth_json(anthropic_expires: i64, codex_expires: i64) -> String {
    json!({
        "anthropic": {
            "type": "oauth",
            "refresh": "refresh-anthropic",
            "access": "access-anthropic",
            "expires": anthropic_expires,
        },
        "openai-codex": {
            "type": "oauth",
            "refresh": "refresh-codex",
            "access": "access-codex",
            "expires": codex_expires,
            "accountId": "acc-codex",
        },
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oauth_identity_reaches_the_script_without_refresh_or_account_leaks() {
    let future = chrono::Utc::now().timestamp_millis() + 3_600_000;
    let tmp = TempDir::new();
    let anthropic_script = tmp.path().join("anthropic.py");
    let anthropic_stdin = tmp.path().join("anthropic-stdin.json");
    let anthropic_access = tmp.path().join("anthropic-access.txt");
    write_script(
        &anthropic_script,
        &format!(
            "#!/bin/sh\ncat > {stdin}\nprintf '%s' \"$RPI_USAGE_OAUTH_ACCESS\" > {access}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"anthropic\",\"displayText\":\"ok\"}}'\n",
            stdin = anthropic_stdin.display(),
            access = anthropic_access.display()
        ),
    );
    let codex_script = tmp.path().join("codex.py");
    let codex_stdin = tmp.path().join("codex-stdin.json");
    let codex_access = tmp.path().join("codex-access.txt");
    let codex_account = tmp.path().join("codex-account.txt");
    write_script(
        &codex_script,
        &format!(
            "#!/bin/sh\ncat > {stdin}\nprintf '%s' \"$RPI_USAGE_OAUTH_ACCESS\" > {access}\nprintf '%s' \"$RPI_USAGE_OAUTH_ACCOUNT_ID\" > {account}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"openai-codex\",\"displayText\":\"ok\"}}'\n",
            stdin = codex_stdin.display(),
            access = codex_access.display(),
            account = codex_account.display()
        ),
    );
    let settings = usage_settings(&[
        ("anthropic", &anthropic_script),
        ("openai-codex", &codex_script),
    ]);
    let fixture = fixture_with(
        Vec::new(),
        Some(&settings),
        FixtureOptions {
            auth_json: Some(oauth_auth_json(future, future)),
            ..FixtureOptions::default()
        },
    )
    .await;

    let result = fetch_result(
        fixture
            .api
            .usage_fetch_detailed("anthropic", true)
            .await
            .expect("anthropic"),
    );
    assert!(result.error.is_none(), "{:?}", result.error);
    assert!(result.envelope.is_some());
    let stdin: Value =
        serde_json::from_str(&std::fs::read_to_string(&anthropic_stdin).expect("stdin"))
            .expect("stdin json");
    assert_eq!(stdin["credentialKind"], "oauth");
    assert_eq!(
        std::fs::read_to_string(&anthropic_access).expect("access"),
        "access-anthropic"
    );

    let result = fetch_result(
        fixture
            .api
            .usage_fetch_detailed("openai-codex", true)
            .await
            .expect("codex"),
    );
    assert!(result.error.is_none(), "{:?}", result.error);
    let stdin: Value = serde_json::from_str(&std::fs::read_to_string(&codex_stdin).expect("stdin"))
        .expect("stdin json");
    assert_eq!(stdin["credentialKind"], "oauth");
    assert!(stdin.get("apiKeyEnv").is_none());
    assert_eq!(
        std::fs::read_to_string(&codex_access).expect("access"),
        "access-codex"
    );
    assert_eq!(
        std::fs::read_to_string(&codex_account).expect("account"),
        "acc-codex"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_oauth_and_custom_origin_fail_before_the_script() {
    let tmp = TempDir::new();
    let script = tmp.path().join("anthropic.py");
    let runs = tmp.path().join("runs");
    write_script(
        &script,
        &format!(
            "#!/bin/sh\necho run >> {}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"anthropic\",\"displayText\":\"ok\"}}'\n",
            runs.display()
        ),
    );
    let settings = usage_settings(&[("anthropic", &script)]);

    // Expired access token (past epoch ms).
    let expired = fixture_with(
        Vec::new(),
        Some(&settings),
        FixtureOptions {
            auth_json: Some(oauth_auth_json(1, 1)),
            ..FixtureOptions::default()
        },
    )
    .await;
    let result = fetch_result(
        expired
            .api
            .usage_fetch_detailed("anthropic", true)
            .await
            .expect("expired"),
    );
    assert_eq!(
        result.error.map(|error| error.kind),
        Some(rpi_ext_host::usage::UsageErrorKind::AuthUnavailable)
    );
    assert!(!runs.exists(), "expired OAuth must fail before the script");
    drop(expired);

    // Custom/proxy model origin cannot inherit the subscription identity.
    let future = chrono::Utc::now().timestamp_millis() + 3_600_000;
    let proxy = fixture_with(
        Vec::new(),
        Some(&settings),
        FixtureOptions {
            auth_json: Some(oauth_auth_json(future, future)),
            models_json: Some(
                json!({"providers": {"anthropic": {"baseUrl": "https://proxy.example.com/v1"}}})
                    .to_string(),
            ),
            ..FixtureOptions::default()
        },
    )
    .await;
    let result = fetch_result(
        proxy
            .api
            .usage_fetch_detailed("anthropic", true)
            .await
            .expect("proxy"),
    );
    assert_eq!(
        result.error.map(|error| error.kind),
        Some(rpi_ext_host::usage::UsageErrorKind::OriginMismatch)
    );
    assert!(
        !runs.exists(),
        "origin mismatch must fail before the script"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stored_api_key_and_env_before_store_ordering() {
    let tmp = TempDir::new();
    let deepseek_script = tmp.path().join("deepseek.py");
    let deepseek_key = tmp.path().join("deepseek-key.txt");
    write_script(
        &deepseek_script,
        &format!(
            "#!/bin/sh\ncat > /dev/null\nprintf '%s' \"$DEEPSEEK_API_KEY\" > {}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"deepseek\",\"displayText\":\"ok\"}}'\n",
            deepseek_key.display()
        ),
    );
    let glm_script = tmp.path().join("glm.py");
    let glm_key = tmp.path().join("glm-key.txt");
    write_script(
        &glm_script,
        &format!(
            "#!/bin/sh\ncat > /dev/null\nprintf '%s' \"$ZAI_CODING_CN_API_KEY\" > {}\nprintf '%s' '{{\"schemaVersion\":1,\"provider\":\"glm-coding-plan\",\"displayText\":\"ok\"}}'\n",
            glm_key.display()
        ),
    );
    let settings = usage_settings(&[
        ("deepseek", &deepseek_script),
        ("glm-coding-plan", &glm_script),
    ]);
    let fixture = fixture_with(
        Vec::new(),
        Some(&settings),
        FixtureOptions {
            auth_json: Some(
                json!({
                    "deepseek": {"type": "api_key", "key": "sk-stored"},
                    "zai": {"type": "api_key", "key": "sk-stored-zai"},
                })
                .to_string(),
            ),
            ..FixtureOptions::default()
        },
    )
    .await;
    let _env = EnvVars::set(&[
        ("DEEPSEEK_API_KEY", None),
        ("ZAI_API_KEY", None),
        ("GLM_API_KEY", None),
        ("ZAI_CODING_CN_API_KEY", Some("sk-cn-env")),
    ]);

    let result = fetch_result(
        fixture
            .api
            .usage_fetch_detailed("deepseek", true)
            .await
            .expect("deepseek"),
    );
    assert!(result.error.is_none(), "{:?}", result.error);
    assert_eq!(
        std::fs::read_to_string(&deepseek_key).expect("deepseek key"),
        "sk-stored",
        "auth.json api_key is the third credential source"
    );

    let result = fetch_result(
        fixture
            .api
            .usage_fetch_detailed("glm-coding-plan", true)
            .await
            .expect("glm"),
    );
    assert!(result.error.is_none(), "{:?}", result.error);
    assert_eq!(
        std::fs::read_to_string(&glm_key).expect("glm key"),
        "sk-cn-env",
        "a conventional env key wins over any stored api_key"
    );
}
