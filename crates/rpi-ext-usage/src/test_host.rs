//! Test-only fakes: a usage-provider host with scripted `ctx.usage.*`,
//! `ctx.model`, UI records; plus a self-cleaning temp directory.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::{HostCall, HostError};

/// Scripted usage-framework host.
pub struct UsageFakeHost {
    state: Mutex<FakeState>,
}

struct FakeState {
    has_ui: bool,
    model: Value,
    providers: Vec<String>,
    envelopes: HashMap<String, Value>,
    failing: HashSet<String>,
    failure_results: HashMap<String, Value>,
    session_stats: Value,
    unsupported_structured: bool,
    registrations: Vec<(String, String)>,
    register_error: Option<String>,
    method_errors: HashMap<String, String>,
    statuses: Vec<(String, Option<String>)>,
    notifications: Vec<String>,
    structured_texts: Vec<String>,
    structured_reports: Vec<Value>,
    fetch_calls: Vec<(String, bool)>,
    cached_calls: Vec<String>,
    calls: Vec<(String, Value)>,
}

impl UsageFakeHost {
    /// A UI-capable host with no model, provider, or envelope.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FakeState {
                has_ui: true,
                model: Value::Null,
                providers: Vec::new(),
                envelopes: HashMap::new(),
                failing: HashSet::new(),
                failure_results: HashMap::new(),
                session_stats: Value::Null,
                unsupported_structured: false,
                registrations: Vec::new(),
                register_error: None,
                method_errors: HashMap::new(),
                statuses: Vec::new(),
                notifications: Vec::new(),
                structured_texts: Vec::new(),
                structured_reports: Vec::new(),
                fetch_calls: Vec::new(),
                cached_calls: Vec::new(),
                calls: Vec::new(),
            }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        f(&mut state)
    }

    /// `ctx.hasUI`.
    pub fn set_has_ui(&self, has_ui: bool) {
        self.with(|state| state.has_ui = has_ui);
    }

    /// `ctx.model` JSON.
    pub fn set_model(&self, model: Value) {
        self.with(|state| state.model = model);
    }

    /// `ctx.usage.listProviders()` answer.
    pub fn set_providers(&self, providers: Vec<String>) {
        self.with(|state| state.providers = providers);
    }

    /// Successful `ctx.usage.fetch` envelope for a provider.
    pub fn set_envelope(&self, provider: &str, envelope: Value) {
        self.with(|state| {
            state.envelopes.insert(provider.to_owned(), envelope);
        });
    }

    /// Make `ctx.usage.fetch` / `fetchDetailed` fail (no envelope) for a
    /// provider.
    pub fn fail_fetch(&self, provider: &str) {
        self.with(|state| {
            state.failing.insert(provider.to_owned());
        });
    }

    /// Script exact `UsageFetchResult` JSON for a provider's detailed/cached
    /// calls (stale, error combinations).
    pub fn set_failure(&self, provider: &str, result: Value) {
        self.with(|state| {
            state.failure_results.insert(provider.to_owned(), result);
        });
    }

    /// `ctx.sessionStats` answer.
    pub fn set_session_stats(&self, stats: Value) {
        self.with(|state| state.session_stats = stats);
    }

    /// Make `ui.emitStructured` answer `unsupported-output` (NullUi).
    pub fn set_structured_unsupported(&self) {
        self.with(|state| state.unsupported_structured = true);
    }

    /// Make one host method fail with a scripted error.
    pub fn set_method_error(&self, method: &str, message: &str) {
        self.with(|state| {
            state
                .method_errors
                .insert(method.to_owned(), message.to_owned());
        });
    }

    /// Recorded `ui.setStatus` calls.
    pub fn statuses(&self) -> Vec<(String, Option<String>)> {
        self.with(|state| state.statuses.clone())
    }

    /// Recorded `ui.notify` messages.
    #[allow(dead_code)]
    pub fn notifications(&self) -> Vec<String> {
        self.with(|state| state.notifications.clone())
    }

    /// Recorded `ui.emitStructured usageText` payloads (actual emissions).
    pub fn structured_texts(&self) -> Vec<String> {
        self.with(|state| state.structured_texts.clone())
    }

    /// Recorded `ui.emitStructured usageReport` payloads (actual emissions).
    #[allow(dead_code)]
    pub fn structured_reports(&self) -> Vec<Value> {
        self.with(|state| state.structured_reports.clone())
    }

    /// Recorded `ctx.usage.cached` providers.
    pub fn cached_calls(&self) -> Vec<String> {
        self.with(|state| state.cached_calls.clone())
    }

    /// Recorded `ctx.usage.register` pairs.
    pub fn registrations(&self) -> Vec<(String, String)> {
        self.with(|state| state.registrations.clone())
    }

    /// Recorded `(provider, force)` fetch calls.
    pub fn fetch_calls(&self) -> Vec<(String, bool)> {
        self.with(|state| state.fetch_calls.clone())
    }

    /// Fetch count for one provider.
    pub fn fetch_count(&self, provider: &str) -> usize {
        self.with(|state| {
            state
                .fetch_calls
                .iter()
                .filter(|(name, _)| name == provider)
                .count()
        })
    }

    /// Every recorded call.
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.with(|state| state.calls.clone())
    }

    /// Whether a method was called at least once.
    pub fn called(&self, method: &str) -> bool {
        self.with(|state| state.calls.iter().any(|(name, _)| name == method))
    }
}

