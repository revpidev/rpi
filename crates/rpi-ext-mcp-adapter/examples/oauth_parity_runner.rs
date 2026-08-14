//! OAuth parity: rpi (Rust) side driver (TE02 self-check item 5).
//!
//! Drives this crate's authorization-code + PKCE flow (`oauth.rs`) against
//! the same stub authorization server as the upstream Node driver and
//! prints the normalized transcript (authorization URL params + token
//! request params) in the same shape. Spawned by
//! `scripts/mcp-parity/run-oauth-parity.mjs`; not part of `cargo test`.

use rpi_ext_mcp_adapter::metadata::ServerEntry;
use rpi_ext_mcp_adapter::oauth::store::AuthStorageOptions;
use rpi_ext_mcp_adapter::oauth::{authenticate, AuthenticateOptions};
use serde_json::{json, Value};

fn normalize_params(params: &serde_json::Map<String, Value>) -> Value {
    let mut out = serde_json::Map::new();
    for (key, value) in params {
        let normalized = match (key.as_str(), value) {
            ("code_challenge", Value::String(v)) if v.len() >= 40 => json!("$challenge"),
            ("state", Value::String(v)) if v.len() >= 8 => json!("$state"),
            ("redirect_uri", Value::String(v)) => {
                json!(v.replace(
                    &format!("localhost:{}/", extract_port(v)),
                    "localhost:$port/"
                ))
            }
            ("code", Value::String(v)) if v == "stub-code" => json!("$code"),
            ("code_verifier", Value::String(v)) if v.len() >= 40 => json!("$verifier"),
            ("resource", Value::String(v)) => json!(v.replace(
                &format!("localhost:{}/", extract_port(v)),
                "localhost:$asport/",
            )),
            ("redirect_uris", Value::Array(uris)) => json!(uris
                .iter()
                .map(|u| match u.as_str() {
                    Some(s) => json!(s.replace(
                        &format!("localhost:{}/", extract_port(s)),
                        "localhost:$port/",
                    )),
                    None => u.clone(),
                })
                .collect::<Vec<_>>()),
            // O1 brand exemption: client identity fields (upstream "Pi
            // Coding Agent" / adapter repo vs rpi's own product identity).
            ("client_name", _) => json!("$client_name"),
            ("client_uri", _) => json!("$client_uri"),
            _ => value.clone(),
        };
        out.insert(key.clone(), normalized);
    }
    Value::Object(out)
}

fn extract_port(url: &str) -> String {
    url.split("localhost:")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?']).next())
        .unwrap_or("0")
        .to_string()
}

#[tokio::main]
async fn main() {
    let server_url = std::env::var("RPI_MCP_OAUTH_SERVER_URL").unwrap_or_default();
    let transcript = std::env::var("RPI_MCP_OAUTH_TRANSCRIPT").unwrap_or_default();
    let dir = std::env::var("RPI_MCP_OAUTH_STORE_DIR").unwrap_or_default();
    if server_url.is_empty() || transcript.is_empty() || dir.is_empty() {
        eprintln!("orchestrator env missing");
        std::process::exit(2);
    }

    // Play the browser inside the on_authorization_url callback: the stub
    // AS 302s with code+state into the crate's own callback listener, so
    // fetching the authorization URL (redirects followed by reqwest into
    // the callback) completes the flow — same headless-browser role as
    // oauth-upstream-driver.mjs.
    let store_dir = std::path::PathBuf::from(&dir);
    let definition = ServerEntry(
        json!({ "url": server_url, "oauth": {} })
            .as_object()
            .cloned()
            .unwrap_or_default(),
    );
    let options = AuthenticateOptions {
        on_authorization_url: Some(std::sync::Arc::new(|url: &str| {
            let url = url.to_string();
            tokio::spawn(async move {
                if let Err(error) = reqwest::get(&url).await {
                    eprintln!("authorization endpoint fetch failed: {error}");
                }
            });
        })),
        auth_storage_options: AuthStorageOptions {
            base_dir: Some(store_dir),
        },
        ..Default::default()
    };

    let status = match authenticate("fixture-oauth", &server_url, &definition, &options).await {
        Ok(result) => format!("{result:?}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    let entries = std::fs::read_to_string(&transcript)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|entry| {
            json!({
                "kind": entry["kind"],
                "params": normalize_params(entry["params"].as_object().unwrap_or(&serde_json::Map::new())),
            })
        })
        .collect::<Vec<_>>();

    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({ "side": "rpi", "status": status, "entries": entries })
        )
        .unwrap_or_default()
    );
}
