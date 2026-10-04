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
}

/// Build the host/session pipeline. `pre_register` entries are registered by
/// the extension factory (pre-bind => queued); `settings_json` is written to
/// the global settings before the services are created.
async fn fixture(pre_register: Vec<(String, String)>, settings_json: Option<&str>) -> Fixture {
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

    let provider = FauxProvider::new(FauxProviderOptions::default());
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

/// Unknown providers answer `null` (the wire arm maps `None`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_provider_answers_none() {
    let fixture = fixture(Vec::new(), None).await;
    let result = fixture.api.usage_fetch("missing", false).await.unwrap();
    assert!(result.is_none());
    let _: Option<Value> = result;
}
