//! T15 W7 tests: extension install management e2e (local wasm package
//! install/list/config enable-disable/remove + startup-chain loading),
//! the switchSession cross-cwd trust selector (ADR-0006/D-044), and the
//! llama built-in extension registration path (D-047).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rpi_ext_host::host::NativeExtensionHost;
use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rpi-ext-w7-{tag}-{}-{id}", std::process::id()));
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

/// Minimal gate guest (on tool_call → block "w7-block").
const GATE_WAT: &str = r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param i32 i32) nop)
  (func $strlen (param $ptr i32) (result i32)
    (local $n i32)
    (block $done
      (loop $scan
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $ptr) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $scan)))
    (local.get $n))
  (func $pack (param $ptr i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (call $strlen (local.get $ptr)))))
  (func (export "rpi_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (return (call $pack (i32.const 512))))
  (func (export "rpi_dispatch") (param i32 i32) (result i64)
    (return (call $pack (i32.const 1024))))
  (data (i32.const 16) "{\"call\":\"on\",\"args\":{\"event\":\"tool_call\"}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 1024) "{\"block\":true,\"reason\":\"w7-block\"}\00")
)
"#;

/// Writes a wasm extension package directory (rpi-extension.json + dist/guest.wasm).
fn write_wasm_package(root: &Path, name: &str) -> PathBuf {
    let pkg = root.join(name);
    std::fs::create_dir_all(pkg.join("dist")).expect("dist");
    std::fs::write(pkg.join("dist/guest.wasm"), GATE_WAT).expect("guest");
    std::fs::write(
        pkg.join("rpi-extension.json"),
        format!(
            r#"{{"name":"{name}","version":"1.2.3","description":"W7 test package","wasm":"dist/guest.wasm","capabilities":[],"rpiAbi":1}}"#
        ),
    )
    .expect("manifest");
    pkg
}

fn resolve_extensions(cwd: &Path, agent_dir: &Path) -> Vec<(PathBuf, bool)> {
    let settings_manager = rpi::core::settings_manager::SettingsManager::create(
        cwd,
        Some(agent_dir),
        rpi::core::settings_manager::SettingsManagerCreateOptions {
            project_trusted: true,
        },
    );
    let resolved = rpi::core::package_manager::DefaultPackageManager::with_options(
        rpi::core::package_manager::PackageManagerOptions {
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            settings_manager,
            runner: None,
            offline: None,
            registry: None,
        },
    )
    .resolve(None)
    .expect("resolve");
    resolved
        .extensions
        .iter()
        .map(|entry| (entry.path.clone(), entry.enabled))
        .collect()
}

// ---------------------------------------------------------------------------
// Install management e2e (local wasm package)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w7_install_list_disable_enable_remove_wasm_package() {
    let tmp = TempDir::new("pkg");
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let _pkg = write_wasm_package(&cwd, "gate-pkg");

    // install (local path; manifest validation via resolve_extension_entries).
    let parsed = rpi::cli::package_command::parse_package_command(&[
        "install".to_owned(),
        "./gate-pkg".to_owned(),
    ])
    .expect("parse install");
    let code = rpi::cli::package_command::run_package_command_in(&parsed, &cwd, &agent_dir, None);
    assert_eq!(code, 0, "install failed");
    let settings: Value = serde_json::from_str(
        &std::fs::read_to_string(agent_dir.join("settings.json")).expect("settings"),
    )
    .expect("settings json");
    assert!(
        settings["packages"][0]
            .as_str()
            .unwrap()
            .contains("gate-pkg"),
        "settings: {settings}"
    );

    // resolve → package-dir entries enabled (upstream directory-form semantics; the
    // wasm entry is resolved on the host side).
    let entries = resolve_extensions(&cwd, &agent_dir);
    assert!(
        entries
            .iter()
            .any(|(path, enabled)| *enabled && path.ends_with("gate-pkg")),
        "resolved entries: {entries:?}"
    );

    // list output includes the package.
    let parsed =
        rpi::cli::package_command::parse_package_command(&["list".to_owned()]).expect("parse list");
    let code = rpi::cli::package_command::run_package_command_in(&parsed, &cwd, &agent_dir, None);
    assert_eq!(code, 0);

    // config (disable/enable): the config TUI's toggle persists as a settings
    // object-form entry + `-pattern` filtering (config-selector.ts:580-637); the
    // integration layer writes that wire format directly to verify discovery-filter
    // semantics.
    let settings_path = agent_dir.join("settings.json");
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("settings"))
            .expect("json");
    let source = settings["packages"][0].as_str().unwrap().to_owned();
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&json!({
            "packages": [{"source": source, "extensions": ["-**"]}]
        }))
        .expect("write"),
    )
    .expect("write settings");
    let entries = resolve_extensions(&cwd, &agent_dir);
    assert!(
        entries
            .iter()
            .all(|(path, enabled)| !(path.ends_with("gate-pkg") && *enabled)),
        "disabled after override (absent or disabled): {entries:?}"
    );
    // Re-enable (back to string form = no filtering).
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&json!({ "packages": [source] })).expect("write"),
    )
    .expect("write settings");
    let entries = resolve_extensions(&cwd, &agent_dir);
    assert!(
        entries
            .iter()
            .find(|(path, _)| path.ends_with("gate-pkg"))
            .map(|(_, enabled)| *enabled)
            .unwrap_or(false),
        "enabled after override removal: {entries:?}"
    );

    // remove。
    let parsed = rpi::cli::package_command::parse_package_command(&[
        "remove".to_owned(),
        "./gate-pkg".to_owned(),
    ])
    .expect("parse remove");
    let code = rpi::cli::package_command::run_package_command_in(&parsed, &cwd, &agent_dir, None);
    assert_eq!(code, 0, "remove failed");
    let settings: Value = serde_json::from_str(
        &std::fs::read_to_string(agent_dir.join("settings.json")).expect("settings"),
    )
    .expect("settings json");
    assert!(
        settings["packages"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "packages empty after remove: {settings}"
    );
}

