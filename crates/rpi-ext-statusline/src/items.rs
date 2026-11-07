//! `statusLine.type:"items"` configuration (TE52 / ADR-0040 R20 §4.1).
//!
//! Two modes share the `statusLine` object:
//! - `type:"command"` keeps the TE12 script contract unchanged;
//! - `type:"items"` renders the strict 29-field whitelist without starting a
//!   shell.
//!
//! The parser canonicalizes aliases, keeps the first occurrence of a
//! canonical id, retains unknown ids (diagnosed once) and falls back to the
//! frozen defaults for every malformed display option. Values are sanitized
//! at render time (`fields::sanitize_text`).

use serde_json::{Map, Value, json};

use crate::config::{DEFAULT_LIVE_REFRESH_MS, LiveTokens, Placement};
use crate::fields::{FieldId, NumberFormat, valid_separator};

/// Default `separator` (R20 §4.1).
pub const DEFAULT_SEPARATOR: &str = " · ";
/// Default local refresh interval in seconds (items mode; R20 §4.1).
pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 5;

/// One configured item. `field` is `None` for an unknown id: the raw spelling
/// is preserved verbatim (never dropped on save) but never rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemEntry {
    pub raw: String,
    pub field: Option<FieldId>,
}

impl ItemEntry {
    fn known(id: FieldId) -> Self {
        Self {
            raw: id.id().to_owned(),
            field: Some(id),
        }
    }
}

/// Parsed items configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemsConfig {
    /// Selected known fields in display order (canonical ids).
    pub entries: Vec<ItemEntry>,
    /// Unknown raw ids, preserved in encounter order and re-emitted on save.
    pub unknown: Vec<String>,
    pub separator: String,
    pub padding: usize,
    pub placement: Placement,
    pub refresh_interval_secs: u64,
    pub use_colors: bool,
    pub number_format: NumberFormat,
    pub live_tokens: LiveTokens,
}

impl Default for ItemsConfig {
    fn default() -> Self {
        // Default install: the session's absolute token total plus the live
        // output rate. Every other field (model/session/context/cost/Git/...)
        // stays off until the user enables it through `/statusline`.
        // `liveTokens` is on so the rate row actually renders while a
        // response streams.
        Self {
            entries: vec![
                ItemEntry::known(FieldId::TotalTokens),
                ItemEntry::known(FieldId::LiveOutputRate),
            ],
            unknown: Vec::new(),
            separator: DEFAULT_SEPARATOR.to_owned(),
            padding: 0,
            // Keep the default rows on the shared bottom status line with
            // rpi-usage (dot-separated by the host), instead of a separate
            // widget above the built-in footer.
            placement: Placement::Status,
            refresh_interval_secs: DEFAULT_REFRESH_INTERVAL_SECS,
            use_colors: true,
            number_format: NumberFormat::Compact,
            live_tokens: LiveTokens {
                enabled: true,
                refresh_ms: DEFAULT_LIVE_REFRESH_MS,
            },
        }
    }
}

impl ItemsConfig {
    /// Configured known fields in display order.
    pub fn fields(&self) -> impl Iterator<Item = FieldId> + '_ {
        self.entries.iter().filter_map(|entry| entry.field)
    }

    pub fn has_field(&self, id: FieldId) -> bool {
        self.fields().any(|field| field == id)
    }

    pub fn has_live_field(&self) -> bool {
        self.fields().any(FieldId::is_live)
    }

    pub fn has_session_total(&self) -> bool {
        self.fields().any(FieldId::is_session_total)
    }

    pub fn needs_git(&self) -> bool {
        self.fields().any(FieldId::needs_git)
    }

    pub fn needs_duration(&self) -> bool {
        self.has_field(FieldId::SessionDuration)
    }

    /// Any field that reads the absolute `ctx.sessionStats` snapshot:
    /// session totals/counts plus `session-duration` (its `startedAt`
    /// anchor).
    pub fn needs_session_stats(&self) -> bool {
        self.has_session_total() || self.needs_duration()
    }

    /// Live data is collected only when `liveTokens` is enabled AND at least
    /// one live field is selected (R20 §4.4).
    pub fn live_enabled(&self) -> bool {
        self.live_tokens.enabled && self.has_live_field()
    }

    /// Replace the configured item list (editor draft); canonical alias
    /// resolution and unknown retention apply.
    pub fn set_items(&mut self, raw_items: &[String]) {
        let (entries, unknown) = canonicalize(raw_items);
        self.entries = entries;
        self.unknown = unknown;
    }

    /// The `statusLine` object this config serializes to. The editor merges
    /// it over the existing object so `command`, `timeoutMs` and unknown keys
    /// survive.
    pub fn to_settings_object(&self) -> Map<String, Value> {
        let mut object = Map::new();
        object.insert("type".into(), json!("items"));
        let mut items: Vec<Value> = self.entries.iter().map(|entry| json!(entry.raw)).collect();
        items.extend(self.unknown.iter().map(|raw| json!(raw)));
        object.insert("items".into(), Value::Array(items));
        object.insert("separator".into(), json!(self.separator));
        object.insert("padding".into(), json!(self.padding));
        object.insert("placement".into(), json!(placement_name(self.placement)));
        object.insert("refreshInterval".into(), json!(self.refresh_interval_secs));
        object.insert("useColors".into(), json!(self.use_colors));
        object.insert(
            "numberFormat".into(),
            json!(match self.number_format {
                NumberFormat::Compact => "compact",
                NumberFormat::Exact => "exact",
            }),
        );
        object.insert("liveTokens".into(), live_tokens_value(self.live_tokens));
        object
    }
}

