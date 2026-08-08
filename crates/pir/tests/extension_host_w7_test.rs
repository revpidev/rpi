//! T15 W7 tests: extension install management e2e (local wasm package
//! install/list/config enable-disable/remove + startup-chain loading),
//! the switchSession cross-cwd trust selector (ADR-0006/D-044), and the
//! llama built-in extension registration path (D-047).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pir_ext_host::host::NativeExtensionHost;
use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("pir-ext-w7-{tag}-{}-{id}", std::process::id()));
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

/// 最小 gate guest（on tool_call → block "w7-block"）。
const GATE_WAT: &str = r#"
(module
  (import "pir" "pir_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "pir_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "pir_dealloc") (param i32 i32) nop)
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
  (func (export "pir_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (return (call $pack (i32.const 512))))
  (func (export "pir_dispatch") (param i32 i32) (result i64)
    (return (call $pack (i32.const 1024))))
  (data (i32.const 16) "{\"call\":\"on\",\"args\":{\"event\":\"tool_call\"}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 1024) "{\"block\":true,\"reason\":\"w7-block\"}\00")
)
"#;

/// 写一个 wasm 扩展包目录（pir-extension.json + dist/guest.wasm）。
fn write_wasm_package(root: &Path, name: &str) -> PathBuf {
    let pkg = root.join(name);
    std::fs::create_dir_all(pkg.join("dist")).expect("dist");
    std::fs::write(pkg.join("dist/guest.wasm"), GATE_WAT).expect("guest");
    std::fs::write(
        pkg.join("pir-extension.json"),
        format!(
            r#"{{"name":"{name}","version":"1.2.3","description":"W7 test package","wasm":"dist/guest.wasm","capabilities":[],"pirAbi":1}}"#
        ),
    )
    .expect("manifest");
    pkg
}

fn resolve_extensions(cwd: &Path, agent_dir: &Path) -> Vec<(PathBuf, bool)> {
    let settings_manager = pir::core::settings_manager::SettingsManager::create(
        cwd,
        Some(agent_dir),
        pir::core::settings_manager::SettingsManagerCreateOptions {
            project_trusted: true,
        },
    );
    let resolved = pir::core::package_manager::DefaultPackageManager::with_options(
        pir::core::package_manager::PackageManagerOptions {
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
            settings_manager,
            runner: None,
            offline: None,
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
// 安装管理 e2e（本地 wasm 包）
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w7_install_list_disable_enable_remove_wasm_package() {
    let tmp = TempDir::new("pkg");
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let _pkg = write_wasm_package(&cwd, "gate-pkg");

    // install（本地路径，manifest 校验经 resolve_extension_entries）。
    let parsed = pir::cli::package_command::parse_package_command(&[
        "install".to_owned(),
        "./gate-pkg".to_owned(),
    ])
    .expect("parse install");
    let code = pir::cli::package_command::run_package_command_in(&parsed, &cwd, &agent_dir, None);
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

    // resolve → 包目录条目启用（上游目录形语义；wasm 入口在 host 侧解析）。
    let entries = resolve_extensions(&cwd, &agent_dir);
    assert!(
        entries
            .iter()
            .any(|(path, enabled)| *enabled && path.ends_with("gate-pkg")),
        "resolved entries: {entries:?}"
    );

    // list 输出含该包。
    let parsed =
        pir::cli::package_command::parse_package_command(&["list".to_owned()]).expect("parse list");
    let code = pir::cli::package_command::run_package_command_in(&parsed, &cwd, &agent_dir, None);
    assert_eq!(code, 0);

    // config（禁用/启用）：config TUI 的 toggle 落盘为 settings 对象形
    // 条目 + `-pattern` 过滤（config-selector.ts:580-637）；集成层直接
    // 写该线格式验证 discovery 过滤语义。
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
    // 重新启用（回到字符串形 = 无过滤）。
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
    let parsed = pir::cli::package_command::parse_package_command(&[
        "remove".to_owned(),
        "./gate-pkg".to_owned(),
    ])
    .expect("parse remove");
    let code = pir::cli::package_command::run_package_command_in(&parsed, &cwd, &agent_dir, None);
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

/// 安装后的 wasm 包经启动链路（resolve → package paths → host 加载）生效。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w7_installed_wasm_package_loads_and_blocks() {
    let tmp = TempDir::new("load");
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    write_wasm_package(&cwd, "gate-pkg");

    let parsed = pir::cli::package_command::parse_package_command(&[
        "install".to_owned(),
        "./gate-pkg".to_owned(),
    ])
    .expect("parse");
    assert_eq!(
        pir::cli::package_command::run_package_command_in(&parsed, &cwd, &agent_dir, None),
        0
    );

    // 启动链路：resolve → enabled extension paths → host final load。
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
// switchSession 异 cwd 信任选择器（ADR-0006/D-044）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w7_async_trust_selector_resolves_and_persists() {
    let tmp = TempDir::new("trust");
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(cwd.join(".pir")).expect(".pir");
    std::fs::write(cwd.join(".pir/settings.json"), "{}").expect("settings");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let trust_store = pir::core::trust_manager::ProjectTrustStore::new(&agent_dir);

    // 选择 "Trust"（第一个选项）→ trusted + 持久化。
    let mut context = pir::core::trust_manager::ProjectTrustContext {
        has_ui: true,
        select: None,
        select_async: Some(Arc::new(|_title, options| {
            Box::pin(async move { options.into_iter().next() })
        })),
    };
    let trusted = pir::core::trust_manager::resolve_project_trusted_async(
        &cwd,
        &trust_store,
        None,
        pir::core::trust_manager::DefaultProjectTrust::Ask,
        None,
        &mut context,
    )
    .await
    .expect("resolve");
    assert!(trusted, "async selector trust");
    assert_eq!(trust_store.get(&cwd).expect("store"), Some(true));

    // 取消（None）→ 不信任。
    let cwd2 = tmp.path().join("other");
    std::fs::create_dir_all(cwd2.join(".pir")).expect(".pir");
    std::fs::write(cwd2.join(".pir/settings.json"), "{}").expect("settings");
    let mut context = pir::core::trust_manager::ProjectTrustContext {
        has_ui: true,
        select: None,
        select_async: Some(Arc::new(|_title, _options| Box::pin(async move { None }))),
    };
    let trusted = pir::core::trust_manager::resolve_project_trusted_async(
        &cwd2,
        &trust_store,
        None,
        pir::core::trust_manager::DefaultProjectTrust::Ask,
        None,
        &mut context,
    )
    .await
    .expect("resolve");
    assert!(!trusted, "cancelled selection stays untrusted");
}

/// 全链路：switch_session 到异 cwd，工厂注入的 async 选择器被消费。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w7_switch_session_cross_cwd_uses_async_trust_selector() {
    use pir_test_support::faux::{
        FauxAiProvider, FauxModelDefinition, FauxProvider, FauxProviderOptions,
    };

    let tmp = TempDir::new("switch");
    let cwd_a = tmp.path().join("a");
    let cwd_b = tmp.path().join("b");
    let agent_dir = tmp.path().join("agent");
    for dir in [&cwd_a, &cwd_b, &agent_dir] {
        std::fs::create_dir_all(dir).expect("dir");
    }
    // B 有信任门控资源。
    std::fs::create_dir_all(cwd_b.join(".pir")).expect("b/.pir");
    std::fs::write(cwd_b.join(".pir/settings.json"), "{}").expect("b settings");
    // B 的会话文件：手写最小合法 JSONL（header 带 cwd）。
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
        pir::core::trust_manager::ProjectTrustContext {
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

    // 初始 runtime（A cwd）。
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
    let model_runtime = pir::core::model_runtime::ModelRuntime::create(
        pir::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: pir::core::model_runtime::ModelsPathInput::Path(
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
        move |options: pir::core::agent_session_runtime::CreateRuntimeOptions| {
            let agent_dir = agent_dir.clone();
            let model_runtime = model_runtime.clone();
            let model = model.clone();
            Box::pin(async move {
                let services = pir::core::agent_session_services::create_agent_session_services(
                    pir::core::agent_session_services::CreateAgentSessionServicesOptions {
                        cwd: options.cwd.clone(),
                        agent_dir: Some(agent_dir.clone()),
                        settings_manager: None,
                        model_runtime: Some(model_runtime.clone()),
                        extension_flag_values: Vec::new(),
                        resource_loader_options: None,
                    },
                )
                .await?;
                // app.rs 的异 cwd 信任判定（ADR-0006 关闭路径）：async
                // 选择器决议 → 可信则翻转并 reload。
                if let Some(mut trust_context) = options.project_trust_context {
                    let trust_store = pir::core::trust_manager::ProjectTrustStore::new(&agent_dir);
                    let trusted = pir::core::trust_manager::resolve_project_trusted_async(
                        &options.cwd,
                        &trust_store,
                        None,
                        pir::core::trust_manager::DefaultProjectTrust::Ask,
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
                let created = pir::sdk::create_agent_session(pir::sdk::CreateAgentSessionOptions {
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
                pir::core::extension_actions::bind_session_actions(&host, &created.session).await;
                Ok(
                    pir::core::agent_session_runtime::CreateAgentSessionRuntimeResult {
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
                        pir::core::agent_session_runtime::CreateAgentSessionRuntimeResult,
                        pir::error::PirError,
                    >,
                >
        }
    };

    let session_manager = Arc::new(Mutex::new(
        pir::core::session_manager::SessionManager::in_memory(
            Some(&cwd_a),
            pir::core::session_manager::NewSessionOptions::default(),
        )
        .expect("session"),
    ));
    let mut runtime = pir::core::agent_session_runtime::create_agent_session_runtime(
        Arc::new(factory_for_runtime),
        pir::core::agent_session_runtime::CreateRuntimeOptions {
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
// llama 内置扩展（D-047）：经真宿主注册后的命令分发路径
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn w7_llama_command_dispatches_through_prompt_path() {
    use pir_test_support::faux::{
        FauxAiProvider, FauxModelDefinition, FauxProvider, FauxProviderOptions,
    };

    let tmp = TempDir::new("llama");
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let errors = host
        .load_inline(&[pir::extensions::llama::inline_extension()])
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
    let model_runtime = pir::core::model_runtime::ModelRuntime::create(
        pir::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: pir::core::model_runtime::ModelsPathInput::Path(
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
    // 模拟 app.rs 的 pre-session 冲刷：llama provider 进 model runtime。
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

    let services = pir::core::agent_session_services::create_agent_session_services(
        pir::core::agent_session_services::CreateAgentSessionServicesOptions {
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
        pir::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            pir::core::session_manager::NewSessionOptions::default(),
        )
        .expect("session"),
    ));
    let created = pir::sdk::create_agent_session(pir::sdk::CreateAgentSessionOptions {
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
    pir::core::extension_actions::bind_session_actions(&host, &created.session).await;

    // /llama 经 prompt 的扩展命令路径执行：print 模式（非 TUI）下 handler
    // notify 并返回，不触发 LLM 调用。
    created
        .session
        .prompt("/llama", pir::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt dispatch");
    assert_eq!(provider.pending_response_count(), 0, "no LLM call");
}
