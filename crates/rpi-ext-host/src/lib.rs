//! `rpi-ext-host` — Rust + Wasm extension host @ design doc §7 (ADR-0001).
//!
//! This crate is a rpi-native addition (no upstream counterpart): it
//! implements the extension-host capability surface (33 events + 24 API
//! methods + 28 UI methods) with Rust built-in (L0) and Wasm (L1,
//! `wasmtime`) backends. No JS/TS execution capability anywhere (red line,
//! coding-standards §1.3).
//!
//! L0 core: [`host::NativeExtensionHost`] = [`loader::ExtensionLoader`]
//! (factories, discovery, cache) + [`runner::ExtensionRunnerCore`]
//! (registries, conflict rules, serial emit dispatch), with extensions
//! driving [`api::ExtensionApi`]. L1 (wasm): [`wasm`] — ABI v1 host
//! (docs/extension-abi.md). The T02 spike was removed in W6 (its protocol
//! conclusions became the ABI).
//!
//! [`interactive_ui`] is the native guest-side mirror of the wasm SDK's
//! interactive custom UI ABI (ADR-0024, C0 protocol freeze).

pub mod api;
pub mod bridges;
pub mod error;
pub mod host;
pub mod interactive_ui;
pub mod loader;
pub mod native;
pub mod runner;
pub mod system_prompt_bridge;
pub mod types;
pub mod wasm;

#[cfg(test)]
mod test_bridge;

pub use error::ExtError;

// Package-entry re-export of the hook event/result type surface (#9642,
// 4c2d91339 — upstream completed the same export from
// `@earendil-works/pi-coding-agent`'s entry). Native extension authors
// import these from the crate root; everything was already reachable via
// `types::` (Rust `pub mod` ≠ upstream's unexported modules), so this is
// surface alignment, zero behavior.
pub use types::{
    AfterProviderResponseEvent, AgentEndEvent, BeforeAgentStartCombinedResult,
    BeforeAgentStartEvent, BeforeAgentStartEventResult, BeforeAgentStartMessage,
    BeforeProviderHeadersEvent, BeforeProviderRequestEvent, CacheWarmingAction,
    CacheWarmingDecisionEvent, CacheWarmingDecisionEventResult, CompactionReason, ContextEvent,
    ContextEventResult, ExtensionBranchSummary, ForkPosition, InputEvent, InputEventResult,
    InputSource, MessageEndEventResult, MessageEvent, MessageUpdateEvent, ModelSelectEvent,
    ModelSelectSource, ProjectTrustDecision, ProjectTrustEvent, ProjectTrustEventResult,
    ResourcesDiscoverEvent, ResourcesDiscoverReason, ResourcesDiscoverResult,
    SessionBeforeCompactEvent, SessionBeforeCompactResult, SessionBeforeForkEvent,
    SessionBeforeForkResult, SessionBeforeSwitchEvent, SessionBeforeSwitchResult,
    SessionBeforeTreeEvent, SessionBeforeTreeResult, SessionCompactEvent, SessionShutdownEvent,
    SessionShutdownReason, SessionStartEvent, SessionStartReason, SessionSwitchReason,
    SessionTreeEvent, StreamingBehavior, ThinkingLevelSelectEvent, ToolCallEvent,
    ToolCallEventResult, ToolExecutionEndEvent, ToolExecutionStartEvent, ToolExecutionUpdateEvent,
    ToolResultEvent, ToolResultEventResult, TreePreparation, TurnEndEvent, TurnStartEvent,
    UserBashEvent, UserBashEventResult,
};