/// An installed wasm package takes effect through the startup chain (resolve → package
/// paths → host load).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w7_installed_wasm_package_loads_and_blocks() {
    let tmp = TempDir::new("load");
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    write_wasm_package(&cwd, "gate-pkg");

    let parsed = rpi::cli::package_command::parse_package_command(&[
        "install".to_owned(),
        "./gate-pkg".to_owned(),
    ])
    .expect("parse");
    assert_eq!(
        rpi::cli::package_command::run_package_command_in(&parsed, &cwd, &agent_dir, None),
        0
    );

    // Startup chain: resolve → enabled extension paths → host final load.
    let package_paths: Vec<String> = resolve_extensions(&cwd, &agent_dir)
        .into_iter()
        .filter(|(_, enabled)| *enabled)
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(package_paths.len(), 1);
    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let errors = host
        .load_startup_final(
            agent_dir,
            Vec::new(),
            package_paths,
            Vec::new(),
            true,
            false,
        )
        .await;
    assert!(errors.is_empty(), "{errors:?}");

    let result = host
        .emit_tool_call(json!({
            "type": "tool_call", "toolCallId": "c1", "toolName": "read",
            "input": {"path": "/etc/passwd"}
        }))
        .await
        .expect("no host error")
        .expect("gate blocks");
    assert_eq!(result["reason"], "w7-block");
}

