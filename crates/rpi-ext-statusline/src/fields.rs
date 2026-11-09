//! The strict 30-field registry for `statusLine.type:"items"` (TE52 /
//! ADR-0040 R20 §4.2–§4.3).
//!
//! One static descriptor table describes every field: its canonical id, an
//! optional display alias, the data class that must be collected for it, and
//! the value accessor. Renderers never special-case fields; they order the
//! configured ids, call [`value`] and drop `None` (missing data is omitted,
//! never invented).
//!
//! Semantics frozen by ADR-0040:
//! - Session totals and counts come exclusively from the host's absolute
//!   `ctx.sessionStats()` snapshot; this module only derives display values
//!   (`used = input + cacheWrite + output`,
//!   `total-input = input + cacheRead + cacheWrite`, `total-output = output`,
//!   `total = input + output + cacheRead + cacheWrite`,
//!   `turns = userMessages`, `steps = assistantMessages`).
//! - Context values come from `ctx.getContextUsage()` (the active context),
//!   not from the session totals.
//! - Live values come from [`LiveView`]: the provider's exact output usage
//!   when available, otherwise the model tokenizer / CJK-aware estimate from
//!   [`crate::tokens`]. Estimates are marked with `~`; they are never
//!   presented as provider-exact. While the live measurement is armed a live
//!   field with no value yet renders its dash placeholder
//!   ([`FieldId::placeholder`]) in the field's own tone so the line keeps its
//!   width and color; every other missing field is omitted.
//! - Values are sanitized (control characters, data ANSI SGR stripped) and
//!   colors are generated only from the host theme palette.

use chrono::{DateTime, Utc};

use crate::git::GitInfo;
use crate::session_stats::SessionStats;
use crate::tokens::TokenBreakdown;

/// Token/window number formatting (`statusLine.numberFormat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NumberFormat {
    /// `128K`-style abbreviation (the built-in footer's `formatTokens`).
    #[default]
    Compact,
    /// Full decimal digits.
    Exact,
}

impl NumberFormat {
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("exact") => Self::Exact,
            _ => Self::Compact,
        }
    }
}

/// The 29 whitelisted fields, in the canonical registry order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldId {
    // Model & mode (5)
    Model,
    ModelWithReasoning,
    Reasoning,
    PermissionMode,
    RunState,
    // Workspace (3)
    CurrentDir,
    ProjectName,
    GitBranch,
    // Context (4)
    ContextUsed,
    ContextRemaining,
    ContextWindowSize,
    ContextTokens,
    // Session & tokens (12)
    SessionName,
    SessionCost,
    SessionDuration,
    UsedTokens,
    TotalInputTokens,
    TotalOutputTokens,
    TotalTokens,
    /// Session snapshot total plus the in-flight assistant output
    /// (`liveTotal = snapshot + pendingExact + currentLive`).
    LiveTotalTokens,
    TurnCount,
    StepCount,
    CacheReadTokens,
    CacheWriteTokens,
    // Live (6)
    Streaming,
    LiveOutputTokens,
    LiveOutputRate,
    LiveTextTokens,
    LiveThinkingTokens,
    LiveToolcallTokens,
}

/// All 30 fields in registry order.
pub const ALL_FIELDS: [FieldId; 30] = [
    FieldId::Model,
    FieldId::ModelWithReasoning,
    FieldId::Reasoning,
    FieldId::PermissionMode,
    FieldId::RunState,
    FieldId::CurrentDir,
    FieldId::ProjectName,
    FieldId::GitBranch,
    FieldId::ContextUsed,
    FieldId::ContextRemaining,
    FieldId::ContextWindowSize,
    FieldId::ContextTokens,
    FieldId::SessionName,
    FieldId::SessionCost,
    FieldId::SessionDuration,
    FieldId::UsedTokens,
    FieldId::TotalInputTokens,
    FieldId::TotalOutputTokens,
    FieldId::TotalTokens,
    FieldId::LiveTotalTokens,
    FieldId::TurnCount,
    FieldId::StepCount,
    FieldId::CacheReadTokens,
    FieldId::CacheWriteTokens,
    FieldId::Streaming,
    FieldId::LiveOutputTokens,
    FieldId::LiveOutputRate,
    FieldId::LiveTextTokens,
    FieldId::LiveThinkingTokens,
    FieldId::LiveToolcallTokens,
];

impl FieldId {
    /// Canonical id as written in settings.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::ModelWithReasoning => "model-with-reasoning",
            Self::Reasoning => "reasoning",
            Self::PermissionMode => "permission-mode",
            Self::RunState => "run-state",
            Self::CurrentDir => "current-dir",
            Self::ProjectName => "project-name",
            Self::GitBranch => "git-branch",
            Self::ContextUsed => "context-used",
            Self::ContextRemaining => "context-remaining",
            Self::ContextWindowSize => "context-window-size",
            Self::ContextTokens => "context-tokens",
            Self::SessionName => "session-name",
            Self::SessionCost => "session-cost",
            Self::SessionDuration => "session-duration",
            Self::UsedTokens => "used-tokens",
            Self::TotalInputTokens => "total-input-tokens",
            Self::TotalOutputTokens => "total-output-tokens",
            Self::TotalTokens => "total-tokens",
            Self::LiveTotalTokens => "live-total-tokens",
            Self::TurnCount => "turn-count",
            Self::StepCount => "step-count",
            Self::CacheReadTokens => "cache-read-tokens",
            Self::CacheWriteTokens => "cache-write-tokens",
            Self::Streaming => "streaming",
            Self::LiveOutputTokens => "live-output-tokens",
            Self::LiveOutputRate => "live-output-rate",
            Self::LiveTextTokens => "live-text-tokens",
            Self::LiveThinkingTokens => "live-thinking-tokens",
            Self::LiveToolcallTokens => "live-toolcall-tokens",
        }
    }

    /// Resolve a canonical id or one of the five frozen aliases.
    ///
    /// `permissions` is deliberately NOT an alias: it means sandbox/approval
    /// in other tools, which exceeds rpi's `default`/`plan` permission mode.
    pub fn parse(raw: &str) -> Option<Self> {
        let canonical = match raw {
            // White-listed semantic aliases (R20 §4.3).
            "model-name" => "model",
            "project" | "project-root" => "project-name",
            "status" => "run-state",
            "context-usage" => "context-used",
            "thread-name" => "session-name",
            other => other,
        };
        ALL_FIELDS.iter().copied().find(|id| id.id() == canonical)
    }

    /// Whether the field belongs to the live measurement group.
    pub const fn is_live(self) -> bool {
        matches!(
            self,
            Self::Streaming
                | Self::LiveOutputTokens
                | Self::LiveOutputRate
                | Self::LiveTextTokens
                | Self::LiveThinkingTokens
                | Self::LiveToolcallTokens
                | Self::LiveTotalTokens
        )
    }

    /// Whether the field reads the session snapshot (totals and counts).
    pub const fn is_session_total(self) -> bool {
        matches!(
            self,
            Self::SessionCost
                | Self::UsedTokens
                | Self::TotalInputTokens
                | Self::TotalOutputTokens
                | Self::TotalTokens
                | Self::LiveTotalTokens
                | Self::TurnCount
                | Self::StepCount
                | Self::CacheReadTokens
                | Self::CacheWriteTokens
        )
    }

    /// Whether the field needs the local Git probe.
    pub const fn needs_git(self) -> bool {
        matches!(self, Self::ProjectName | Self::GitBranch)
    }

    /// Placeholder text **and its tone** for a live field whose value is
    /// temporarily absent (before the first delta, while the rate window is
    /// still too short, or for a tier that is still empty).
    ///
    /// Live rows keep their slot with this text instead of disappearing, so a
    /// refreshing status line does not reflow while a stream is measured. The
    /// tone is the field's own (the live rows are all success-toned): a dash
    /// in the muted gray reads as "disabled" and visibly loses the color the
    /// surrounding numbers have. Non-live fields keep the "missing data is
    /// omitted" contract and return `None` here.
    pub const fn placeholder(self) -> Option<(&'static str, Tone)> {
        match self {
            Self::Streaming => Some(("-", Tone::Success)),
            Self::LiveOutputTokens => Some(("-", Tone::Success)),
            Self::LiveOutputRate => Some(("- tok/s", Tone::Success)),
            Self::LiveTextTokens => Some(("- text", Tone::Success)),
            Self::LiveThinkingTokens => Some(("- think", Tone::Success)),
            Self::LiveToolcallTokens => Some(("- tool", Tone::Success)),
            _ => None,
        }
    }
}

