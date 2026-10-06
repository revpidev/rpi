//! Test-only fake host: scripted replies in call order, per-method
//! fallbacks, and a full call recording.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::{HostCall, HostError};

/// A scripted reply.
pub type Reply = Result<Value, HostError>;

/// Success helper.
pub fn fake_reply(value: Value) -> Reply {
    Ok(value)
}

/// Scripted host call recorder.
#[derive(Default)]
pub struct FakeHost {
    queued: Mutex<VecDeque<(String, Reply)>>,
    fallback: Mutex<HashMap<String, Reply>>,
    recorded: Mutex<Vec<(String, Value)>>,
}

impl FakeHost {
    /// New empty host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one expected call (method + reply) in order. The queue is
    /// consulted before the per-method fallbacks.
    pub fn push(&self, method: &str, reply: Reply) {
        self.queued
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back((method.to_owned(), reply));
    }

    /// Set a per-method fallback used when the queue head does not match.
    pub fn set(&self, method: &str, reply: Reply) {
        self.fallback
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(method.to_owned(), reply);
    }

    /// Every recorded call, in order.
    pub fn recorded(&self) -> Vec<(String, Value)> {
        self.recorded
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Recorded methods only.
    pub fn methods(&self) -> Vec<String> {
        self.recorded()
            .into_iter()
            .map(|(method, _)| method)
            .collect()
    }

    /// Whether a method was called at least once.
    pub fn called(&self, method: &str) -> bool {
        self.methods().iter().any(|entry| entry == method)
    }

    /// The args of the first call to `method`.
    pub fn args_of(&self, method: &str) -> Option<Value> {
        self.recorded()
            .into_iter()
            .find(|(name, _)| name == method)
            .map(|(_, args)| args)
    }
}

impl HostCall for FakeHost {
    fn call(&self, method: &str, args: Value) -> Reply {
        self.recorded
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((method.to_owned(), args.clone()));
        {
            let mut queued = self
                .queued
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some((front, _)) = queued.front()
                && front == method
            {
                let (_, reply) = queued.pop_front().expect("checked front");
                return reply;
            }
        }
        self.fallback
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(method)
            .cloned()
            .unwrap_or_else(|| {
                Err(HostError {
                    kind: "unknownMethod".to_owned(),
                    message: format!("unexpected host call: {method}"),
                })
            })
    }
}

// ---------------------------------------------------------------------------
// Session-modelling fake (state machine tests)
// ---------------------------------------------------------------------------

/// A fake that models the host session state the plugin mutates: permission
/// mode, registered tools (natural + override exposure), active set, UI
/// dialogs, widgets, messages. Every handled call is recorded.
pub struct SessionFakeHost {
    state: Mutex<SessionState>,
}

#[derive(Clone)]
struct SessionState {
    mode: String,
    session_id: String,
    cwd: PathBuf,
    has_ui: bool,
    ctx_mode: String,
    session_path: Option<String>,
    tools: Vec<(String, String)>,
    overrides: HashMap<String, String>,
    active: Vec<String>,
    widget: Option<Vec<String>>,
    notifications: Vec<String>,
    user_messages: Vec<String>,
    user_message_async: Vec<(bool, String)>,
    selects: VecDeque<Value>,
    inputs: VecDeque<Value>,
    editor_texts: VecDeque<Value>,
    editor_errors: bool,
    /// Methods answered with a transport failure (test injection).
    failed_methods: Vec<String>,
    registered: Vec<Value>,
    events: Vec<String>,
    calls: Vec<(String, Value)>,
}

/// A snapshot of the fake session for assertions.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionView {
    /// Current permission mode.
    pub mode: String,
    /// Effective exposure per registered tool.
    pub exposures: Vec<(String, String)>,
    /// Current active set.
    pub active: Vec<String>,
    /// Current widget content (`None` = removed).
    pub widget: Option<Vec<String>>,
    /// Recorded `ui.notify` messages.
    pub notifications: Vec<String>,
    /// Recorded `sendUserMessage` contents.
    pub user_messages: Vec<String>,
    /// Recorded `sendUserMessage` `(deliverAsFollowUp, content)`.
    pub user_message_options: Vec<(bool, String)>,
    /// Registered tool definitions.
    pub registered_tools: Vec<Value>,
    /// Subscribed events.
    pub events: Vec<String>,
}

