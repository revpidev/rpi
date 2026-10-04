//! Virtual models (V16-12, R3.11): catalog entries that route each request
//! to a physical model.
//!
//! Port of `packages/coding-agent/src/core/virtual-models.ts` @ `540e174c7`
//! (upstream v0.99.0; zero change in the v1.0.0 interval).
//!
//! The selection (`model_change`, agent state, `ctx.model`) may name a
//! virtual model. Everything below the routing step only sees physical
//! models: providers stream them and assistant messages record them. A
//! virtual model never reaches a provider. Virtual models belong to a
//! provider id but are not provider models; [`crate::core::model_runtime`]
//! keeps them separately and adds them to the provider's catalog through
//! [`crate::core::model_runtime::VirtualModelProvider`].
//!
//! Brand note (V16-12 §8-6 decision, de-pi policy of ADR-0001): the `api`
//! value is `rpi-virtual`. `rpi-ai`'s `ApiKind` normalization maps the
//! upstream `pi-virtual` spelling to `rpi-virtual` on every input path
//! (legacy alias, [`is_virtual_model`] also accepts the legacy spelling as
//! a defensive fallback). The session-format tag `pi.virtual-model-state`
//! intentionally keeps the upstream literal — rpi session format tags match
//! upstream byte-for-byte (coding-agent `session.rs` red line).

use rpi_agent::messages::AgentMessage;
use rpi_agent::session::{CustomEntry, ModelChangeEntry, SessionEntry};
use rpi_ai::types::{
    ApiKind, AssistantMessage, InputModality, Message, Model, ModelCost, ModelThinkingLevel,
    StopReason, ThinkingLevelMap,
};
use serde_json::{Value, json};

/// API id of virtual catalog entries. Requests for it fail unless routed
/// first (`virtual-models.ts:29`; upstream value `pi-virtual`).
pub const VIRTUAL_MODEL_API: &str = "rpi-virtual";

/// Legacy `api` spelling accepted on input (brand rename, §8-6).
pub const VIRTUAL_MODEL_API_ALIAS: &str = "pi-virtual";

/// Custom entry type that stores router state on the session branch
/// (`virtual-models.ts:32`; upstream literal kept — session format red
/// line).
pub const VIRTUAL_MODEL_STATE_ENTRY: &str = "pi.virtual-model-state";

/// Every thinking level, in upstream `EXTENDED_THINKING_LEVELS` order
/// (`models.ts:1219`).
const THINKING_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

/// Why a request is being routed (`virtual-models.ts:44-50`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelRouteReason {
    /// First request after a message the user wrote (prompt, steering, or
    /// follow-up).
    User,
    /// Any other request in the agent loop, e.g. after tool results or
    /// extension messages.
    Continuation,
    /// Automatic retry after a failed request, including after compaction
    /// for a context overflow.
    Retry,
    /// A request outside the agent loop, e.g. a compaction summary or an
    /// extension call.
    Direct,
}

impl ModelRouteReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelRouteReason::User => "user",
            ModelRouteReason::Continuation => "continuation",
            ModelRouteReason::Retry => "retry",
            ModelRouteReason::Direct => "direct",
        }
    }
}

/// `VirtualModelDefinition` (extension registration surface,
/// `virtual-models.ts:84-102`; extension variant `types.ts:1865-1873`).
///
/// The `route` callback does not cross the JSON host boundary; it is
/// carried separately as a host-side callback (see
/// `rpi-ext-host`'s `VirtualModelRouteFn`).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualModelDefinition {
    /// Provider the virtual model is listed under. May be a provider with
    /// physical models.
    pub provider: String,
    /// Model id. Must not be the id of a physical model of `provider`.
    pub id: String,
    pub name: String,
    /// Thinking levels offered for selection. Defaults to `["off"]`.
    #[serde(default)]
    pub thinking_levels: Option<Vec<ModelThinkingLevel>>,
    /// Limits shown before the first response. Unset limits are unknown (0).
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Input types accepted for selection. Defaults to text and images.
    #[serde(default)]
    pub input: Option<Vec<InputModality>>,
}

/// Data of a `pi.virtual-model-state` custom entry
/// (`virtual-models.ts:35-39`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualModelStateData {
    pub provider: String,
    pub model_id: String,
    pub state: Value,
}

/// Output shape of a route callback (`virtual-models.ts:73-82`).
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualModelRoute {
    /// Provider of the routed physical model.
    pub provider: String,
    /// Id of the routed physical model.
    pub id: String,
    /// Requested thinking level for the routed model (host clamps it).
    pub thinking_level: ModelThinkingLevel,
    /// New router state. `None` and "same as the request state" both mean
    /// keep the current state (`virtual-models.ts:75-80`).
    pub state: Option<Value>,
}