// ---------------------------------------------------------------------------
// switchSession cross-cwd trust selector (ADR-0006/D-044)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w7_async_trust_selector_resolves_and_persists() {
    let tmp = TempDir::new("trust");
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(cwd.join(".rpi")).expect(".rpi");
    std::fs::write(cwd.join(".rpi/settings.json"), "{}").expect("settings");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let trust_store = rpi::core::trust_manager::ProjectTrustStore::new(&agent_dir);

    // Choose "Trust" (first option) → trusted + persisted.
    let mut context = rpi::core::trust_manager::ProjectTrustContext {
        has_ui: true,
        select: None,
        select_async: Some(Arc::new(|_title, options| {
            Box::pin(async move { options.into_iter().next() })
        })),
    };
    let trusted = rpi::core::trust_manager::resolve_project_trusted_async(
        &cwd,
        &trust_store,
        None,
        rpi::core::trust_manager::DefaultProjectTrust::Ask,
        None,
        &mut context,
    )
    .await
    .expect("resolve");
    assert!(trusted, "async selector trust");
    assert_eq!(trust_store.get(&cwd).expect("store"), Some(true));

    // Cancel (None) → not trusted.
    let cwd2 = tmp.path().join("other");
    std::fs::create_dir_all(cwd2.join(".rpi")).expect(".rpi");
    std::fs::write(cwd2.join(".rpi/settings.json"), "{}").expect("settings");
    let mut context = rpi::core::trust_manager::ProjectTrustContext {
        has_ui: true,
        select: None,
        select_async: Some(Arc::new(|_title, _options| Box::pin(async move { None }))),
    };
    let trusted = rpi::core::trust_manager::resolve_project_trusted_async(
        &cwd2,
        &trust_store,
        None,
        rpi::core::trust_manager::DefaultProjectTrust::Ask,
        None,
        &mut context,
    )
    .await
    .expect("resolve");
    assert!(!trusted, "cancelled selection stays untrusted");
}

/// Full chain: switch_session to a different cwd consumes the factory-injected async
/// selector.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w7_switch_session_cross_cwd_uses_async_trust_selector() {
    use rpi_test_support::faux::{
        FauxAiProvider, FauxModelDefinition, FauxProvider, FauxProviderOptions,
    };

    let tmp = TempDir::new("switch");
    let cwd_a = tmp.path().join("a");
    let cwd_b = tmp.path().join("b");
    let agent_dir = tmp.path().join("agent");
    for dir in [&cwd_a, &cwd_b, &agent_dir] {
        std::fs::create_dir_all(dir).expect("dir");
    }
    // B has trust-gated resources.
    std::fs::create_dir_all(cwd_b.join(".rpi")).expect("b/.rpi");
    std::fs::write(cwd_b.join(".rpi/settings.json"), "{}").expect("b settings");
    // B's session file: a handwritten minimal valid JSONL (header carries cwd).
    let b_session_file = tmp.path().join("b-sessions/b.jsonl");
    std::fs::create_dir_all(b_session_file.parent().unwrap()).expect("b dir");
    std::fs::write(
        &b_session_file,
        format!(
            "{}\n",
            serde_json::json!({
                "type": "session",
                "version": 3,
                "id": "019f0000-0000-7000-8000-0000000000b1",
                "timestamp": "2026-08-09T00:00:00.000Z",
                "cwd": cwd_b.to_string_lossy(),
            })
        ),
    )
    .expect("write b session");

    let selector_calls = Arc::new(AtomicU64::new(0));
    let calls_outer = selector_calls.clone();
    let factory = Arc::new(move |_cwd: &Path| {
        let calls = calls_outer.clone();
        rpi::core::trust_manager::ProjectTrustContext {
            has_ui: true,
            select: None,
            select_async: Some(Arc::new(move |_title, options: Vec<String>| {
                let calls = calls.clone();
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    options.into_iter().next()
                })
            })),
        }
    });

    // Initial runtime (A cwd).
    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        }]),
        ..Default::default()
    });
    let model = provider.get_model(None).expect("model");
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
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("register faux");

    let factory_for_runtime = {
        let agent_dir = agent_dir.clone();
        move |options: rpi::core::agent_session_runtime::CreateRuntimeOptions| {
            let agent_dir = agent_dir.clone();
            let model_runtime = model_runtime.clone();
            let model = model.clone();
            Box::pin(async move {
                let services = rpi::core::agent_session_services::create_agent_session_services(
                    rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
                        cwd: options.cwd.clone(),
                        agent_dir: Some(agent_dir.clone()),
                        settings_manager: None,
                        model_runtime: Some(model_runtime.clone()),
                        extension_flag_values: Vec::new(),
                        resource_loader_options: None,
                    },
                )
                .await?;
                // app.rs's cross-cwd trust decision (ADR-0006 closed path): the async
                // selector's resolution → if trusted, flip and reload.
                if let Some(mut trust_context) = options.project_trust_context {
                    let trust_store = rpi::core::trust_manager::ProjectTrustStore::new(&agent_dir);
                    let trusted = rpi::core::trust_manager::resolve_project_trusted_async(
                        &options.cwd,
                        &trust_store,
                        None,
                        rpi::core::trust_manager::DefaultProjectTrust::Ask,
                        None,
                        &mut trust_context,
                    )
                    .await?;
                    if trusted {
                        let mut loader = services
                            .resource_loader
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        loader.settings_manager_mut().set_project_trusted(true);
                        loader.set_project_trusted(true);
                        loader.reload();
                    }
                }
                let host = Arc::new(NativeExtensionHost::new(&options.cwd.to_string_lossy()));
                let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
                    cwd: Some(options.cwd.clone()),
                    agent_dir: Some(agent_dir.clone()),
                    model_runtime: Some(model_runtime.clone()),
                    model: Some(model.clone()),
                    services: Some(services.clone()),
                    session_manager: Some(options.session_manager),
                    session_start_event: options.session_start_event,
                    extension_host: Some(host.clone()),
                    ..Default::default()
                })
                .await?;
                rpi::core::extension_actions::bind_session_actions(&host, &created.session).await;
                Ok(
                    rpi::core::agent_session_runtime::CreateAgentSessionRuntimeResult {
                        session: created.session,
                        services,
                        diagnostics: Vec::new(),
                        model_fallback_message: None,
                    },
                )
            })
                as futures::future::BoxFuture<
                    'static,
                    Result<
                        rpi::core::agent_session_runtime::CreateAgentSessionRuntimeResult,
                        rpi::error::RpiError,
                    >,
                >
        }
    };

    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd_a),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("session"),
    ));
    let mut runtime = rpi::core::agent_session_runtime::create_agent_session_runtime(
        Arc::new(factory_for_runtime),
        rpi::core::agent_session_runtime::CreateRuntimeOptions {
            cwd: cwd_a.clone(),
            agent_dir: agent_dir.clone(),
            session_manager,
            session_start_event: None,
            project_trust_context: None,
        },
    )
    .await
    .expect("runtime");

    let cancelled = runtime
        .switch_session(&b_session_file.to_string_lossy(), None, None, Some(factory))
        .await
        .expect("switch");
    assert!(!cancelled);
    assert_eq!(
        selector_calls.load(Ordering::Relaxed),
        1,
        "async selector consumed"
    );
    assert_eq!(runtime.session().cwd(), cwd_b.to_string_lossy());
    assert!(
        runtime
            .session()
            .settings_manager(|s| s.is_project_trusted()),
        "new session trusted via selector"
    );
}

