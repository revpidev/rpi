//! Interactive-mode message rendering components (T12-S4a).
//!
//! Ports of `packages/coding-agent/src/modes/interactive/components/` @ pi
//! 0.82.1 (2efa728). Component names are snake_case mirrors of the upstream
//! file names; each module header lists its upstream file and intentional
//! differences. Line anchors in comments cite upstream `file.ts:start-end`.

pub mod assistant_message;
pub mod bash_execution;
pub mod bordered_loader;
pub mod branch_summary_message;
pub mod compaction_summary_message;
pub mod config_selector;
pub mod countdown_timer;
pub mod custom_entry;
pub mod custom_message;
pub mod diff;
pub mod dynamic_border;
pub mod extension_editor;
pub mod extension_input;
pub mod extension_selector;
pub mod first_time_setup;
pub mod keybinding_hints;
pub mod llama_view;
pub mod login_dialog;
pub mod model_search;
pub mod model_selector;
pub mod oauth_selector;
pub mod scoped_models_selector;
pub mod session_selector;
pub mod session_selector_search;
pub mod settings_selector;
pub mod show_images_selector;
pub mod skill_invocation_message;
pub mod status_indicator;
pub mod theme_selector;
pub mod thinking_selector;
pub mod tool_execution;
pub mod tree_selector;
pub mod trust_selector;
pub mod user_message;
pub mod user_message_selector;
pub mod visual_truncate;

pub use assistant_message::AssistantMessageComponent;
pub use bash_execution::BashExecutionComponent;
pub use branch_summary_message::BranchSummaryMessageComponent;
pub use compaction_summary_message::CompactionSummaryMessageComponent;
pub use custom_entry::CustomEntryComponent;
pub use custom_message::CustomMessageComponent;
pub use diff::{render_diff, RenderDiffOptions};
pub use dynamic_border::DynamicBorder;
pub use keybinding_hints::{key_display_text, key_hint, key_text, raw_key_hint};
pub use skill_invocation_message::SkillInvocationMessageComponent;
pub use status_indicator::{
    BranchSummaryStatusIndicator, CompactionStatusIndicator, CompactionStatusReason, IdleStatus,
    RetryStatusIndicator, StatusIndicator, StatusIndicatorKind, WorkingStatusIndicator,
};
pub use tool_execution::{
    ToolExecutionComponent, ToolExecutionOptions, ToolResultContentLoose, ToolResultState,
};
pub use user_message::UserMessageComponent;
pub use visual_truncate::{truncate_to_visual_lines, VisualTruncateResult};

/// Component helpers shared by the interactive components.
pub(crate) mod util {
    /// OSC 133 zone markers (assistant-message.ts:5-7, user-message.ts:4-6).
    pub const OSC133_ZONE_START: &str = "\u{1b}]133;A\u{7}";
    pub const OSC133_ZONE_END: &str = "\u{1b}]133;B\u{7}";
    pub const OSC133_ZONE_FINAL: &str = "\u{1b}]133;C\u{7}";

    /// `toLocaleString()` (compaction-summary-message.ts:35): group digits
    /// with `,` separators (en-US default locale). Upstream output depends on
    /// the ICU locale of the Node build; the port pins en-US grouping.
    pub fn to_locale_string(value: u64) -> String {
        let digits = value.to_string();
        let mut out = String::with_capacity(digits.len() + digits.len() / 3);
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(c);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::util::to_locale_string;

    #[test]
    fn locale_string_groups_thousands() {
        assert_eq!(to_locale_string(0), "0");
        assert_eq!(to_locale_string(999), "999");
        assert_eq!(to_locale_string(1000), "1,000");
        assert_eq!(to_locale_string(12345), "12,345");
        assert_eq!(to_locale_string(1234567), "1,234,567");
    }
}
