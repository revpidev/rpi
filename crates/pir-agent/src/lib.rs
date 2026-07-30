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
//! - [`messages`] carries the `AgentMessage` union including the coding-agent
//!   custom message types from `packages/coding-agent/src/core/messages.ts`:
//!   TS declaration merging has no Rust equivalent, so the custom variants are
//!   folded into the union here (structural, not behavioral, difference).
//! - [`session`] consolidates the session entry types of
//!   `packages/coding-agent/src/core/session-manager.ts` and
//!   `packages/agent/src/harness/types.ts` into one serde skeleton shared by
//!   the `pir` main path and the harness layer.

pub mod error;
pub mod messages;
pub mod session;
pub mod stream_fn;
pub mod types;

pub use error::AgentError;
pub use messages::{
    AgentMessage, BashExecutionMessage, BranchSummaryMessage, CompactionSummaryMessage,
    CustomMessage, BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
    COMPACTION_SUMMARY_SUFFIX,
};
pub use stream_fn::{BoxStream, StreamFn};
pub use types::{
    AgentEvent, AgentTool, AgentToolResult, AgentToolUpdateCallback, QueueMode, ToolExecutionMode,
};
