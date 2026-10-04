//! `registerMcpServer` extension-API tests (V16-08 FR-E): the runtime
//! registry's registration checks, rollback on failed factories and the
//! change listener.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rpi_ext_host::api::ExtensionApi;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::InlineExtension;
use serde_json::json;

fn factory(
    name: &'static str,
    body: impl Fn(ExtensionApi) -> Result<(), String> + Send + Sync + 'static,
) -> InlineExtension {
    InlineExtension::Named {
        name: name.to_owned(),
        hidden: true,
        replaceable: false,
        builtin: false,
        factory: Arc::new(move |api| {
            let result = body(api);
            Box::pin(async move { result })
        }),
    }
}

#[tokio::test]
async fn registers_servers_and_reports_them() {
    let host = NativeExtensionHost::new("/x");
    let errors = host
        .load_inline(&[factory("a", |api| {
            api.register_mcp_server("docs", json!({"url": "https://example.com/mcp"}))
                .map_err(|e| e.to_string())?;
            api.register_mcp_server("echo", json!({"command": "node", "args": ["server.js"]}))
                .map_err(|e| e.to_string())?;
            Ok(())
        })])
        .await;
    assert!(errors.is_empty(), "{errors:?}");
    let servers = host.runtime().mcp_servers().lock().unwrap().to_json();
    assert_eq!(servers.len(), 2);
    assert_eq!(servers[0]["name"], "docs");
    assert_eq!(servers[0]["extensionPath"], "<inline:a>");
    assert_eq!(servers[1]["name"], "echo");
}

#[tokio::test]
async fn rejects_foreign_names_clashes_and_invalid_configs() {
    let host = NativeExtensionHost::new("/x");
    let errors = host
        .load_inline(&[
            factory("a", |api| {
                api.register_mcp_server("docs", json!({"url": "https://example.com/mcp"}))
                    .map_err(|e| e.to_string())?;
                api.register_mcp_server("dash-name", json!({"command": "x"}))
                    .map_err(|e| e.to_string())?;
                Ok(())
            }),
            factory("b", |api| {
                let error = api
                    .register_mcp_server("docs", json!({"url": "https://other.example/mcp"}))
                    .unwrap_err();
                assert!(error.to_string().contains("already registered"), "{error}");
                let error = api
                    .register_mcp_server("dash_name", json!({"command": "x"}))
                    .unwrap_err();
                assert!(error.to_string().contains("conflicts"), "{error}");
                let error = api.register_mcp_server("bad", json!({})).unwrap_err();
                assert!(error.to_string().contains("needs either"), "{error}");
                let error = api
                    .register_mcp_server("bad name", json!({"command": "x"}))
                    .unwrap_err();
                assert!(error.to_string().contains("invalid server name"), "{error}");
                Ok(())
            }),
        ])
        .await;
    assert!(errors.is_empty(), "{errors:?}");
    let servers = host.runtime().mcp_servers().lock().unwrap().to_json();
    assert_eq!(servers.len(), 2);
    assert_eq!(servers[0]["config"]["url"], "https://example.com/mcp");
}

#[tokio::test]
async fn failed_factories_roll_back_their_registrations() {
    let host = NativeExtensionHost::new("/x");
    let errors = host
        .load_inline(&[factory("bad", |api| {
            api.register_mcp_server("leaky", json!({"url": "https://example.com/mcp"}))
                .map_err(|e| e.to_string())?;
            Err("factory failed after registering".to_owned())
        })])
        .await;
    assert!(!errors.is_empty());
    assert!(
        host.runtime()
            .mcp_servers()
            .lock()
            .unwrap()
            .list()
            .is_empty()
    );
}

#[tokio::test]
async fn unregisters_only_owned_servers_and_notifies() {
    let host = NativeExtensionHost::new("/x");
    let changes = Arc::new(AtomicUsize::new(0));
    {
        let changes = changes.clone();
        host.runtime()
            .mcp_servers()
            .lock()
            .unwrap()
            .set_change_listener(Some(Arc::new(move || {
                changes.fetch_add(1, Ordering::SeqCst);
            })));
    }
    let errors = host
        .load_inline(&[
            factory("a", |api| {
                api.register_mcp_server("docs", json!({"url": "https://example.com/mcp"}))
                    .map_err(|e| e.to_string())?;
                // Same-extension replacement is allowed.
                api.register_mcp_server("docs", json!({"url": "https://example.com/mcp2"}))
                    .map_err(|e| e.to_string())?;
                Ok(())
            }),
            factory("b", |api| {
                // A different extension cannot remove another's server.
                api.unregister_mcp_server("docs")
                    .map_err(|e| e.to_string())?;
                Ok(())
            }),
        ])
        .await;
    assert!(errors.is_empty(), "{errors:?}");
    let servers = host.runtime().mcp_servers().lock().unwrap().list();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].config["url"], "https://example.com/mcp2");
    // Two registrations fired the listener twice; the foreign unregister
    // did not.
    assert_eq!(changes.load(Ordering::SeqCst), 2);
}