/// Theme-derived color tone. The palette maps tones to SGR sequences; a
/// missing/invalid color renders the field as plain text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    #[default]
    Plain,
    Accent,
    Muted,
    Success,
    Warning,
    Error,
    Info,
}

/// Resolved theme palette (only the semantic colors this plugin consumes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Palette {
    accent: Option<String>,
    muted: Option<String>,
    success: Option<String>,
    warning: Option<String>,
    error: Option<String>,
    info: Option<String>,
    selected_bg: Option<String>,
}

impl Palette {
    /// Resolve the semantic colors from a `ui.theme` reply.
    ///
    /// Theme color values may be hex, a variable name (`accent: "violet"`),
    /// a 256-color index, `oklch(...)`/`okhsl(...)` (host ThemeJson), or a
    /// named base color. Hex/OKLCH/OKHSL resolve to truecolor through the
    /// host color model, a 256-color index to its palette entry, and named
    /// base colors map to their ANSI family;
    /// genuinely unknown values fall back to plain text — the contract says
    /// invalid/missing colors must not be guessed.
    pub fn from_theme(theme: &serde_json::Value) -> Self {
        let resolve = |key: &str| -> Option<String> {
            let colors = theme.get("colors")?.as_object()?;
            let vars = theme.get("vars").and_then(serde_json::Value::as_object);
            let raw = colors.get(key)?;
            resolve_color(raw, vars, 0)
        };
        Self {
            accent: resolve("accent"),
            muted: resolve("muted").or_else(|| resolve("dim")),
            success: resolve("success"),
            warning: resolve("warning"),
            error: resolve("error"),
            info: resolve("border").or_else(|| resolve("accent")),
            selected_bg: resolve("selectedBg").or_else(|| resolve("selectionBg")),
        }
    }

    /// SGR parameters (without `\x1b[` / `m`) for one tone.
    pub fn sgr(&self, tone: Tone) -> Option<&str> {
        match tone {
            Tone::Plain => None,
            Tone::Accent => self.accent.as_deref(),
            Tone::Muted => self.muted.as_deref(),
            Tone::Success => self.success.as_deref(),
            Tone::Warning => self.warning.as_deref(),
            Tone::Error => self.error.as_deref(),
            Tone::Info => self.info.as_deref(),
        }
    }

    /// Selected-row background SGR parameters, when the theme provides one.
    pub fn selected_bg_sgr(&self) -> Option<&str> {
        self.selected_bg.as_deref()
    }
}

/// Resolve one theme color value: hex → truecolor, named base colors → SGR,
/// variable references followed (bounded), everything else plain.
fn resolve_color(
    raw: &serde_json::Value,
    vars: Option<&serde_json::Map<String, serde_json::Value>>,
    depth: usize,
) -> Option<String> {
    if depth > 4 {
        return None;
    }
    match raw {
        serde_json::Value::Number(index) => {
            let index = index.as_u64()?;
            (index <= 255).then(|| format!("38;5;{index}"))
        }
        serde_json::Value::String(text) => {
            let text = text.trim();
            if let Some(hex) = text.strip_prefix('#') {
                return parse_hex(hex).map(|(r, g, b)| format!("38;2;{r};{g};{b}"));
            }
            if let Some(variable) = vars.and_then(|vars| vars.get(text)) {
                return resolve_color(variable, vars, depth + 1);
            }
            // Host theme colors are ThemeJson literals: `oklch(...)` /
            // `okhsl(...)` are first-class, so reuse the host's parser
            // instead of treating them as invalid. The converted RGB gets a
            // truecolor SGR parameter.
            if let Ok(color) = rpi_tui::colors::parse_color(text) {
                let rgb = rpi_tui::colors::color_to_rgb(&color);
                return Some(format!(
                    "38;2;{};{};{}",
                    rgb.r.round() as u8,
                    rgb.g.round() as u8,
                    rgb.b.round() as u8
                ));
            }
            named_sgr(text).map(str::to_owned)
        }
        _ => None,
    }
}

fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim();
    let value = match hex.len() {
        3 => {
            let mut expanded = String::with_capacity(6);
            for ch in hex.chars() {
                expanded.push(ch);
                expanded.push(ch);
            }
            u32::from_str_radix(&expanded, 16).ok()?
        }
        6 => u32::from_str_radix(hex, 16).ok()?,
        _ => return None,
    };
    Some((
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    ))
}

/// Base ANSI color names accepted as a palette value; unknown names are a
/// plain-text fallback (host OKLCH/OKHSL/hex are handled before this).
fn named_sgr(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "black" => Some("30"),
        "red" => Some("31"),
        "green" => Some("32"),
        "yellow" | "gold" | "orange" => Some("33"),
        "blue" => Some("34"),
        "magenta" | "violet" | "purple" | "pink" => Some("35"),
        "cyan" | "teal" => Some("36"),
        "white" => Some("37"),
        "gray" | "grey" | "dim" | "muted" => Some("90"),
        "bright-red" => Some("91"),
        "bright-green" => Some("92"),
        "bright-yellow" => Some("93"),
        "bright-blue" => Some("94"),
        "bright-magenta" | "bright-violet" => Some("95"),
        "bright-cyan" => Some("96"),
        _ => None,
    }
}

/// Current live measurement projection (provider values + token estimate).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveView {
    /// An assistant message is in flight.
    pub streaming: bool,
    /// Provider cumulative output tokens while streaming (`message_update`).
    pub output_tokens: Option<u64>,
    /// Provider exact output tokens after `message_end`.
    pub output_tokens_exact: Option<u64>,
    /// First delta → now / frozen at `message_end`.
    pub decode_ms: u128,
    /// Tiered token count from the model tokenizer or the CJK heuristic.
    /// `None` only for a live view assembled without text data (tests).
    pub tokens: Option<TokenBreakdown>,
    /// This streaming frame switched the rate's numerator source (estimate ↔
    /// provider-reported) and therefore measures two different things at once
    /// (rpi#78 B). The rate row holds the previous reading instead of
    /// publishing a number that mixes bases.
    pub rate_source_switched: bool,
}

impl LiveView {
    /// The output count for the current message: the provider's exact value
    /// while in flight or once the stream settles, otherwise the local
    /// bundled tokenizer/heuristic breakdown (R20 §4.3 + the 2026-11 realtime
    /// estimate extension).
    pub fn reported_output(self) -> Option<TokenBreakdown> {
        let estimated = self.tokens.unwrap_or_default();
        let provider = if self.streaming {
            self.output_tokens.or(self.output_tokens_exact)
        } else {
            self.output_tokens_exact.or(self.output_tokens)
        };
        match provider {
            Some(total) if total > 0 => Some(estimated.with_provider_total(total)),
            _ if estimated.total > 0 => Some(estimated),
            _ => None,
        }
    }
}

/// The model identity consumed by the model fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelInfo {
    pub id: String,
    pub name: Option<String>,
}

impl ModelInfo {
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.id)
    }
}

/// `ctx.getContextUsage()` values (the active context, not the session sum).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ContextInfo {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

