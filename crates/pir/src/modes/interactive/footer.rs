//! Footer component — port of
//! `packages/coding-agent/src/modes/interactive/components/footer.ts` @ pi
//! 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The upstream `FooterComponent` reads the active theme from a process
//!   global and takes a `ReadonlyFooterDataProvider` interface; the port
//!   injects an explicit `Arc<Theme>` and a concrete [`FooterDataProvider`]
//!   (coding-standards §1.2 explicit-injection convention).
//! - `FooterDataProvider` (upstream `core/footer-data-provider.ts`) is ported
//!   in a reduced form: git branch tracking lives in
//!   `git_branch_watcher.rs` (a polling thread writing the provider slot and
//!   queueing `UiCommand::GitBranchChanged`); the `onBranchChange`
//!   subscription stays an empty hook kept for interface parity.
//! - `areExperimentalFeaturesEnabled` (core/experimental.ts) is ported as
//!   [`crate::core::environment::experimental_enabled`] and gates the `xp`
//!   suffix (footer.ts:162-164).

use std::collections::BTreeMap;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::sync::{Arc, Mutex};

use pir_agent::messages::AgentMessage;
use pir_agent::session::SessionEntry;
use pir_tui::tui::Component;
use pir_tui::utils::{truncate_to_width, visible_width};

use crate::core::agent_session::AgentSession;
use crate::core::themes::Theme;
use crate::core::usage_totals::{add_usage_to_totals, create_usage_totals};

/// `formatTokens` (footer.ts:24-30).
pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", (count as f64 / 1000.0).round() as u64);
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", (count as f64 / 1_000_000.0).round() as u64)
}

/// Lexically absolutize and normalize a path (JS `path.resolve` semantics:
/// no symlink resolution, `.`/`..` segments collapsed).
fn lexical_absolute(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            PathComponent::CurDir => {}
            PathComponent::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `formatCwdForFooter` (footer.ts:32-44): replace the home prefix with `~`
/// when the cwd lives inside the home directory.
pub fn format_cwd_for_footer(cwd: &Path, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return cwd.display().to_string();
    };
    let resolved_cwd = lexical_absolute(cwd);
    let resolved_home = lexical_absolute(home);
    match resolved_cwd.strip_prefix(&resolved_home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        // Not inside home: return the original cwd unchanged (footer.ts:42).
        Err(_) => cwd.display().to_string(),
    }
}

/// `sanitizeStatusText` (footer.ts:13-19).
fn sanitize_status_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_space = false;
    for c in text.chars() {
        let is_whitespace = matches!(c, '\r' | '\n' | '\t' | ' ');
        if is_whitespace {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            prev_space = false;
            result.push(c);
        }
    }
    result.trim().to_string()
}

/// Reduced port of `core/footer-data-provider.ts` (`FooterDataProvider`,
/// footer-data-provider.ts:99-382). The git branch refresh is driven by
/// `git_branch_watcher.rs` (polling thread); `setCwd` remains a plain slot
/// update — the watcher re-resolves paths on the next tick (upstream resets
/// the watchers eagerly, footer-data-provider.ts:169-184).
///
/// T15 hang points: extension statuses populated by the extension host,
/// provider count refresh after model catalog reloads.
pub struct FooterDataProvider {
    cwd: Mutex<PathBuf>,
    git_branch: Mutex<Option<String>>,
    available_provider_count: Mutex<usize>,
    extension_statuses: Mutex<BTreeMap<String, String>>,
}

