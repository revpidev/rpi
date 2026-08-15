//! rpi-smart-fetch: fingerprinted web fetch extension (L0 native plugin,
//! TE06 P0 core + TE07 P1 enhancements).
//!
//! Port of pi-smart-fetch v0.3.17 (b0111612) — registers the `web_fetch` and
//! `batch_web_fetch` tools: browser-grade TLS/HTTP2 fingerprinting via wreq
//! (upstream wreq-js engine line), readability extraction via dom_smoothie
//! with a 1:1 DOM fallback chain, five output formats, maxChars truncation,
//! the structured FetchError model, meta-refresh/alternate recursion,
//! streaming attachment downloads and settings.json global defaults. See
//! docs: `rpi-docs/extensions/pi-smart-fetch/` (requirements/design) —
//! extraction-engine divergence is a declared [VARIANT] accepted by
//! metric-based parity (design §3.2).
//!
//! Async model (design §2.2): the host calls the cdylib synchronously; the
//! pipeline runs on a plugin-owned tokio runtime via `block_on`, never on
//! the host runtime.

pub mod batch;
pub mod constants;
pub mod download;
pub mod extract;
pub mod format;
pub mod http;
pub mod pipeline;
pub mod runtime;
pub mod settings;
pub mod types;

use std::sync::OnceLock;

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{json, Map, Value};

use crate::pipeline::FetchPipeline;
use crate::runtime::PluginRuntime;
use crate::settings::ResolvedSettings;
use crate::types::FetchToolDefaults;
use crate::types::{FetchOutcome, WebFetchParams};

/// Plugin state established once by `rpi_extension_init`.
struct PluginState {
    runtime: PluginRuntime,
    /// Host handle for the per-execute `ctx.cwd` lookup (FR-P1-5). `None` in
    /// test installs that never registered through a real host.
    host: Option<(RpiHostCalls, usize)>,
}

static STATE: OnceLock<PluginState> = OnceLock::new();

/// Upstream `toolDescription` (index.ts:29-34), verbatim.
fn tool_description() -> String {
    [
        "Fetch a URL with browser-grade TLS fingerprinting and extract clean, readable content.",
        "Uses wreq-js for browser-like TLS/HTTP2 impersonation and Defuddle for article extraction.",
        "Returns full metadata plus the extracted document to the agent while keeping the pi history preview brief.",
        "Does NOT execute JavaScript — use a browser automation tool for JS-heavy pages.",
    ]
    .join(" ")
}

/// Upstream `batchToolDescription` (index.ts:36-41), verbatim.
fn batch_tool_description() -> String {
    [
        "Fetch multiple URLs with browser-grade TLS fingerprinting and readable extraction.",
        "Each request accepts the same parameters as web_fetch and fans out with bounded concurrency.",
        "Returns full per-item metadata to the agent and streams compact per-item progress in the pi TUI.",
        "Does NOT execute JavaScript — use a browser automation tool for JS-heavy pages.",
    ]
    .join(" ")
}

/// Upstream `promptSnippet` (index.ts:477-479), verbatim.
const TOOL_PROMPT_SNIPPET: &str = "web_fetch(url, browser?, os?, headers?, maxChars?, timeoutMs?, format?, removeImages?, includeReplies?, proxy?, verbose?): fetch browser-fingerprinted readable web content with full agent metadata and a compact pi preview";

/// Upstream batch `promptSnippet` (index.ts:671-672), verbatim.
const BATCH_TOOL_PROMPT_SNIPPET: &str = "batch_web_fetch(requests, verbose?): fetch multiple URLs concurrently with full agent metadata and per-item progress in the pi TUI";

