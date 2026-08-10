//! T15 W7 regression tests for the pre-trust → final(reuse) → `/reload`
//! loading semantics (aligned with `loadFinalExtensionSet`,
//! resource-loader.ts:520-571):
//! - inline (built-in) extensions must survive `/reload` on the trust path
//!   (reload replays the spec and re-runs inline factories,
//!   resource-loader.ts:360-363);
//! - reuse orders extensions by the FINAL path list (pre-trust path
//!   extensions outside the final list are dropped — with `--no-extensions`
//!   no global extension may linger);
//! - pre-trust itself honors `--no-extensions` (only CLI `-e` + inline load,
//!   resource-loader.ts:500-504).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::InlineExtension;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Unique temp directory; removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rpi-ext-host-reload-{tag}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
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

/// A minimal wasm guest subscribing to `session_start` (WAT text; the
/// loader's `wasmtime::Module::new` accepts the text format).
const WATCH_GUEST_WAT: &str = r#"
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
  (data (i32.const 16) "{\"call\":\"on\",\"args\":{\"event\":\"session_start\"}}\00")
  (data (i32.const 512) "{\"ok\":true}\00")
  (data (i32.const 1024) "null\00")
)
"#;

/// A `llama.cpp`-shaped built-in inline extension (hidden, registers a
/// command); counts factory runs.
fn builtin_inline(runs: &Arc<AtomicU64>) -> InlineExtension {
    let runs = runs.clone();
    InlineExtension::Named {
        name: "llama.cpp".to_owned(),
        hidden: true,
        factory: Arc::new(move |api| {
            runs.fetch_add(1, Ordering::Relaxed);
            let api = api.clone();
            Box::pin(async move {
                api.register_command(
                    "llama",
                    Some("Manage llama.cpp router models".to_owned()),
                    Arc::new(|_args, _ctx| Box::pin(async { Ok(()) })),
                )
                .map_err(|e| e.to_string())
            })
        }),
    }
}

fn write_wasm(dir: &Path, rel: &str, wat: &str) -> PathBuf {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dirs");
    }
    std::fs::write(&path, wat).expect("write fixture wasm");
    path
}

/// On the trust path (which should resolve as trusted): pre-trust loads builtins, the
/// final pass reuses them (does not re-run the factory), and `/reload` replays the spec
/// and re-runs inline — built-in extensions must not be lost; reuse order follows the
/// final path table (project-local before global).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trust_path_reload_keeps_builtin_inline_and_orders_by_final_paths() {
    let dir = TempDir::new("order");
    let cwd = dir.path().join("cwd");
    let agent_dir = dir.path().join("agent");
    let runs = Arc::new(AtomicU64::new(0));
    let inline = builtin_inline(&runs);

    write_wasm(&cwd, ".rpi/extensions/local.wasm", WATCH_GUEST_WAT);
    write_wasm(&agent_dir, "extensions/global.wasm", WATCH_GUEST_WAT);

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let pre_errors = host
        .load_startup_pre_trust(agent_dir.clone(), Vec::new(), vec![inline.clone()], false)
        .await;
    assert!(pre_errors.is_empty(), "{pre_errors:?}");
    // Phase one: global + inline; project-local excluded.
    let paths = host.get_extension_paths();
    assert_eq!(paths.len(), 2, "{paths:?}");
    assert!(paths[0].ends_with("global.wasm"));
    assert_eq!(paths[1], "<inline:llama.cpp>");

    let final_errors = host
        .load_startup_final(
            agent_dir.clone(),
            Vec::new(),
            Vec::new(),
            // Built-ins re-passed: recorded for the reload spec, NOT re-run
            // by the reuse pass (pre-trust already loaded them).
            vec![inline.clone()],
            true,
            false,
        )
        .await;
    assert!(final_errors.is_empty(), "{final_errors:?}");
    assert_eq!(
        runs.load(Ordering::Relaxed),
        1,
        "reuse must not re-run inline factories"
    );
    // Final order = final path table [project-local, global] + inline tail.
    let paths = host.get_extension_paths();
    assert_eq!(paths.len(), 3, "{paths:?}");
    assert!(paths[0].ends_with("local.wasm"), "{paths:?}");
    assert!(paths[1].ends_with("global.wasm"), "{paths:?}");
    assert_eq!(paths[2], "<inline:llama.cpp>", "{paths:?}");
    assert!(host.get_command("llama").is_some());

    // `/reload`: spec replay (paths + inline re-run) — built-in extensions must come back.
    let reload_errors = host.reload().await;
    assert!(reload_errors.is_empty(), "{reload_errors:?}");
    assert_eq!(
        runs.load(Ordering::Relaxed),
        2,
        "reload re-runs inline factories"
    );
    assert!(
        host.get_command("llama").is_some(),
        "builtin lost after reload"
    );
    assert!(
        host.has_handlers("session_start"),
        "path extensions lost after reload"
    );
    let paths = host.get_extension_paths();
    assert_eq!(paths.len(), 3, "{paths:?}");
    assert!(paths[0].ends_with("local.wasm"), "{paths:?}");
    assert!(paths[1].ends_with("global.wasm"), "{paths:?}");
    assert_eq!(paths[2], "<inline:llama.cpp>", "{paths:?}");
}