/// Whether a model is a virtual catalog entry (`virtual-models.ts:105-107`).
pub fn is_virtual_model(model: &Model) -> bool {
    model.api.as_str() == VIRTUAL_MODEL_API || model.api.as_str() == VIRTUAL_MODEL_API_ALIAS
}

/// Latest successful response in `messages` (`virtual-models.ts:110-118`).
/// Failed, aborted, and failed-routing requests are skipped.
pub fn find_latest_response(messages: &[Message]) -> Option<&AssistantMessage> {
    for message in messages.iter().rev() {
        if let Message::Assistant(assistant) = message
            && assistant.stop_reason != StopReason::Error
            && assistant.stop_reason != StopReason::Aborted
        {
            return Some(assistant);
        }
    }
    None
}

/// Latest successful assistant response in agent messages (the
/// `previous`/`routedModel` source when the agent state — not the LLM
/// projection — is at hand). Mirrors [`find_latest_response`] over
/// [`AgentMessage`].
pub fn find_latest_agent_response(messages: &[AgentMessage]) -> Option<&AssistantMessage> {
    for message in messages.iter().rev() {
        if let AgentMessage::Assistant(assistant) = message
            && assistant.stop_reason != StopReason::Error
            && assistant.stop_reason != StopReason::Aborted
        {
            return Some(assistant);
        }
    }
    None
}