/// The shared parameter surface (tool.ts:52-116, `createBaseFetchToolParameterProperties`):
/// field names, types and descriptions carried 1:1; the five-literal format
/// union and the boolean|"extractors" includeReplies union render as
/// JSON-Schema enums.
fn base_tool_properties() -> Map<String, Value> {
    json!({
        "url": {"type": "string", "description": "URL to fetch (http/https only)"},
        "browser": {
            "type": "string",
            "description": "Browser profile for TLS fingerprinting. Default: \"chrome_145\". Examples: chrome_145, firefox_147, safari_26, edge_145, opera_127"
        },
        "os": {
            "type": "string",
            "description": "OS profile for fingerprinting. Default: \"windows\". Options: windows, macos, linux, android, ios"
        },
        "headers": {
            "type": "object",
            "additionalProperties": {"type": "string"},
            "description": "Custom HTTP headers to send. By default, Accept and Accept-Language are set automatically."
        },
        "maxChars": {
            "type": "number",
            "description": "Maximum characters to return. Default: 50000"
        },
        "timeoutMs": {
            "type": "number",
            "description": "Request timeout in milliseconds. Default: 15000"
        },
        "format": {
            "type": "string",
            "enum": ["markdown", "html", "text", "json", "raw"],
            "description": "Output format. \"markdown\" (default), \"html\" (cleaned HTML), \"text\" (plain text, no formatting), \"json\" (pretty-printed JSON), or \"raw\" (full raw server response without extraction or truncation, for further parsing)"
        },
        "removeImages": {
            "type": "boolean",
            "description": "Strip image references from output. Default: false"
        },
        "includeReplies": {
            "anyOf": [
                {"type": "boolean"},
                {"type": "string", "enum": ["extractors"]}
            ],
            "description": "Include replies/comments: 'extractors' for site-specific only (default), true for all, false for none"
        },
        "proxy": {
            "type": "string",
            "description": "Proxy URL (http://user:pass@host:port or socks5://host:port)"
        }
    })
    .as_object()
    .expect("literal object")
    .clone()
}

/// The `verbose` compat flag (index.ts:480-487 / 673-681).
fn verbose_property() -> Value {
    json!({
        "type": "boolean",
        "description": "Compatibility flag. pi currently returns the full metadata header to the agent regardless, while keeping the history preview compact. Default: false, or smartFetchVerboseByDefault from pi settings."
    })
}

/// `web_fetch` parameter schema (FR-P0-1): base surface + `verbose`.
fn tool_parameters_schema() -> Value {
    let mut properties = base_tool_properties();
    properties.insert("verbose".to_string(), verbose_property());
    json!({
        "type": "object",
        "properties": properties,
        "required": ["url"]
    })
}

/// `batch_web_fetch` parameter schema (FR-P1-1, tool.ts:118-133): `requests`
/// (minItems 1, per-item base surface with `additionalProperties: false`) +
/// `verbose`.
fn batch_tool_parameters_schema() -> Value {
    let item = json!({
        "type": "object",
        "properties": base_tool_properties(),
        "additionalProperties": false
    });
    let mut properties = Map::new();
    properties.insert(
        "requests".to_string(),
        json!({
            "type": "array",
            "items": item,
            "minItems": 1,
            "description": "Array of fetch requests. Each item accepts the same parameters as the single-item fetch tool."
        }),
    );
    properties.insert("verbose".to_string(), verbose_property());
    json!({
        "type": "object",
        "properties": properties,
        "required": ["requests"]
    })
}

fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    let cookie = cookie as usize;
    let Some(plugin_runtime) = PluginRuntime::new() else {
        return json!({"error": {"kind": "init", "message": "failed to start plugin tokio runtime"}});
    };
    let register = |args: Value| -> Result<(), Value> {
        let request = serde_json::to_vec(&json!({"call": "registerTool", "args": args, "seq": 0}))
            .unwrap_or_default();
        let response = (calls.call)(cookie as PluginCookie, RVec::from(request));
        let response: Value = serde_json::from_slice(&response[..]).unwrap_or(Value::Null);
        if response.get("error").is_some() {
            return Err(response);
        }
        Ok(())
    };

    let web_fetch = json!({
        "name": "web_fetch",
        "label": "web_fetch",
        "description": tool_description(),
        "promptSnippet": TOOL_PROMPT_SNIPPET,
        "parameters": tool_parameters_schema(),
    });
    if let Err(error) = register(web_fetch) {
        return json!({"error": {"kind": "init", "message": error.to_string()}});
    }

    let batch_web_fetch = json!({
        "name": "batch_web_fetch",
        "label": "batch_web_fetch",
        "description": batch_tool_description(),
        "promptSnippet": BATCH_TOOL_PROMPT_SNIPPET,
        "parameters": batch_tool_parameters_schema(),
    });
    if let Err(error) = register(batch_web_fetch) {
        return json!({"error": {"kind": "init", "message": error.to_string()}});
    }

    match STATE.set(PluginState {
        runtime: plugin_runtime,
        host: Some((calls, cookie)),
    }) {
        Ok(()) => json!({"ok": true}),
        // Idempotent init (ambient + explicit load): first instance serves.
        Err(_) => json!({"ok": true}),
    }
}