impl SessionFakeHost {
    /// New host in `default` mode with a plain tool surface.
    pub fn new() -> Self {
        let tools = vec![
            ("read".to_owned(), "direct".to_owned()),
            ("edit".to_owned(), "direct".to_owned()),
            ("write".to_owned(), "direct".to_owned()),
            ("bash".to_owned(), "direct".to_owned()),
        ];
        let active = vec!["read".to_owned(), "edit".to_owned(), "bash".to_owned()];
        Self {
            state: Mutex::new(SessionState {
                mode: "default".to_owned(),
                session_id: "s-1".to_owned(),
                cwd: PathBuf::from("/work/cwd"),
                has_ui: true,
                ctx_mode: "tui".to_owned(),
                session_path: None,
                tools,
                overrides: HashMap::new(),
                active,
                widget: None,
                notifications: Vec::new(),
                user_messages: Vec::new(),
                user_message_async: Vec::new(),
                selects: VecDeque::new(),
                inputs: VecDeque::new(),
                editor_texts: VecDeque::new(),
                editor_errors: false,
                failed_methods: Vec::new(),
                registered: Vec::new(),
                events: Vec::new(),
                calls: Vec::new(),
            }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut SessionState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        f(&mut state)
    }

    /// Snapshot the session for assertions.
    pub fn view(&self) -> SessionView {
        self.with(|state| SessionView {
            mode: state.mode.clone(),
            exposures: state
                .tools
                .iter()
                .map(|(name, natural)| {
                    let exposure = state
                        .overrides
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| natural.clone());
                    (name.clone(), exposure)
                })
                .collect(),
            active: state.active.clone(),
            widget: state.widget.clone(),
            notifications: state.notifications.clone(),
            user_messages: state.user_messages.clone(),
            user_message_options: state.user_message_async.clone(),
            registered_tools: state.registered.clone(),
            events: state.events.clone(),
        })
    }

    /// Recorded container-level calls.
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.with(|state| state.calls.clone())
    }

    /// Recorded methods.
    pub fn methods(&self) -> Vec<String> {
        self.calls().into_iter().map(|(method, _)| method).collect()
    }

    /// Change the host permission mode (models a keybinding/session event).
    pub fn set_mode(&self, mode: &str) {
        self.with(|state| state.mode = mode.to_owned());
    }

    /// Register a new tool (models a late `registerTool`).
    pub fn register_tool(&self, name: &str, exposure: &str, active: bool) {
        self.with(|state| {
            state.tools.push((name.to_owned(), exposure.to_owned()));
            if active {
                state.active.push(name.to_owned());
            }
        });
    }

    /// Set the session id (models a session rebind).
    pub fn set_session_id(&self, id: &str) {
        self.with(|state| state.session_id = id.to_owned());
    }

    /// Enable/disable `ctx.hasUI`.
    pub fn set_has_ui(&self, has_ui: bool) {
        self.with(|state| state.has_ui = has_ui);
    }

    /// Set `ctx.mode`.
    pub fn set_ctx_mode(&self, mode: &str) {
        self.with(|state| state.ctx_mode = mode.to_owned());
    }

    /// Ask the next `ui.select` to answer `answer`.
    pub fn queue_select(&self, answer: Value) {
        self.with(|state| state.selects.push_back(answer));
    }

    /// Ask the next `ui.input` to answer `answer`.
    pub fn queue_input(&self, answer: Value) {
        self.with(|state| state.inputs.push_back(answer));
    }

    /// Ask the next `ui.editExternal` to answer `answer`.
    pub fn queue_edit_external(&self, answer: Value) {
        self.with(|state| state.editor_texts.push_back(answer));
    }

    /// Make `ui.editExternal` fail with `unknownMethod`.
    pub fn fail_edit_external(&self) {
        self.with(|state| state.editor_errors = true);
    }

    /// Make one host method answer with a transport-shaped failure (models
    /// a transient host-call outage).
    pub fn fail_method(&self, method: &str) {
        self.with(|state| state.failed_methods.push(method.to_owned()));
    }

    /// Clear every injected method failure.
    pub fn clear_failed_methods(&self) {
        self.with(|state| state.failed_methods.clear());
    }
}

