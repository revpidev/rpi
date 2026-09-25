//! Transcript rendering tables (glyphs/colors/action prefixes) and the
//! renderCall/renderResult component builders, plus the overlay row
//! formatter shared by the persistent widget.
//!
//! Port of upstream `packages/rpiv-todo/view/format.ts` @ `0fdf4f8`.
//!
//! Theme access is abstracted over [`TodoTheme`] so the pure formatting
//! functions stay testable without a host: the identity theme reproduces
//! the upstream test harness (`makeTheme`/`identityTheme` — golden-frame
//! snapshots are plain text), and [`AnsiTheme`] resolves the runtime
//! `ctx.ui.theme` JSON (semantic token → theme var → hex → SGR) the same
//! way the rpi host's `Theme::fg`/`bold`/`strikethrough` do
//! (`{prefix}{text}\x1b[39m` / `\x1b[1m…\x1b[22m` / `\x1b[9m…\x1b[29m`).
//! Unknown tokens render unstyled (the host's lenient fallback), never
//! panic.

use serde_json::{json, Value};

use crate::i18n::I18n;
use crate::state::selectors;
use crate::state::TaskState;
use crate::tool::sanitize::sanitize_terminal_text;
use crate::tool::types::{Task, TaskAction, TaskStatus};

// ---------------------------------------------------------------------------
// Theme abstraction
// ---------------------------------------------------------------------------

/// The upstream `Theme` surface the formatters use (`fg`/`bold`/
/// `strikenhrough`).
pub trait TodoTheme {
    /// Wrap `text` in a foreground color (semantic token name).
    fn fg(&self, color: &str, text: &str) -> String;
    /// Bold text.
    fn bold(&self, text: &str) -> String;
    /// Strikethrough text.
    fn strikethrough(&self, text: &str) -> String;
}

/// Identity theme — the test-harness form (`identityTheme`: every wrap
/// returns the text unchanged); golden-frame snapshots are plain text.
pub struct IdentityTheme;

impl TodoTheme for IdentityTheme {
    fn fg(&self, _color: &str, text: &str) -> String {
        text.to_owned()
    }
    fn bold(&self, text: &str) -> String {
        text.to_owned()
    }
    fn strikethrough(&self, text: &str) -> String {
        text.to_owned()
    }
}

/// Runtime theme over the `ctx.ui.theme` JSON: `colors` maps semantic
/// tokens to a theme var name / hex / 256-index, `vars` maps var names to
/// hex. Colors render as truecolor SGR (the host default
/// `ColorMode::TrueColor`); unresolvable tokens render unstyled.
pub struct AnsiTheme {
    prefixes: std::collections::HashMap<String, String>,
}

impl AnsiTheme {
    /// Resolve the prefix table from a `ctx.ui.theme` JSON value.
    pub fn from_theme_json(theme: &Value) -> Self {
        let mut prefixes = std::collections::HashMap::new();
        let Some(colors) = theme.get("colors").and_then(Value::as_object) else {
            return AnsiTheme { prefixes };
        };
        let vars = theme.get("vars").and_then(Value::as_object);
        for (token, value) in colors {
            let prefix = match value {
                Value::String(raw) => {
                    if let Some(hex) = raw.strip_prefix('#') {
                        hex_prefix(hex)
                    } else if raw.is_empty() {
                        // Upstream: an empty color value is a bare reset.
                        "\x1b[39m".to_owned()
                    } else {
                        // Variable reference: resolve through `vars`.
                        vars.and_then(|vars| vars.get(raw))
                            .and_then(|resolved| match resolved {
                                Value::String(hex) => hex.strip_prefix('#').map(hex_prefix),
                                _ => None,
                            })
                            .unwrap_or_default()
                    }
                }
                Value::Number(number) => number
                    .as_u64()
                    .filter(|index| *index <= 255)
                    .map(|index| format!("\x1b[38;5;{index}m"))
                    .unwrap_or_default(),
                _ => String::new(),
            };
            prefixes.insert(token.clone(), prefix);
        }
        AnsiTheme { prefixes }
    }
}