/// The per-execute runtime state (index.ts:496-499 / 684-687): settings are
/// re-read on EVERY call (`loadPiSmartFetchSettings(ctx.cwd, getAgentDir())`)
/// and folded into tool defaults.
fn resolve_runtime(state: &PluginState) -> (ResolvedSettings, FetchToolDefaults) {
    let cwd = state
        .host
        .as_ref()
        .and_then(|(calls, cookie)| {
            let request = serde_json::to_vec(&json!({"call": "ctx.cwd", "args": {}, "seq": 0}))
                .unwrap_or_default();
            let response = (calls.call)(*cookie as PluginCookie, RVec::from(request));
            let response: Value = serde_json::from_slice(&response[..]).unwrap_or(Value::Null);
            response
                .get("ok")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|cwd| cwd.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| ".".to_string());

    let resolved = settings::load_settings(std::path::Path::new(&cwd));
    let defaults = batch::resolve_fetch_tool_defaults(&resolved.config);
    (resolved, defaults)
}

/// `web_fetch` execute (index.ts:489-626, no spinner/onUpdate stream: the
/// on_update ABI gap, design §1.3 #3).
fn execute_web_fetch(params: &Value, state: &PluginState) -> Value {
    let parsed_params: WebFetchParams = match serde_json::from_value(params.clone()) {
        Ok(parsed) => parsed,
        Err(error) => {
            // Upstream catch-all template (index.ts:604-621) for unexpected
            // failures; param deserialization is the Rust-side equivalent.
            let url = params.get("url").and_then(Value::as_str).unwrap_or("URL");
            return json!({
                "content": [{ "type": "text", "text":
                    format!("Error: Unexpected web_fetch failure for {url}.\n\n{error}")
                }],
                "details": {
                    "error": true,
                    "userErrorSummary": "The request failed before a usable response was returned.",
                    "status": "connecting",
                },
            });
        }
    };

    // FR-P1-5: per-execute settings read (no caching, upstream semantics).
    let (resolved, defaults) = resolve_runtime(state);
    let verbose = parsed_params.verbose.unwrap_or(resolved.verbose_by_default);
    let format = parsed_params
        .format
        .map(types::OutputFormat::from)
        .unwrap_or_default();
    let opts = batch::build_fetch_options_from_params(&parsed_params, &defaults);

    let pipeline = FetchPipeline::default();
    let outcome = state.runtime.block_on(pipeline.fetch(&opts));

    match outcome {
        FetchOutcome::Error(error) => {
            let error_text = format::build_fetch_error_response_text(&error);
            json!({
                "content": [{ "type": "text", "text": error_text }],
                "details": {
                    "error": true,
                    "errorText": error_text,
                    "userErrorSummary": format::build_user_facing_fetch_error_summary(&error),
                    "verbose": verbose,
                    "format": format.as_str(),
                    "maxChars": defaults.max_chars,
                    "started": true,
                    "status": "error",
                    "progress": 1,
                    "phase": "error",
                    "url": opts.url,
                },
            })
        }
        FetchOutcome::Result(result) => {
            // index.ts:584-589: the agent text always carries the FULL
            // metadata header (verbose is a compat flag only).
            let response_text = format::build_fetch_response_text(&result, true);
            let url = if result.final_url.is_empty() {
                result.url.clone()
            } else {
                result.final_url.clone()
            };
            json!({
                "content": [{ "type": "text", "text": response_text }],
                "details": {
                    "verbose": verbose,
                    "format": format.as_str(),
                    "maxChars": defaults.max_chars,
                    "fetchResult": serde_json::to_value(&result).unwrap_or(Value::Null),
                    "started": true,
                    "status": "done",
                    "progress": 1,
                    "phase": "done",
                    "url": url,
                },
            })
        }
    }
}

/// `batch_web_fetch` execute (index.ts:683-771, progress snapshots only in
/// the final result — the on_update ABI gap, design §1.3 #3).
fn execute_batch_web_fetch(params: &Value, state: &PluginState) -> Value {
    let parsed: batch::BatchWebFetchParams = match serde_json::from_value(params.clone()) {
        Ok(parsed) => parsed,
        Err(error) => {
            return json!({
                "content": [{ "type": "text", "text":
                    format!("Error: Unexpected batch_web_fetch failure.\n\n{error}")
                }],
                "details": {
                    "error": true,
                    "userErrorSummary": "The request failed before a usable response was returned.",
                },
            });
        }
    };

    let (resolved, defaults) = resolve_runtime(state);
    let verbose = parsed.verbose.unwrap_or(resolved.verbose_by_default);

    let pipeline = FetchPipeline::default();
    let batch_result = state.runtime.block_on(batch::execute_batch_entries(
        &pipeline,
        &parsed.requests,
        &defaults,
    ));

    // index.ts:753-755: the agent text carries the FULL per-item headers.
    let response_text = batch::build_batch_fetch_response_text(&batch_result, true);
    json!({
        "content": [{ "type": "text", "text": response_text }],
        "details": batch::batch_details_json(&batch_result, verbose),
    })
}

fn dispatch_message(message: &Value) -> Value {
    let Some(state) = STATE.get() else {
        return Value::Null;
    };
    match message.get("kind").and_then(Value::as_str) {
        Some("toolExecute") => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            match message.get("toolName").and_then(Value::as_str) {
                Some("batch_web_fetch") => execute_batch_web_fetch(&params, state),
                _ => execute_web_fetch(&params, state),
            }
        }
        _ => Value::Null,
    }
}

