//! Tests for `ExtensionLoader` — discovery, load order, dedupe, error
//! isolation, and the cwd+generation factory cache, anchored to
//! `external/pi/packages/coding-agent/src/core/extensions/loader.ts` and
//! `resource-loader.ts` @ 2efa728.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rpi_ext_host::api::ExtensionRuntime;
use rpi_ext_host::loader::{
    discover_extensions_in_dir, ExtensionFactory, ExtensionLoader, FactoryCache, InlineExtension,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique temp directory; removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rpi-ext-host-loader-{tag}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.0.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&path, content).expect("write fixture file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ok_factory() -> ExtensionFactory {
    Arc::new(|_api| Box::pin(async { Ok(()) }))
}

// ---------------------------------------------------------------------------
// Discovery (loader.ts:581-668 adapted to wasm entries)
// ---------------------------------------------------------------------------

#[test]
fn loader_discovers_one_level_wasm_files_and_dir_entries() {
    let temp = TempDir::new("discover");
    // 1. direct .wasm file (loader.ts:649-653).
    let direct = temp.write("direct.wasm", "wasm");
    // non-wasm files are ignored.
    temp.write("notes.txt", "no");
    // 2. subdirectory with index.wasm (loader.ts:613-621).
    let indexed = temp.write("indexed/index.wasm", "wasm");
    // 3. subdirectory with rpi-extension.json manifest naming its wasm.
    let manifested = temp.write("packed/dist/main.wasm", "wasm");
    temp.write(
        "packed/rpi-extension.json",
        r#"{"name": "packed", "wasm": "dist/main.wasm", "rpiAbi": 1}"#,
    );
    // manifest naming a missing wasm falls through to index.wasm (none) →
    // no entries.
    temp.write("broken/rpi-extension.json", r#"{"wasm": "missing.wasm"}"#);
    // no recursion beyond one level (loader.ts:634).
    temp.write("nested/inner/deep.wasm", "wasm");
    // empty subdirectory → no entries.
    std::fs::create_dir_all(temp.path().join("empty")).expect("mkdir empty");

    let mut discovered: Vec<PathBuf> = discover_extensions_in_dir(temp.path())
        .into_iter()
        .map(|p| std::fs::canonicalize(p).expect("canonicalize"))
        .collect();
    discovered.sort();

    let mut expected = vec![direct, indexed, manifested]
        .into_iter()
        .map(|p| std::fs::canonicalize(p).expect("canonicalize"))
        .collect::<Vec<_>>();
    expected.sort();

    assert_eq!(discovered, expected);
}

#[test]
fn loader_discovery_missing_dir_yields_nothing() {
    // loader.ts:637-639.
    let missing = Path::new("/definitely/not/a/real/dir");
    assert!(discover_extensions_in_dir(missing).is_empty());
}

// ---------------------------------------------------------------------------
// Inline factories: naming, hidden, error isolation
// (resource-loader.ts:892-913, loader.ts:454-480, 485-498)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loader_inline_factories_get_numbered_and_named_paths() {
    let runtime = ExtensionRuntime::new();
    let loader = ExtensionLoader::new(runtime);
    let inline = vec![
        InlineExtension::Anonymous(ok_factory()),
        InlineExtension::Named {
            name: "gate".to_owned(),
            factory: ok_factory(),
            hidden: true,
        },
        InlineExtension::Anonymous(ok_factory()),
    ];

    let result = loader.load_inline(&inline, Path::new("/cwd")).await;
    assert!(result.errors.is_empty());
    let summary: Vec<(String, bool)> = result
        .extensions
        .iter()
        .map(|ext| (ext.path.clone(), ext.hidden()))
        .collect();
    // Anonymous factories are 1-based numbered; `hidden` only applies to
    // named ones (resource-loader.ts:902-906).
    assert_eq!(
        summary,
        [
            ("<inline:1>".to_owned(), false),
            ("<inline:gate>".to_owned(), true),
            ("<inline:3>".to_owned(), false),
        ]
    );
    // Synthetic source info for `<...>` paths (loader.ts:434-441).
    assert_eq!(result.extensions[0].source_info.source, "inline");
    assert_eq!(result.extensions[0].source_info.base_dir, None);
}

#[tokio::test]
async fn loader_isolates_factory_errors_and_continues() {
    let runtime = ExtensionRuntime::new();
    let loader = ExtensionLoader::new(runtime);
    let failing: ExtensionFactory =
        Arc::new(|_api| Box::pin(async { Err("factory exploded".to_owned()) }));
    let inline = vec![
        InlineExtension::Anonymous(ok_factory()),
        InlineExtension::Anonymous(failing),
        InlineExtension::Anonymous(ok_factory()),
    ];

    let result = loader.load_inline(&inline, Path::new("/cwd")).await;
    assert_eq!(result.extensions.len(), 2);
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.errors[0].path, "<inline:2>");
    assert_eq!(result.errors[0].error, "factory exploded");
}

