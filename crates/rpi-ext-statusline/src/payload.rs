//! stdin JSON assembly — the Claude Code statusline contract (TE12 FR-B).
//!
//! Field names and shapes follow the CC documentation ("Customize your
//! status line", code.claude.com/docs/en/statusline) so community scripts
//! run unmodified. Values rpi cannot source are OMITTED (CC scripts are
//! required to fall back), never invented. The `rpi` block is a documented
//! rpi extension (FR-L): it carries the session totals the plugin already
//! accumulates, because the rpi transcript JSONL differs from the CC
//! transcript shape (camelCase usage, no top-level `type: "assistant"`
//! rows) — a CC script parsing `transcript_path` itself reads zeros
//! (TE-D34).

use serde_json::{json, Map, Value};

use crate::state::{Snapshot, Totals};

/// CC fixed threshold for `exceeds_200k_tokens` ("a fixed threshold
/// regardless of actual context window size").
const EXCEEDS_THRESHOLD: u64 = 200_000;

/// `rpi.transcript_format` marker for the v0.1 session JSONL shape.
pub const TRANSCRIPT_FORMAT: &str = "rpi-v3";

/// Build the JSON piped to the script's stdin. Insertion-ordered
/// (serde_json preserve_order) so the payload is byte-stable for tests.
pub fn build_stdin_json(snapshot: &Snapshot) -> Value {
    let mut root = Map::new();
    root.insert("hook_event_name".into(), json!("Status"));
    root.insert("cwd".into(), json!(snapshot.cwd));
    if let Some(id) = &snapshot.session_id {
        root.insert("session_id".into(), json!(id));
    }
    if let Some(name) = &snapshot.session_name {
        root.insert("session_name".into(), json!(name));
    }
    if let Some(path) = &snapshot.transcript_path {
        root.insert("transcript_path".into(), json!(path));
    }
    // model: {id, display_name} from ctx.model ({id, name, ...}).
    if let Some(model) = &snapshot.model {
        if let (Some(id), Some(name)) = (
            model.get("id").and_then(Value::as_str),
            model.get("name").and_then(Value::as_str),
        ) {
            root.insert("model".into(), json!({"id": id, "display_name": name}));
        }
    }
    root.insert("version".into(), json!(env!("CARGO_PKG_VERSION")));
    root.insert(
        "workspace".into(),
        json!({
            "current_dir": snapshot.cwd,
            "project_dir": snapshot.cwd,
            "added_dirs": [],
        }),
    );

    // cost: plugin-accumulated (message_end usage.cost.total) + session
    // clock. Fields without an rpi source are omitted (CC scripts use
    // `.get() or 0` fallbacks).
    root.insert(
        "cost".into(),
        json!({
            "total_cost_usd": snapshot.totals.cost,
            "total_duration_ms": snapshot.session_elapsed_ms.min(u64::MAX as u128) as u64,
        }),
    );

    // context_window: ctx.getContextUsage is authoritative (handles
    // compaction); the window size falls back to ctx.model.contextWindow.
    let context_window = context_window_object(snapshot);
    root.insert("context_window".into(), Value::Object(context_window));

    root.insert(
        "exceeds_200k_tokens".into(),
        json!(exceeds_200k(&snapshot.last_usage)),
    );

    if let Some(level) = &snapshot.thinking_level {
        root.insert("effort".into(), json!({"level": level}));
        root.insert("thinking".into(), json!({"enabled": level != "off"}));
    }

    // rpi extension block (FR-L, TE-D34 mitigation).
    root.insert(
        "rpi".into(),
        json!({
            "session_totals": totals_object(&snapshot.totals),
            "transcript_format": TRANSCRIPT_FORMAT,
        }),
    );

    Value::Object(root)
}