#[allow(clippy::missing_safety_doc)]
pub extern "C" fn init(calls: RpiHostCalls, cookie: PluginCookie) -> RVec<u8> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| install(calls, cookie)))
        .unwrap_or_else(|_| json!({"error": {"kind": "init", "message": "panic during init"}}));
    pack(&result)
}

pub extern "C" fn dispatch(_cookie: PluginCookie, message: RVec<u8>) -> RVec<u8> {
    let parsed: Value = serde_json::from_slice(&message[..]).unwrap_or(Value::Null);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch_message(&parsed)
    }))
    .unwrap_or_else(|_| {
        json!({
            "content": [{ "type": "text", "text": "smart-fetch extension panicked while handling a dispatch" }],
            "isError": true,
        })
    });
    pack(&result)
}

fn pack(value: &Value) -> RVec<u8> {
    RVec::from(serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec()))
}

/// The root module export (abi_stable).
#[abi_stable::export_root_module]
pub fn module() -> RpiNativeModule_Ref {
    RpiNativeModule {
        rpi_extension_init: init,
        rpi_dispatch: dispatch,
    }
    .leak_into_prefix()
}

/// Test seam: install with a fake host (mcp-adapter `install_for_test`
/// pattern; one install per test binary because of the OnceLock state).
#[doc(hidden)]
pub fn install_for_test(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    install(calls, cookie)
}

/// Test seam: drive a `web_fetch` execution against an installed state.
#[doc(hidden)]
pub fn execute_for_test(params: &Value) -> Value {
    dispatch_message(&json!({
        "kind": "toolExecute",
        "toolName": "web_fetch",
        "toolCallId": "test",
        "params": params,
    }))
}

/// Parity harness facade: exposes the pure-port functions for the
/// scripts/smart-fetch-parity fixture runner without crate internals.
pub mod parity {
    pub use crate::batch::{
        build_batch_fetch_response_text, resolve_batch_concurrency, resolve_fetch_tool_defaults,
    };
    pub use crate::download::{resolve_download_target, sanitize_base_name, sanitize_extension};
    pub use crate::format::{
        build_fetch_error_response_text, build_fetch_response_text,
        build_user_facing_fetch_error_summary, estimate_word_count, format_byte_count,
        format_duration_ms, markdown_to_text, parse_and_format_json, render_json_content,
        strip_extractor_comments, truncate_content,
    };
    pub use crate::http::{resolve_platform, resolve_profile, UPSTREAM_PROFILES};
    pub use crate::pipeline::{
        extract_client_side_redirect, extract_qualified_alternate_links, is_attachment_disposition,
        is_textual_content_type, normalize_content_type,
    };
    pub use crate::settings::{normalize_settings, resolve_settings};
}
