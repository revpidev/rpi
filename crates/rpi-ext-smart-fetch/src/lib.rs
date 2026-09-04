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
pub mod render;
pub mod runtime;
pub mod settings;
pub mod types;

use std::sync::{Arc, Mutex, OnceLock, RwLock};

use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::RVec;
use rpi_ext_host::native::{PluginCookie, RpiHostCalls, RpiNativeModule, RpiNativeModule_Ref};
use serde_json::{json, Map, Value};

use crate::batch::BatchProgressSink;
use crate::pipeline::{FetchExecutionHooks, FetchPipeline};
use crate::runtime::PluginRuntime;
use crate::settings::ResolvedSettings;
use crate::types::FetchToolDefaults;
use crate::types::{FetchOutcome, WebFetchParams};

/// `SPINNER_INTERVAL_MS` (index.ts:75): the plugin-side push clock — the
/// frames themselves are a render-side concern (render.rs `SPINNER_FRAMES`).
const SPINNER_INTERVAL_MS: u64 = 100;

/// Host-call channel to the CURRENT extension host (mcp-adapter lib.rs
/// precedent). `RawLibrary::load_at` dlopen-memoizes per path, so a session
/// switch re-runs `install` on the same statics — the channel must follow
/// the newest host or calls go through the replaced host's freed cookie.
#[derive(Clone, Copy)]
struct HostChannel {
    call: extern "C" fn(PluginCookie, RVec<u8>) -> RVec<u8>,
    cookie: usize,
}

/// Plugin state established once by `rpi_extension_init`.
struct PluginState {
    runtime: PluginRuntime,
    /// Rebindable host channel for the per-execute `ctx.cwd` lookup
    /// (FR-P1-5) and streaming `toolUpdate` pushes. `None` in test
    /// installs that never registered through a real host.
    host: RwLock<Option<HostChannel>>,
}

impl PluginState {
    /// The current host channel (read at call time so executes never act
    /// on a stale, dropped host).
    fn channel(&self) -> Option<HostChannel> {
        *self.host.read().unwrap_or_else(|e| e.into_inner())
    }
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

/// The `verbose` compat flag (index.ts:480-487 web / :673-681 batch): the
/// two tools differ by ONE word pair — the batch description says the full
/// header is kept "for successful results", the single tool says "to the
/// agent". Both verbatim.
fn verbose_property(batch: bool) -> Value {
    let kept = if batch {
        "for successful results"
    } else {
        "to the agent"
    };
    json!({
        "type": "boolean",
        "description": format!("Compatibility flag. pi currently returns the full metadata header {kept} regardless, while keeping the history preview compact. Default: false, or smartFetchVerboseByDefault from pi settings.")
    })
}

/// `web_fetch` parameter schema (FR-P0-1): base surface + `verbose`.
fn tool_parameters_schema() -> Value {
    let mut properties = base_tool_properties();
    properties.insert("verbose".to_string(), verbose_property(false));
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
    properties.insert("verbose".to_string(), verbose_property(true));
    json!({
        "type": "object",
        "properties": properties,
        "required": ["requests"]
    })
}

fn install(calls: RpiHostCalls, cookie: PluginCookie) -> Value {
    let cookie = cookie as usize;
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
        // FR-P2-E: the host dispatches {"kind":"render"} back here and
        // receives ComponentTree v1 JSON (host_call.rs render arms).
        "renderCall": true,
        "renderResult": true,
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
        "renderCall": true,
        "renderResult": true,
    });
    if let Err(error) = register(batch_web_fetch) {
        return json!({"error": {"kind": "init", "message": error.to_string()}});
    }

    let Some(plugin_runtime) = PluginRuntime::new() else {
        return json!({"error": {"kind": "init", "message": "failed to start plugin tokio runtime"}});
    };
    match STATE.set(PluginState {
        runtime: plugin_runtime,
        host: RwLock::new(Some(HostChannel {
            call: calls.call,
            cookie,
        })),
    }) {
        Ok(()) => {}
        // Rebind (mcp-adapter / subagents session-switch discipline): a
        // second install on a fresh `NativeExtensionHost` (session
        // replacement via `/resume` `/new` `/fork` `/clone` `/import`
        // re-loads this same dlopen-memoized cdylib) adopts the NEW host
        // channel — the replaced host is dropped once the outgoing session
        // goes away, and its cookie dangles, so every later `ctx.cwd`
        // lookup and streaming `toolUpdate` push must go through the
        // newest binding. The tool registrations above already ran
        // against this host.
        Err(_) => {
            if let Some(state) = STATE.get() {
                *state.host.write().unwrap_or_else(|e| e.into_inner()) = Some(HostChannel {
                    call: calls.call,
                    cookie,
                });
            }
        }
    }
    json!({"ok": true})
}