// ---------------------------------------------------------------------------
// Path loading: W6 placeholder flows through error isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loader_path_entries_fail_with_isolated_read_errors() {
    // W6: path entries load for real; a missing file is an isolated load
    // error (loader.ts:454-480), never a panic and never blocks siblings.
    let runtime = ExtensionRuntime::new();
    let loader = ExtensionLoader::new(runtime);
    let paths = vec![PathBuf::from("/x/a.wasm"), PathBuf::from("/x/b.wasm")];

    let result = loader.load_paths(&paths, Path::new("/x")).await;
    assert!(result.extensions.is_empty());
    assert_eq!(result.errors.len(), 2);
    assert!(result.errors[0].error.contains("Failed to load extension"));
}

// ---------------------------------------------------------------------------
// Load order + canonical dedupe (resource-loader.ts:494-514,
// loader.ts:673-721)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loader_load_order_cli_then_local_then_global_then_packages_then_inline() {
    let temp = TempDir::new("order");
    let cwd = temp.path().join("project");
    let agent_dir = temp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("mkdir cwd");
    std::fs::create_dir_all(&agent_dir).expect("mkdir agent");

    let cli = temp.write("cli/flag.wasm", "wasm");
    let local = temp.write("project/.rpi/extensions/local.wasm", "wasm");
    let global = temp.write("agent/extensions/global.wasm", "wasm");
    let package = temp.write("pkg/pkg.wasm", "wasm");

    let runtime = ExtensionRuntime::new();
    let loader = ExtensionLoader::new(runtime);
    let config = rpi_ext_host::loader::DiscoverConfig {
        cwd: cwd.clone(),
        agent_dir,
        // CLI paths resolve relative to cwd (loader.ts:704).
        cli_paths: vec![cli.to_string_lossy().into_owned()],
        // Duplicate of the discovered local file via a different spelling —
        // canonical dedupe keeps the first occurrence (loader.ts:684-692).
        package_paths: vec![
            package.to_string_lossy().into_owned(),
            local.to_string_lossy().into_owned(),
        ],
        inline: vec![InlineExtension::Anonymous(ok_factory())],
        include_project_local: true,
        no_extensions: false,
    };

    let result = loader.discover_and_load(&config).await;

    // Path loads fail in W1 (wasm lands in W6) but the error list preserves
    // the load order; the inline extension loads last and succeeds.
    let order: Vec<String> = result.errors.iter().map(|e| e.path.clone()).collect();
    let canonical = |p: &Path| {
        std::fs::canonicalize(p)
            .expect("canonicalize")
            .to_string_lossy()
            .into_owned()
    };
    assert_eq!(
        order,
        [
            cli.to_string_lossy().into_owned(),
            local.to_string_lossy().into_owned(),
            global.to_string_lossy().into_owned(),
            package.to_string_lossy().into_owned(),
        ],
        "expected CLI → local → global → package order; canonical keys: {:?}",
        [
            canonical(&cli),
            canonical(&local),
            canonical(&global),
            canonical(&package)
        ]
    );
    // The duplicated local path appears exactly once, and the inline
    // extension trails.
    assert_eq!(result.extensions.len(), 1);
    assert_eq!(result.extensions[0].path, "<inline:1>");
}

// ---------------------------------------------------------------------------
// Factory cache (loader.ts:142-164, 395-428)
// ---------------------------------------------------------------------------

#[test]
fn loader_cache_invalidates_on_clear_and_cwd_change() {
    let cache = FactoryCache::new();

    let token_a = cache.use_cwd("/a");
    cache.insert(PathBuf::from("/x/e.wasm"), ok_factory(), &token_a);
    assert!(cache.get(Path::new("/x/e.wasm"), &token_a).is_some());

    // clear() bumps the generation: the old token no longer validates
    // (loader.ts:151-155, 395-401).
    cache.clear();
    assert!(cache.get(Path::new("/x/e.wasm"), &token_a).is_none());
    // A stale token cannot insert either (loader.ts:424-426).
    cache.insert(PathBuf::from("/x/f.wasm"), ok_factory(), &token_a);
    assert!(cache.get(Path::new("/x/f.wasm"), &token_a).is_none());

    // New token under the same cwd works again.
    let token_a2 = cache.use_cwd("/a");
    assert_ne!(token_a.generation, token_a2.generation);
    cache.insert(PathBuf::from("/x/e.wasm"), ok_factory(), &token_a2);
    assert!(cache.get(Path::new("/x/e.wasm"), &token_a2).is_some());

    // A cwd switch clears the cache (loader.ts:157-164).
    let token_b = cache.use_cwd("/b");
    assert!(cache.get(Path::new("/x/e.wasm"), &token_b).is_none());
    // ... and the previous cwd's token is stale.
    assert!(cache.get(Path::new("/x/e.wasm"), &token_a2).is_none());
}