/// `#rrggbb` → truecolor SGR prefix (host `fg_ansi` truecolor arm).
fn hex_prefix(hex: &str) -> String {
    if hex.len() == 6 {
        if let (Some(r), Some(g), Some(b)) = (
            u8::from_str_radix(&hex[0..2], 16).ok(),
            u8::from_str_radix(&hex[2..4], 16).ok(),
            u8::from_str_radix(&hex[4..6], 16).ok(),
        ) {
            return format!("\x1b[38;2;{r};{g};{b}m");
        }
    }
    String::new()
}

impl TodoTheme for AnsiTheme {
    fn fg(&self, color: &str, text: &str) -> String {
        let prefix = self.prefixes.get(color).map(String::as_str).unwrap_or("");
        format!("{prefix}{text}\x1b[39m")
    }
    fn bold(&self, text: &str) -> String {
        format!("\x1b[1m{text}\x1b[22m")
    }
    fn strikethrough(&self, text: &str) -> String {
        format!("\x1b[9m{text}\x1b[29m")
    }
}

// ---------------------------------------------------------------------------
// Status presentation tables — the single source of truth for glyph/color
// (upstream format.ts constants).
// ---------------------------------------------------------------------------

/// renderResult status echo glyph (upstream `STATUS_GLYPH`).
pub fn status_glyph(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "○",
        TaskStatus::InProgress => "◐",
        TaskStatus::Completed => "●",
        TaskStatus::Deleted => "⊘",
    }
}

/// renderResult status echo color (upstream `STATUS_COLOR`). `deleted`
/// uses `muted` so a successful delete is visually distinct from the
/// overlay's error-toned `✗` (which lives in [`overlay_status_glyph`]).
pub fn status_color(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "dim",
        TaskStatus::InProgress => "warning",
        TaskStatus::Completed => "success",
        TaskStatus::Deleted => "muted",
    }
}

/// Per-action prefix glyph for renderCall (upstream `ACTION_GLYPH`):
/// `+` create, `→` update, `×` delete, `›` get, `☰` list, `∅` clear.
pub fn action_glyph(action: TaskAction) -> &'static str {
    match action {
        TaskAction::Create => "+",
        TaskAction::Update => "→",
        TaskAction::Delete => "×",
        TaskAction::Get => "›",
        TaskAction::List => "☰",
        TaskAction::Clear => "∅",
    }
}

/// Glyph for the persistent overlay's per-task row (upstream
/// `overlayStatusGlyph`). Differs from [`status_glyph`] for `completed`
/// (`✓` vs `●`) and `deleted` (`✗` vs `⊘`) because the overlay caller
/// never renders a `deleted` row but uses `✗` in its error-toned palette.
pub fn overlay_status_glyph(status: TaskStatus, theme: &dyn TodoTheme) -> String {
    match status {
        TaskStatus::Pending => theme.fg("dim", "○"),
        TaskStatus::InProgress => theme.fg("warning", "◐"),
        TaskStatus::Completed => theme.fg("success", "✓"),
        TaskStatus::Deleted => theme.fg("error", "✗"),
    }
}

/// Format a single task row for the persistent overlay (upstream
/// `formatOverlayTaskLine`). The subject color reflects task state while
/// IDs and supporting metadata stay visually quiet.
pub fn format_overlay_task_line(task: &Task, theme: &dyn TodoTheme, show_id: bool) -> String {
    let glyph = overlay_status_glyph(task.status, theme);
    let subject_color = match task.status {
        TaskStatus::InProgress => "accent",
        TaskStatus::Completed | TaskStatus::Deleted => "muted",
        TaskStatus::Pending => "text",
    };
    let mut subject = theme.fg(subject_color, &sanitize_terminal_text(&task.subject));
    if matches!(task.status, TaskStatus::Completed | TaskStatus::Deleted) {
        subject = theme.strikethrough(&subject);
    }
    let mut line = glyph;
    if show_id {
        line.push(' ');
        line.push_str(&theme.fg("dim", &format!("#{}", task.id)));
    }
    line.push(' ');
    line.push_str(&subject);
    if task.status == TaskStatus::InProgress {
        if let Some(active_form) = task.active_form.as_deref().filter(|form| !form.is_empty()) {
            line.push(' ');
            line.push_str(&theme.fg(
                "muted",
                &format!("({})", sanitize_terminal_text(active_form)),
            ));
        }
    }
    if let Some(deps) = task.blocked_by.as_ref().filter(|deps| !deps.is_empty()) {
        let chain = deps
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(",");
        line.push(' ');
        line.push_str(&theme.fg("muted", &format!("⛓ {chain}")));
    }
    line
}

