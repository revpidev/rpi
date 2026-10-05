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
    registrations: Vec<(String, String)>,
    register_error: Option<String>,
    method_errors: HashMap<String, String>,
    statuses: Vec<(String, Option<String>)>,
    notifications: Vec<String>,
    fetch_calls: Vec<(String, bool)>,
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
                registrations: Vec::new(),
                register_error: None,
                method_errors: HashMap::new(),
                statuses: Vec::new(),
                notifications: Vec::new(),
                fetch_calls: Vec::new(),
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

    /// Make `ctx.usage.fetch` answer `null` (failure) for a provider.
    pub fn fail_fetch(&self, provider: &str) {
        self.with(|state| {
            state.failing.insert(provider.to_owned());
        });
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
    pub fn notifications(&self) -> Vec<String> {
        self.with(|state| state.notifications.clone())
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