// ---------------------------------------------------------------------------
// llama built-in extension (D-047): command dispatch path after registering through
// the real host
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w7_llama_command_dispatches_through_prompt_path() {
    use rpi_test_support::faux::{
        FauxAiProvider, FauxModelDefinition, FauxProvider, FauxProviderOptions,
    };

    let tmp = TempDir::new("llama");
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let errors = host
        .load_inline(&[rpi::extensions::llama::inline_extension()])
        .await;
    assert!(errors.is_empty(), "{errors:?}");
    assert!(host.get_command("llama").is_some(), "/llama registered");

    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        }]),
        ..Default::default()
    });
    provider.set_responses(vec![]);
    let model = provider.get_model(None).expect("model");
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
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("faux");
    // Mimic app.rs's pre-session flush: llama provider enters the model runtime.
    let host = Arc::new(host);
    for registration in host.runtime().take_pending_native_provider_registrations() {
        model_runtime
            .register_native_provider(registration.provider)
            .await
            .expect("llama provider");
    }
    assert!(
        model_runtime.get_model("llama.cpp", "any").is_none(),
        "llama has no static models until a router is configured"
    );

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
        .expect("session"),
    ));
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
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

    // /llama executes through the prompt's extension-command path: in print mode
    // (non-TUI) the handler notifies and returns without triggering an LLM call.
    created
        .session
        .prompt("/llama", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt dispatch");
    assert_eq!(provider.pending_response_count(), 0, "no LLM call");
}