fn placement_name(placement: Placement) -> &'static str {
    match placement {
        Placement::Replace => "replace",
        Placement::Widget => "widget",
        Placement::Status => "status",
    }
}

fn live_tokens_value(live: LiveTokens) -> Value {
    if !live.enabled {
        return json!(false);
    }
    if live.refresh_ms == DEFAULT_LIVE_REFRESH_MS {
        json!(true)
    } else {
        json!({"refreshMs": live.refresh_ms})
    }
}

/// Parse a `statusLine` object of `type:"items"`.
pub fn parse_items(object: &Map<String, Value>) -> ItemsConfig {
    let mut config = ItemsConfig::default();
    match object.get("items") {
        None => {}
        Some(Value::Array(items)) => {
            let raw: Vec<String> = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let (entries, unknown) = canonicalize(&raw);
            config.entries = entries;
            config.unknown = unknown;
        }
        // A non-array `items` is a malformed option: keep the default preset
        // rather than silently producing an empty footer.
        Some(_) => {}
    }
    if let Some(separator) = object.get("separator").and_then(Value::as_str)
        && valid_separator(separator)
    {
        config.separator = separator.to_owned();
    }
    if let Some(padding) = object.get("padding").and_then(Value::as_u64) {
        config.padding = padding.min(crate::config::MAX_PADDING as u64) as usize;
    }
    if let Some(placement) = object.get("placement").and_then(Value::as_str) {
        config.placement = Placement::parse_items(placement);
    }
    if let Some(refresh) = object.get("refreshInterval").and_then(Value::as_u64) {
        config.refresh_interval_secs = refresh.max(1);
    }
    if let Some(use_colors) = object.get("useColors").and_then(Value::as_bool) {
        config.use_colors = use_colors;
    }
    if let Some(format) = object.get("numberFormat").and_then(Value::as_str) {
        config.number_format = NumberFormat::parse(Some(format));
    }
    config.live_tokens =
        crate::config::parse_live_tokens_with_default(object.get("liveTokens"), true);
    config
}

/// Canonicalize a raw item list: aliases resolve, first occurrence of a
/// canonical id wins, unknown ids are retained once (diagnosed by the
/// caller).
pub fn canonicalize(raw_items: &[String]) -> (Vec<ItemEntry>, Vec<String>) {
    let (entries, unknown) = canonicalize_inner(raw_items);
    diagnose_unknown(&unknown);
    (entries, unknown)
}

fn canonicalize_inner(raw_items: &[String]) -> (Vec<ItemEntry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut unknown = Vec::new();
    for raw in raw_items {
        match FieldId::parse(raw) {
            Some(id) => {
                if !entries
                    .iter()
                    .any(|entry: &ItemEntry| entry.field == Some(id))
                {
                    entries.push(ItemEntry::known(id));
                }
            }
            None => {
                if !unknown.iter().any(|seen| seen == raw) {
                    unknown.push(raw.clone());
                }
            }
        }
    }
    (entries, unknown)
}

