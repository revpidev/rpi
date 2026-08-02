//! `pir-agent` — port of `@earendil-works/pi-agent-core` @ pi 0.82.1 (2efa728).
//!
//! Agent loop, state, events, and the optional harness layer (ADR-0003 §1).
//! Depends only on `pir-ai`'s type layer — never on concrete provider
//! implementations; streaming is injected via [`StreamFn`] (coding-standards
//! §2.2, §4.2).
//!
//! Module mapping notes:
//! - [`types`] mirrors `packages/agent/src/types.ts`.
//! - [`stream_fn`] mirrors `packages/agent/src/stream-fn.ts` (shape pinned by
//!   design doc §4.4).
//! - [`agent_loop`] mirrors `packages/agent/src/agent-loop.ts` plus the
//!   loop-facing hook types of `types.ts`.
//! - [`agent`] mirrors `packages/agent/src/agent.ts` plus `AgentState`.
//! - [`messages`] carries the `AgentMessage` union including the coding-agent
//!   custom message types from `packages/coding-agent/src/core/messages.ts`:
//!   TS declaration merging has no Rust equivalent, so the custom variants are
//!   folded into the union here (structural, not behavioral, difference).
//! - [`session`] consolidates the session entry types of
//!   `packages/coding-agent/src/core/session-manager.ts` and
//!   `packages/agent/src/harness/types.ts` into one serde skeleton shared by
//!   the `pir` main path and the harness layer.

pub mod agent;
pub mod agent_loop;
pub mod error;
pub mod harness;
pub mod messages;
pub mod session;
pub mod stream_fn;
pub mod types;

pub use agent::{Agent, AgentListener, AgentOptions, AgentState, InitialAgentState, PromptInput};
pub use agent_loop::{
    agent_loop, agent_loop_continue, run_agent_loop, run_agent_loop_continue, AfterToolCallContext,
    AfterToolCallFn, AfterToolCallResult, AgentContext, AgentEventSink, AgentEventStream,
    AgentLoopConfig, AgentLoopTurnUpdate, BeforeToolCallContext, BeforeToolCallFn,
    BeforeToolCallResult, ConvertToLlmFn, GetApiKeyFn, GetQueuedMessagesFn, PrepareNextTurnContext,
    PrepareNextTurnFn, ShouldStopAfterTurnContext, ShouldStopAfterTurnFn, TransformContextFn,
};
pub use error::AgentError;
pub use messages::{
    bash_execution_to_text, convert_to_llm, AgentMessage, BashExecutionMessage,
    BranchSummaryMessage, CompactionSummaryMessage, CustomMessage, BRANCH_SUMMARY_PREFIX,
    BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX,
};
pub use stream_fn::{BoxStream, StreamFn};
pub use types::{
    AgentEvent, AgentTool, AgentToolResult, AgentToolUpdateCallback, QueueMode, ToolExecutionMode,
};