impl FooterDataProvider {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: Mutex::new(cwd.to_path_buf()),
            git_branch: Mutex::new(None),
            available_provider_count: Mutex::new(0),
            extension_statuses: Mutex::new(BTreeMap::new()),
        }
    }

    /// `getGitBranch` (footer-data-provider.ts:127-132). Refreshed from the
    /// repository by the polling watcher in `git_branch_watcher.rs`.
    pub fn get_git_branch(&self) -> Option<String> {
        self.git_branch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_git_branch(&self, branch: Option<String>) {
        *self.git_branch.lock().unwrap_or_else(|e| e.into_inner()) = branch;
    }

    /// `getAvailableProviderCount` (footer-data-provider.ts:77-82).
    pub fn get_available_provider_count(&self) -> usize {
        *self
            .available_provider_count
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_available_provider_count(&self, count: usize) {
        *self
            .available_provider_count
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = count;
    }

    /// `getExtensionStatuses` (footer-data-provider.ts:90-98).
    pub fn get_extension_statuses(&self) -> BTreeMap<String, String> {
        self.extension_statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_extension_status(&self, key: impl Into<String>, text: impl Into<String>) {
        self.extension_statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.into(), text.into());
    }

    /// `onBranchChange` (footer-data-provider.ts:139-143). The mode wires
    /// branch changes through `UiCommand::GitBranchChanged` instead; this
    /// stays a no-op hook kept for interface parity (extensions).
    pub fn on_branch_change(&self, _callback: Box<dyn FnMut() + Send>) {}

    /// `setCwd` (footer-data-provider.ts; applied by `applyRuntimeSettings`
    /// on session rebind, interactive-mode.ts:1713).
    pub fn set_cwd(&self, cwd: &Path) {
        *self.cwd.lock().unwrap_or_else(|e| e.into_inner()) = cwd.to_path_buf();
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// `FooterComponent` (footer.ts:50-245).
pub struct FooterComponent {
    session: AgentSession,
    footer_data: Arc<FooterDataProvider>,
    auto_compact_enabled: bool,
    theme: Arc<Theme>,
}

impl FooterComponent {
    /// Whether auto-compaction is reflected in the footer (test accessor).
    #[cfg(test)]
    pub(crate) fn auto_compact_enabled(&self) -> bool {
        self.auto_compact_enabled
    }
}

impl FooterComponent {
    pub fn new(
        session: AgentSession,
        footer_data: Arc<FooterDataProvider>,
        theme: Arc<Theme>,
    ) -> Self {
        Self {
            session,
            footer_data,
            auto_compact_enabled: true,
            theme,
        }
    }

    /// `setSession` (footer.ts:60-62) — used on session rebind
    /// (`rebind_session_ui`).
    pub fn set_session(&mut self, session: AgentSession) {
        self.session = session;
    }

    /// `setAutoCompactEnabled` (footer.ts:64-66).
    pub fn set_auto_compact_enabled(&mut self, enabled: bool) {
        self.auto_compact_enabled = enabled;
    }

    /// The footer data provider (shared with the mode so provider count /
    /// git branch can be refreshed live).
    pub fn data_provider(&self) -> &Arc<FooterDataProvider> {
        &self.footer_data
    }
}

impl Component for FooterComponent {
    fn render(&self, width: usize) -> Vec<String> {
        // Cumulative usage across ALL session entries — not just
        // post-compaction messages (footer.ts:87-104).
        let mut usage_totals = create_usage_totals();
        let mut latest_cache_hit_rate: Option<f64> = None;

        let entries = self
            .session
            .session_manager()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_entries();
        for entry in &entries {
            match entry.known() {
                Some(SessionEntry::Message(message_entry)) => match &message_entry.message {
                    AgentMessage::Assistant(assistant) => {
                        add_usage_to_totals(&mut usage_totals, &assistant.usage);
                        let latest_prompt_tokens = assistant.usage.input
                            + assistant.usage.cache_read
                            + assistant.usage.cache_write;
                        latest_cache_hit_rate = if latest_prompt_tokens > 0 {
                            Some(
                                assistant.usage.cache_read as f64 / latest_prompt_tokens as f64
                                    * 100.0,
                            )
                        } else {
                            None
                        };
                    }
                    AgentMessage::ToolResult(tool_result) => {
                        if let Some(usage) = &tool_result.usage {
                            add_usage_to_totals(&mut usage_totals, usage);
                        }
                    }
                    _ => {}
                },
                Some(SessionEntry::BranchSummary(branch_summary)) => {
                    if let Some(usage) = &branch_summary.usage {
                        add_usage_to_totals(&mut usage_totals, usage);
                    }
                }
                Some(SessionEntry::Compaction(compaction)) => {
                    if let Some(usage) = &compaction.usage {
                        add_usage_to_totals(&mut usage_totals, usage);
                    }
                }
                _ => {}
            }
        }

        // Context usage from the session (handles compaction correctly);
        // tokens are unknown until the next LLM response after compaction
        // (footer.ts:106-111).
        let context_usage = self.session.get_context_usage();
        let context_window = context_usage
            .as_ref()
            .map(|c| c.context_window)
            .or_else(|| self.session.model().map(|m| u64::from(m.context_window)))
            .unwrap_or(0);
        let context_percent_value = context_usage
            .as_ref()
            .and_then(|c| c.percent)
            .unwrap_or(0.0);
        let context_percent = match context_usage.as_ref().and_then(|c| c.percent) {
            Some(percent) => format!("{percent:.1}"),
            None => "?".to_string(),
        };

        // pwd with `~` for home, git branch, session name (footer.ts:113-126).
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let cwd = self.footer_data.cwd();
        let mut pwd = format_cwd_for_footer(&cwd, home.as_deref());
        if let Some(branch) = self.footer_data.get_git_branch() {
            pwd = format!("{pwd} ({branch})");
        }
        if let Some(session_name) = self
            .session
            .session_manager()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_session_name()
        {
            pwd = format!("{pwd} • {session_name}");
        }

        // Stats line (footer.ts:128-137).
        let mut stats_parts: Vec<String> = Vec::new();
        if usage_totals.input > 0 {
            stats_parts.push(format!("↑{}", format_tokens(usage_totals.input)));
        }
        if usage_totals.output > 0 {
            stats_parts.push(format!("↓{}", format_tokens(usage_totals.output)));
        }
        if usage_totals.cache_read > 0 {
            stats_parts.push(format!("R{}", format_tokens(usage_totals.cache_read)));
        }
        if usage_totals.cache_write > 0 {
            stats_parts.push(format!("W{}", format_tokens(usage_totals.cache_write)));
        }
        if (usage_totals.cache_read > 0 || usage_totals.cache_write > 0)
            && latest_cache_hit_rate.is_some()
        {
            let hit_rate = latest_cache_hit_rate.unwrap_or(0.0);
            stats_parts.push(format!("CH{hit_rate:.1}%"));
        }

        // Kimi Coding is subscription-backed despite using API-key
        // authentication (footer.ts:138-145).
        let model = self.session.model();
        let using_subscription = model.as_ref().is_some_and(|m| {
            m.provider == "kimi-coding" || self.session.model_runtime().is_using_oauth(&m.provider)
        });
        if usage_totals.cost > 0.0 || using_subscription {
            let cost_str = format!(
                "${:.3}{}",
                usage_totals.cost,
                if using_subscription { " (sub)" } else { "" }
            );
            stats_parts.push(cost_str);
        }

        // Colorize context percentage (footer.ts:147-161).
        let auto_indicator = if self.auto_compact_enabled {
            " (auto)"
        } else {
            ""
        };
        let context_percent_display = if context_percent == "?" {
            format!("?/{}{auto_indicator}", format_tokens(context_window))
        } else {
            format!(
                "{context_percent}%/{}{auto_indicator}",
                format_tokens(context_window)
            )
        };
        let context_percent_str = if context_percent_value > 90.0 {
            self.theme.fg("error", &context_percent_display)
        } else if context_percent_value > 70.0 {
            self.theme.fg("warning", &context_percent_display)
        } else {
            context_percent_display
        };
        stats_parts.push(context_percent_str);
        // `areExperimentalFeaturesEnabled()` (footer.ts:162-164) →
        // `experimental_enabled` (core/environment.rs, PIR_EXPERIMENTAL=1).
        if crate::core::environment::experimental_enabled() {
            stats_parts.push(format!(
                "{} {}",
                self.theme.fg("dim", "•"),
                Theme::bold(&self.theme.fg("warning", "xp"))
            ));
        }

        let mut stats_left = stats_parts.join(" ");

        // Model name on the right side, plus thinking level if supported
        // (footer.ts:168-198).
        let model_name = model
            .as_ref()
            .map(|m| m.id.clone())
            .unwrap_or_else(|| "no-model".to_string());

        let mut stats_left_width = visible_width(&stats_left);
        if stats_left_width > width {
            stats_left = truncate_to_width(&stats_left, width, "...", false);
            stats_left_width = visible_width(&stats_left);
        }

        let min_padding = 2;

        let mut right_side_without_provider = model_name;
        if let Some(model) = &model {
            if model.reasoning {
                let thinking_level = self.session.thinking_level().as_str();
                right_side_without_provider = if thinking_level == "off" {
                    format!("{right_side_without_provider} • thinking off")
                } else {
                    format!("{right_side_without_provider} • {thinking_level}")
                };
            }
        }

        let mut right_side = right_side_without_provider.clone();
        if self.footer_data.get_available_provider_count() > 1 && model.is_some() {
            let provider = model.as_ref().map(|m| m.provider.as_str()).unwrap_or("");
            right_side = format!("({provider}) {right_side_without_provider}");
            if stats_left_width + min_padding + visible_width(&right_side) > width {
                right_side = right_side_without_provider.clone();
            }
        }

        let right_side_width = visible_width(&right_side);
        let total_needed = stats_left_width + min_padding + right_side_width;

        let stats_line = if total_needed <= width {
            let padding = " ".repeat(width - stats_left_width - right_side_width);
            format!("{stats_left}{padding}{right_side}")
        } else {
            let available_for_right = width as i64 - stats_left_width as i64 - min_padding as i64;
            if available_for_right > 0 {
                let truncated_right =
                    truncate_to_width(&right_side, available_for_right as usize, "", false);
                let truncated_right_width = visible_width(&truncated_right);
                let padding = " ".repeat(
                    (width as i64 - stats_left_width as i64 - truncated_right_width as i64).max(0)
                        as usize,
                );
                format!("{stats_left}{padding}{truncated_right}")
            } else {
                stats_left.clone()
            }
        };

        // Dim each part separately: stats_left may contain color codes (the
        // context %) that end with a reset, which would clear an outer dim
        // wrapper (footer.ts:222-227).
        let dim_stats_left = self.theme.fg("dim", &stats_left);
        let remainder = &stats_line[stats_left.len()..];
        let dim_remainder = self.theme.fg("dim", remainder);

        let pwd_line = truncate_to_width(
            &self.theme.fg("dim", &pwd),
            width,
            &self.theme.fg("dim", "..."),
            false,
        );
        let mut lines = vec![pwd_line, format!("{dim_stats_left}{dim_remainder}")];

        // Extension statuses on a single line, sorted by key (footer.ts:232-241).
        let extension_statuses = self.footer_data.get_extension_statuses();
        if !extension_statuses.is_empty() {
            let status_line = extension_statuses
                .values()
                .map(|text| sanitize_status_text(text))
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(truncate_to_width(
                &status_line,
                width,
                &self.theme.fg("dim", "..."),
                false,
            ));
        }

        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;

    #[test]
    fn format_tokens_boundaries() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1.0k");
        assert_eq!(format_tokens(9999), "10.0k");
        assert_eq!(format_tokens(10_000), "10k");
        assert_eq!(format_tokens(999_999), "1000k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(9_999_999), "10.0M");
        assert_eq!(format_tokens(10_000_000), "10M");
        assert_eq!(format_tokens(12_345_678), "12M");
    }

    #[test]
    fn format_cwd_replaces_home_prefix() {
        let home = Path::new("/home/user");
        assert_eq!(
            format_cwd_for_footer(Path::new("/home/user"), Some(home)),
            "~"
        );
        assert_eq!(
            format_cwd_for_footer(Path::new("/home/user/projects/pir"), Some(home)),
            "~/projects/pir"
        );
        // Outside home: unchanged.
        assert_eq!(
            format_cwd_for_footer(Path::new("/opt/pir"), Some(home)),
            "/opt/pir"
        );
        // Prefix-but-not-component is not inside home (/home/user2).
        assert_eq!(
            format_cwd_for_footer(Path::new("/home/user2/x"), Some(home)),
            "/home/user2/x"
        );
        // No home: unchanged.
        assert_eq!(
            format_cwd_for_footer(Path::new("/some/cwd"), None),
            "/some/cwd"
        );
    }

    #[test]
    fn sanitize_status_collapses_whitespace() {
        assert_eq!(sanitize_status_text("a\nb\tc  d"), "a b c d");
        assert_eq!(
            sanitize_status_text("  leading and trailing  "),
            "leading and trailing"
        );
        assert_eq!(sanitize_status_text("plain"), "plain");
    }

    #[test]
    fn lexical_absolute_normalizes_dot_segments() {
        assert_eq!(
            lexical_absolute(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(lexical_absolute(Path::new("/..")), PathBuf::from("/"));
    }

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme must load"))
    }

    // ---------------------------------------------------------------------
    // Render snapshots (real session over a temp agent dir; deterministic
    // usage-driven context percent, see `getContextUsage`).
    // ---------------------------------------------------------------------

    use crate::core::session_manager::SessionManager;
    use pir_ai::types::{
        ApiKind, AssistantContent, AssistantMessage, AssistantRole, StopReason, TextContent, Usage,
        UsageCost,
    };

    fn assistant_message(usage: Usage) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![AssistantContent::Text(TextContent {
                text: "ok".to_string(),
                text_signature: None,
            })],
            api: ApiKind("openai-completions".into()),
            provider: "custom".to_string(),
            model: "m1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage,
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 1_700_000_000_000,
        })
    }

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, cost: f64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
            cache_write1h: None,
            reasoning: None,
            total_tokens: input + output + cache_read + cache_write,
            cost: UsageCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: cost,
            },
        }
    }

    /// Render the footer for a session whose entries were appended through
    /// `append`.
    async fn render_footer(
        append: impl FnOnce(&mut SessionManager),
        width: usize,
    ) -> (
        Vec<String>,
        crate::modes::interactive::test_support::TestSession,
    ) {
        let harness = crate::modes::interactive::test_support::build_test_session().await;
        {
            let manager = harness.session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            append(&mut manager);
        }
        let footer = FooterComponent::new(
            harness.session.clone(),
            Arc::new(FooterDataProvider::new(&harness.cwd)),
            theme(),
        );
        let lines = footer.render(width);
        (lines, harness)
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::new();
        let mut chars = input.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for next in chars.by_ref() {
                    if next == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[tokio::test]
    async fn empty_session_renders_pwd_and_model() {
        let (lines, harness) = render_footer(|_| {}, 80).await;
        assert_eq!(lines.len(), 2);
        // Line 1: dim pwd (dim wrap leaves visible text intact).
        let pwd_line = strip_ansi(&lines[0]);
        assert!(
            pwd_line.contains("cwd"),
            "pwd line should contain the cwd, got: {pwd_line:?}"
        );
        // Line 2: no stats (no usage), right-aligned model name.
        let stats_line = strip_ansi(&lines[1]);
        let trimmed = stats_line.trim_end();
        assert!(
            trimmed.ends_with("m1"),
            "model name should be right-aligned, got: {trimmed:?}"
        );
        assert_eq!(trimmed.len(), 80, "stats line should be padded to width");
        let _ = harness;
    }

    #[tokio::test]
    async fn usage_totals_and_cache_hit_rate_render() {
        let (lines, _harness) = render_footer(
            |manager| {
                manager
                    .append_message(assistant_message(usage(
                        1500, 999999, 12000, 2_500_000, 0.1234,
                    )))
                    .expect("append assistant");
            },
            120,
        )
        .await;
        let stats = strip_ansi(&lines[1]);
        // formatTokens branches: <10k one decimal k, <1M integer k, <10M one
        // decimal M.
        assert!(stats.contains("↑1.5k"), "stats: {stats}");
        assert!(stats.contains("↓1000k"), "stats: {stats}");
        assert!(stats.contains("R12k"), "stats: {stats}");
        assert!(stats.contains("W2.5M"), "stats: {stats}");
        // CH = cacheRead / (input + cacheRead + cacheWrite) * 100
        // = 12000 / 2513500 * 100 = 0.477... -> "CH0.5%".
        assert!(stats.contains("CH0.5%"), "stats: {stats}");
        // Cost renders at 3 decimals.
        assert!(stats.contains("$0.123"), "stats: {stats}");
        // Context window display (200000 -> "200.0k") with the auto indicator.
        assert!(stats.contains("(auto)"), "stats: {stats}");
    }

    #[tokio::test]
    async fn context_percent_color_thresholds() {
        // The context estimate is usage-anchored: with a single assistant
        // message the estimate equals input+cacheRead+cacheWrite (200k
        // window, see agent_session.rs getContextUsage).
        let error_ansi = {
            let theme = theme();
            theme
                .fg("error", "")
                .strip_suffix("\u{1b}[39m")
                .expect("fg suffix")
                .to_string()
        };
        let warning_ansi = {
            let theme = theme();
            theme
                .fg("warning", "")
                .strip_suffix("\u{1b}[39m")
                .expect("fg suffix")
                .to_string()
        };

        for (input, band) in [
            (190_000u64, "error"),
            (150_000u64, "warning"),
            (100_000u64, "plain"),
        ] {
            let harness = crate::modes::interactive::test_support::build_test_session().await;
            // The context estimate is usage-anchored on the last assistant
            // message in the agent state (agent_session.rs getContextUsage);
            // seed the state directly instead of driving a real prompt.
            harness
                .session
                .agent()
                .set_messages(vec![assistant_message(usage(input, 0, 0, 0, 0.0))]);
            let footer = FooterComponent::new(
                harness.session.clone(),
                Arc::new(FooterDataProvider::new(&harness.cwd)),
                theme(),
            );
            let lines = footer.render(120);
            let stats = lines[1].clone();
            let plain = strip_ansi(&stats);
            // 95% / 75% / 50% of the 200k window.
            let percent = input as f64 / 200_000.0 * 100.0;
            let display = format!("{percent:.1}%/{} (auto)", format_tokens(200_000));
            assert!(plain.contains(&display), "band {band}: {plain}");
            match band {
                "error" => assert!(
                    stats.contains(&error_ansi),
                    "expected error color: {stats:?}"
                ),
                "warning" => assert!(
                    stats.contains(&warning_ansi),
                    "expected warning color: {stats:?}"
                ),
                _ => {
                    assert!(
                        !stats.contains(&error_ansi),
                        "unexpected error color: {stats:?}"
                    );
                    assert!(
                        !stats.contains(&warning_ansi),
                        "unexpected warning color: {stats:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn truncation_falls_back_in_stages() {
        let (lines, _harness) = render_footer(
            |manager| {
                manager
                    .append_message(assistant_message(usage(
                        1500, 999999, 12000, 2_500_000, 0.1234,
                    )))
                    .expect("append assistant");
            },
            30,
        )
        .await;
        let stats = lines[1].clone();
        // Too narrow for the full stats: left side truncates with "...".
        assert!(stats.contains("..."), "stats: {stats:?}");
        assert!(
            pir_tui::utils::visible_width(&stats) <= 30,
            "width: {stats:?}"
        );

        // Narrower still: the model name is dropped entirely.
        let (lines, _harness) = render_footer(
            |manager| {
                manager
                    .append_message(assistant_message(usage(1500, 0, 0, 0, 0.0)))
                    .expect("append assistant");
            },
            10,
        )
        .await;
        let stats = strip_ansi(&lines[1]);
        assert!(!stats.contains("m1"), "model should be dropped: {stats:?}");
        assert!(!stats.is_empty(), "left stats remain");
    }

    #[tokio::test]
    async fn provider_prefix_appears_with_multiple_providers() {
        let harness = crate::modes::interactive::test_support::build_test_session().await;
        let footer_data = Arc::new(FooterDataProvider::new(&harness.cwd));
        footer_data.set_available_provider_count(2);
        let footer = FooterComponent::new(harness.session.clone(), footer_data, theme());
        let stats = strip_ansi(&footer.render(80)[1]);
        // "(custom) m1" when there is room.
        assert!(stats.contains("(custom) m1"), "stats: {stats}");

        // Too narrow for the provider prefix: falls back to the bare model
        // name (footer.ts:192-198).
        let stats = strip_ansi(&footer.render(20)[1]);
        assert!(!stats.contains("(custom)"), "stats: {stats}");
        assert!(stats.contains("m1"), "stats: {stats}");
    }

    #[tokio::test]
    async fn pwd_line_includes_branch_and_session_name() {
        let harness = crate::modes::interactive::test_support::build_test_session().await;
        harness
            .session
            .session_manager()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_session_info("my session")
            .expect("append session info");
        let footer_data = Arc::new(FooterDataProvider::new(&harness.cwd));
        footer_data.set_git_branch(Some("main".to_string()));
        let footer = FooterComponent::new(harness.session.clone(), footer_data, theme());
        let pwd_line = strip_ansi(&footer.render(80)[0]);
        assert!(pwd_line.contains("cwd"), "pwd: {pwd_line}");
        assert!(pwd_line.contains("(main)"), "pwd: {pwd_line}");
        assert!(pwd_line.contains("• my session"), "pwd: {pwd_line}");
    }

    #[tokio::test]
    async fn extension_status_line_is_sorted_and_truncated() {
        let harness = crate::modes::interactive::test_support::build_test_session().await;
        let footer_data = Arc::new(FooterDataProvider::new(&harness.cwd));
        footer_data.set_extension_status("zeta", "z status");
        footer_data.set_extension_status("alpha", "a  status\nwith\tnewlines");
        let footer = FooterComponent::new(harness.session.clone(), footer_data, theme());
        let lines = footer.render(80);
        assert_eq!(lines.len(), 3);
        let status_line = strip_ansi(&lines[2]);
        // Sorted by key and sanitized (single spaces).
        assert_eq!(
            status_line, "a status with newlines z status",
            "status: {status_line}"
        );
    }

    /// T12-S7a：compaction 后无 post-compaction usage → context 百分比 "?"。
    #[tokio::test]
    async fn context_percent_unknown_after_compaction_without_post_usage() {
        let harness = crate::modes::interactive::test_support::build_test_session().await;
        harness
            .session
            .agent()
            .set_messages(vec![assistant_message(usage(1500, 0, 0, 0, 0.0))]);
        {
            let manager = harness.session.session_manager();
            let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .append_message(assistant_message(usage(1500, 0, 0, 0, 0.0)))
                .expect("append");
            manager
                .append_compaction("summary", "kept", 1234, None, None, None)
                .expect("append compaction");
        }
        let footer = FooterComponent::new(
            harness.session.clone(),
            Arc::new(FooterDataProvider::new(&harness.cwd)),
            theme(),
        );
        let lines = footer.render(80);
        let stats = strip_ansi(&lines[1]);
        // Compaction entry with no post-compaction assistant usage → "?".
        assert!(
            stats.contains(&format!("?/{} (auto)", format_tokens(200_000))),
            "context percent unknown after compaction: {stats}"
        );
    }
}