impl Default for UsageFakeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl HostCall for UsageFakeHost {
    fn call(&self, method: &str, args: Value) -> Result<Value, HostError> {
        self.with(|state| {
            state.calls.push((method.to_owned(), args.clone()));
            if let Some(message) = state.method_errors.get(method) {
                return Err(HostError {
                    kind: "invalidRequest".to_owned(),
                    message: message.clone(),
                });
            }
            match method {
                "ctx.hasUI" => Ok(json!(state.has_ui)),
                "ctx.model" => Ok(state.model.clone()),
                "ctx.usage.listProviders" => Ok(json!(state.providers)),
                "ctx.usage.fetch" => {
                    let provider = args
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
                    state.fetch_calls.push((provider.clone(), force));
                    if state.failing.contains(&provider) {
                        return Ok(Value::Null);
                    }
                    Ok(state
                        .envelopes
                        .get(&provider)
                        .cloned()
                        .unwrap_or(Value::Null))
                }
                "ctx.usage.fetchDetailed" => {
                    let provider = args
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
                    state.fetch_calls.push((provider.clone(), force));
                    if let Some(result) = state.failure_results.get(&provider) {
                        return Ok(result.clone());
                    }
                    if state.failing.contains(&provider) {
                        return Ok(json!({
                            "stale": false,
                            "error": {
                                "kind": "transport",
                                "action": "Check the network and provider status, then retry."
                            }
                        }));
                    }
                    match state.envelopes.get(&provider) {
                        Some(envelope) => Ok(json!({
                            "envelope": envelope,
                            "source": "network",
                            "stale": false
                        })),
                        None => Ok(json!({
                            "stale": false,
                            "error": {"kind": "no-data", "action": "This provider has no usage data."}
                        })),
                    }
                }
                "ctx.usage.cached" => {
                    let provider = args
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    state.cached_calls.push(provider.clone());
                    if let Some(result) = state.failure_results.get(&provider) {
                        return Ok(result.clone());
                    }
                    if state.failing.contains(&provider) {
                        return Ok(json!({
                            "stale": false,
                            "error": {"kind": "cache-miss", "action": "Run /usage without --cached to fetch fresh data."}
                        }));
                    }
                    match state.envelopes.get(&provider) {
                        Some(envelope) => Ok(json!({
                            "envelope": envelope,
                            "source": "cache",
                            "stale": false
                        })),
                        None => Ok(json!({
                            "stale": false,
                            "error": {"kind": "cache-miss", "action": "Run /usage without --cached to fetch fresh data."}
                        })),
                    }
                }
                "ctx.sessionStats" => Ok(state.session_stats.clone()),
                "ctx.usage.register" => {
                    if let Some(message) = &state.register_error {
                        return Err(HostError {
                            kind: "invalidRequest".to_owned(),
                            message: message.clone(),
                        });
                    }
                    let provider = args
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let path = args
                        .get("scriptPath")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    state.registrations.push((provider, path));
                    Ok(Value::Null)
                }
                "ui.setStatus" => {
                    let key = args
                        .get("key")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let text = args.get("text").and_then(Value::as_str).map(str::to_owned);
                    state.statuses.push((key, text));
                    Ok(Value::Null)
                }
                "ui.notify" => {
                    if let Some(message) = args.get("message").and_then(Value::as_str) {
                        state.notifications.push(message.to_owned());
                    }
                    Ok(Value::Null)
                }
                "ui.emitStructured" => {
                    if state.unsupported_structured {
                        return Err(HostError {
                            kind: "call".to_owned(),
                            message: "unsupported-output".to_owned(),
                        });
                    }
                    let validate_only =
                        args.get("validateOnly").and_then(Value::as_bool).unwrap_or(false);
                    if validate_only {
                        return Ok(Value::Null);
                    }
                    let method = args.get("method").and_then(Value::as_str).unwrap_or_default();
                    let data = args.get("data").cloned().unwrap_or(Value::Null);
                    match method {
                        "usageText" => {
                            if let Some(text) = data.as_str() {
                                state.structured_texts.push(text.to_owned());
                            }
                            Ok(Value::Null)
                        }
                        "usageReport" => {
                            state.structured_reports.push(data);
                            Ok(Value::Null)
                        }
                        _ => Err(HostError {
                            kind: "invalidRequest".to_owned(),
                            message: format!("unexpected structured method: {method}"),
                        }),
                    }
                }
                "registerCommand" | "on" => Ok(Value::Null),
                _ => Err(HostError {
                    kind: "unknownMethod".to_owned(),
                    message: format!("unexpected host call: {method}"),
                }),
            }
        })
    }
}

/// A self-cleaning temporary directory for file-write tests.
pub struct TestDir(PathBuf);

impl TestDir {
    /// Create a unique directory under the system temp root.
    pub fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("rpi-usage-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");
        TestDir(dir)
    }

    /// The directory path.
    pub fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
