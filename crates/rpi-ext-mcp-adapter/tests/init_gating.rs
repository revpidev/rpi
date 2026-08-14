//! Init-gating direct-injection tests (task TE02 self-check item 3, design
//! §5.2): the three gate arms `init_timeout` / `init_failed` /
//! `not_initialized` (index.ts:758-783) exercised through the dispatcher's
//! injected-future test seam instead of real 30-second initialization paths.

use std::sync::Arc;

use futures::future::{BoxFuture, FutureExt, Shared};
use rpi_ext_mcp_adapter::proxy::{initialize_mcp, McpRuntime, ProxyDispatcher};
use serde_json::json;

type InitFuture = Shared<BoxFuture<'static, Result<Arc<McpRuntime>, Arc<String>>>>;

fn never_resolves() -> InitFuture {
    std::future::pending().boxed().shared()
}

fn fails_with(message: &str) -> InitFuture {
    let message: Arc<String> = Arc::from(message.to_string());
    std::future::ready(Err(message)).boxed().shared()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "rpi-mcp-initgate-{}-{}-{}",
        tag,
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// `not_initialized`: execute before any `start_init` (session never
/// started), proxy + direct-tool gate shapes.
#[tokio::test]
async fn gate_not_initialized_before_start() {
    let dispatcher = Arc::new(ProxyDispatcher::new());
    let result = dispatcher.execute(&json!({ "status": true }), &[]).await;
    assert_eq!(result["content"][0]["text"], json!("MCP not initialized"));
    assert_eq!(result["details"]["error"], json!("not_initialized"));

    let direct = match dispatcher.current_direct().await {
        Ok(_) => panic!("gate must not be ready"),
        Err(result) => result,
    };
    assert_eq!(direct["content"][0]["text"], json!("MCP not initialized"));
    assert_eq!(direct["details"]["error"], json!("not_initialized"));
}

/// `init_timeout`: an init future that never resolves, with an injected
/// sub-30s wait bound. The reported `timeoutMs` stays the production
/// constant (index.ts:38), and a retry while still initializing hits the
/// same arm again (state stays `Initializing`, never poisoned).
#[tokio::test]
async fn gate_init_timeout_direct_injection() {
    let dispatcher = Arc::new(ProxyDispatcher::new());
    dispatcher.set_init_wait_timeout(std::time::Duration::from_millis(50));
    dispatcher.start_init_with(never_resolves());

    for attempt in 0..2 {
        let result = dispatcher.execute(&json!({ "status": true }), &[]).await;
        assert_eq!(
            result["content"][0]["text"],
            json!("MCP initialization is still in progress. Try again shortly."),
            "attempt {attempt}"
        );
        assert_eq!(result["details"]["error"], json!("init_timeout"));
        assert_eq!(result["details"]["timeoutMs"], json!(30000));
    }

    // The direct-tool gate has NO timeout bound (direct-tools.ts:310-326):
    // with an injectable proxy bound alone it must not observe the failure.
    let direct = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        dispatcher.current_direct(),
    )
    .await;
    assert!(direct.is_err(), "current_direct must still be waiting");
}

/// `init_failed`: an init future that resolves with an error message.
#[tokio::test]
async fn gate_init_failed_direct_injection() {
    let dispatcher = Arc::new(ProxyDispatcher::new());
    dispatcher.start_init_with(fails_with("fixture-init-boom"));

    let result = dispatcher.execute(&json!({ "status": true }), &[]).await;
    assert_eq!(
        result["content"][0]["text"],
        json!("MCP initialization failed: fixture-init-boom")
    );
    assert_eq!(result["details"]["error"], json!("init_failed"));
    assert_eq!(result["details"]["message"], json!("fixture-init-boom"));

    let direct = match dispatcher.current_direct().await {
        Ok(_) => panic!("gate must not be ready"),
        Err(result) => result,
    };
    assert_eq!(
        direct["content"][0]["text"],
        json!("MCP initialization failed: fixture-init-boom")
    );
    assert_eq!(direct["details"]["error"], json!("init_failed"));
    assert_eq!(direct["details"]["message"], json!("fixture-init-boom"));

    // A later session restart must clear the failed state (not_initialized
    // again, not a sticky failure).
    dispatcher.shutdown().await;
    let result = dispatcher.execute(&json!({ "status": true }), &[]).await;
    assert_eq!(result["details"]["error"], json!("not_initialized"));
}

/// The success arm for contrast: an injected ready runtime transitions the
/// gate to Ready, fires `on_ready` once, and executes against the runtime.
#[tokio::test]
async fn gate_ready_transitions_and_fires_on_ready() {
    let dir = temp_dir("ready");
    let runtime = initialize_mcp(&dir, None, Some(dir.join("cache.json"))).await;
    let ready = std::future::ready(Ok(runtime)).boxed().shared();

    let dispatcher = Arc::new(ProxyDispatcher::new());
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fired_hook = fired.clone();
    dispatcher.set_hooks(rpi_ext_mcp_adapter::proxy::DispatcherHooks {
        on_ready: Some(Arc::new(move || {
            fired_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })),
        on_connect_sync: None,
    });
    dispatcher.start_init_with(ready);

    assert!(
        dispatcher.try_runtime().is_none(),
        "not Ready before execute"
    );
    let result = dispatcher.execute(&json!({ "status": true }), &[]).await;
    assert_eq!(result["details"]["mode"], json!("status"));
    assert!(dispatcher.try_runtime().is_some(), "Ready after execute");
    // fire_on_ready is guarded by the Ready transition (index.ts:302-326):
    // one execute already transitioned, a second must not re-fire.
    let _ = dispatcher.execute(&json!({ "status": true }), &[]).await;
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);

    dispatcher.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// B-1 regression: `start_init` on a tokio runtime spawns a background
/// driver that polls the init future to completion — the gate reaches Ready
/// with NO caller awaiting it (upstream `setImmediate` prewarm semantics).
/// A server-less config keeps the real `initialize_mcp` fast.
#[tokio::test]
async fn start_init_drives_background_task_to_ready_without_awaiter() {
    let dir = temp_dir("driver");
    let dispatcher = Arc::new(ProxyDispatcher::new());

    dispatcher.start_init(dir.clone(), None);
    assert!(
        dispatcher.try_runtime().is_none(),
        "still initializing right after start_init"
    );

    let deadline = std::time::Duration::from_secs(10);
    let ready = tokio::time::timeout(deadline, async {
        while dispatcher.try_runtime().is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(ready.is_ok(), "background driver must reach Ready");

    dispatcher.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// B-1 regression, background on_ready: the driver publishes Ready and
/// fires the hook even when no dispatch ever awaits the gate.
#[tokio::test]
async fn start_init_background_driver_fires_on_ready() {
    let dir = temp_dir("driver-ready");
    let dispatcher = Arc::new(ProxyDispatcher::new());
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fired_hook = fired.clone();
    dispatcher.set_hooks(rpi_ext_mcp_adapter::proxy::DispatcherHooks {
        on_ready: Some(Arc::new(move || {
            fired_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })),
        on_connect_sync: None,
    });

    dispatcher.start_init(dir.clone(), None);

    let deadline = std::time::Duration::from_secs(10);
    let fired_once = tokio::time::timeout(deadline, async {
        while fired.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(fired_once.is_ok(), "driver must fire on_ready exactly once");
    assert!(
        dispatcher.try_runtime().is_some(),
        "Ready published by the driver"
    );

    dispatcher.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
