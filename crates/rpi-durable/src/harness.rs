//! Durable agent harness core (port of the `packages/durable/src/harness/*`
//! modules @ `abe508e1b`: `types.ts`, `define.ts`, `registry.ts`, `agent.ts`,
//! `live.ts`, `usage.ts`, `provider.ts`, `json.ts`, `util.ts`, `submissions.ts`,
//! `inbox.ts`, `context.ts`, `task-graph.ts`, and the `harness.ts` composition).
//!
//! The module carries the harness type surface, the extension registry, the
//! per-conversation `pi.*` documents, submission admission/waits, inbox boundary
//! placement, committed context derivation, the live task-graph mount, and the
//! [`HarnessImpl`] composition. Task scheduling and generation are a separate
//! worker: [`scheduler::SchedulerService`] is the injection seam, defaulting to
//! [`scheduler::UnavailableScheduler`].
//!
//! Rust shape notes (registered V17-14 §7.3): the upstream behaviour interfaces
//! become seam traits; `Draft<T>` document proxies become `DocCell`-backed
//! [`crate::session::DocDraft`] mutations with typed JSON helpers; `Map`/`Set`
//! iteration becomes `BTreeMap` plus explicit order where observable.

pub mod agent;
pub mod context;
pub mod define;
pub mod harness;
pub mod inbox;
pub mod json;
pub mod live;
pub mod provider;
pub mod registry;
pub mod scheduler;
pub mod submissions;
pub mod task_graph;
pub mod types;
pub mod usage;
pub mod util;

pub use agent::{
    AGENT_DOC, DEFAULT_COMPACTION_POLICY, DEFAULT_CONTEXT_RETENTION_MS, DEFAULT_PROGRESS_POLICY,
    DEFAULT_RETRY_POLICY, INSTRUCTIONS_KEY, add_tools, agent_hooks, apply_change, configure,
    create_agent, resolve_agent, resolve_settings, select_extensions,
};
pub use context::{
    ContextBounds, ContextRange, MISSING_RESULT_TEXT, active_entries, capture_context_bounds,
    derive_context, is_excluded_stop_reason, order_tool_results, read_context, read_context_from,
};
pub use define::{define_extension, define_tool, hook, section, wrap_section, wrap_tool};
pub use harness::{
    BoundConversation, ConversationImpl, HarnessImpl, bound_conversation, open_harness,
    open_harness_with_scheduler,
};
pub use inbox::{
    Boundary, BoundaryAt, BoundaryResult, INBOX_DOC, InboxItem, InboxState, QueueModes,
    apply_boundary, entry_from_json, entry_to_json, is_stale, prepare_boundary, remove_inbox_item,
    withdraw_queued_inputs,
};
pub use json::{assign_json, assign_json_index};
pub use live::{
    COMPACTION_TASK_KIND, LIVE_DOC, LiveGeneration, LiveRun, LiveState, RUN_TASK_KINDS,
    SchedulerOutcome, TOOL_TASK_KIND, ToolSlot, ToolSlotStatus, add_compaction_status,
    clear_progress, compaction_status, end_run, finish_slot, remove_compaction_status,
    settle_scheduler_outcome, tool_slot,
};
pub use provider::{PROVIDER_DOC, ProviderState, ensure_provider_session_id};
pub use registry::{
    BUILTIN_TASK_NAMES, COMPACTION_TASK_NAME, GENERATION_TASK_NAME, RegistryImpl, RegistryState,
    TOOL_TASK_NAME, create_registry, validate_extension,
};
pub use scheduler::{
    InvocationBinding, SchedulerFactory, SchedulerService, UNAVAILABLE_MESSAGE,
    UnavailableScheduler,
};
pub use submissions::{SubmissionHandle, Submissions, admit_submission, is_settled};
pub use task_graph::{TaskGraph, TaskGraphNode, TaskGraphState, TaskGraphView, TaskGraphWatch};
pub use types::{
    Agent, AgentChange, AgentState, AnyTask, AnyTaskDef, BlockedReason, CompactionPolicy,
    CompactionReason, CompactionResult, Conversation, ConversationAbortOptions,
    ConversationCreateOptions, ConversationHandle, ConversationInit, DeferredOption,
    DeferredWindow, EnvFactory, EnvTarget, ExecutionEnv, Extension, ExtensionsChange,
    ExtensionsEdit, ExtensionsSelection, Harness, HarnessInspection, HarnessOptions,
    HarnessSettings, HookApi, HookHandlers, HookRegistration, InputSubmissionDraft, ModelRef,
    PromptInput, PromptRender, PromptSection, QueueMode, Registry, RegistryError, RegistryListener,
    RegistryReader, RegistrySnapshot, ReportFn, RootOptions, Scheduling, Settings,
    SettledSubmissionRecord, SettledTask, Submission, SubmissionAbort, SubmissionAbortResult,
    SubmissionDraft, TaskAbort, TaskDefinition, TaskInspection, TaskInspectionState, ToolControl,
    ToolDiagnostic, ToolExecutionApi, ToolExecutionMode, ToolRegistration, ToolReplay, ToolsChange,
    ToolsSelection, UserInput, WhenBusy, Wrap, WriteSubmissionDraft,
};
pub use usage::{USAGE_DOC, UsageBucket, UsageState, add_usage, add_usage_state, record_usage};
pub use util::{WaitError, WaitFuture, Waiters, closed_error, scan_all};
