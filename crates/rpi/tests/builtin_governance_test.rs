//! V16-13 FR-A end-to-end: the four built-in extensions load from their
//! `builtin:<name>` paths through the real host with the real factories, and
//! every naming surface reports the same `builtin:<name>` string.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rpi::extensions::codemode::{CodemodeMode, CodemodeSettings};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::InlineExtension;
use rpi_test_support::faux::{
    FauxAiProvider, FauxModelDefinition, FauxProvider, FauxProviderOptions,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rpi-builtin-governance-{}-{id}",
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

async fn model_runtime(agent_dir: &Path) -> Arc<rpi::core::model_runtime::ModelRuntime> {
    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            input_limits: None,
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(8192),
        }]),
        ..Default::default()
    });
    let runtime = rpi::core::model_runtime::ModelRuntime::create(
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
    runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider)))
        .await
        .expect("register faux provider");
    runtime
}

/// The upstream registry order (`extensions/index.ts:7-14`).
fn builtin_factories(
    model_runtime: Arc<rpi::core::model_runtime::ModelRuntime>,
) -> Vec<InlineExtension> {
    let settings: rpi::extensions::codemode::CodemodeSettingsFn = Arc::new(|| CodemodeSettings {
        mode: CodemodeMode::On,
        inline_budget: None,
    });
    vec![
        rpi::extensions::llama::inline_extension(),
        rpi::extensions::codemode::inline_extension(settings, model_runtime.clone()),
        rpi::extensions::tool_search::inline_extension(),
        rpi::extensions::mcp::inline_extension(model_runtime),
    ]
}

fn builtin_paths() -> Vec<String> {
    rpi::extensions::builtin_extension_names()
        .into_iter()
        .map(|name| format!("builtin:{name}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn builtin_paths_load_all_four_with_builtin_naming() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let errors = host
        .load_startup_final(
            agent_dir.clone(),
            Vec::new(),
            builtin_paths(),
            builtin_factories(model_runtime(&agent_dir).await),
            false,
            false,
        )
        .await;
    assert!(errors.is_empty(), "{errors:?}");

    let core = host.core();
    let extensions = core.extensions();
    let paths: Vec<&str> = extensions.iter().map(|ext| ext.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "builtin:llama.cpp",
            "builtin:codemode",
            "builtin:tool-search",
            "builtin:mcp"
        ]
    );
    // FR-A R4 (task gate 4): path and sourceInfo carry the same
    // `builtin:<name>` string; the source id is `builtin`.
    for ext in extensions {
        assert!(ext.builtin(), "{}", ext.path);
        assert!(ext.hidden(), "{}", ext.path);
        assert_eq!(ext.source_info.path, ext.path);
        assert_eq!(ext.source_info.source, "builtin");
        assert!(!ext.replaceable() || ext.path != "builtin:llama.cpp");
    }
    assert!(!extensions[0].replaceable(), "llama.cpp is not replaceable");
    for ext in &extensions[1..] {
        assert!(ext.replaceable(), "{}", ext.path);
    }

    // Registered surfaces: llama's command, codemode's inactive tool and
    // tool_search's inactive tool.
    assert!(host.get_command("llama").is_some());
    let tool_names: Vec<String> = host
        .get_all_registered_tools()
        .into_iter()
        .map(|tool| tool.definition.name)
        .collect();
    assert!(
        tool_names.contains(&"codemode".to_string()),
        "{tool_names:?}"
    );
    assert!(
        tool_names.contains(&"tool_search".to_string()),
        "{tool_names:?}"
    );
}

/// A `builtin: true` factory never loads through the inline entry point —
/// only the `builtin:<name>` path loads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn builtin_factories_are_inert_in_the_inline_pass() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let factories = builtin_factories(model_runtime(&agent_dir).await);
    let errors = host.load_inline(&factories).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert!(host.core().extensions().is_empty());
    assert!(host.get_command("llama").is_none());
}

/// The pre-trust pass cannot load built-ins even when `-e builtin:<name>`
/// named one; the final pass does (resource-loader.ts:668-671).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pre_trust_pass_defers_builtins_to_the_final_pass() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let runtime = model_runtime(&agent_dir).await;

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let cli = vec!["builtin:codemode".to_owned()];
    let errors = host
        .load_startup_pre_trust(
            agent_dir.clone(),
            cli.clone(),
            builtin_factories(runtime.clone()),
            false,
        )
        .await;
    assert!(errors.is_empty(), "{errors:?}");
    assert!(host.core().extensions().is_empty());

    let errors = host
        .load_startup_final(
            agent_dir,
            cli,
            Vec::new(),
            builtin_factories(runtime),
            false,
            false,
        )
        .await;
    assert!(errors.is_empty(), "{errors:?}");
    let core = host.core();
    let paths: Vec<&str> = core
        .extensions()
        .iter()
        .map(|ext| ext.path.as_str())
        .collect();
    assert_eq!(paths, ["builtin:codemode"]);
}

/// An unknown `builtin:<name>` path fails with the upstream error text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_builtin_path_reports_unknown_builtin_extension() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let errors = host
        .load_startup_final(
            agent_dir.clone(),
            Vec::new(),
            vec!["builtin:nope".to_owned()],
            builtin_factories(model_runtime(&agent_dir).await),
            false,
            false,
        )
        .await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].error, "Unknown built-in extension: builtin:nope");
}

/// `--no-extensions` drops resolved builtins; an explicit `-e
/// builtin:<name>` still loads (docs/cli.md:196-197).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_extensions_drops_resolved_builtins_but_keeps_explicit_ones() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let runtime = model_runtime(&agent_dir).await;

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let _ = host
        .load_startup_final(
            agent_dir.clone(),
            vec!["builtin:codemode".to_owned()],
            vec!["builtin:mcp".to_owned()],
            builtin_factories(runtime.clone()),
            false,
            true,
        )
        .await;
    let core = host.core();
    let paths: Vec<&str> = core
        .extensions()
        .iter()
        .map(|ext| ext.path.as_str())
        .collect();
    assert_eq!(paths, ["builtin:codemode"]);
    assert_eq!(
        host.core().extensions()[0].source_info.path,
        "builtin:codemode"
    );
}