/// Everything a field accessor may read, assembled once per render frame.
#[derive(Debug, Clone, Default)]
pub struct FieldData {
    pub cwd: Option<String>,
    pub model: Option<ModelInfo>,
    pub thinking: Option<String>,
    pub mode: Option<String>,
    pub idle: Option<bool>,
    pub context: Option<ContextInfo>,
    pub session_name: Option<String>,
    pub stats: Option<SessionStats>,
    /// Exact output tokens of assistant messages that ended after the last
    /// applied `ctx.sessionStats` snapshot (`live-total-tokens` only).
    pub pending_output: u64,
    pub live: Option<LiveView>,
    /// Last rendered value of every live row (items mode): a row with no
    /// fresh value in this frame (the gap between two assistant messages, the
    /// rate window before 0.5 s, a tier the new message has not produced yet)
    /// keeps this value instead of flickering to its dash placeholder.
    pub live_held: Vec<(FieldId, FieldValue)>,
    pub git: Option<GitInfo>,
    /// Frame clock (session-duration).
    pub now: Option<DateTime<Utc>>,
    pub number_format: NumberFormat,
    pub palette: Option<Palette>,
    /// Preview mode: missing values render as the field placeholder instead
    /// of disappearing (editor only; the live footer never uses this).
    pub preview: bool,
}

/// One rendered field value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldValue {
    pub text: String,
    pub tone: Tone,
}

impl FieldValue {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: sanitize_text(&text.into()),
            tone,
        }
    }
}

/// The placeholder shown by the editor preview when a field has no data.
pub fn placeholder(id: FieldId) -> String {
    format!("({})", id.id())
}

/// Evaluate one field. `None` = the field has no data for this frame and is
/// omitted (never replaced by a zero, a guess or an estimate).
///
/// Two deliberate exceptions keep a slot instead of dropping it:
/// - the editor preview renders every missing field as `(id)`;
/// - a live field renders [`FieldId::placeholder`] while the live
///   measurement is armed, so its row does not vanish when a stream starts
///   or the rate window opens. The placeholder keeps the field's tone, not
///   the muted one.
pub fn value(id: FieldId, data: &FieldData) -> Option<FieldValue> {
    let value = raw_value(id, data);
    if value.is_some() {
        return value;
    }
    if data.preview {
        return Some(FieldValue::new(placeholder(id), Tone::Muted));
    }
    let (text, tone) = id.placeholder()?;
    // `live` is `Some` only while the measurement is armed (`liveTokens`
    // enabled AND at least one live field selected): a disabled or
    // unselected live field stays hidden instead of showing a stale dash.
    let live = data.live.as_ref()?;
    // Hold the last rendered value while this frame has no fresh one — the
    // gap between two assistant messages (measurement reset, no delta yet)
    // and the rate window before it is stable. The hold is STREAMING-ONLY:
    // once a message settles, its frozen measurements are authoritative, so
    // a missing value must render the placeholder instead of leaking the
    // previous message's number (a short reply used to keep the old rate
    // forever). `streaming` is never held: during a real gap it must not
    // claim a stream is running.
    if id != FieldId::Streaming
        && live.streaming
        && let Some((_, held)) = data.live_held.iter().find(|(held, _)| *held == id)
    {
        return Some(held.clone());
    }
    Some(FieldValue::new(text, tone))
}

/// This frame's own reading for a field, without the preview placeholder and
/// without the [`FieldData::live_held`] fallback. The items refresh uses it
/// to record what the next frame may hold.
pub fn fresh_value(id: FieldId, data: &FieldData) -> Option<FieldValue> {
    raw_value(id, data)
}