impl Default for SessionFakeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl HostCall for SessionFakeHost {
    fn call(&self, method: &str, args: Value) -> Reply {
        self.with(|state| {
            state.calls.push((method.to_owned(), args.clone()));
            if state.failed_methods.iter().any(|failed| failed == method) {
                return Err(HostError {
                    kind: "transport".to_owned(),
                    message: format!("{method} failed (test injection)"),
                });
            }
            match method {
                "ctx.sessionFile" => Ok(json!({
                    "path": state.session_path,
                    "id": state.session_id,
                })),
                "ctx.cwd" => Ok(json!(state.cwd.to_string_lossy())),
                "ctx.mode" => Ok(json!(state.ctx_mode)),
                "ctx.hasUI" => Ok(json!(state.has_ui)),
                "getMode" => Ok(json!(state.mode)),
                "setMode" => {
                    let mode = args
                        .get("mode")
                        .and_then(Value::as_str)
                        .unwrap_or("default");
                    if mode == "plan" || mode == "default" {
                        state.mode = mode.to_owned();
                    }
                    Ok(Value::Null)
                }
                "getAllTools" => {
                    let tools: Vec<Value> = state
                        .tools
                        .iter()
                        .map(|(name, natural)| {
                            let exposure = state
                                .overrides
                                .get(name)
                                .cloned()
                                .unwrap_or_else(|| natural.clone());
                            json!({"name": name, "exposure": exposure})
                        })
                        .collect();
                    Ok(Value::Array(tools))
                }
                "getActiveTools" => Ok(json!(state.active)),
                "setActiveTools" => {
                    let names: Vec<String> = args
                        .get("toolNames")
                        .and_then(|value| serde_json::from_value(value.clone()).ok())
                        .unwrap_or_default();
                    // `apply_tool_loadout`: unknown and hidden names drop.
                    let mut next: Vec<String> = Vec::new();
                    for name in names {
                        let exposure = state.tools.iter().find(|(tool, _)| *tool == name).map(
                            |(_, natural)| {
                                state
                                    .overrides
                                    .get(&name)
                                    .cloned()
                                    .unwrap_or_else(|| natural.clone())
                            },
                        );
                        if exposure.as_deref().is_some_and(|exp| exp != "hidden")
                            && !next.contains(&name)
                        {
                            next.push(name);
                        }
                    }
                    state.active = next;
                    Ok(Value::Null)
                }
                "setToolExposures" => {
                    let exposures = args
                        .get("exposures")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let mut updated = Vec::new();
                    for (name, exposure) in exposures {
                        if state.tools.iter().any(|(tool, _)| *tool == name) {
                            state.overrides.insert(
                                name.clone(),
                                exposure.as_str().unwrap_or("direct").to_owned(),
                            );
                            updated.push(Value::String(name));
                        }
                    }
                    Ok(json!({ "updated": updated }))
                }
                "clearToolExposures" => {
                    let names: Vec<String> = args
                        .get("names")
                        .and_then(|value| serde_json::from_value(value.clone()).ok())
                        .unwrap_or_default();
                    let mut cleared = Vec::new();
                    for name in names {
                        if state.overrides.remove(&name).is_some() {
                            cleared.push(Value::String(name));
                        }
                    }
                    Ok(json!({ "cleared": cleared }))
                }
                "ui.setWidget" => {
                    let content = args.get("content").cloned().unwrap_or(Value::Null);
                    state.widget = if content.is_null() {
                        None
                    } else {
                        content.as_array().map(|lines| {
                            lines
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                    };
                    Ok(Value::Null)
                }
                "ui.notify" => {
                    if let Some(message) = args.get("message").and_then(Value::as_str) {
                        state.notifications.push(message.to_owned());
                    }
                    Ok(Value::Null)
                }
                "ui.select" => Ok(state.selects.pop_front().unwrap_or(Value::Null)),
                "ui.input" => Ok(state.inputs.pop_front().unwrap_or(Value::Null)),
                "ui.editExternal" => {
                    if state.editor_errors {
                        return Err(HostError {
                            kind: "unknownMethod".to_owned(),
                            message: "ui.editExternal is not supported by this host mode"
                                .to_owned(),
                        });
                    }
                    Ok(json!({ "text": state.editor_texts.pop_front().unwrap_or(Value::Null) }))
                }
                "ui.editor" => Ok(state.editor_texts.pop_front().unwrap_or(Value::Null)),
                "sendUserMessage" => {
                    if let Some(content) = args.get("content").and_then(Value::as_str) {
                        state.user_messages.push(content.to_owned());
                        let follow_up = args
                            .get("options")
                            .and_then(|options| options.get("deliverAs"))
                            .and_then(Value::as_str)
                            == Some("followUp");
                        state
                            .user_message_async
                            .push((follow_up, content.to_owned()));
                    }
                    Ok(Value::Null)
                }
                "registerTool" => {
                    state.registered.push(args.clone());
                    Ok(Value::Null)
                }
                "registerCommand" => Ok(Value::Null),
                "on" => {
                    if let Some(event) = args.get("event").and_then(Value::as_str) {
                        state.events.push(event.to_owned());
                    }
                    Ok(Value::Null)
                }
                _ => Err(HostError {
                    kind: "unknownMethod".to_owned(),
                    message: format!("unexpected host call: {method}"),
                }),
            }
        })
    }
}
// ---------------------------------------------------------------------------
// Temp directory helper
// ---------------------------------------------------------------------------

/// A self-cleaning temporary directory for file-write tests.
pub struct TestDir(PathBuf);

impl TestDir {
    /// Create a unique directory under the system temp root.
    pub fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rpi-plan-mode-{tag}-{}-{nanos}",
            std::process::id()
        ));
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