/// With `--no-extensions`, the final pass must drop the global extensions loaded by the
/// pre-trust pass (final path table = CLI only; loadFinalExtensionSet assembles from the
/// final path table).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_extensions_final_drops_pretrust_global_extensions() {
    let dir = TempDir::new("noext-final");
    let cwd = dir.path().join("cwd");
    let agent_dir = dir.path().join("agent");
    let runs = Arc::new(AtomicU64::new(0));
    let inline = builtin_inline(&runs);

    write_wasm(&agent_dir, "extensions/global.wasm", WATCH_GUEST_WAT);

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let pre_errors = host
        .load_startup_pre_trust(agent_dir.clone(), Vec::new(), vec![inline.clone()], false)
        .await;
    assert!(pre_errors.is_empty(), "{pre_errors:?}");
    assert!(
        host.has_handlers("session_start"),
        "global loaded pre-trust"
    );

    let final_errors = host
        .load_startup_final(
            agent_dir.clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            true,
            true,
        )
        .await;
    assert!(final_errors.is_empty(), "{final_errors:?}");
    assert!(
        !host.has_handlers("session_start"),
        "global extension must not survive a --no-extensions final pass"
    );
    let paths = host.get_extension_paths();
    assert!(
        paths.iter().all(|p| !p.ends_with("global.wasm")),
        "{paths:?}"
    );
}

/// With `--no-extensions`, the pre-trust pass itself loads only CLI `-e` + inline
/// (upstream loadProjectTrustExtensions → loadCurrentExtensionSet respects
/// noExtensions）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_extensions_pretrust_skips_global_discovery() {
    let dir = TempDir::new("noext-pretrust");
    let cwd = dir.path().join("cwd");
    let agent_dir = dir.path().join("agent");
    let runs = Arc::new(AtomicU64::new(0));
    let inline = builtin_inline(&runs);

    write_wasm(&agent_dir, "extensions/global.wasm", WATCH_GUEST_WAT);

    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let pre_errors = host
        .load_startup_pre_trust(agent_dir.clone(), Vec::new(), vec![inline.clone()], true)
        .await;
    assert!(pre_errors.is_empty(), "{pre_errors:?}");
    assert!(
        !host.has_handlers("session_start"),
        "--no-extensions pre-trust must not load global extensions"
    );
    let paths = host.get_extension_paths();
    assert_eq!(paths, vec!["<inline:llama.cpp>".to_owned()], "{paths:?}");
    assert!(host.get_command("llama").is_some());
}

/// Sanity: the fixture WAT guest is a valid extension (the tests above rely
/// on it loading successfully).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wat_fixture_guest_loads_and_subscribes() {
    let dir = TempDir::new("fixture");
    let cwd = dir.path().join("cwd");
    let agent_dir = dir.path().join("agent");
    let wasm = write_wasm(&agent_dir, "extensions/g.wasm", WATCH_GUEST_WAT);
    let host = NativeExtensionHost::new(&cwd.to_string_lossy());
    let errors = host.load_paths(&[wasm]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert!(host.has_handlers("session_start"));
}
