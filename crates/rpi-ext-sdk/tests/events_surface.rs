//! Compile-time existence test for the full wasm-guest hook event type
//! surface (V15-09 #9642): every type exported by `rpi_ext_sdk::events`
//! must keep existing — a removal/rename breaks THIS test at compile time,
//! instead of being discovered by review (the "Finding B" lesson: the
//! 60-type re-export surface had no test pinning it).

#[allow(unused)]
use rpi_ext_sdk::events;

#[test]
fn all_hook_event_types_exist() {
    // Referencing each type forces the compiler to resolve it; deleting or
    // renaming any of the 60 fails the build here.
    let _: Option<events::ProjectTrustDecision> = None;
    let _: Option<events::ResourcesDiscoverReason> = None;
    let _: Option<events::SessionStartReason> = None;
    let _: Option<events::SessionShutdownReason> = None;
    let _: Option<events::SessionSwitchReason> = None;
    let _: Option<events::ForkPosition> = None;
    let _: Option<events::CompactionReason> = None;
    let _: Option<events::ModelSelectSource> = None;
    let _: Option<events::InputSource> = None;
    let _: Option<events::StreamingBehavior> = None;
    let _: Option<events::CacheWarmingAction> = None;
    let _: Option<events::ProjectTrustEvent> = None;
    let _: Option<events::ProjectTrustEventResult> = None;
    let _: Option<events::ResourcesDiscoverEvent> = None;
    let _: Option<events::ResourcesDiscoverResult> = None;
    let _: Option<events::SessionStartEvent> = None;
    let _: Option<events::SessionInfoChangedEvent> = None;
    let _: Option<events::SessionBeforeSwitchEvent> = None;
    let _: Option<events::SessionBeforeSwitchResult> = None;
    let _: Option<events::SessionBeforeForkEvent> = None;
    let _: Option<events::SessionBeforeForkResult> = None;
    let _: Option<events::SessionBeforeCompactEvent> = None;
    let _: Option<events::SessionBeforeCompactResult> = None;
    let _: Option<events::SessionCompactEvent> = None;
    let _: Option<events::SessionShutdownEvent> = None;
    let _: Option<events::TreePreparation> = None;
    let _: Option<events::SessionBeforeTreeEvent> = None;
    let _: Option<events::ExtensionBranchSummary> = None;
    let _: Option<events::SessionBeforeTreeResult> = None;
    let _: Option<events::SessionTreeEvent> = None;
    let _: Option<events::ContextEvent> = None;
    let _: Option<events::ContextEventResult> = None;
    let _: Option<events::CacheWarmingDecisionEvent> = None;
    let _: Option<events::CacheWarmingDecisionEventResult> = None;
    let _: Option<events::BeforeProviderRequestEvent> = None;
    let _: Option<events::BeforeProviderHeadersEvent> = None;
    let _: Option<events::AfterProviderResponseEvent> = None;
    let _: Option<events::BeforeAgentStartEvent> = None;
    let _: Option<events::BeforeAgentStartMessage> = None;
    let _: Option<events::BeforeAgentStartEventResult> = None;
    let _: Option<events::BeforeAgentStartCombinedResult> = None;
    let _: Option<events::AgentEndEvent> = None;
    let _: Option<events::TurnStartEvent> = None;
    let _: Option<events::TurnEndEvent> = None;
    let _: Option<events::MessageEvent> = None;
    let _: Option<events::MessageUpdateEvent> = None;
    let _: Option<events::MessageEndEventResult> = None;
    let _: Option<events::ToolExecutionStartEvent> = None;
    let _: Option<events::ToolExecutionUpdateEvent> = None;
    let _: Option<events::ToolExecutionEndEvent> = None;
    let _: Option<events::ModelSelectEvent> = None;
    let _: Option<events::ThinkingLevelSelectEvent> = None;
    let _: Option<events::UserBashEvent> = None;
    let _: Option<events::UserBashEventResult> = None;
    let _: Option<events::InputEvent> = None;
    let _: Option<events::InputEventResult> = None;
    let _: Option<events::ToolCallEvent> = None;
    let _: Option<events::ToolCallEventResult> = None;
    let _: Option<events::ToolResultEvent> = None;
    let _: Option<events::ToolResultEventResult> = None;
}