/// Format a single task line for the `/todos` slash command (upstream
/// `formatCommandTaskLine` — no glyph color, indented bullet prefix).
pub fn format_command_task_line(task: &Task, glyph: &str) -> String {
    let form = match (task.status, task.active_form.as_deref()) {
        (TaskStatus::InProgress, Some(form)) if !form.is_empty() => {
            format!(" ({})", sanitize_terminal_text(form))
        }
        _ => String::new(),
    };
    let block = task
        .blocked_by
        .as_ref()
        .filter(|deps| !deps.is_empty())
        .map(|deps| {
            format!(
                "    ⛓ {}",
                deps.iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .unwrap_or_default();
    format!(
        "  {glyph} #{} {}{}{}",
        task.id,
        sanitize_terminal_text(&task.subject),
        form,
        block
    )
}

// ---------------------------------------------------------------------------
// Tool render hooks (upstream `renderTodoCall` / `renderTodoResult`)
// ---------------------------------------------------------------------------

/// The `renderCall` body (upstream `renderTodoCall`): the action prefix
/// line, reading the FOREGROUND slot for subject resolution (the render
/// context carries no session identity — a detached call whose task lives
/// in another slot misses the foreground lookup and falls back to `#<id>`;
/// per-session ids restart at 1, so searching sibling slots could surface
/// the WRONG subject — the fallback is intentional, upstream comment).
///
/// Returns a ComponentTree `text` node whose text value carries the theme
/// wrapping (the upstream `Text` component's text property carries the
/// same wrapped string; no declarative `fg` prop — the segments are
/// multi-colored).
pub fn render_todo_call(
    args: &Value,
    theme: &dyn TodoTheme,
    state: &TaskState,
    i18n: &I18n,
) -> Value {
    // `ACTION_GLYPH[args.action] ?? args.action`: an unknown/missing
    // action renders its own JS stringification (a MISSING property is
    // `undefined`; a JSON `null` is `"null"`) — partial streaming args
    // hit this on the first render.
    let glyph = match args
        .get("action")
        .and_then(Value::as_str)
        .and_then(TaskAction::parse)
    {
        Some(action) => action_glyph(action).to_owned(),
        None => match args.get("action") {
            Some(value) => crate::state::reducer::js_string(value),
            None => "undefined".to_owned(),
        },
    };
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .and_then(TaskAction::parse);
    let mut text = format!(
        "{}{}",
        theme.fg("toolTitle", &theme.bold("todo ")),
        theme.fg("muted", &glyph)
    );

    if action == Some(TaskAction::Create) {
        // JS truthiness: an empty-string subject renders nothing extra.
        if let Some(subject) = args
            .get("subject")
            .and_then(Value::as_str)
            .filter(|subject| !subject.is_empty())
        {
            text.push(' ');
            text.push_str(&theme.fg("dim", &sanitize_terminal_text(subject)));
        }
    } else if matches!(
        action,
        Some(TaskAction::Update) | Some(TaskAction::Get) | Some(TaskAction::Delete)
    ) {
        // JS `args.id !== undefined`: a JSON null passes (renders
        // "#null" through the JS stringification) — only a MISSING id
        // skips the branch.
        if let Some(id_value) = args.get("id") {
            let id = id_value.as_i64();
            let label = match id.and_then(|id| selectors::select_task_subject_by_id(state, id)) {
                Some(subject) => sanitize_terminal_text(subject),
                None => format!("#{}", crate::state::reducer::js_string(id_value)),
            };
            text.push(' ');
            text.push_str(&theme.fg("accent", &label));
        }
    } else if action == Some(TaskAction::List) {
        if let Some(status) = args
            .get("status")
            .and_then(Value::as_str)
            .and_then(TaskStatus::parse)
        {
            text.push(' ');
            text.push_str(&theme.fg("muted", i18n.format_status_label(status)));
        }
    }
    json!({ "type": "text", "props": { "text": text } })
}

/// The `renderResult` body (upstream `renderTodoResult`): the per-action
/// status echo (only `create`/`update`/`delete` advertise a status;
/// `list`/`get`/`clear` — and any result without a resolvable status —
/// fall back to the plain success `✓`).
pub fn render_todo_result(result: &Value, theme: &dyn TodoTheme, i18n: &I18n) -> Value {
    let details = result.get("details").filter(|value| !value.is_null());
    let mut status: Option<TaskStatus> = None;
    if let Some(details) = details {
        let params = details.get("params");
        match details.get("action").and_then(Value::as_str) {
            Some("create") => {
                status = details
                    .get("tasks")
                    .and_then(Value::as_array)
                    .and_then(|tasks| tasks.last())
                    .and_then(|task| task.get("status"))
                    .and_then(Value::as_str)
                    .and_then(TaskStatus::parse);
            }
            Some("update") => {
                status = params
                    .and_then(|params| params.get("status"))
                    .and_then(Value::as_str)
                    .and_then(TaskStatus::parse);
                if status.is_none() {
                    let id = params
                        .and_then(|params| params.get("id"))
                        .and_then(Value::as_i64);
                    status = id.and_then(|id| {
                        details
                            .get("tasks")
                            .and_then(Value::as_array)
                            .and_then(|tasks| {
                                tasks
                                    .iter()
                                    .find(|task| task.get("id").and_then(Value::as_i64) == Some(id))
                            })
                            .and_then(|task| task.get("status"))
                            .and_then(Value::as_str)
                            .and_then(TaskStatus::parse)
                    });
                }
            }
            Some("delete") => {
                let id = params
                    .and_then(|params| params.get("id"))
                    .and_then(Value::as_i64);
                status = id.and_then(|id| {
                    details
                        .get("tasks")
                        .and_then(Value::as_array)
                        .and_then(|tasks| {
                            tasks
                                .iter()
                                .find(|task| task.get("id").and_then(Value::as_i64) == Some(id))
                        })
                        .and_then(|task| task.get("status"))
                        .and_then(Value::as_str)
                        .and_then(TaskStatus::parse)
                });
            }
            _ => {}
        }
    }
    let text = match status {
        Some(status) => theme.fg(
            status_color(status),
            &format!(
                "{} {}",
                status_glyph(status),
                i18n.format_status_label(status)
            ),
        ),
        None => theme.fg("success", "✓"),
    };
    json!({ "type": "text", "props": { "text": text } })
}

#[cfg(test)]
mod tests {
    //! Port of upstream `view/format.test.ts` @ `0fdf4f8` (the
    //! recording-theme assertions are byte-exact) plus the renderCall /
    //! renderResult cases of `todo.register.test.ts` and the guidance-free
    //! command line formatting of `todo.command.test.ts`.

    use super::*;
    use serde_json::json;

    /// Recording theme (upstream `makeTheme` form): wraps segments so the
    /// color/attribute composition is byte-visible.
    struct RecordingTheme;

    impl TodoTheme for RecordingTheme {
        fn fg(&self, color: &str, text: &str) -> String {
            format!("<{color}>{text}</{color}>")
        }
        fn bold(&self, text: &str) -> String {
            format!("<b>{text}</b>")
        }
        fn strikethrough(&self, text: &str) -> String {
            format!("<strike>{text}</strike>")
        }
    }

    fn task(overrides: impl FnOnce(&mut Task)) -> Task {
        let mut base = Task {
            id: 1,
            subject: "quiet task".to_owned(),
            status: TaskStatus::Pending,
            description: None,
            active_form: None,
            blocked_by: None,
            owner: None,
            metadata: None,
        };
        overrides(&mut base);
        base
    }

    // ------------------------------------------------------------------
    // formatOverlayTaskLine — semantic color hierarchy (format.test.ts)
    // ------------------------------------------------------------------

    #[test]
    fn pending_subjects_primary_with_quiet_ids() {
        assert_eq!(
            format_overlay_task_line(&task(|_| {}), &RecordingTheme, true),
            "<dim>○</dim> <dim>#1</dim> <text>quiet task</text>"
        );
    }

    #[test]
    fn in_progress_emphasized_with_muted_metadata() {
        assert_eq!(
            format_overlay_task_line(
                &task(|t| {
                    t.status = TaskStatus::InProgress;
                    t.active_form = Some("Working".to_owned());
                    t.blocked_by = Some(vec![2, 3]);
                }),
                &RecordingTheme,
                true,
            ),
            "<warning>◐</warning> <dim>#1</dim> <accent>quiet task</accent> <muted>(Working)</muted> <muted>⛓ #2,#3</muted>"
        );
    }

    #[test]
    fn completed_subjects_muted_and_struck() {
        assert_eq!(
            format_overlay_task_line(
                &task(|t| t.status = TaskStatus::Completed),
                &RecordingTheme,
                false,
            ),
            "<success>✓</success> <strike><muted>quiet task</muted></strike>"
        );
    }

    // ------------------------------------------------------------------
    // formatOverlayTaskLine — terminal control characters (format.test.ts)
    // ------------------------------------------------------------------

    #[test]
    fn escape_sequences_stripped_before_theming() {
        assert_eq!(
            format_overlay_task_line(
                &task(|t| {
                    t.status = TaskStatus::InProgress;
                    t.subject = "quiet\u{1b}[2Jtask".to_owned();
                    t.active_form = Some("Work\u{9b}cing".to_owned());
                }),
                &RecordingTheme,
                false,
            ),
            "<warning>◐</warning> <accent>quiettask</accent> <muted>(Working)</muted>"
        );
    }

    // ------------------------------------------------------------------
    // Glyph/color/prefix tables (pin the upstream literals)
    // ------------------------------------------------------------------

    #[test]
    fn glyph_tables_match_upstream_literals() {
        assert_eq!(status_glyph(TaskStatus::Pending), "○");
        assert_eq!(status_glyph(TaskStatus::InProgress), "◐");
        assert_eq!(status_glyph(TaskStatus::Completed), "●");
        assert_eq!(status_glyph(TaskStatus::Deleted), "⊘");
        assert_eq!(status_color(TaskStatus::Pending), "dim");
        assert_eq!(status_color(TaskStatus::InProgress), "warning");
        assert_eq!(status_color(TaskStatus::Completed), "success");
        assert_eq!(status_color(TaskStatus::Deleted), "muted");
        assert_eq!(action_glyph(TaskAction::Create), "+");
        assert_eq!(action_glyph(TaskAction::Update), "→");
        assert_eq!(action_glyph(TaskAction::Delete), "×");
        assert_eq!(action_glyph(TaskAction::Get), "›");
        assert_eq!(action_glyph(TaskAction::List), "☰");
        assert_eq!(action_glyph(TaskAction::Clear), "∅");
        // The overlay row table differs from the echo table on exactly
        // the completed/deleted cells (upstream comment).
        assert_eq!(
            overlay_status_glyph(TaskStatus::Completed, &IdentityTheme),
            "✓"
        );
        assert_eq!(
            overlay_status_glyph(TaskStatus::Deleted, &IdentityTheme),
            "✗"
        );
    }

    // ------------------------------------------------------------------
    // renderTodoCall (todo.register.test.ts — renderCall describe)
    // ------------------------------------------------------------------

    fn call_text(args: &Value, state: &TaskState) -> String {
        let i18n = I18n::for_locale("en");
        render_todo_call(args, &IdentityTheme, state, &i18n)["props"]["text"]
            .as_str()
            .expect("text prop")
            .to_owned()
    }

    fn seeded_state() -> TaskState {
        TaskState {
            tasks: vec![task(|t| t.subject = "seeded-subject".to_owned())],
            next_id: 2,
        }
    }

    #[test]
    fn render_call_create_emits_prefix_and_subject() {
        let text = call_text(
            &json!({"action": "create", "subject": "hello"}),
            &TaskState::empty(),
        );
        assert!(text.contains("todo "), "{text}");
        assert!(text.contains("+"), "{text}");
        assert!(text.contains("hello"), "{text}");
    }

    #[test]
    fn render_call_update_renders_the_bare_id_when_unregistered() {
        let text = call_text(&json!({"action": "update", "id": 42}), &TaskState::empty());
        assert!(text.contains("#42"), "{text}");
    }

    #[test]
    fn render_call_update_renders_the_subject_when_seeded() {
        let text = call_text(&json!({"action": "update", "id": 1}), &seeded_state());
        assert!(text.contains("seeded-subject"), "{text}");
    }

    #[test]
    fn render_call_list_with_status_renders_the_humanized_label() {
        let text = call_text(
            &json!({"action": "list", "status": "in_progress"}),
            &TaskState::empty(),
        );
        assert!(text.contains("in progress"), "{text}");
    }

    #[test]
    fn render_call_clear_renders_only_the_base_prefix() {
        let text = call_text(&json!({"action": "clear"}), &TaskState::empty());
        assert!(text.contains("∅"), "{text}");
    }

    #[test]
    fn render_call_segments_compose_through_the_theme() {
        let i18n = I18n::for_locale("en");
        let tree = render_todo_call(
            &json!({"action": "create", "subject": "hello"}),
            &RecordingTheme,
            &TaskState::empty(),
            &i18n,
        );
        assert_eq!(tree["type"], json!("text"));
        assert_eq!(
            tree["props"]["text"],
            json!("<toolTitle><b>todo </b></toolTitle><muted>+</muted> <dim>hello</dim>")
        );
    }

    #[test]
    fn render_call_missing_action_renders_the_js_undefined_glyph() {
        // Partial streaming args: upstream `ACTION_GLYPH[undefined] ??
        // undefined` stringifies to the literal "undefined".
        let text = call_text(&json!({}), &TaskState::empty());
        assert!(text.contains("undefined"), "{text}");
    }

    // ------------------------------------------------------------------
    // renderTodoResult (todo.register.test.ts — renderResult describe)
    // ------------------------------------------------------------------

    fn result_text(result: &Value) -> String {
        let i18n = I18n::for_locale("en");
        render_todo_result(result, &IdentityTheme, &i18n)["props"]["text"]
            .as_str()
            .expect("text prop")
            .to_owned()
    }

    #[test]
    fn render_result_create_echoes_the_new_tasks_status() {
        let result = json!({
            "content": [],
            "details": {
                "action": "create",
                "params": {"action": "create", "subject": "a"},
                "tasks": [{"id": 1, "subject": "a", "status": "pending"}],
                "nextId": 2,
            }
        });
        let text = result_text(&result);
        assert!(text.contains("pending"), "{text}");
        assert!(text.contains("○"), "{text}");
    }

    #[test]
    fn render_result_update_echoes_the_transitioned_status() {
        let result = json!({
            "content": [],
            "details": {
                "action": "update",
                "params": {"action": "update", "id": 1, "status": "in_progress"},
                "tasks": [{"id": 1, "subject": "a", "status": "in_progress"}],
                "nextId": 2,
            }
        });
        let text = result_text(&result);
        assert!(text.contains("in progress"), "{text}");
        assert!(text.contains("◐"), "{text}");
    }

    #[test]
    fn render_result_delete_echoes_the_tombstone_label() {
        let result = json!({
            "content": [],
            "details": {
                "action": "delete",
                "params": {"action": "delete", "id": 1},
                "tasks": [{"id": 1, "subject": "a", "status": "deleted"}],
                "nextId": 2,
            }
        });
        let text = result_text(&result);
        assert!(text.contains("deleted"), "{text}");
        assert!(text.contains("⊘"), "{text}");
    }

    #[test]
    fn render_result_list_get_clear_render_the_plain_check() {
        for action in ["list", "get", "clear"] {
            let result = json!({
                "content": [],
                "details": {
                    "action": action,
                    "params": {"action": action},
                    "tasks": [{"id": 1, "subject": "a", "status": "pending"}],
                    "nextId": 2,
                }
            });
            let text = result_text(&result);
            assert!(text.contains("✓"), "{action}: {text}");
        }
    }

    #[test]
    fn render_result_missing_details_falls_back_to_the_check() {
        let text = result_text(&json!({"content": [], "details": null}));
        assert_eq!(text, "✓");
        let text = result_text(&json!({"content": []}));
        assert_eq!(text, "✓");
    }

    #[test]
    fn render_result_status_echo_composes_through_the_theme() {
        let i18n = I18n::for_locale("en");
        let tree = render_todo_result(
            &json!({
                "content": [],
                "details": {
                    "action": "create",
                    "params": {},
                    "tasks": [{"id": 1, "subject": "a", "status": "pending"}],
                    "nextId": 2,
                }
            }),
            &RecordingTheme,
            &i18n,
        );
        assert_eq!(tree["props"]["text"], json!("<dim>○ pending</dim>"));
    }

    // ------------------------------------------------------------------
    // /todos command line (todo.command.test.ts formatting)
    // ------------------------------------------------------------------

    #[test]
    fn command_task_line_formats_id_subject_form_and_chain() {
        let line = format_command_task_line(
            &task(|t| {
                t.subject = "build".to_owned();
                t.active_form = Some("Building".to_owned());
                t.status = TaskStatus::InProgress;
            }),
            "◐",
        );
        assert_eq!(line, "  ◐ #1 build (Building)");
        let line = format_command_task_line(
            &task(|t| {
                t.subject = "follow-up".to_owned();
                t.blocked_by = Some(vec![1, 3]);
            }),
            "○",
        );
        assert_eq!(line, "  ○ #1 follow-up    ⛓ #1,#3");
    }

    // ------------------------------------------------------------------
    // AnsiTheme resolution
    // ------------------------------------------------------------------

    #[test]
    fn ansi_theme_resolves_tokens_through_vars_to_truecolor() {
        let theme_json = json!({
            "name": "dark",
            "vars": {"green": "#b5bd68", "gray": "#808080"},
            "colors": {
                "success": "green",
                "muted": "gray",
                "accent": "#8abeb7",
                "index": 42,
                "empty": "",
                "missing-var": "nonexistent",
            }
        });
        let theme = AnsiTheme::from_theme_json(&theme_json);
        assert_eq!(theme.fg("success", "x"), "\x1b[38;2;181;189;104mx\x1b[39m");
        assert_eq!(theme.fg("accent", "y"), "\x1b[38;2;138;190;183my\x1b[39m");
        assert_eq!(theme.fg("index", "z"), "\x1b[38;5;42mz\x1b[39m");
        assert_eq!(theme.fg("empty", "e"), "\x1b[39me\x1b[39m");
        // Unresolvable/unknown tokens render unstyled (no panic).
        assert_eq!(theme.fg("missing-var", "u"), "u\x1b[39m");
        assert_eq!(theme.fg("not-a-token", "n"), "n\x1b[39m");
        assert_eq!(theme.bold("b"), "\x1b[1mb\x1b[22m");
        assert_eq!(theme.strikethrough("s"), "\x1b[9ms\x1b[29m");
    }

    #[test]
    fn ansi_theme_tolerates_degenerate_json() {
        let theme = AnsiTheme::from_theme_json(&json!(null));
        assert_eq!(theme.fg("dim", "x"), "x\x1b[39m");
        let theme = AnsiTheme::from_theme_json(&json!({"colors": "nope"}));
        assert_eq!(theme.fg("dim", "x"), "x\x1b[39m");
    }
}