/// Build the catalog entry of a virtual model (`virtual-models.ts:170-188`).
pub fn create_virtual_model(definition: &VirtualModelDefinition) -> Model {
    let levels: Vec<ModelThinkingLevel> = definition
        .thinking_levels
        .clone()
        .unwrap_or_else(|| vec![ModelThinkingLevel::Off]);
    let mut thinking_level_map = ThinkingLevelMap::new();
    for level in THINKING_LEVELS {
        thinking_level_map.insert(
            level,
            levels.contains(&level).then(|| level.as_str().to_owned()),
        );
    }
    Model {
        id: definition.id.clone(),
        name: definition.name.clone(),
        api: ApiKind::from(VIRTUAL_MODEL_API),
        provider: definition.provider.clone(),
        base_url: String::new(),
        reasoning: levels.iter().any(|level| *level != ModelThinkingLevel::Off),
        thinking_level_map: Some(thinking_level_map),
        input: definition
            .input
            .clone()
            .unwrap_or_else(|| vec![InputModality::Text, InputModality::Image]),
        input_limits: None,
        cost: ModelCost::default(),
        prompt_cache: None,
        context_window: definition.context_window.unwrap_or(0),
        max_tokens: definition.max_tokens.unwrap_or(0),
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// Parse a route callback result (`virtual-models.ts:73-82`, resolved
/// through `getPhysicalModel` at `model-runtime.ts:1013-1024`). Only the
/// routed model's provider/id matter here; every other model field is
/// ignored like upstream.
pub fn parse_route_result(value: &Value) -> Result<VirtualModelRoute, String> {
    let model = value
        .get("model")
        .ok_or_else(|| "route() must return a model".to_owned())?;
    let provider = model
        .get("provider")
        .and_then(Value::as_str)
        .filter(|provider| !provider.is_empty())
        .ok_or_else(|| "route() model must name a provider".to_owned())?
        .to_owned();
    let id = model
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "route() model must name a model id".to_owned())?
        .to_owned();
    let thinking_level = match value.get("thinkingLevel") {
        None | Some(Value::Null) => ModelThinkingLevel::Off,
        Some(level) => serde_json::from_value(level.clone())
            .map_err(|_| format!("route() returned an unknown thinking level {level}"))?,
    };
    let state = value.get("state").cloned().filter(|state| !state.is_null());
    Ok(VirtualModelRoute {
        provider,
        id,
        thinking_level,
        state,
    })
}

/// JSON request handed to a host-side route callback
/// (`virtual-models.ts:52-70`). `signal` cannot cross the JSON boundary and
/// is omitted. `previous`/`failed` carry the physical models the assistant
/// messages name (upstream passes the resolved `Model` objects).
#[allow(clippy::too_many_arguments)]
pub fn route_request_json(
    model: &Model,
    thinking_level: ModelThinkingLevel,
    reason: ModelRouteReason,
    previous: Option<(&Model, Option<ModelThinkingLevel>)>,
    failed: Option<(&Model, Option<ModelThinkingLevel>, &AssistantMessage)>,
    state: Option<&Value>,
    messages: &[Message],
) -> Value {
    let mut request = json!({
        "model": model,
        "thinkingLevel": thinking_level.as_str(),
        "reason": reason.as_str(),
        "messages": messages,
    });
    let object = request
        .as_object_mut()
        .expect("route request is a JSON object");
    if let Some((previous_model, level)) = previous {
        object.insert(
            "previous".to_owned(),
            json!({
                "model": previous_model,
                "thinkingLevel": level.map(ModelThinkingLevel::as_str),
            }),
        );
    }
    if let Some((failed_model, level, message)) = failed {
        object.insert(
            "failed".to_owned(),
            json!({
                "model": failed_model,
                "thinkingLevel": level.map(ModelThinkingLevel::as_str),
                "message": message,
            }),
        );
    }
    if let Some(state) = state {
        object.insert("state".to_owned(), state.clone());
    }
    request
}

/// The selection a session branch records (`virtual-models.ts:128-145`).
///
/// A virtual `model_change` holds until the next `model_change`, because
/// responses name the physical models it routed to. Otherwise the latest
/// physical response wins. A virtual model that is no longer registered
/// does not hold, so the selection falls back to the physical model that
/// answered last. Only the last `model_change` can hold, so this looks up
/// at most one model through `get_model`.
pub fn get_branch_selection(
    branch: &[SessionEntry],
    get_model: impl Fn(&str, &str) -> Option<Model>,
) -> Option<(String, String)> {
    for (index, entry) in branch.iter().enumerate().rev() {
        match entry {
            SessionEntry::ModelChange(change) => {
                return Some((change.provider.clone(), change.model_id.clone()));
            }
            SessionEntry::Message(message) => {
                let AgentMessage::Assistant(assistant) = &message.message else {
                    continue;
                };
                // A failed routing attempt names the virtual model; there is
                // no physical request to report.
                if assistant.api.as_str() == VIRTUAL_MODEL_API
                    || assistant.api.as_str() == VIRTUAL_MODEL_API_ALIAS
                {
                    continue;
                }
                let response = (assistant.provider.clone(), assistant.model.clone());
                let change = find_last_model_change(&branch[..index]);
                let model = change.and_then(|change| get_model(&change.provider, &change.model_id));
                return match (change, model) {
                    (Some(change), Some(model)) if is_virtual_model(&model) => {
                        Some((change.provider.clone(), change.model_id.clone()))
                    }
                    _ => Some(response),
                };
            }
            _ => {}
        }
    }
    None
}

/// `findLastModelChange` (`virtual-models.ts:147-157`).
fn find_last_model_change(branch: &[SessionEntry]) -> Option<&ModelChangeEntry> {
    branch.iter().rev().find_map(|entry| match entry {
        SessionEntry::ModelChange(change) => Some(change),
        _ => None,
    })
}

/// Latest router state a session branch stores for a virtual model
/// (`virtual-models.ts:159-167`).
pub fn get_virtual_model_state(
    branch: &[SessionEntry],
    provider: &str,
    model_id: &str,
) -> Option<Value> {
    for entry in branch.iter().rev() {
        let SessionEntry::Custom(CustomEntry {
            custom_type, data, ..
        }) = entry
        else {
            continue;
        };
        if custom_type != VIRTUAL_MODEL_STATE_ENTRY {
            continue;
        }
        let Some(data) = data else { continue };
        let parsed: VirtualModelStateData = match serde_json::from_value(data.clone()) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if parsed.provider == provider && parsed.model_id == model_id {
            return Some(parsed.state);
        }
    }
    None
}

/// `unroutedStream` (`virtual-models.ts:190-194`): the stream answer for a
/// virtual entry that was not routed first (e.g. API-specific `stream()`
/// options). `streamSimple` requests outside the agent loop are routed by
/// `ModelRuntime`.
pub fn unrouted_stream(model: &Model) -> rpi_ai::utils::event_stream::AssistantMessageEventStream {
    let message = format!(
        "Virtual model {}/{} must be routed before streaming",
        model.provider, model.id
    );
    rpi_ai::api::lazy::lazy_stream(model, async move {
        Err(rpi_ai::auth::resolve::ModelsError::new(
            rpi_ai::auth::resolve::ModelsErrorCode::Stream,
            message,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::session::MessageEntry;
    use rpi_ai::types::{AssistantRole, ModelThinkingLevel, Usage};

    fn assistant(provider: &str, model: &str, api: &str, stop: StopReason) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![],
            api: ApiKind::from(api),
            provider: provider.to_owned(),
            model: model.to_owned(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: stop,
            error_message: None,
            timestamp: 0,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
        })
    }

    fn message_entry(id: &str, message: AgentMessage) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: id.to_owned(),
            parent_id: None,
            timestamp: String::new(),
            message,
        })
    }

    fn model_change(id: &str, provider: &str, model_id: &str) -> SessionEntry {
        SessionEntry::ModelChange(ModelChangeEntry {
            id: id.to_owned(),
            parent_id: None,
            timestamp: String::new(),
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
        })
    }

    fn custom_state(provider: &str, model_id: &str, state: Value) -> SessionEntry {
        SessionEntry::Custom(CustomEntry {
            id: "custom".to_owned(),
            parent_id: None,
            timestamp: String::new(),
            custom_type: VIRTUAL_MODEL_STATE_ENTRY.to_owned(),
            data: Some(json!({ "provider": provider, "modelId": model_id, "state": state })),
        })
    }

    fn definition() -> VirtualModelDefinition {
        VirtualModelDefinition {
            provider: "jev".to_owned(),
            id: "auto".to_owned(),
            name: "Auto (Jev)".to_owned(),
            thinking_levels: Some(vec![ModelThinkingLevel::Low, ModelThinkingLevel::High]),
            context_window: Some(272_000),
            max_tokens: Some(128_000),
            input: None,
        }
    }

    #[test]
    fn creates_a_virtual_catalog_entry() {
        let model = create_virtual_model(&definition());
        assert!(is_virtual_model(&model));
        assert_eq!(model.api.as_str(), "rpi-virtual");
        assert_eq!(model.provider, "jev");
        assert_eq!(model.id, "auto");
        assert!(model.reasoning);
        assert_eq!(model.context_window, 272_000);
        assert_eq!(model.max_tokens, 128_000);
        assert_eq!(model.input, vec![InputModality::Text, InputModality::Image]);
        let map = model.thinking_level_map.expect("map present");
        assert_eq!(
            map.get(&ModelThinkingLevel::Low),
            Some(&Some("low".to_owned()))
        );
        assert_eq!(map.get(&ModelThinkingLevel::Off), Some(&None));
        assert_eq!(map.get(&ModelThinkingLevel::Max), Some(&None));
    }

    #[test]
    fn definition_defaults_match_upstream() {
        let mut definition = definition();
        definition.thinking_levels = None;
        definition.context_window = None;
        definition.max_tokens = None;
        let model = create_virtual_model(&definition);
        assert!(!model.reasoning);
        assert_eq!(model.context_window, 0);
        assert_eq!(model.max_tokens, 0);
        let map = model.thinking_level_map.expect("map present");
        assert_eq!(
            map.get(&ModelThinkingLevel::Off),
            Some(&Some("off".to_owned()))
        );
        assert_eq!(map.get(&ModelThinkingLevel::Low), Some(&None));
    }

    #[test]
    fn latest_response_skips_error_and_aborted() {
        let messages = vec![
            Message::Assistant(assistant_match(
                "openai",
                "gpt",
                "openai-responses",
                StopReason::Stop,
            )),
            Message::Assistant(assistant_match(
                "openai",
                "gpt",
                "openai-responses",
                StopReason::Error,
            )),
        ];
        let latest = find_latest_response(&messages).expect("successful response");
        assert_eq!(latest.model, "gpt");
    }

    fn assistant_match(
        provider: &str,
        model: &str,
        api: &str,
        stop: StopReason,
    ) -> rpi_ai::types::AssistantMessage {
        let AgentMessage::Assistant(message) = assistant(provider, model, api, stop) else {
            unreachable!()
        };
        message
    }

    #[test]
    fn branch_selection_prefers_the_last_model_change() {
        let branch = vec![
            message_entry(
                "1",
                assistant("openai", "gpt-5", "openai-responses", StopReason::Stop),
            ),
            model_change("2", "jev", "auto"),
        ];
        let selection = get_branch_selection(&branch, |_, _| None);
        assert_eq!(selection, Some(("jev".to_owned(), "auto".to_owned())));
    }

    #[test]
    fn branch_selection_falls_back_to_the_physical_response() {
        let branch = vec![
            model_change("1", "openai", "gpt-5"),
            message_entry(
                "2",
                assistant("openai", "gpt-5", "openai-responses", StopReason::Stop),
            ),
        ];
        let selection = get_branch_selection(&branch, |_, _| None);
        assert_eq!(selection, Some(("openai".to_owned(), "gpt-5".to_owned())));
    }

    #[test]
    fn branch_selection_holds_a_registered_virtual_change() {
        let virtual_model = create_virtual_model(&definition());
        let branch = vec![
            model_change("1", "jev", "auto"),
            message_entry(
                "2",
                assistant(
                    "openai-codex",
                    "gpt-5.6-sol",
                    "openai-codex-responses",
                    StopReason::Stop,
                ),
            ),
        ];
        let selection = get_branch_selection(&branch, |provider, id| {
            (provider == "jev" && id == "auto").then(|| virtual_model.clone())
        });
        assert_eq!(selection, Some(("jev".to_owned(), "auto".to_owned())));
    }

    #[test]
    fn branch_selection_drops_an_unregistered_virtual_change() {
        let branch = vec![
            model_change("1", "jev", "auto"),
            message_entry(
                "2",
                assistant(
                    "openai-codex",
                    "gpt-5.6-sol",
                    "openai-codex-responses",
                    StopReason::Stop,
                ),
            ),
        ];
        let selection = get_branch_selection(&branch, |_, _| None);
        assert_eq!(
            selection,
            Some(("openai-codex".to_owned(), "gpt-5.6-sol".to_owned()))
        );
    }

    #[test]
    fn branch_selection_skips_failed_routing_responses() {
        let branch = vec![
            model_change("1", "jev", "auto"),
            message_entry(
                "2",
                assistant("jev", "auto", "rpi-virtual", StopReason::Error),
            ),
            message_entry(
                "3",
                assistant(
                    "openai-codex",
                    "gpt-5.6-luna",
                    "openai-codex-responses",
                    StopReason::Stop,
                ),
            ),
        ];
        let virtual_model = create_virtual_model(&definition());
        let selection = get_branch_selection(&branch, |provider, id| {
            (provider == "jev" && id == "auto").then(|| virtual_model.clone())
        });
        assert_eq!(selection, Some(("jev".to_owned(), "auto".to_owned())));
    }

    #[test]
    fn virtual_state_reads_back_the_latest_entry() {
        let branch = vec![
            custom_state("jev", "auto", json!({ "phase": "planning" })),
            custom_state("other", "auto", json!({ "phase": "x" })),
            custom_state("jev", "auto", json!({ "phase": "implementation" })),
        ];
        assert_eq!(
            get_virtual_model_state(&branch, "jev", "auto"),
            Some(json!({ "phase": "implementation" }))
        );
        assert_eq!(get_virtual_model_state(&branch, "jev", "missing"), None);
    }

    #[test]
    fn route_request_and_result_shapes() {
        let model = create_virtual_model(&definition());
        let previous_model = {
            let mut model = create_virtual_model(&definition());
            model.provider = "openai-codex".to_owned();
            model.id = "gpt-5.6-sol".to_owned();
            model
        };
        let request = route_request_json(
            &model,
            ModelThinkingLevel::High,
            ModelRouteReason::User,
            Some((&previous_model, Some(ModelThinkingLevel::Medium))),
            None,
            Some(&json!({ "phase": "planning" })),
            &[],
        );
        assert_eq!(request["reason"], "user");
        assert_eq!(request["thinkingLevel"], "high");
        assert_eq!(request["previous"]["model"]["id"], "gpt-5.6-sol");
        assert_eq!(request["previous"]["thinkingLevel"], "medium");
        assert_eq!(request["state"]["phase"], "planning");

        let route = parse_route_result(&json!({
            "model": { "provider": "openai-codex", "id": "gpt-5.6-luna" },
            "thinkingLevel": "medium",
            "state": { "phase": "implementation" },
        }))
        .expect("parsed route");
        assert_eq!(route.provider, "openai-codex");
        assert_eq!(route.id, "gpt-5.6-luna");
        assert_eq!(route.thinking_level, ModelThinkingLevel::Medium);
        assert_eq!(route.state, Some(json!({ "phase": "implementation" })));
    }

    #[test]
    fn parse_route_result_rejects_missing_target() {
        assert!(parse_route_result(&json!({})).is_err());
        assert!(parse_route_result(&json!({ "model": { "id": "x" } })).is_err());
        assert!(parse_route_result(&json!({ "model": { "provider": "p" } })).is_err());
        let route = parse_route_result(&json!({
            "model": { "provider": "p", "id": "x" },
            "thinkingLevel": "off",
        }))
        .expect("parsed");
        assert_eq!(route.thinking_level, ModelThinkingLevel::Off);
        assert_eq!(route.state, None);
    }
}