/// The per-execute runtime state (index.ts:496-499 / 684-687): settings are
/// re-read on EVERY call (`loadPiSmartFetchSettings(ctx.cwd, getAgentDir())`)
/// and folded into tool defaults.
fn resolve_runtime(state: &PluginState) -> (ResolvedSettings, FetchToolDefaults) {
    let cwd = state
        .channel()
        .and_then(|channel| {
            let request = serde_json::to_vec(&json!({"call": "ctx.cwd", "args": {}, "seq": 0}))
                .unwrap_or_default();
            let response = (channel.call)(channel.cookie as PluginCookie, RVec::from(request));
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

// ===== FR-P2-B/D: toolUpdate streaming + spinner push =====

/// The per-execute `toolUpdate` sender (ADR-0015 host call, `tools`
/// capability). Synchronous FFI like upstream's synchronous `onUpdate` JS
/// callback; created only when the dispatch carried a `toolCallId` (test
/// seams dispatch without one and stay non-streaming).
struct UpdateSink {
    call: extern "C" fn(PluginCookie, RVec<u8>) -> RVec<u8>,
    cookie: usize,
    tool_call_id: String,
}

impl UpdateSink {
    /// One partial `AgentToolResult` frame. Response errors are swallowed —
    /// the host drops unknown/stale ids by design (settle semantics), so a
    /// late or unrecognized frame is not the plugin's failure to report.
    fn push(&self, content_text: &str, details: &Value) {
        let request = serde_json::to_vec(&json!({
            "call": "toolUpdate",
            "args": {
                "toolCallId": self.tool_call_id,
                "update": {
                    "content": [{ "type": "text", "text": content_text }],
                    "details": details,
                }
            },
            "seq": 0
        }))
        .unwrap_or_default();
        let response = (self.call)(self.cookie as PluginCookie, RVec::from(request));
        let _ = serde_json::from_slice::<Value>(&response[..]);
    }
}

/// `WebFetchRenderDetails` (index.ts:45-65) — the mutable latest-frame state
/// the event hooks update and the spinner re-pushes. Only fields carried by
/// a PARTIAL frame live here (maxChars/fetchResult are final-frame only).
#[derive(Debug, Clone)]
struct WebFetchRenderDetails {
    verbose: bool,
    format: Option<String>,
    started: bool,
    status: String,
    progress: f64,
    phase: Option<String>,
    url: Option<String>,
    spinner_tick: u64,
}

impl WebFetchRenderDetails {
    fn initial(verbose: bool, format: Option<String>, params_url: Option<String>) -> Self {
        // index.ts:503-512: the execute-open state (never pushed as a frame —
        // the pipeline's `fetch_start` event produces the first one).
        WebFetchRenderDetails {
            verbose,
            format,
            started: true,
            status: "connecting".to_string(),
            progress: 0.0,
            phase: Some("fetch_start".to_string()),
            url: params_url,
            spinner_tick: 0,
        }
    }

    /// The partial-frame details JSON (index.ts:516-524 payload shape).
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("verbose".to_string(), json!(self.verbose));
        if let Some(format) = &self.format {
            map.insert("format".to_string(), json!(format));
        }
        map.insert("started".to_string(), json!(self.started));
        map.insert("status".to_string(), json!(self.status));
        map.insert(
            "progress".to_string(),
            crate::batch::progress_json(self.progress),
        );
        if let Some(phase) = &self.phase {
            map.insert("phase".to_string(), json!(phase));
        }
        if let Some(url) = &self.url {
            map.insert("url".to_string(), json!(url));
        }
        map.insert("spinnerTick".to_string(), json!(self.spinner_tick));
        Value::Object(map)
    }
}

/// `Fetching ${details.url ?? params.url ?? "URL"}...` (index.ts:520).
fn web_fetch_update_text(details: &WebFetchRenderDetails, params_url: Option<&str>) -> String {
    let url = details.url.as_deref().or(params_url).unwrap_or("URL");
    format!("Fetching {url}...")
}

/// One partial-frame push (content text + details JSON). The execute layer
/// binds this to the [`UpdateSink`] FFI; tests bind collectors.
type FramePush = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// The web_fetch event hooks (index.ts:540-564): every event re-renders the
/// latest frame and pushes it. `onStatusChange` keeps the current progress —
/// upstream's `latestDetails.progress ?? …` right side is dead code (the
/// initial state already carries `progress: 0`, never undefined).
fn web_fetch_hooks(
    latest: Arc<Mutex<WebFetchRenderDetails>>,
    push: FramePush,
    params_url: Option<String>,
) -> FetchExecutionHooks {
    let sink_for_status = Arc::clone(&push);
    let latest_for_status = Arc::clone(&latest);
    let params_url_for_status = params_url.clone();
    let on_status_change = Arc::new(move |status: &str| {
        let frame = {
            let mut details = latest_for_status.lock().unwrap_or_else(|e| e.into_inner());
            details.status = status.to_string();
            details.clone()
        };
        sink_for_status(
            &web_fetch_update_text(&frame, params_url_for_status.as_deref()),
            &frame.to_json(),
        );
    });
    let sink_for_progress = Arc::clone(&push);
    let latest_for_progress = Arc::clone(&latest);
    let params_url_for_progress = params_url;
    let on_progress_change = Arc::new(move |update: &crate::pipeline::FetchProgressUpdate| {
        let frame = {
            let mut details = latest_for_progress
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            details.status = update.status.to_string();
            details.progress = update.progress;
            details.phase = Some(update.phase.to_string());
            details.clone()
        };
        sink_for_progress(
            &web_fetch_update_text(&frame, params_url_for_progress.as_deref()),
            &frame.to_json(),
        );
    });
    FetchExecutionHooks {
        on_status_change: Some(on_status_change),
        on_progress_change: Some(on_progress_change),
    }
}

/// FR-P2-D: race the pipeline future against the 100ms spinner clock
/// (index.ts:528-537). Terminal state (`status === "done"`) stops the push
/// while the pipeline settles its final result. The first interval tick is
/// consumed pre-loop — tokio intervals fire immediately, `setInterval`
/// fires after the delay.
async fn run_web_fetch_with_spinner(
    mut fetch: std::pin::Pin<&mut dyn std::future::Future<Output = FetchOutcome>>,
    latest: Arc<Mutex<WebFetchRenderDetails>>,
    push: FramePush,
    params_url: Option<String>,
) -> FetchOutcome {
    use tokio::time::MissedTickBehavior;
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(SPINNER_INTERVAL_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval.tick().await;
    loop {
        tokio::select! {
            outcome = &mut fetch => return outcome,
            _ = interval.tick() => {
                let frame = {
                    let mut details = latest.lock().unwrap_or_else(|e| e.into_inner());
                    if details.status == "done" {
                        continue;
                    }
                    details.spinner_tick += 1;
                    details.clone()
                };
                push(
                    &web_fetch_update_text(&frame, params_url.as_deref()),
                    &frame.to_json(),
                );
            }
        }
    }
}

/// `web_fetch` execute (index.ts:489-626 + the FR-P2-B/D stream).
fn execute_web_fetch(params: &Value, state: &PluginState, tool_call_id: Option<&str>) -> Value {
    let params_url = params
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string);
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

    // FR-P2-B/D: the streaming layer exists only under a real dispatch id.
    let sink = tool_call_id.zip(state.channel()).map(|(id, channel)| {
        Arc::new(UpdateSink {
            call: channel.call,
            cookie: channel.cookie,
            tool_call_id: id.to_string(),
        })
    });
    let latest = Arc::new(Mutex::new(WebFetchRenderDetails::initial(
        verbose,
        Some(format.as_str().to_string()),
        params_url.clone(),
    )));
    let opts_url = opts.url.clone();

    let outcome = match &sink {
        None => {
            let pipeline = FetchPipeline::default();
            state.runtime.block_on(pipeline.fetch(&opts))
        }
        Some(sink) => {
            let push: FramePush = {
                let sink = Arc::clone(sink);
                Arc::new(move |text: &str, details: &Value| sink.push(text, details))
            };
            let hooks = web_fetch_hooks(Arc::clone(&latest), Arc::clone(&push), params_url.clone());
            let pipeline = FetchPipeline::with_hooks(hooks);
            let latest_for_spinner = Arc::clone(&latest);
            let push_for_spinner = Arc::clone(&push);
            state.runtime.block_on(async move {
                let mut fetch = Box::pin(pipeline.fetch(&opts));
                run_web_fetch_with_spinner(
                    fetch.as_mut(),
                    latest_for_spinner,
                    push_for_spinner,
                    params_url,
                )
                .await
            })
        }
    };

    // The final-frame spinnerTick (0 when the run never streamed — test
    // dispatches without an id) plus the latest status/phase. The error
    // frame echoes those instead of hardcoding error/error — upstream
    // spreads `latestDetails` (index.ts:576-577): an in-engine error has
    // already emitted {status:"error", phase:"error"} (extract.ts catch),
    // while a return-path error (URL validation and friends) never emits,
    // so the frame keeps the initial connecting/fetch_start state.
    let (final_spinner_tick, latest_status, latest_phase) = {
        let latest = latest.lock().unwrap_or_else(|e| e.into_inner());
        (
            latest.spinner_tick,
            latest.status.clone(),
            latest.phase.clone(),
        )
    };

    match outcome {
        FetchOutcome::Error(error) => {
            let error_text = format::build_fetch_error_response_text(&error);
            // Field-set note (index.ts:551-565): the upstream error details
            // literal carries only error/errorText/userErrorSummary/verbose/
            // status/phase/url/spinnerTick. rpi additionally keeps format/
            // maxChars/started/progress (1) — the rpi renderer reads
            // `progress` for the bar and TE08 accepted that shape; the two
            // extra fields are additive and no consumer branches on their
            // absence upstream.
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
                    "status": latest_status,
                    "progress": 1,
                    "phase": latest_phase,
                    "url": opts_url,
                    "spinnerTick": final_spinner_tick,
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
                    "spinnerTick": final_spinner_tick,
                },
            })
        }
    }
}

