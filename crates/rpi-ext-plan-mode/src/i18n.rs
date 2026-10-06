//! User-facing strings for `rpi-plan-mode`.
//!
//! The first release ships a single English table (plugin 02 §2 lists
//! `i18n.rs` as optional; the plan-mode surface is owned by rpi and the
//! repo language standard keeps host-facing chrome English). Keeping the
//! strings in one module leaves room for a locale table later without
//! touching the logic.

/// One-line editor hint shown while Plan mode is active (01 §2 R-PM-1.1).
pub const HINT_PLAN_MODE: &str =
    "plan mode · research and planning — write tools are disabled (Shift+Tab to exit)";

/// Approval dialog title prefix (01 §6 R-PM-5.1).
pub const REVIEW_TITLE: &str = "Review the plan";

/// Approval dialog options, in order. `ui.select` returns the exact label.
pub const REVIEW_APPROVE: &str = "Approve and execute";
/// Second option: collect feedback and re-inject it.
pub const REVIEW_REVISE: &str = "Continue revising";
/// Third option: leave Plan mode without injecting anything.
pub const REVIEW_ABANDON: &str = "Abandon";

/// Revision feedback prompt.
pub const REVISE_PROMPT: &str = "What should the plan change?";
/// Revision feedback placeholder.
pub const REVISE_PLACEHOLDER: &str = "Describe the requested revision";

/// `/plan` usage line for an unknown subcommand.
pub const COMMAND_USAGE: &str =
    "usage: /plan (toggle plan mode), /plan status, /plan file, /plan edit";

/// Non-interactive `/plan` note (the host gate keeps the mode `default`).
pub const NON_INTERACTIVE_NOTE: &str = "plan mode is only available in interactive sessions";

/// Warning when leaving Plan mode could not release the tool boundary; the
/// mirror state is kept so the next event retries.
pub const EXIT_CLEANUP_FAILED: &str = "plan mode is off, but some tool restrictions could not be restored; the cleanup will be retried";

/// Warning when the entry active-set snapshot is unavailable on exit: the
/// boundary is released, but the pre-plan active set cannot be restored.
pub const ACTIVE_SET_NOT_RESTORED: &str = "plan mode is off, but the pre-plan active tool set could not be restored; the active set stays at its plan-mode value";

/// External-editor fallback title (`ui.editor` after `ui.editExternal`).
pub const EDIT_PLAN_TITLE: &str = "Edit the plan";

/// Tool description (LLM-facing).
pub const TOOL_DESCRIPTION: &str = "Write the complete plan for the current task to this session's plan file. The path is fixed per session — do not attempt to pass a path. Call this when the plan is ready for user review; the user is asked to approve, revise, or abandon it.";

/// Tool prompt snippet (available-tools section).
pub const TOOL_PROMPT_SNIPPET: &str =
    "write_plan(content): write the complete plan to the session plan file and request user review";

/// Result text when the user approved the plan.
pub const APPROVED_NOTE: &str = "The user approved the plan. Plan mode is off; the plan summary was injected — start executing it.";

/// Result text when the plan was abandoned.
pub const ABANDONED_NOTE: &str =
    "The user abandoned the plan. Plan mode is off; do not continue executing it.";

/// Prefix for the revision feedback echoed back to the model.
pub const REVISION_PREFIX: &str = "The user requested revisions before approving. Feedback:";

/// Degradation note when no interactive review dialog is available.
pub const NO_DIALOG_NOTE: &str = "The interactive review dialog is unavailable in this mode; the plan was saved and the user must review it manually (leave plan mode when ready to execute).";

/// Instruction injected with the approved plan summary.
pub const EXECUTION_INSTRUCTION: &str = "The plan above was approved. Start executing it now.";

/// Truncation marker for the injected plan summary.
pub const SUMMARY_TRUNCATED: &str = "\n\n[... plan truncated for the summary ...]";