/// One diagnostic per unknown id per process (the config is re-read every
/// tick; the raw id stays in the file and is never dropped).
fn diagnose_unknown(unknown: &[String]) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    if unknown.is_empty() {
        return;
    }
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut seen = seen.lock().unwrap_or_else(|error| error.into_inner());
    for id in unknown {
        if seen.insert(id.clone()) {
            tracing::warn!(id, "rpi-statusline: unknown item id kept but not rendered");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value) -> ItemsConfig {
        parse_items(value.as_object().expect("object"))
    }

    #[test]
    fn defaults_are_the_session_token_total_and_live_rate_with_display_defaults() {
        let config = parse(json!({"type": "items"}));
        let fields: Vec<FieldId> = config.fields().collect();
        assert_eq!(fields, vec![FieldId::TotalTokens, FieldId::LiveOutputRate]);
        assert_eq!(config.separator, " · ");
        assert_eq!(config.padding, 0);
        assert_eq!(config.placement, Placement::Status);
        assert_eq!(config.refresh_interval_secs, 5);
        assert!(config.use_colors);
        assert_eq!(config.number_format, NumberFormat::Compact);
        // Default install: the session total plus the live rate are the only
        // enabled fields, and the live switch is on.
        assert!(config.live_tokens.enabled);
        assert!(config.live_enabled());
        assert!(config.needs_session_stats());
        assert!(!config.needs_duration());
    }

    #[test]
    fn explicit_empty_array_is_a_valid_empty_config() {
        let config = parse(json!({"type": "items", "items": []}));
        assert!(config.entries.is_empty());
        assert!(config.unknown.is_empty());
        assert!(!config.has_live_field());
    }

    #[test]
    fn aliases_canonicalize_and_first_occurrence_wins() {
        let config = parse(json!({
            "type": "items",
            "items": ["model-name", "model", "status", "context-usage", "project", "thread-name"]
        }));
        let fields: Vec<FieldId> = config.fields().collect();
        assert_eq!(
            fields,
            vec![
                FieldId::Model,
                FieldId::RunState,
                FieldId::ContextUsed,
                FieldId::ProjectName,
                FieldId::SessionName
            ]
        );
    }

    #[test]
    fn unknown_ids_are_retained_once_but_never_rendered() {
        let config = parse(json!({
            "type": "items",
            "items": ["hostname", "model", "hostname", "permissions", "unknown-x"]
        }));
        let fields: Vec<FieldId> = config.fields().collect();
        assert_eq!(fields, vec![FieldId::Model]);
        assert_eq!(config.unknown, vec!["hostname", "permissions", "unknown-x"]);
        // Save round-trips them at the tail.
        let object = config.to_settings_object();
        assert_eq!(
            object["items"],
            json!(["model", "hostname", "permissions", "unknown-x"])
        );
    }

    #[test]
    fn malformed_display_options_fall_back_to_defaults() {
        let config = parse(json!({
            "type": "items",
            "items": "not-an-array",
            "separator": "bad\nseparator",
            "padding": 99,
            "placement": "nowhere",
            "refreshInterval": 0,
            "useColors": "yes",
            "numberFormat": "weird",
            "liveTokens": "yes"
        }));
        // Non-array items → default preset (session total + live rate).
        let fields: Vec<FieldId> = config.fields().collect();
        assert_eq!(fields, vec![FieldId::TotalTokens, FieldId::LiveOutputRate]);
        assert_eq!(config.separator, DEFAULT_SEPARATOR);
        assert_eq!(config.padding, crate::config::MAX_PADDING);
        assert_eq!(config.placement, Placement::Status);
        assert_eq!(config.refresh_interval_secs, 1);
        assert!(config.use_colors);
        assert_eq!(config.number_format, NumberFormat::Compact);
        // A malformed liveTokens value falls back to the items default (on).
        assert!(config.live_tokens.enabled);
    }

    #[test]
    fn explicit_options_parse_and_clamp() {
        let config = parse(json!({
            "type": "items",
            "items": ["model", "live-output-tokens"],
            "separator": " | ",
            "padding": 3,
            "placement": "status",
            "refreshInterval": 9,
            "useColors": false,
            "numberFormat": "exact",
            "liveTokens": {"refreshMs": 1}
        }));
        assert_eq!(config.separator, " | ");
        assert_eq!(config.padding, 3);
        assert_eq!(config.placement, Placement::Status);
        assert_eq!(config.refresh_interval_secs, 9);
        assert!(!config.use_colors);
        assert_eq!(config.number_format, NumberFormat::Exact);
        assert!(config.live_tokens.enabled);
        assert_eq!(
            config.live_tokens.refresh_ms,
            crate::config::MIN_LIVE_REFRESH_MS
        );
        assert!(config.live_enabled());
    }

    #[test]
    fn live_needs_both_the_switch_and_a_live_field() {
        let mut config = parse(json!({"type": "items", "items": ["model"]}));
        config.live_tokens.enabled = true;
        assert!(!config.live_enabled());
        config.set_items(&["model".into(), "streaming".into()]);
        assert!(config.live_enabled());
        config.live_tokens.enabled = false;
        assert!(!config.live_enabled());
    }

    #[test]
    fn set_items_canonicalizes_and_keeps_unknowns() {
        let mut config = ItemsConfig::default();
        config.set_items(&[
            "git-branch".into(),
            "project".into(),
            "git-branch".into(),
            "legacy-item".into(),
        ]);
        let fields: Vec<FieldId> = config.fields().collect();
        assert_eq!(fields, vec![FieldId::GitBranch, FieldId::ProjectName]);
        assert_eq!(config.unknown, vec!["legacy-item"]);
    }

    #[test]
    fn settings_object_has_the_frozen_shape() {
        let config = ItemsConfig {
            live_tokens: LiveTokens {
                enabled: true,
                refresh_ms: 500,
            },
            ..ItemsConfig::default()
        };
        let object = config.to_settings_object();
        assert_eq!(object["type"], json!("items"));
        assert_eq!(object["items"], json!(["total-tokens", "live-output-rate"]));
        assert_eq!(object["separator"], json!(" · "));
        assert_eq!(object["placement"], json!("status"));
        assert_eq!(object["refreshInterval"], json!(5));
        assert_eq!(object["useColors"], json!(true));
        assert_eq!(object["numberFormat"], json!("compact"));
        assert_eq!(object["liveTokens"], json!({"refreshMs": 500}));
    }
}
