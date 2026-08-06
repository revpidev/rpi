//! Harness layer — port of `packages/agent/src/harness/` @ pi 0.82.1 (2efa728).
//!
//! T16 first block: the complete type layer ([`types`], mirroring
//! `packages/agent/src/harness/types.ts` — errors, phase, events, hook
//! results, storage/repo and filesystem/shell contracts, options). The
//! concrete storage implementations and the `Session` / `SessionRepo`
//! classes land in later T16 blocks (ports of `jsonl-storage.ts` /
//! `memory-storage.ts` / `session/session.ts`; ADR-0003 §1).
//!
//! Module mapping:
//! - [`types`] — `packages/agent/src/harness/types.ts`.
//!
//! Intentional differences are documented per item in `types.rs`.

pub mod agent_harness;
pub mod env;
pub mod prompt_templates;
pub mod session;
pub mod skills;
pub mod system_prompt;
pub mod tools;
pub mod types;
pub mod utils;

pub use agent_harness::{
    AgentHarness, AgentHarnessHook, AgentHarnessListener, NavigateTreeOptions,
};
pub use types::{
    apply_stream_options_patch, AbortResult, AgentHarnessError, AgentHarnessErrorCode,
    AgentHarnessEvent, AgentHarnessOptions, AgentHarnessOwnEvent, AgentHarnessPhase,
    AgentHarnessPromptOptions, AgentHarnessResources, AgentHarnessStreamOptions,
    AgentHarnessStreamOptionsPatch, AgentHarnessSystemPrompt, AgentHarnessTool,
    AgentHarnessToolContextSource, BeforeAgentStartResult, BeforeProviderPayloadResult,
    BeforeProviderRequestResult, BranchSummaryError, BranchSummaryErrorCode, BranchSummaryResult,
    ChunkCallback, CompactResult, CompactionError, CompactionErrorCode, CompactionPreparation,
    CompactionSettings, ContextResult, CreateDirOptions, CreateTempFileOptions, ExecutionEnv,
    ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo, FileKind,
    FileOperations, FileSystem, ForkPosition, GenerateBranchSummaryOptions, HarnessHookResult,
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata, JsonlSessionRepoApi,
    NavigateTreeResult, PatchMap, PendingSessionWrite, PromptTemplate, ReadTextLinesOptions,
    RemoveOptions, RetryOperation, Session, SessionBeforeCompactResult, SessionBeforeTreeResult,
    SessionContext, SessionCreateOptions, SessionEntryCursorOptions, SessionError,
    SessionErrorCode, SessionForkOptions, SessionMetadata, SessionModelRef, SessionRepo,
    SessionStats, SessionStorage, Shell, ShellExecOptions, ShellExecResult, Skill,
    SystemPromptContext, ToolCallResult, ToolResultPatch, TreePreparation, TreeSummary, TurnState,
    UpdateSource,
};