/// `context_window` object: `context_window_size` always present (0 when
/// unknown — scripts like the local statusline.py detect the model by name
/// in that case); `total_input_tokens` / `used_percentage` /
/// `remaining_percentage` only when the host knows them (omitted right
/// after compaction, mirroring `ContextUsage.tokens == null`); the
/// `current_usage` sub-object only once a message has landed.
fn context_window_object(snapshot: &Snapshot) -> Map<String, Value> {
    let usage = snapshot.context_usage.as_ref();
    let size = usage
        .and_then(|c| c.get("contextWindow"))
        .and_then(Value::as_u64)
        .or_else(|| {
            snapshot
                .model
                .as_ref()
                .and_then(|m| m.get("contextWindow"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    let mut object = Map::new();
    object.insert("context_window_size".into(), json!(size));
    if let Some(tokens) = usage.and_then(|c| c.get("tokens")).and_then(Value::as_u64) {
        object.insert("total_input_tokens".into(), json!(tokens));
    }
    if let Some(percent) = usage.and_then(|c| c.get("percent")).and_then(Value::as_f64) {
        object.insert("used_percentage".into(), json!(percent));
        object.insert("remaining_percentage".into(), json!(100.0 - percent));
    }
    if let Some(last) = &snapshot.last_usage {
        object.insert(
            "current_usage".into(),
            json!({
                "input_tokens": last.input,
                "output_tokens": last.output,
                "cache_creation_input_tokens": last.cache_write,
                "cache_read_input_tokens": last.cache_read,
            }),
        );
    }
    object
}

fn exceeds_200k(last_usage: &Option<Totals>) -> bool {
    last_usage
        .map(|t| t.input + t.output + t.cache_read + t.cache_write > EXCEEDS_THRESHOLD)
        .unwrap_or(false)
}

fn totals_object(totals: &Totals) -> Value {
    json!({
        "input": totals.input,
        "output": totals.output,
        "cache_read": totals.cache_read,
        "cache_write": totals.cache_write,
        "cost_usd": totals.cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_snapshot() -> Snapshot {
        Snapshot {
            cwd: "/home/leven/develop/ai/revpi".into(),
            model: Some(json!({
                "id": "glm-5.1",
                "name": "GLM-5.1",
                "provider": "custom",
                "contextWindow": 200_000,
            })),
            context_usage: Some(json!({
                "tokens": 85_400,
                "contextWindow": 200_000,
                "percent": 42.7,
            })),
            thinking_level: Some("xhigh".into()),
            session_name: Some("statusline-work".into()),
            totals: Totals {
                input: 1500,
                output: 999,
                cache_read: 12_000,
                cache_write: 2_500_000,
                cost: 0.1234,
            },
            last_usage: Some(Totals {
                input: 85_400,
                output: 1200,
                cache_read: 70_000,
                cache_write: 15_000,
                cost: 0.05,
            }),
            session_elapsed_ms: 754_000,
            transcript_path: Some(
                "/home/leven/.rpi/agent/sessions/--x--/2026-08-19T10-00-00-000_018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e.jsonl".into(),
            ),
            session_id: Some("018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e".into()),
        }
    }

    #[test]
    fn full_snapshot_maps_every_cc_field() {
        let payload = build_stdin_json(&full_snapshot());
        assert_eq!(payload["hook_event_name"], "Status");
        assert_eq!(payload["cwd"], "/home/leven/develop/ai/revpi");
        assert_eq!(
            payload["session_id"],
            "018f6a1e-4c3b-7abc-8d2e-9f0a1b2c3d4e"
        );
        assert_eq!(payload["session_name"], "statusline-work");
        assert!(payload["transcript_path"]
            .as_str()
            .unwrap()
            .ends_with(".jsonl"));
        assert_eq!(payload["model"]["id"], "glm-5.1");
        assert_eq!(payload["model"]["display_name"], "GLM-5.1");
        assert_eq!(
            payload["workspace"]["current_dir"],
            "/home/leven/develop/ai/revpi"
        );
        assert_eq!(payload["workspace"]["added_dirs"], json!([]));
        assert_eq!(payload["cost"]["total_cost_usd"], 0.1234);
        assert_eq!(payload["cost"]["total_duration_ms"], 754_000);
        assert_eq!(payload["context_window"]["context_window_size"], 200_000);
        assert_eq!(payload["context_window"]["total_input_tokens"], 85_400);
        assert_eq!(payload["context_window"]["used_percentage"], 42.7);
        assert_eq!(payload["context_window"]["remaining_percentage"], 57.3);
        assert_eq!(
            payload["context_window"]["current_usage"]["cache_creation_input_tokens"],
            15_000
        );
        assert_eq!(payload["effort"]["level"], "xhigh");
        assert_eq!(payload["thinking"]["enabled"], true);
        // exceeds_200k: 85400+1200+70000+15000 = 171600 < 200k.
        assert_eq!(payload["exceeds_200k_tokens"], false);
        // rpi extension block.
        assert_eq!(payload["rpi"]["session_totals"]["input"], 1500);
        assert_eq!(payload["rpi"]["transcript_format"], "rpi-v3");
        // CC-only fields rpi cannot source stay omitted.
        for absent in ["output_style", "vim", "pr", "fast_mode", "prompt_id"] {
            assert!(payload.get(absent).is_none(), "{absent} must be omitted");
        }
    }

    #[test]
    fn empty_snapshot_omits_optional_fields_and_zeroes_context() {
        let payload = build_stdin_json(&Snapshot::default());
        for absent in [
            "session_id",
            "session_name",
            "transcript_path",
            "model",
            "effort",
            "thinking",
        ] {
            assert!(payload.get(absent).is_none(), "{absent} must be omitted");
        }
        assert_eq!(payload["cost"]["total_cost_usd"], 0.0);
        assert_eq!(payload["context_window"]["context_window_size"], 0);
        assert!(payload["context_window"]
            .get("total_input_tokens")
            .is_none());
        assert!(payload["context_window"].get("used_percentage").is_none());
        assert!(payload["context_window"].get("current_usage").is_none());
        assert_eq!(payload["exceeds_200k_tokens"], false);
        assert_eq!(payload["rpi"]["session_totals"]["input"], 0);
    }

    #[test]
    fn exceeds_threshold_uses_last_usage_not_totals() {
        let mut snapshot = full_snapshot();
        // Totals above the threshold but the LAST response below it → false
        // (CC: "from the most recent API response").
        snapshot.last_usage = Some(Totals {
            input: 1,
            output: 1,
            cache_read: 1,
            cache_write: 1,
            cost: 0.0,
        });
        assert_eq!(build_stdin_json(&snapshot)["exceeds_200k_tokens"], false);
        snapshot.last_usage = Some(Totals {
            input: 250_000,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            cost: 0.0,
        });
        assert_eq!(build_stdin_json(&snapshot)["exceeds_200k_tokens"], true);
    }

    #[test]
    fn context_size_falls_back_to_model_window() {
        let mut snapshot = full_snapshot();
        snapshot.context_usage = None;
        let payload = build_stdin_json(&snapshot);
        assert_eq!(payload["context_window"]["context_window_size"], 200_000);
        assert!(payload["context_window"]
            .get("total_input_tokens")
            .is_none());
    }
}