fn raw_value(id: FieldId, data: &FieldData) -> Option<FieldValue> {
    match id {
        FieldId::Model => {
            let model = data.model.as_ref()?;
            Some(FieldValue::new(model.display_name(), Tone::Accent))
        }
        FieldId::ModelWithReasoning => {
            let model = data.model.as_ref()?;
            match reasoning_text(data) {
                Some(reasoning) => Some(FieldValue::new(
                    format!("{} ({reasoning})", model.display_name()),
                    Tone::Accent,
                )),
                None => Some(FieldValue::new(model.display_name(), Tone::Accent)),
            }
        }
        FieldId::Reasoning => {
            reasoning_text(data).map(|reasoning| FieldValue::new(reasoning, Tone::Muted))
        }
        FieldId::PermissionMode => match data.mode.as_deref() {
            Some("default") | Some("plan") => {
                Some(FieldValue::new(data.mode.clone()?, Tone::Warning))
            }
            _ => None,
        },
        FieldId::RunState => data.idle.map(|idle| {
            if idle {
                FieldValue::new("Ready", Tone::Muted)
            } else {
                FieldValue::new("Working", Tone::Success)
            }
        }),
        FieldId::CurrentDir => data
            .cwd
            .as_deref()
            .filter(|cwd| !cwd.is_empty())
            .map(|cwd| FieldValue::new(cwd, Tone::Plain)),
        FieldId::ProjectName => project_name(data).map(|name| FieldValue::new(name, Tone::Accent)),
        FieldId::GitBranch => data
            .git
            .as_ref()
            .and_then(|git| git.branch.clone())
            .filter(|branch| !branch.is_empty())
            .map(|branch| FieldValue::new(branch, Tone::Info)),
        FieldId::ContextUsed => {
            let percent = context_percent(data)?;
            Some(FieldValue::new(
                format!("Context {} used", format_percent(percent)),
                context_tone(percent),
            ))
        }
        FieldId::ContextRemaining => {
            let percent = context_percent(data)?;
            Some(FieldValue::new(
                format!("Context {} left", format_percent(100.0 - percent)),
                context_tone(percent),
            ))
        }
        FieldId::ContextWindowSize => {
            let window = data
                .context
                .as_ref()
                .map(|context| context.context_window)?;
            (window > 0).then(|| {
                FieldValue::new(
                    format!("{} window", format_tokens(window, data.number_format)),
                    Tone::Muted,
                )
            })
        }
        FieldId::ContextTokens => data
            .context
            .as_ref()
            .and_then(|context| context.tokens)
            .map(|tokens| FieldValue::new(format_tokens(tokens, data.number_format), Tone::Muted)),
        FieldId::SessionName => data
            .session_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map(|name| FieldValue::new(name, Tone::Accent)),
        FieldId::SessionCost => {
            let cost = data.stats.as_ref()?.cost;
            Some(FieldValue::new(format_cost(cost), Tone::Muted))
        }
        FieldId::SessionDuration => {
            let started = data.stats.as_ref()?.started_at_utc()?;
            let now = data.now?;
            let elapsed = now.signed_duration_since(started);
            let ms = elapsed.num_milliseconds().max(0) as u128;
            Some(FieldValue::new(format_duration(ms), Tone::Muted))
        }
        FieldId::UsedTokens => {
            let tokens = data.stats.as_ref()?.tokens;
            let used = tokens
                .input
                .saturating_add(tokens.cache_write)
                .saturating_add(tokens.output);
            Some(FieldValue::new(
                format_tokens(used, data.number_format),
                Tone::Plain,
            ))
        }
        FieldId::TotalInputTokens => {
            let tokens = data.stats.as_ref()?.tokens;
            let total = tokens
                .input
                .saturating_add(tokens.cache_read)
                .saturating_add(tokens.cache_write);
            Some(FieldValue::new(
                format!("{} in", format_tokens(total, data.number_format)),
                Tone::Muted,
            ))
        }
        FieldId::TotalOutputTokens => {
            let tokens = data.stats.as_ref()?.tokens;
            Some(FieldValue::new(
                format!("{} out", format_tokens(tokens.output, data.number_format)),
                Tone::Muted,
            ))
        }
        FieldId::TotalTokens => {
            let tokens = data.stats.as_ref()?.tokens;
            let total = tokens
                .input
                .saturating_add(tokens.output)
                .saturating_add(tokens.cache_read)
                .saturating_add(tokens.cache_write);
            Some(FieldValue::new(
                format_tokens(total, data.number_format),
                Tone::Plain,
            ))
        }
        FieldId::LiveTotalTokens => {
            // The snapshot total plus the not-yet-committed assistant output:
            // the exact output of messages that ended since the last applied
            // snapshot (`pending_output`), plus the current message's live
            // output while it streams. Estimates keep the `~` marker; the
            // snapshot handoff clears the pending part, so the row never
            // double counts a committed message.
            let tokens = data.stats.as_ref()?.tokens;
            let mut total = tokens
                .input
                .saturating_add(tokens.output)
                .saturating_add(tokens.cache_read)
                .saturating_add(tokens.cache_write)
                .saturating_add(data.pending_output);
            let mut marker = "";
            if let Some(output) = data
                .live
                .filter(|live| live.streaming)
                .and_then(LiveView::reported_output)
            {
                total = total.saturating_add(output.total);
                marker = output.total_marker();
            }
            Some(FieldValue::new(
                format!("{marker}{}", format_tokens(total, data.number_format)),
                Tone::Plain,
            ))
        }
        FieldId::TurnCount => {
            let turns = data.stats.as_ref()?.user_messages;
            Some(FieldValue::new(format!("{turns} turns"), Tone::Muted))
        }
        FieldId::StepCount => {
            let steps = data.stats.as_ref()?.assistant_messages;
            Some(FieldValue::new(format!("{steps} steps"), Tone::Muted))
        }
        FieldId::CacheReadTokens => {
            let tokens = data.stats.as_ref()?.tokens;
            Some(FieldValue::new(
                format!(
                    "{} cache-read",
                    format_tokens(tokens.cache_read, data.number_format)
                ),
                Tone::Muted,
            ))
        }
        FieldId::CacheWriteTokens => {
            let tokens = data.stats.as_ref()?.tokens;
            Some(FieldValue::new(
                format!(
                    "{} cache-write",
                    format_tokens(tokens.cache_write, data.number_format)
                ),
                Tone::Muted,
            ))
        }
        FieldId::Streaming => {
            let live = data.live?;
            live.streaming
                .then(|| FieldValue::new("streaming", Tone::Success))
        }
        FieldId::LiveOutputTokens => {
            let tokens = data.live?.reported_output()?;
            Some(FieldValue::new(
                format!(
                    "{}{}",
                    tokens.total_marker(),
                    format_tokens(tokens.total, data.number_format)
                ),
                Tone::Success,
            ))
        }
        FieldId::LiveOutputRate => {
            let live = data.live?;
            let tokens = live.reported_output()?;
            // While streaming, keep the reference 0.5 s guard: the decode
            // window is still growing and a not-yet-stable denominator must
            // not make the row jump. Once the message settled, `decode_ms`
            // is the FROZEN first-delta→end window, so any positive value
            // yields the final measured rate — a short reply must refresh
            // its own rate instead of holding the previous message's.
            if live.streaming && live.decode_ms < MIN_RATE_DECODE_MS {
                return None;
            }
            // rpi#78 B: a frame whose numerator source just changed (local
            // estimate ↔ provider-reported) mixes two measurement bases over
            // one window; hold the previous reading rather than publish the
            // jump. The settled frame never suppresses: both numerator and
            // denominator are frozen there, so the number is the message's
            // own average.
            if live.streaming && live.rate_source_switched {
                return None;
            }
            if live.decode_ms == 0 {
                return None;
            }
            let rate = tokens.total as f64 * 1000.0 / live.decode_ms as f64;
            Some(FieldValue::new(
                format!("{}{rate:.1} tok/s", tokens.total_marker()),
                Tone::Success,
            ))
        }
        FieldId::LiveTextTokens => {
            let tokens = data.live?.reported_output()?;
            (tokens.text > 0).then(|| {
                FieldValue::new(
                    format!(
                        "{}{} text",
                        tokens.tier_marker(),
                        format_tokens(tokens.text, data.number_format)
                    ),
                    Tone::Success,
                )
            })
        }
        FieldId::LiveThinkingTokens => {
            let tokens = data.live?.reported_output()?;
            (tokens.thinking > 0).then(|| {
                FieldValue::new(
                    format!(
                        "{}{} think",
                        tokens.tier_marker(),
                        format_tokens(tokens.thinking, data.number_format)
                    ),
                    Tone::Success,
                )
            })
        }
        FieldId::LiveToolcallTokens => {
            let tokens = data.live?.reported_output()?;
            (tokens.toolcall > 0).then(|| {
                FieldValue::new(
                    format!(
                        "{}{} tool",
                        tokens.tier_marker(),
                        format_tokens(tokens.toolcall, data.number_format)
                    ),
                    Tone::Success,
                )
            })
        }
    }
}

/// While streaming, the rate is hidden until the decode window is long
/// enough to be stable (same 0.5 s guard as the reference statusline
/// script). A settled message ignores the guard: its window is frozen and
/// the final rate is the accurate value for that message.
const MIN_RATE_DECODE_MS: u128 = 500;

fn reasoning_text(data: &FieldData) -> Option<String> {
    data.thinking
        .as_deref()
        .filter(|level| !level.is_empty() && *level != "off")
        .map(str::to_owned)
}

fn project_name(data: &FieldData) -> Option<String> {
    if let Some(root) = data.git.as_ref().and_then(|git| git.root.as_deref()) {
        return std::path::Path::new(root)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty());
    }
    let cwd = data.cwd.as_deref().filter(|cwd| !cwd.is_empty())?;
    std::path::Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
}

fn context_percent(data: &FieldData) -> Option<f64> {
    let context = data.context.as_ref()?;
    if let Some(percent) = context.percent {
        return Some(percent.clamp(0.0, 100.0));
    }
    let tokens = context.tokens?;
    if context.context_window == 0 {
        return None;
    }
    Some((tokens as f64 / context.context_window as f64 * 100.0).clamp(0.0, 100.0))
}

fn context_tone(percent: f64) -> Tone {
    if percent >= 90.0 {
        Tone::Error
    } else if percent >= 80.0 {
        Tone::Warning
    } else {
        Tone::Muted
    }
}

/// Render the configured item order into one line. `None` when no selected
/// field produced a value (the empty-output case: replace restores the
/// built-in footer, widget/status clear their own key).
pub fn render_line(
    entries: &[crate::items::ItemEntry],
    separator: &str,
    use_colors: bool,
    data: &FieldData,
) -> Option<String> {
    let mut parts = Vec::new();
    for entry in entries {
        let Some(id) = entry.field else {
            continue;
        };
        let Some(value) = value(id, data) else {
            continue;
        };
        let text = if use_colors {
            match data
                .palette
                .as_ref()
                .and_then(|palette| palette.sgr(value.tone))
            {
                Some(sgr) => format!("\x1b[{sgr}m{}\x1b[0m", value.text),
                None => value.text,
            }
        } else {
            value.text
        };
        parts.push(text);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(separator))
    }
}

/// `formatTokens` (footer.ts:24-30), shared with the built-in footer so the
/// items numbers read identically; `exact` keeps full decimal digits.
pub fn format_tokens(count: u64, format: NumberFormat) -> String {
    if format == NumberFormat::Exact {
        return count.to_string();
    }
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

/// Percent as a whole number (`42.7 → 43%`).
pub fn format_percent(percent: f64) -> String {
    format!("{}%", percent.round() as i64)
}

/// Local token-cost estimate text (not a bill).
pub fn format_cost(cost: f64) -> String {
    if !cost.is_finite() || cost <= 0.0 {
        return "$0.00".to_owned();
    }
    if cost < 1.0 {
        format!("${cost:.4}")
    } else {
        format!("${cost:.2}")
    }
}

/// Wall-clock duration for the current session.
pub fn format_duration(ms: u128) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m {:02}s", secs / 60, secs % 60);
    }
    if secs < 86_400 {
        return format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60);
    }
    format!("{}d {:02}h", secs / 86_400, (secs % 86_400) / 3600)
}