/// FR-P2-D batch variant (index.ts:710-720): re-push the latest snapshot
/// each tick unless the batch is complete (or none arrived yet).
async fn run_batch_with_spinner<T>(
    mut batch: std::pin::Pin<&mut dyn std::future::Future<Output = T>>,
    latest: Arc<Mutex<Option<crate::batch::BatchFetchProgressSnapshot>>>,
    spinner_tick: Arc<Mutex<u64>>,
    push: FramePush,
    verbose: bool,
) -> T {
    use tokio::time::MissedTickBehavior;
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(SPINNER_INTERVAL_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval.tick().await;
    loop {
        tokio::select! {
            outcome = &mut batch => return outcome,
            _ = interval.tick() => {
                let frame = {
                    let latest = latest.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(snapshot) = latest.as_ref() else {
                        continue;
                    };
                    if snapshot.completed >= snapshot.total {
                        continue;
                    }
                    snapshot.clone()
                };
                let tick = {
                    let mut tick = spinner_tick.lock().unwrap_or_else(|e| e.into_inner());
                    *tick += 1;
                    *tick
                };
                push(&batch_update_text(&frame), &batch_frame_details(&frame, verbose, tick));
            }
        }
    }
}

/// `Started batch fetch for ${total} URLs (${completed}/${total} complete).`
/// (index.ts:698).
fn batch_update_text(snapshot: &crate::batch::BatchFetchProgressSnapshot) -> String {
    format!(
        "Started batch fetch for {} URLs ({}/{} complete).",
        snapshot.total, snapshot.completed, snapshot.total
    )
}

/// The batch partial-frame details JSON (index.ts:701-707).
fn batch_frame_details(
    snapshot: &crate::batch::BatchFetchProgressSnapshot,
    verbose: bool,
    spinner_tick: u64,
) -> Value {
    let mut map = Map::new();
    map.insert("verbose".to_string(), json!(verbose));
    map.insert("started".to_string(), json!(true));
    map.insert("batchProgress".to_string(), snapshot.to_json());
    map.insert("spinnerTick".to_string(), json!(spinner_tick));
    Value::Object(map)
}

/// `batch_web_fetch` execute (index.ts:683-771 + the FR-P2-B/C/D stream).
fn execute_batch_web_fetch(
    params: &Value,
    state: &PluginState,
    tool_call_id: Option<&str>,
) -> Value {
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

    // FR-P2-B/D: same streaming gate as web_fetch — a real dispatch id.
    let sink = tool_call_id.zip(state.channel()).map(|(id, channel)| {
        Arc::new(UpdateSink {
            call: channel.call,
            cookie: channel.cookie,
            tool_call_id: id.to_string(),
        })
    });
    let latest: Arc<Mutex<Option<crate::batch::BatchFetchProgressSnapshot>>> =
        Arc::new(Mutex::new(None));
    let spinner_tick: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

    let batch_result = match &sink {
        None => {
            let pipeline = FetchPipeline::default();
            state.runtime.block_on(batch::execute_batch_entries(
                &pipeline,
                &parsed.requests,
                &defaults,
                None,
            ))
        }
        Some(sink) => {
            let push: FramePush = {
                let sink = Arc::clone(sink);
                Arc::new(move |text: &str, details: &Value| sink.push(text, details))
            };
            // The snapshot sink (index.ts:727-730): record + push per event.
            let latest_for_sink = Arc::clone(&latest);
            let push_for_progress = Arc::clone(&push);
            let spinner_tick_for_sink = Arc::clone(&spinner_tick);
            let on_progress: BatchProgressSink = Arc::new(move |snapshot| {
                {
                    let mut latest = latest_for_sink.lock().unwrap_or_else(|e| e.into_inner());
                    *latest = Some(snapshot.clone());
                }
                let tick = *spinner_tick_for_sink
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                push_for_progress(
                    &batch_update_text(snapshot),
                    &batch_frame_details(snapshot, verbose, tick),
                );
            });
            let pipeline = FetchPipeline::default();
            let latest_for_spinner = Arc::clone(&latest);
            let tick_for_spinner = Arc::clone(&spinner_tick);
            let push_for_spinner = Arc::clone(&push);
            state.runtime.block_on(async move {
                let mut batch = Box::pin(batch::execute_batch_entries(
                    &pipeline,
                    &parsed.requests,
                    &defaults,
                    Some(on_progress),
                ));
                run_batch_with_spinner(
                    batch.as_mut(),
                    latest_for_spinner,
                    tick_for_spinner,
                    push_for_spinner,
                    verbose,
                )
                .await
            })
        }
    };

    let final_spinner_tick = *spinner_tick.lock().unwrap_or_else(|e| e.into_inner());
    // index.ts:753-755: the agent text carries the FULL per-item headers.
    let response_text = batch::build_batch_fetch_response_text(&batch_result, true);
    let mut details = batch::batch_details_json(&batch_result, verbose);
    if let Some(details) = details.as_object_mut() {
        details.insert("spinnerTick".to_string(), json!(final_spinner_tick));
    }
    json!({
        "content": [{ "type": "text", "text": response_text }],
        "details": details,
    })
}

fn dispatch_message(message: &Value) -> Value {
    let Some(state) = STATE.get() else {
        return Value::Null;
    };
    match message.get("kind").and_then(Value::as_str) {
        // Render protocol (host_call.rs:245-281): synchronous, pure JSON —
        // never touches the plugin runtime (FR-P2-E).
        Some("render") => {
            let tree = match (
                message.get("what").and_then(Value::as_str),
                message.get("toolName").and_then(Value::as_str),
            ) {
                (Some("toolCall"), Some("batch_web_fetch")) => render::render_batch_call(
                    message
                        .get("context")
                        .and_then(|c| c.get("args"))
                        .unwrap_or(&Value::Null),
                ),
                (Some("toolCall"), _) => render::render_web_fetch_call(
                    message
                        .get("context")
                        .and_then(|c| c.get("args"))
                        .unwrap_or(&Value::Null),
                ),
                (Some("toolResult"), Some("batch_web_fetch")) => render::render_batch_result(
                    message.get("result").unwrap_or(&Value::Null),
                    message.get("options").unwrap_or(&Value::Null),
                    message.get("context").unwrap_or(&Value::Null),
                ),
                (Some("toolResult"), _) => render::render_web_fetch_result(
                    message.get("result").unwrap_or(&Value::Null),
                    message.get("options").unwrap_or(&Value::Null),
                    message.get("context").unwrap_or(&Value::Null),
                ),
                _ => Value::Null,
            };
            tree
        }
        Some("toolExecute") => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            // ADR-0015: the host forwards the toolCallId so `toolUpdate`
            // frames can address the in-flight execution (test seams may
            // omit it — the run then skips streaming).
            let tool_call_id = message
                .get("toolCallId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            match message.get("toolName").and_then(Value::as_str) {
                Some("batch_web_fetch") => execute_batch_web_fetch(&params, state, tool_call_id),
                _ => execute_web_fetch(&params, state, tool_call_id),
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
        build_batch_fetch_response_text, progress_by_status, resolve_batch_concurrency,
        resolve_fetch_tool_defaults,
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
    pub use crate::render::{
        render_batch_call, render_batch_result, render_web_fetch_call, render_web_fetch_result,
        truncate_middle, SPINNER_FRAMES,
    };
    pub use crate::settings::{normalize_settings, resolve_settings};
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details_with(url: Option<&str>) -> WebFetchRenderDetails {
        WebFetchRenderDetails {
            verbose: false,
            format: Some("markdown".to_string()),
            started: true,
            status: "connecting".to_string(),
            progress: 0.0,
            phase: Some("fetch_start".to_string()),
            url: url.map(str::to_string),
            spinner_tick: 0,
        }
    }

    /// index.ts:520 fallback chain: details.url → params.url → "URL".
    #[test]
    fn web_fetch_update_text_fallback_chain() {
        assert_eq!(
            web_fetch_update_text(
                &details_with(Some("https://a.example/")),
                Some("https://b.example/")
            ),
            "Fetching https://a.example/..."
        );
        assert_eq!(
            web_fetch_update_text(&details_with(None), Some("https://b.example/")),
            "Fetching https://b.example/..."
        );
        assert_eq!(
            web_fetch_update_text(&details_with(None), None),
            "Fetching URL..."
        );
    }

    /// index.ts:503-512: the initial frame state (never pushed itself —
    /// the pipeline's fetch_start event produces the first frame).
    #[test]
    fn initial_details_shape() {
        let details = details_with(Some("https://x.example/"));
        let json = details.to_json();
        assert_eq!(json.get("status"), Some(&json!("connecting")));
        assert_eq!(json.get("progress"), Some(&json!(0)));
        assert_eq!(json.get("phase"), Some(&json!("fetch_start")));
        assert_eq!(json.get("spinnerTick"), Some(&json!(0)));
        // partial frames carry no maxChars/fetchResult (final-frame only)
        assert!(json.get("maxChars").is_none());
        assert!(json.get("fetchResult").is_none());
    }

    /// index.ts:698: the batch onUpdate text.
    #[test]
    fn batch_update_text_template() {
        let snapshot = crate::batch::BatchFetchProgressSnapshot {
            items: Vec::new(),
            total: 3,
            completed: 1,
            succeeded: 1,
            failed: 0,
            batch_concurrency: 2,
        };
        assert_eq!(
            batch_update_text(&snapshot),
            "Started batch fetch for 3 URLs (1/3 complete)."
        );
    }

    /// index.ts:75: the push clock matches upstream SPINNER_INTERVAL_MS.
    #[test]
    fn spinner_interval_pinned() {
        assert_eq!(SPINNER_INTERVAL_MS, 100);
        assert_eq!(crate::render::SPINNER_FRAMES.len(), 10);
        assert_eq!(crate::render::SPINNER_FRAMES[0], "⠋");
        assert_eq!(crate::render::SPINNER_FRAMES[9], "⠏");
    }

    /// FR-P2-D (index.ts:528-537): 100ms ticks push `spinnerTick+1` frames
    /// while pending and STOP once `status === "done"` — deterministic under
    /// paused tokio time (a 250ms pending fetch sees exactly ticks 1 and 2;
    /// the done state at 250ms suppresses any further push before the
    /// outcome lands).
    #[tokio::test(start_paused = true)]
    async fn spinner_ticks_at_100ms_and_stops_on_done() {
        let latest = Arc::new(Mutex::new(WebFetchRenderDetails::initial(
            false,
            None,
            Some("https://ex.example/x".to_string()),
        )));
        let pushed: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let pushed_for_fn = Arc::clone(&pushed);
        let push: FramePush = Arc::new(move |text: &str, details: &Value| {
            let tick = details
                .get("spinnerTick")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            pushed_for_fn.lock().unwrap().push((text.to_string(), tick));
        });

        let latest_in_future = Arc::clone(&latest);
        let fetch = async move {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let mut details = latest_in_future.lock().unwrap();
            details.status = "done".to_string();
            details.progress = 1.0;
            details.phase = Some("done".to_string());
            FetchOutcome::Result(crate::types::FetchResult::content(
                "https://ex.example/x",
                "https://ex.example/x",
                1,
                "body",
                "chrome_145",
                "windows",
            ))
        };
        let mut fetch = Box::pin(fetch);
        let outcome = run_web_fetch_with_spinner(
            fetch.as_mut(),
            Arc::clone(&latest),
            Arc::clone(&push),
            Some("https://ex.example/x".to_string()),
        )
        .await;
        assert!(matches!(outcome, FetchOutcome::Result(_)));

        let frames = pushed.lock().unwrap().clone();
        assert_eq!(frames.len(), 2, "ticks at 100ms and 200ms only: {frames:?}");
        assert_eq!(frames[0].1, 1, "first tick increments to 1: {frames:?}");
        assert_eq!(frames[1].1, 2);
        for (text, _) in &frames {
            assert_eq!(text, "Fetching https://ex.example/x...");
        }
    }

    /// FR-P2-D batch variant (index.ts:710-720): ticks before the first
    /// snapshot arrive push nothing, and completed >= total suppresses the
    /// rest — a snapshot landing at 120ms and completing at 160ms leaves
    /// both the 100ms (no snapshot) and 200ms (complete) ticks silent.
    #[tokio::test(start_paused = true)]
    async fn batch_spinner_waits_for_snapshot_and_stops_on_complete() {
        let complete_snapshot = crate::batch::BatchFetchProgressSnapshot {
            items: Vec::new(),
            total: 1,
            completed: 1,
            succeeded: 1,
            failed: 0,
            batch_concurrency: 1,
        };
        let pending_snapshot = crate::batch::BatchFetchProgressSnapshot {
            completed: 0,
            ..complete_snapshot.clone()
        };
        let latest: Arc<Mutex<Option<crate::batch::BatchFetchProgressSnapshot>>> =
            Arc::new(Mutex::new(None));
        let pushed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let pushed_for_fn = Arc::clone(&pushed);
        let push: FramePush = Arc::new(move |_text, details| {
            let tick = details
                .get("spinnerTick")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            pushed_for_fn.lock().unwrap().push(tick);
        });
        let spinner_tick: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

        let latest_in_future = Arc::clone(&latest);
        let batch = async move {
            // 120ms: first snapshot arrives — the 100ms tick found no
            // snapshot and pushed nothing.
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            *latest_in_future.lock().unwrap() = Some(pending_snapshot.clone());
            // 160ms: the batch completes — the 200ms tick is suppressed.
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            *latest_in_future.lock().unwrap() = Some(complete_snapshot.clone());
            42
        };
        let mut batch = Box::pin(batch);
        let outcome = run_batch_with_spinner(
            batch.as_mut(),
            Arc::clone(&latest),
            Arc::clone(&spinner_tick),
            Arc::clone(&push),
            false,
        )
        .await;
        assert_eq!(outcome, 42);

        let ticks = pushed.lock().unwrap().clone();
        assert!(
            ticks.is_empty(),
            "no spinner frame before the first snapshot or after completion: {ticks:?}"
        );
        assert_eq!(*spinner_tick.lock().unwrap(), 0);
    }
}