/// Strip data ANSI SGR, C0/C1 control characters, CR/LF/TAB and collapse
/// whitespace (R20 §4.1 R6). The renderer's own colors are added afterwards.
pub fn sanitize_text(text: &str) -> String {
    let without_ansi = strip_sgr(text);
    let mut out = String::with_capacity(without_ansi.len());
    let mut previous_space = false;
    for ch in without_ansi.chars() {
        let ch = match ch {
            '\r' | '\n' | '\t' => ' ',
            ch if ch.is_control() => continue,
            ch => ch,
        };
        if ch == ' ' {
            if !previous_space {
                out.push(' ');
            }
            previous_space = true;
        } else {
            previous_space = false;
            out.push(ch);
        }
    }
    out.trim().to_owned()
}

/// Remove CSI SGR (`ESC [ ... m`) sequences from untrusted data.
fn strip_sgr(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && index + 1 < bytes.len() && bytes[index + 1] == b'[' {
            let mut end = index + 2;
            while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                end += 1;
            }
            if end < bytes.len() {
                if bytes[end] == b'm' {
                    index = end + 1;
                    continue;
                }
                // Other CSI sequences are control data too — drop them.
                index = end + 1;
                continue;
            }
        }
        // Copy one UTF-8 scalar.
        let ch = text[index..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// `false` when a separator contains a newline or any other control
/// character (R20 §4.1 R6); the caller falls back to the default.
pub fn valid_separator(separator: &str) -> bool {
    !separator.is_empty()
        && !separator.chars().any(|ch| ch.is_control())
        && !separator.contains('\u{1b}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_stats::TokenStats;
    use crate::tokens::{TokenBreakdown, TokenSource};
    use serde_json::json;

    fn stats() -> SessionStats {
        SessionStats {
            session_id: "s1".into(),
            session_file: None,
            started_at: "2026-10-27T10:00:00Z".into(),
            entry_count: 3,
            user_messages: 2,
            assistant_messages: 3,
            tokens: TokenStats {
                input: 10,
                output: 20,
                cache_read: 30,
                cache_write: 40,
            },
            cost: 0.0123,
        }
    }

    fn data() -> FieldData {
        FieldData {
            cwd: Some("/work/project".into()),
            model: Some(ModelInfo {
                id: "glm-5.1".into(),
                name: Some("GLM-5.1".into()),
            }),
            thinking: Some("high".into()),
            mode: Some("plan".into()),
            idle: Some(true),
            context: Some(ContextInfo {
                tokens: Some(85_400),
                context_window: 200_000,
                percent: Some(42.7),
            }),
            session_name: Some("te52".into()),
            stats: Some(stats()),
            pending_output: 0,
            live: Some(LiveView {
                streaming: true,
                output_tokens: Some(123),
                output_tokens_exact: None,
                decode_ms: 2_500,
                tokens: Some(TokenBreakdown::from_tiers(
                    100,
                    20,
                    3,
                    TokenSource::Estimated,
                )),
                rate_source_switched: false,
            }),
            live_held: Vec::new(),
            git: Some(GitInfo {
                root: Some("/work/project".into()),
                branch: Some("v0.1.8-baseline".into()),
            }),
            now: Some(
                DateTime::parse_from_rfc3339("2026-10-27T12:34:56Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            number_format: NumberFormat::Compact,
            palette: None,
            preview: false,
        }
    }

    /// rpi#78 B: a streaming frame that switched the rate's numerator source
    /// (estimate ↔ provider-reported) holds the previous reading instead of
    /// publishing a ratio that mixes two measurement bases. The settled frame is
    /// never suppressed: both numerator and denominator are frozen there.
    #[test]
    fn rate_holds_across_a_numerator_source_switch() {
        let mut data = data();
        let frame =
            |streaming: bool, output: Option<u64>, exact: Option<u64>, switched: bool| LiveView {
                streaming,
                output_tokens: output,
                output_tokens_exact: exact,
                decode_ms: 4_000,
                tokens: Some(TokenBreakdown::from_tiers(
                    800,
                    0,
                    0,
                    TokenSource::Estimated,
                )),
                rate_source_switched: switched,
            };

        data.live = Some(frame(true, None, None, false));
        assert_eq!(
            value(FieldId::LiveOutputRate, &data).unwrap().text,
            "~200.0 tok/s",
            "the estimate-based streaming average"
        );

        // Provider running total arrives → the numerator basis changed.
        data.live = Some(frame(true, Some(800), None, true));
        assert_eq!(
            value(FieldId::LiveOutputRate, &data).unwrap().text,
            "- tok/s",
            "the switching frame falls back to the placeholder instead of a mixed-basis rate"
        );

        // The next frame is back to a single basis.
        data.live = Some(frame(true, Some(800), None, false));
        assert_eq!(
            value(FieldId::LiveOutputRate, &data).unwrap().text,
            "200.0 tok/s",
            "the provider-reported average"
        );

        // Settling is authoritative even when the source changed on that frame.
        data.live = Some(frame(false, None, Some(800), true));
        assert_eq!(
            value(FieldId::LiveOutputRate, &data).unwrap().text,
            "200.0 tok/s",
            "the settled rate is the message's own average"
        );
    }

    #[test]
    fn registry_is_exactly_30_unique_fields() {
        assert_eq!(ALL_FIELDS.len(), 30);
        let mut ids: Vec<&str> = ALL_FIELDS.iter().map(|id| id.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 30);
        for id in ALL_FIELDS {
            assert_eq!(FieldId::parse(id.id()), Some(id));
        }
    }

    #[test]
    fn aliases_map_only_the_five_frozen_names() {
        assert_eq!(FieldId::parse("model-name"), Some(FieldId::Model));
        assert_eq!(FieldId::parse("project"), Some(FieldId::ProjectName));
        assert_eq!(FieldId::parse("project-root"), Some(FieldId::ProjectName));
        assert_eq!(FieldId::parse("status"), Some(FieldId::RunState));
        assert_eq!(FieldId::parse("context-usage"), Some(FieldId::ContextUsed));
        assert_eq!(FieldId::parse("thread-name"), Some(FieldId::SessionName));
        // Non-targets are not aliases.
        assert_eq!(FieldId::parse("permissions"), None);
        assert_eq!(FieldId::parse("estimated-thread-cost"), None);
        assert_eq!(FieldId::parse("hostname"), None);
    }

    #[test]
    fn session_formulas_match_the_frozen_contract() {
        // input=10, output=20, cacheRead=30, cacheWrite=40.
        let data = data();
        assert_eq!(value(FieldId::UsedTokens, &data).unwrap().text, "70");
        assert_eq!(
            value(FieldId::TotalInputTokens, &data).unwrap().text,
            "80 in"
        );
        assert_eq!(
            value(FieldId::TotalOutputTokens, &data).unwrap().text,
            "20 out"
        );
        // input + output + cacheRead + cacheWrite.
        assert_eq!(value(FieldId::TotalTokens, &data).unwrap().text, "100");
        assert_eq!(value(FieldId::TurnCount, &data).unwrap().text, "2 turns");
        assert_eq!(value(FieldId::StepCount, &data).unwrap().text, "3 steps");
        assert_eq!(
            value(FieldId::CacheReadTokens, &data).unwrap().text,
            "30 cache-read"
        );
        assert_eq!(
            value(FieldId::CacheWriteTokens, &data).unwrap().text,
            "40 cache-write"
        );
        assert_eq!(value(FieldId::SessionCost, &data).unwrap().text, "$0.0123");
        // started 10:00 → now 12:34:56 = 2h 34m.
        assert_eq!(
            value(FieldId::SessionDuration, &data).unwrap().text,
            "2h 34m"
        );
    }

    #[test]
    fn live_total_adds_pending_and_streaming_output() {
        // Snapshot total = 10 + 20 + 30 + 40 = 100.
        let mut data = data();
        data.pending_output = 7;
        // Streaming with a provider exact output (123): no estimate marker.
        assert_eq!(value(FieldId::LiveTotalTokens, &data).unwrap().text, "230");
        // Only the local estimate is available while streaming: keep the
        // `~` marker on the whole live total.
        let mut estimated = data.clone();
        estimated.live = Some(LiveView {
            streaming: true,
            output_tokens: None,
            output_tokens_exact: None,
            decode_ms: 1_000,
            tokens: Some(TokenBreakdown::from_tiers(2, 0, 0, TokenSource::Estimated)),
            rate_source_switched: false,
        });
        assert_eq!(
            value(FieldId::LiveTotalTokens, &estimated).unwrap().text,
            "~109"
        );
        // Settled: the frozen output is already in the pending/snapshot sum,
        // so the live part is not added again.
        let mut settled = data.clone();
        settled.live = Some(LiveView {
            streaming: false,
            ..data.live.expect("fixture live view")
        });
        assert_eq!(
            value(FieldId::LiveTotalTokens, &settled).unwrap().text,
            "107"
        );
        // Live disarmed: the row still renders the snapshot + pending total.
        let mut disarmed = data.clone();
        disarmed.live = None;
        assert_eq!(
            value(FieldId::LiveTotalTokens, &disarmed).unwrap().text,
            "107"
        );
        // No snapshot (old host): omitted, like the other session totals.
        disarmed.stats = None;
        assert_eq!(value(FieldId::LiveTotalTokens, &disarmed), None);
    }

    #[test]
    fn context_fields_use_the_active_context_and_distinct_directions() {
        let data = data();
        assert_eq!(
            value(FieldId::ContextUsed, &data).unwrap().text,
            "Context 43% used"
        );
        assert_eq!(
            value(FieldId::ContextRemaining, &data).unwrap().text,
            "Context 57% left"
        );
        assert_eq!(
            value(FieldId::ContextWindowSize, &data).unwrap().text,
            "200k window"
        );
        assert_eq!(value(FieldId::ContextTokens, &data).unwrap().text, "85k");
        // No percentage: derive from tokens/window.
        let mut no_percent = data.clone();
        no_percent.context.as_mut().unwrap().percent = None;
        assert_eq!(
            value(FieldId::ContextUsed, &no_percent).unwrap().text,
            "Context 43% used"
        );
        // Unknown tokens after compaction: context-tokens omitted.
        let mut unknown = data.clone();
        unknown.context.as_mut().unwrap().tokens = None;
        unknown.context.as_mut().unwrap().percent = None;
        assert_eq!(value(FieldId::ContextTokens, &unknown), None);
        assert_eq!(value(FieldId::ContextUsed, &unknown), None);
        assert_eq!(
            value(FieldId::ContextWindowSize, &unknown).unwrap().text,
            "200k window"
        );
    }

    #[test]
    fn model_mode_and_run_state_follow_the_whitelist() {
        let data = data();
        assert_eq!(value(FieldId::Model, &data).unwrap().text, "GLM-5.1");
        assert_eq!(
            value(FieldId::ModelWithReasoning, &data).unwrap().text,
            "GLM-5.1 (high)"
        );
        assert_eq!(value(FieldId::Reasoning, &data).unwrap().text, "high");
        assert_eq!(value(FieldId::PermissionMode, &data).unwrap().text, "plan");
        assert_eq!(value(FieldId::RunState, &data).unwrap().text, "Ready");
        // Display name missing → id.
        let mut no_name = data.clone();
        no_name.model.as_mut().unwrap().name = None;
        assert_eq!(value(FieldId::Model, &no_name).unwrap().text, "glm-5.1");
        // thinking=off → model-with-reasoning degrades to the name; reasoning omitted.
        let mut off = data.clone();
        off.thinking = Some("off".into());
        assert_eq!(
            value(FieldId::ModelWithReasoning, &off).unwrap().text,
            "GLM-5.1"
        );
        assert_eq!(value(FieldId::Reasoning, &off), None);
        // Unknown permission modes and busy run states.
        let mut busy = data.clone();
        busy.mode = Some("bypass".into());
        busy.idle = Some(false);
        assert_eq!(value(FieldId::PermissionMode, &busy), None);
        assert_eq!(value(FieldId::RunState, &busy).unwrap().text, "Working");
        // No model at all.
        busy.model = None;
        assert_eq!(value(FieldId::Model, &busy), None);
        assert_eq!(value(FieldId::ModelWithReasoning, &busy), None);
    }

    #[test]
    fn workspace_fields_fall_back_and_omit_detached_git() {
        let data = data();
        assert_eq!(
            value(FieldId::CurrentDir, &data).unwrap().text,
            "/work/project"
        );
        assert_eq!(value(FieldId::ProjectName, &data).unwrap().text, "project");
        assert_eq!(
            value(FieldId::GitBranch, &data).unwrap().text,
            "v0.1.8-baseline"
        );
        // Non-git: cwd basename; no branch.
        let mut plain = data.clone();
        plain.git = None;
        assert_eq!(value(FieldId::ProjectName, &plain).unwrap().text, "project");
        assert_eq!(value(FieldId::GitBranch, &plain), None);
        // Detached HEAD: root present, branch absent.
        let detached = FieldData {
            git: Some(GitInfo {
                root: Some("/work/project".into()),
                branch: None,
            }),
            ..data.clone()
        };
        assert_eq!(
            value(FieldId::ProjectName, &detached).unwrap().text,
            "project"
        );
        assert_eq!(value(FieldId::GitBranch, &detached), None);
        // No cwd → both omitted.
        let mut empty = data.clone();
        empty.cwd = None;
        empty.git = None;
        assert_eq!(value(FieldId::CurrentDir, &empty), None);
        assert_eq!(value(FieldId::ProjectName, &empty), None);
        assert_eq!(value(FieldId::SessionName, &empty).unwrap().text, "te52");
    }

    #[test]
    fn live_fields_prefer_provider_then_estimate() {
        let data = data();
        assert_eq!(value(FieldId::Streaming, &data).unwrap().text, "streaming");
        assert_eq!(value(FieldId::LiveOutputTokens, &data).unwrap().text, "123");
        // 123 tokens over 2.5 s = 49.2 tok/s, one decimal.
        assert_eq!(
            value(FieldId::LiveOutputRate, &data).unwrap().text,
            "49.2 tok/s"
        );
        // The tier rows are available while streaming.
        assert_eq!(
            value(FieldId::LiveTextTokens, &data).unwrap().text,
            "~100 text"
        );
        assert_eq!(
            value(FieldId::LiveThinkingTokens, &data).unwrap().text,
            "~20 think"
        );
        assert_eq!(
            value(FieldId::LiveToolcallTokens, &data).unwrap().text,
            "~3 tool"
        );
        // No provider usage → the estimate still shows (marked `~`).
        let mut estimated = data.clone();
        estimated.live.as_mut().unwrap().output_tokens = None;
        assert_eq!(
            value(FieldId::LiveOutputTokens, &estimated).unwrap().text,
            "~123"
        );
        assert_eq!(
            value(FieldId::LiveOutputRate, &estimated).unwrap().text,
            "~49.2 tok/s"
        );
        // Too-short decode window → the rate row keeps its slot as a
        // placeholder instead of disappearing (stable display guard).
        let mut no_decode = data.clone();
        no_decode.live.as_mut().unwrap().decode_ms = 0;
        assert_eq!(
            value(FieldId::LiveOutputRate, &no_decode).unwrap().text,
            "- tok/s"
        );
        // Streaming stopped: exact takes over; the streaming row falls back
        // to its placeholder.
        let stopped = FieldData {
            live: Some(LiveView {
                streaming: false,
                output_tokens: Some(123),
                output_tokens_exact: Some(500),
                decode_ms: 4_000,
                tokens: Some(TokenBreakdown::from_tiers(
                    100,
                    20,
                    3,
                    TokenSource::Estimated,
                )),
                rate_source_switched: false,
            }),
            ..data.clone()
        };
        assert_eq!(value(FieldId::Streaming, &stopped).unwrap().text, "-");
        assert_eq!(
            value(FieldId::LiveOutputTokens, &stopped).unwrap().text,
            "500"
        );
        // 500 output over 4 s = 125.0 tok/s.
        assert_eq!(
            value(FieldId::LiveOutputRate, &stopped).unwrap().text,
            "125.0 tok/s"
        );
        // Exact missing (provider reported no `usage.output`): fall back to
        // the provider's streaming running total, then to the estimate.
        let mut no_exact = stopped.clone();
        no_exact.live.as_mut().unwrap().output_tokens_exact = None;
        assert_eq!(
            value(FieldId::LiveOutputTokens, &no_exact).unwrap().text,
            "123"
        );
        // liveTokens disabled → all live fields omitted.
        let disabled = FieldData {
            live: None,
            ..data.clone()
        };
        assert_eq!(value(FieldId::Streaming, &disabled), None);
        assert_eq!(value(FieldId::LiveOutputTokens, &disabled), None);
        assert_eq!(value(FieldId::LiveOutputRate, &disabled), None);
    }

    /// `live_held`: a live row with no fresh value this frame keeps the last
    /// value the footer actually measured (the gap between two assistant
    /// messages, the rate window, a tier the message has not produced yet).
    /// A fresh value always wins; `streaming` is never held.
    #[test]
    fn live_rows_keep_the_last_measured_value_until_refreshed() {
        let previous = FieldValue::new("500", Tone::Success);
        let previous_rate = FieldValue::new("125.0 tok/s", Tone::Success);
        let mut gap = FieldData {
            live: Some(LiveView {
                streaming: true,
                ..LiveView::default()
            }),
            ..data()
        };
        gap.live_held = vec![
            (FieldId::LiveOutputTokens, previous.clone()),
            (FieldId::LiveOutputRate, previous_rate.clone()),
        ];
        let total = value(FieldId::LiveOutputTokens, &gap).unwrap();
        assert_eq!((total.text.as_str(), total.tone), ("500", Tone::Success));
        let rate = value(FieldId::LiveOutputRate, &gap).unwrap();
        assert_eq!(rate.text, "125.0 tok/s");

        // Fresh data refreshes the row (the held copy never shadows it).
        let mut fresh = gap.clone();
        fresh.live = Some(LiveView {
            streaming: true,
            output_tokens: Some(7),
            output_tokens_exact: None,
            decode_ms: 1_200,
            tokens: Some(TokenBreakdown::from_tiers(7, 0, 0, TokenSource::Estimated)),
            rate_source_switched: false,
        });
        assert_eq!(value(FieldId::LiveOutputTokens, &fresh).unwrap().text, "7");
        assert_eq!(
            value(FieldId::LiveOutputRate, &fresh).unwrap().text,
            "5.8 tok/s"
        );

        // `streaming` is never held: an idle gap reports the placeholder.
        let mut idle = gap.clone();
        idle.live = Some(LiveView::default());
        idle.live_held.push((
            FieldId::Streaming,
            FieldValue::new("streaming", Tone::Success),
        ));
        assert_eq!(value(FieldId::Streaming, &idle).unwrap().text, "-");

        // Disarmed measurement: held values are not rendered (hidden fields).
        let mut off = gap.clone();
        off.live = None;
        assert_eq!(value(FieldId::LiveOutputTokens, &off), None);
    }

    /// A settled message is authoritative: its frozen decode window gives the
    /// final rate even below the streaming 0.5 s guard, and the previous
    /// message's held values are never reused after the stream ends.
    #[test]
    fn settled_messages_refresh_their_own_final_rate() {
        let held = vec![
            (
                FieldId::LiveOutputTokens,
                FieldValue::new("500", Tone::Success),
            ),
            (
                FieldId::LiveOutputRate,
                FieldValue::new("125.0 tok/s", Tone::Success),
            ),
        ];

        // Short reply: 3 exact output tokens over a frozen 150 ms window.
        let mut short = FieldData {
            live: Some(LiveView {
                streaming: false,
                output_tokens: None,
                output_tokens_exact: Some(3),
                decode_ms: 150,
                tokens: Some(TokenBreakdown::from_tiers(2, 1, 0, TokenSource::Estimated)),
                rate_source_switched: false,
            }),
            ..data()
        };
        short.live_held = held.clone();
        assert_eq!(value(FieldId::LiveOutputTokens, &short).unwrap().text, "3");
        assert_eq!(
            value(FieldId::LiveOutputRate, &short).unwrap().text,
            "20.0 tok/s",
            "the frozen final window replaces the previous rate"
        );

        // The same short window while STREAMING still holds the previous
        // rate: the guard only protects unstable in-flight measurements.
        let mut early = short.clone();
        early.live.as_mut().unwrap().streaming = true;
        assert_eq!(
            value(FieldId::LiveOutputRate, &early).unwrap().text,
            "125.0 tok/s"
        );

        // Settled without a decode window: the placeholder, never the old
        // rate (the hold is streaming-only).
        let mut empty = short.clone();
        empty.live.as_mut().unwrap().decode_ms = 0;
        assert_eq!(
            value(FieldId::LiveOutputRate, &empty).unwrap().text,
            "- tok/s"
        );
    }

    #[test]
    fn live_rows_keep_dash_placeholders_while_armed() {
        // Armed but idle (no assistant message yet): every live row keeps its
        // slot with a dash placeholder, so the status line does not reflow
        // when a stream starts. The dash keeps the field's own tone — a muted
        // dash reads as "disabled" and loses the color of the numbers it
        // stands in for.
        use crate::items::ItemEntry;
        let idle = FieldData {
            live: Some(LiveView::default()),
            ..data()
        };
        for (field, text) in [
            (FieldId::Streaming, "-"),
            (FieldId::LiveOutputTokens, "-"),
            (FieldId::LiveOutputRate, "- tok/s"),
            (FieldId::LiveTextTokens, "- text"),
            (FieldId::LiveThinkingTokens, "- think"),
            (FieldId::LiveToolcallTokens, "- tool"),
        ] {
            let value = value(field, &idle).unwrap();
            assert_eq!(value.text, text);
            assert_eq!(
                value.tone,
                Tone::Success,
                "the placeholder keeps the live success tone"
            );
        }
        // The colored render path emits the success SGR, not the muted one.
        let mut themed = idle.clone();
        themed.palette = Some(Palette::from_theme(&json!({
            "colors": {"success": "green", "muted": "#112233"}
        })));
        let entries = vec![ItemEntry {
            raw: "live-output-tokens".into(),
            field: Some(FieldId::LiveOutputTokens),
        }];
        let colored = render_line(&entries, " · ", true, &themed).unwrap();
        assert!(
            colored.contains("\x1b[32m-\x1b[0m"),
            "placeholder must keep the success color: {colored:?}"
        );
        assert!(
            !colored.contains("38;2;17;34;51"),
            "placeholder must not fall back to the muted color: {colored:?}"
        );
        // Placeholders join like real values (no empty separator slots).
        let joined = vec![
            ItemEntry {
                raw: "model".into(),
                field: Some(FieldId::Model),
            },
            ItemEntry {
                raw: "live-output-rate".into(),
                field: Some(FieldId::LiveOutputRate),
            },
        ];
        assert_eq!(
            render_line(&joined, " · ", false, &idle).unwrap(),
            "GLM-5.1 · - tok/s"
        );
        // Disarmed (`liveTokens` off / no live field selected) → hidden, so a
        // disabled plugin never renders a stale dash.
        let off = FieldData {
            live: None,
            ..data()
        };
        for field in [
            FieldId::Streaming,
            FieldId::LiveOutputTokens,
            FieldId::LiveOutputRate,
            FieldId::LiveTextTokens,
            FieldId::LiveThinkingTokens,
            FieldId::LiveToolcallTokens,
        ] {
            assert_eq!(value(field, &off), None);
        }
        // Non-live fields keep the omission contract while live is armed.
        let empty = FieldData {
            live: Some(LiveView::default()),
            ..FieldData::default()
        };
        assert_eq!(value(FieldId::Model, &empty), None);
        assert_eq!(value(FieldId::SessionCost, &empty), None);
    }

    #[test]
    fn number_format_only_affects_token_and_window_numbers() {
        let mut exact = data();
        exact.number_format = NumberFormat::Exact;
        assert_eq!(value(FieldId::ContextTokens, &exact).unwrap().text, "85400");
        assert_eq!(
            value(FieldId::ContextWindowSize, &exact).unwrap().text,
            "200000 window"
        );
        assert_eq!(value(FieldId::UsedTokens, &exact).unwrap().text, "70");
        // Percentages, cost and duration are unchanged.
        assert_eq!(
            value(FieldId::ContextUsed, &exact).unwrap().text,
            "Context 43% used"
        );
        assert_eq!(
            value(FieldId::SessionDuration, &exact).unwrap().text,
            "2h 34m"
        );
    }

    #[test]
    fn missing_data_omits_or_uses_the_preview_placeholder() {
        let empty = FieldData::default();
        assert_eq!(value(FieldId::Model, &empty), None);
        let preview = FieldData {
            preview: true,
            ..FieldData::default()
        };
        assert_eq!(value(FieldId::Model, &preview).unwrap().text, "(model)");
        assert_eq!(
            value(FieldId::LiveOutputRate, &preview).unwrap().text,
            "(live-output-rate)"
        );
    }

    #[test]
    fn sanitize_strips_control_characters_and_data_sgr() {
        assert_eq!(
            sanitize_text("  a\nb\tc \x1b[31mred\x1b[0m\x07 d  "),
            "a b c red d"
        );
        assert_eq!(sanitize_text("C1\u{9b}31m"), "C131m");
        assert!(!valid_separator(" ·\n"));
        assert!(!valid_separator(" \x1b[31m"));
        assert!(valid_separator(" | "));
        assert!(!valid_separator(""));
    }

    #[test]
    fn render_line_orders_omits_and_joins() {
        use crate::items::ItemEntry;
        let data = data();
        let entries = vec![
            ItemEntry {
                raw: "model".into(),
                field: Some(FieldId::Model),
            },
            ItemEntry {
                raw: "permissions".into(),
                field: None,
            },
            ItemEntry {
                raw: "git-branch".into(),
                field: Some(FieldId::GitBranch),
            },
            ItemEntry {
                raw: "context-used".into(),
                field: Some(FieldId::ContextUsed),
            },
        ];
        assert_eq!(
            render_line(&entries, " · ", false, &data).unwrap(),
            "GLM-5.1 · v0.1.8-baseline · Context 43% used"
        );
        // All fields missing → None (empty-output handling).
        assert_eq!(
            render_line(
                &entries,
                " · ",
                false,
                &FieldData {
                    live: None,
                    ..FieldData::default()
                }
            ),
            None
        );
    }

    #[test]
    fn palette_resolves_hex_named_vars_and_truecolor() {
        let theme = json!({
            "colors": {
                "accent": "violet",
                "muted": "#112233",
                "success": 42,
                "warning": "okhsl(83 88% 67%)",
                "error": "red"
            },
            "vars": {"violet": "#aabbcc"}
        });
        let palette = Palette::from_theme(&theme);
        assert_eq!(palette.sgr(Tone::Accent), Some("38;2;170;187;204"));
        assert_eq!(palette.sgr(Tone::Muted), Some("38;2;17;34;51"));
        assert_eq!(palette.sgr(Tone::Success), Some("38;5;42"));
        // 03 §4.1: `okhsl()` is a legal host theme color; it resolves through
        // the host color model to truecolor.
        assert_eq!(palette.sgr(Tone::Warning), Some("38;2;205;154;34"));
        assert_eq!(palette.sgr(Tone::Error), Some("31"));
        // Background variables resolve through one hop and named base colors.
        let built_in = json!({
            "colors": {"accent": "violet", "muted": "muted"},
            "vars": {"violet": "magenta", "muted": "okhsl(1 2% 3%)"}
        });
        let palette = Palette::from_theme(&built_in);
        assert_eq!(palette.sgr(Tone::Accent), Some("35"));
        assert_eq!(palette.sgr(Tone::Muted), Some("38;2;4;3;3"));
    }

    #[test]
    fn built_in_dark_theme_palette_is_fully_resolved() {
        // The built-in dark theme's semantic colors all pass through `vars`
        // to `okhsl(...)` literals (core/themes.rs DARK_THEME_JSON). The
        // default `useColors:true` path must produce truecolor SGR, not the
        // plain-text fallback.
        let theme = json!({
            "appearance": "dark",
            "vars": {
                "text": "okhsl(234 3% 89%)",
                "muted": "okhsl(229 6% 67%)",
                "violet": "okhsl(295 50% 67%)",
                "blue": "okhsl(232 54% 67%)",
                "green": "okhsl(159 59% 67%)",
                "red": "okhsl(20 72% 67%)",
                "yellow": "okhsl(83 88% 67%)"
            },
            "colors": {
                "accent": "violet",
                "border": "okhsl(231 57% 65%)",
                "success": "green",
                "error": "red",
                "warning": "yellow",
                "muted": "muted",
                "dim": "okhsl(229 8% 56%)",
                "text": "text"
            }
        });
        let palette = Palette::from_theme(&theme);
        for tone in [
            Tone::Accent,
            Tone::Muted,
            Tone::Success,
            Tone::Warning,
            Tone::Error,
            Tone::Info,
        ] {
            let sgr = palette.sgr(tone).expect("built-in theme color resolves");
            assert!(
                sgr.starts_with("38;2;"),
                "{tone:?} must be truecolor, got {sgr}"
            );
        }
        // Spot-check the actual conversions (host colors.rs math).
        assert_eq!(palette.sgr(Tone::Accent), Some("38;2;167;152;215"));
        assert_eq!(palette.sgr(Tone::Muted), Some("38;2;157;165;169"));
        assert_eq!(palette.sgr(Tone::Success), Some("38;2;104;183;141"));
    }

    #[test]
    fn format_helpers_are_deterministic() {
        assert_eq!(format_tokens(999, NumberFormat::Compact), "999");
        assert_eq!(format_tokens(1500, NumberFormat::Compact), "1.5k");
        assert_eq!(format_tokens(1_500_000, NumberFormat::Compact), "1.5M");
        assert_eq!(format_tokens(1_500_000, NumberFormat::Exact), "1500000");
        assert_eq!(format_cost(0.0), "$0.00");
        assert_eq!(format_cost(0.005), "$0.0050");
        assert_eq!(format_cost(0.123), "$0.1230");
        assert_eq!(format_cost(1.2345), "$1.23");
        assert_eq!(format_duration(59_000), "59s");
        assert_eq!(format_duration(60_000), "1m 00s");
        assert_eq!(format_duration(3_600_000), "1h 00m");
        assert_eq!(format_duration(90_000_000), "1d 01h");
        assert_eq!(format_percent(42.7), "43%");
        assert_eq!(format_percent(42.4), "42%");
    }

    #[test]
    fn colored_render_wraps_each_value_and_resets() {
        use crate::items::ItemEntry;
        let mut data = data();
        data.palette = Some(Palette {
            accent: Some("35".into()),
            muted: Some("90".into()),
            ..Palette::default()
        });
        let entries = vec![
            ItemEntry {
                raw: "model".into(),
                field: Some(FieldId::Model),
            },
            ItemEntry {
                raw: "session-duration".into(),
                field: Some(FieldId::SessionDuration),
            },
        ];
        assert_eq!(
            render_line(&entries, " · ", true, &data).unwrap(),
            "\x1b[35mGLM-5.1\x1b[0m · \x1b[90m2h 34m\x1b[0m"
        );
    }
}
