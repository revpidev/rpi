//! Output guard + spill-to-disk for oversized MCP results (FR-P1-05, design
//! §3.9).
//!
//! Port of `mcp-output-guard.ts` @ pi-mcp-adapter v2.24.0 (3d953f90):
//! `guardMcpOutput` (50 KiB / 2000 lines inline text, 16 KiB
//! `details.mcpResult`), `resolveMcpOutputGuardOptions` (settings +
//! `MCP_OUTPUT_GUARD=0` kill switch), spill files in a fresh 0600 temp file
//! per artifact, image blocks passed through uncounted.
//!
//! Security: spill files contain tool OUTPUT only — never credentials
//! (coding-standards §11.2; G4). `!command` secret values must never reach
//! this module's inputs.

use serde_json::{json, Value};

/// Defaults (mcp-output-guard.ts:17-19 area).
pub const DEFAULT_MCP_OUTPUT_MAX_BYTES: usize = 50 * 1024;
pub const DEFAULT_MCP_OUTPUT_MAX_LINES: usize = 2000;
pub const DEFAULT_MCP_DETAILS_MAX_BYTES: usize = 16 * 1024;

const KEY_PREVIEW_LIMIT: usize = 20;
/// `KEY_MAX_BYTES` (mcp-output-guard.ts:15 @ 10a45367): byte cap for
/// summary keys (the old port was char-based).
const KEY_MAX_BYTES: usize = 120;
/// `STRUCTURED_CONTENT_PRESERVE_MAX_BYTES` (mcp-output-guard.ts:14):
/// bounded `details.mcpResult.structuredContent` preservation (#430).
const STRUCTURED_CONTENT_PRESERVE_MAX_BYTES: usize = 4 * 1024;
/// `STRUCTURED_CONTENT_FIELD_PRESERVE_MAX_BYTES` (mcp-output-guard.ts:15).
const STRUCTURED_CONTENT_FIELD_PRESERVE_MAX_BYTES: usize = 512;
const CONTENT_SUMMARY_LIMIT: usize = 20;

/// `McpOutputGuardOptions` (mcp-output-guard.ts:41-58).
#[derive(Debug, Clone, Default)]
pub struct GuardOptions {
    pub enabled: Option<bool>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub empty_text_fallback: Option<String>,
    pub max_bytes: Option<usize>,
    pub max_lines: Option<usize>,
    pub details_max_bytes: Option<usize>,
    pub raw_mcp_result: Option<Value>,
}

/// `GuardedMcpOutput` (mcp-output-guard.ts:60-64).
#[derive(Debug, Clone)]
pub struct GuardedOutput {
    pub content: Vec<Value>,
    pub output_guard: Option<Value>,
    pub mcp_result: Option<Value>,
}

/// `resolveMcpOutputGuardOptions` (mcp-output-guard.ts:66-75):
/// `MCP_OUTPUT_GUARD` env kill switch beats settings; tuning object adjusts
/// the three thresholds.
pub fn resolve_guard_options(settings: Option<&serde_json::Map<String, Value>>) -> GuardOptions {
    let configured = settings.and_then(|s| s.get("outputGuard"));
    let tuning = configured.and_then(Value::as_object);
    let enabled =
        env_kill_switch("MCP_OUTPUT_GUARD").unwrap_or(configured != Some(&Value::Bool(false)));
    GuardOptions {
        enabled: Some(enabled),
        max_bytes: Some(
            positive_int(tuning.and_then(|t| t.get("maxBytes")))
                .unwrap_or(DEFAULT_MCP_OUTPUT_MAX_BYTES),
        ),
        max_lines: Some(
            positive_int(tuning.and_then(|t| t.get("maxLines")))
                .unwrap_or(DEFAULT_MCP_OUTPUT_MAX_LINES),
        ),
        details_max_bytes: Some(
            positive_int(tuning.and_then(|t| t.get("detailsMaxBytes")))
                .unwrap_or(DEFAULT_MCP_DETAILS_MAX_BYTES),
        ),
        ..Default::default()
    }
}

fn positive_int(value: Option<&Value>) -> Option<usize> {
    let n = value.and_then(Value::as_f64).filter(|v| v.is_finite())?;
    let integer = n.floor();
    if integer > 0.0 {
        Some(integer as usize)
    } else {
        None
    }
}

fn env_kill_switch(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }
    if ["0", "false", "no", "off"].contains(&value.as_str()) {
        return Some(false);
    }
    if ["1", "true", "yes", "on"].contains(&value.as_str()) {
        return Some(true);
    }
    None
}

/// `guardedMcpDetails` (mcp-output-guard.ts:78-83).
pub fn guarded_mcp_details(guarded: &GuardedOutput) -> Value {
    let mut details = serde_json::Map::new();
    if let Some(mcp_result) = &guarded.mcp_result {
        details.insert("mcpResult".to_string(), mcp_result.clone());
    }
    if let Some(output_guard) = &guarded.output_guard {
        details.insert("outputGuard".to_string(), output_guard.clone());
    }
    Value::Object(details)
}

fn byte_length(text: &str) -> usize {
    text.len()
}

fn text_stats(text: &str) -> (usize, usize) {
    let bytes = byte_length(text);
    let lines = if text.is_empty() {
        0
    } else {
        text.split('\n').count()
    };
    (bytes, lines)
}

/// `sanitizeContent` (mcp-output-guard.ts:157-165): image mimeType
/// normalization (trim, 100-char cap, default image/png).
fn sanitize_content(content: Vec<Value>) -> Vec<Value> {
    content
        .into_iter()
        .map(|mut block| {
            if block.get("type").and_then(Value::as_str) == Some("image") {
                let mime = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(|m| m.chars().take(100).collect::<String>())
                    .unwrap_or_else(|| "image/png".to_string());
                block["mimeType"] = json!(mime);
            }
            block
        })
        .collect()
}

/// `withEmptyTextFallback` (mcp-output-guard.ts:167-175).
fn with_empty_text_fallback(content: Vec<Value>, fallback: Option<&str>) -> Vec<Value> {
    let Some(fallback) = fallback else {
        return content;
    };
    let text_output: String = content
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if !text_output.is_empty() {
        return content;
    }
    let mut out = vec![json!({ "type": "text", "text": fallback })];
    out.extend(
        content
            .into_iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("image")),
    );
    out
}

/// `addAffixes` (mcp-output-guard.ts:177-208).
fn add_affixes(content: Vec<Value>, prefix: &str, suffix: &str) -> Vec<Value> {
    if prefix.is_empty() && suffix.is_empty() {
        return content;
    }
    let mut next = content;
    if !prefix.is_empty() {
        match next
            .iter()
            .position(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        {
            Some(index) => {
                let text = next[index]
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                next[index]["text"] = json!(format!("{prefix}{text}"));
            }
            None => next.insert(0, json!({ "type": "text", "text": prefix })),
        }
    }
    if !suffix.is_empty() {
        match next
            .iter()
            .rposition(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        {
            Some(index) => {
                let text = next[index]
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                next[index]["text"] = json!(format!("{text}{suffix}"));
            }
            None => next.push(json!({ "type": "text", "text": suffix })),
        }
    }
    next
}

/// `truncateStringToBytes` (mcp-output-guard.ts:243-249): byte cap, backing
/// off UTF-8 continuation bytes.
fn truncate_string_to_bytes(value: &str, max_bytes: usize) -> String {
    if byte_length(value) <= max_bytes {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut end = max_bytes;
    while end > 0 && (bytes[end] & 0xc0) == 0x80 {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// #500 (4444b49, mcp-output-guard.ts @ Unreleased): the adapter's own
// byte-loop `truncateHead` is REPLACED by the host truncation semantics
// (`crate::truncate::truncate_head`, itself a verbatim port of the host
// `crates/rpi/src/tools/truncate.rs`) — never partial lines, first-line
// classification, and the host `formatSize` spelling (B/KB/MB, not the
// adapter's old " B"/" KiB").
use crate::truncate::{format_size, truncate_head, TruncateOptions, TruncationResult};

/// `Number.prototype.toLocaleString()` for the integers the notice prints
/// (en-US grouping).
fn to_locale_string(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `formatTruncationNotice` (#500, mcp-output-guard.ts:259-273 @ 4444b49):
/// the classification reason line + the host size format.
fn format_truncation_notice(
    truncation: &TruncationResult,
    full_output_path: Option<&str>,
    write_error: Option<&str>,
) -> String {
    let reason = if truncation.first_line_exceeds_limit {
        format!(
            "First line exceeds {} limit",
            format_size(truncation.max_bytes)
        )
    } else if truncation.truncated_by == Some(crate::truncate::TruncatedBy::Lines) {
        format!(
            "Truncated: showing {} of {} lines ({} line limit)",
            truncation.output_lines, truncation.total_lines, truncation.max_lines
        )
    } else {
        format!(
            "Truncated: {} lines shown ({} limit)",
            truncation.output_lines,
            format_size(truncation.max_bytes)
        )
    };
    let base = format!(
        "[MCP text output truncated: original {} lines / {}. {}.",
        to_locale_string(truncation.total_lines),
        format_size(truncation.total_bytes),
        reason
    );
    match full_output_path {
        Some(path) => format!(
            "{base} Full text saved to: {path} — use read with offset/limit or grep to inspect.]"
        ),
        None => format!(
            "{base} Full output could not be saved: {}]",
            write_error.unwrap_or("unknown error")
        ),
    }
}

/// `saveArtifact` (mcp-output-guard.ts:358-367): fresh `rpi-mcp-output-*`
/// temp dir per artifact (upstream `pi-mcp-output-`, ADR-0001 rename), file
/// created with mode 0600 / dir 0700 in one step (no post-create chmod
/// window; upstream relies on `fs.mkdtemp` 0700).
fn save_artifact(kind: &str, text: &str) -> (Option<String>, Option<String>) {
    let base = std::env::temp_dir().join(format!(
        "rpi-mcp-output-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&base)
            // Collision with an in-flight artifact dir: reuse it without
            // force-chmodding (the random name makes this near-impossible).
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(e)
                }
            })
    };
    #[cfg(not(unix))]
    let created = std::fs::create_dir(&base);
    if let Err(error) = created {
        return (None, Some(error.to_string()));
    }
    let path = base.join(format!("{kind}-{:08x}.txt", rand_u32()));
    #[cfg(unix)]
    let result = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(text.as_bytes())
            })
    };
    #[cfg(not(unix))]
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(text.as_bytes())
        });
    match result {
        Ok(()) => (Some(path.to_string_lossy().into_owned()), None),
        Err(error) => (None, Some(error.to_string())),
    }
}

/// 32-bit PRNG for spill file names (no crypto need; avoids a `rand` dep).
fn rand_u32() -> u32 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let tick = COUNTER.fetch_add(1, Ordering::SeqCst);
    // xorshift mix of time + counter + pid
    let mut x = nanos ^ tick.wrapping_mul(0x9E37_79B9) ^ u64::from(std::process::id());
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x as u32
}

fn as_record(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn safe_stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// `truncateKey` (mcp-output-guard.ts:412-416 @ 10a45367): byte cap with
/// the `…` suffix budgeted in.
fn truncate_key(key: &str) -> String {
    if byte_length(key) <= KEY_MAX_BYTES {
        return key.to_string();
    }
    let suffix = "…";
    format!(
        "{}{suffix}",
        truncate_string_to_bytes(key, KEY_MAX_BYTES - byte_length(suffix))
    )
}

/// `uniqueBoundedKeys` (mcp-output-guard.ts:418-430 @ 10a45367): truncate
/// each key, then disambiguate collisions with `~N` ordinals (the suffix
/// budget shrinks the truncated prefix).
fn unique_bounded_keys(keys: Vec<String>) -> Vec<String> {
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    keys.into_iter()
        .map(|key| {
            let mut candidate = truncate_key(&key);
            let mut ordinal = 2;
            while used.contains(&candidate) {
                let suffix = format!("~{ordinal}");
                candidate = format!(
                    "{}{suffix}",
                    truncate_string_to_bytes(
                        &key,
                        KEY_MAX_BYTES.saturating_sub(byte_length(&suffix))
                    )
                );
                ordinal += 1;
            }
            used.insert(candidate.clone());
            candidate
        })
        .collect()
}

/// `serializedObjectEntryBytes` (mcp-output-guard.ts:432-434): the byte
/// cost of `{key: value}` minus the empty-object base, +1 separator when
/// not first.
fn serialized_object_entry_bytes(key: &str, value: &Value, has_previous: bool) -> usize {
    let serialized = safe_stringify(&json!({ key: value }));
    serialized.len().saturating_sub(byte_length("{}")) + usize::from(has_previous)
}

/// `summarizeStructuredContent` (mcp-output-guard.ts:373-399 @ 10a45367,
/// #430): preserve small fields verbatim (≤512 B each, ≤4 KiB total),
/// summarize the rest; previews carry unique bounded keys.
fn summarize_structured_content(value: &Value) -> Value {
    let Some(record) = value.as_object() else {
        return summarize_value(value);
    };
    let key_count = record.len();
    let entries: Vec<(&String, &Value)> = record.iter().take(KEY_PREVIEW_LIMIT).collect();
    let preview_keys = unique_bounded_keys(
        entries
            .iter()
            .map(|(key, _)| (*key).clone())
            .collect::<Vec<String>>(),
    );
    let mut fields = serde_json::Map::new();
    let mut preserved_bytes = byte_length("{}");
    for (key, field) in &entries {
        let field_bytes = byte_length(&safe_stringify(field));
        let candidate = if field_bytes <= STRUCTURED_CONTENT_FIELD_PRESERVE_MAX_BYTES {
            (*field).clone()
        } else {
            summarize_value(field)
        };
        let entry_bytes = serialized_object_entry_bytes(key, &candidate, !fields.is_empty());
        if preserved_bytes + entry_bytes > STRUCTURED_CONTENT_PRESERVE_MAX_BYTES {
            continue;
        }
        fields.insert((*key).clone(), candidate);
        preserved_bytes += entry_bytes;
    }
    json!({
        "preservedFields": Value::Object(fields),
        "summary": {
            "type": "object",
            "estimatedBytes": estimate_value_bytes(value, 0),
            "keyCount": key_count,
            "keysPreview": preview_keys,
            "omitted": true,
        },
    })
}

fn estimate_value_bytes(value: &Value, depth: usize) -> usize {
    match value {
        Value::Null => 0,
        Value::String(s) => byte_length(s),
        Value::Number(n) => n.to_string().len(),
        Value::Bool(b) => b.to_string().len(),
        Value::Array(items) if depth < 2 => items
            .iter()
            .take(KEY_PREVIEW_LIMIT)
            .map(|item| estimate_value_bytes(item, depth + 1))
            .sum(),
        Value::Object(map) if depth < 2 => map
            .values()
            .take(KEY_PREVIEW_LIMIT)
            .map(|item| estimate_value_bytes(item, depth + 1))
            .sum(),
        _ => 0,
    }
}

fn summarize_value(value: &Value) -> Value {
    // JS `typeof [] === "object"`: arrays enter the record branch upstream
    // (Object.keys = index strings).
    if let Value::Array(items) = value {
        let keys: Vec<String> = (0..items.len()).map(|i| i.to_string()).collect();
        return json!({
            "type": "array",
            "estimatedBytes": estimate_value_bytes(value, 0),
            "keyCount": keys.len(),
            "keysPreview": unique_bounded_keys(keys.into_iter().take(KEY_PREVIEW_LIMIT).collect()),
            "omitted": true,
        });
    }
    let Some(record) = as_record(value) else {
        return json!({
            "type": value_type_name(value),
            "estimatedBytes": estimate_value_bytes(value, 0),
            "omitted": true,
        });
    };
    let keys: Vec<&String> = record.keys().collect();
    json!({
        "type": if value.is_array() { "array" } else { "object" },
        "estimatedBytes": estimate_value_bytes(value, 0),
        "keyCount": keys.len(),
        "keysPreview": unique_bounded_keys(keys.into_iter().take(KEY_PREVIEW_LIMIT).cloned().collect()),
        "omitted": true,
    })
}

/// `summarizeContent` (mcp-output-guard.ts:309-327).
fn summarize_content(content: &[Value]) -> Value {
    let mut summaries: Vec<Value> = content
        .iter()
        .take(CONTENT_SUMMARY_LIMIT)
        .map(|block| {
            let Some(record) = as_record(block) else {
                return json!({ "type": value_type_name(block), "omitted": true });
            };
            match record.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = record.get("text").and_then(Value::as_str).unwrap_or("");
                    json!({
                        "type": "text",
                        "bytes": byte_length(text),
                        "lines": text_stats(text).1,
                        "textOmitted": true,
                    })
                }
                Some("image") => {
                    let data = record.get("data").and_then(Value::as_str).unwrap_or("");
                    // Upstream: `typeof record.mimeType === "string" ?
                    // record.mimeType : undefined` — undefined keys are
                    // omitted by JSON.stringify, so we conditionally insert.
                    let mut entry = serde_json::Map::new();
                    entry.insert("type".to_string(), json!("image"));
                    if let Some(mt) = record.get("mimeType").and_then(Value::as_str) {
                        entry.insert("mimeType".to_string(), json!(mt));
                    }
                    entry.insert("dataBytes".to_string(), json!(byte_length(data)));
                    entry.insert("dataOmitted".to_string(), json!(true));
                    Value::Object(entry)
                }
                other => json!({
                    "type": other.unwrap_or("unknown"),
                    "estimatedBytes": estimate_value_bytes(block, 0),
                    "omitted": true,
                }),
            }
        })
        .collect();
    if content.len() > CONTENT_SUMMARY_LIMIT {
        summaries.push(json!({
            "type": "omitted",
            "count": content.len() - CONTENT_SUMMARY_LIMIT,
        }));
    }
    Value::Array(summaries)
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `summarizeMcpResult` (mcp-output-guard.ts:275-307).
fn summarize_mcp_result(result: &Value, raw: &str, raw_bytes: usize) -> Value {
    let (full_result_path, result_write_error) = save_artifact("mcp-result", raw);
    let record = as_record(result);
    let content: &[Value] = record
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut summary = json!({
        "omitted": true,
        "reason": "Raw MCP result exceeded the details size limit and was replaced with this summary to keep session context bounded.",
        "isError": record.and_then(|r| r.get("isError")).and_then(Value::as_bool) == Some(true),
        "contentBlocks": content.len(),
        "contentSummary": summarize_content(content),
        "rawResultBytes": raw_bytes,
    });
    if let Some(path) = full_result_path {
        summary["fullResultPath"] = json!(path);
    }
    if let Some(error) = result_write_error {
        summary["resultWriteError"] = json!(error);
    }
    if let Some(record) = record {
        if record.contains_key("structuredContent") {
            // #430: bounded structured preservation instead of a plain
            // omission summary.
            summary["structuredContent"] =
                summarize_structured_content(&record["structuredContent"]);
        }
        if record.contains_key("_meta") {
            summary["meta"] = summarize_value(&record["_meta"]);
        }
        let standard: [&str; 4] = ["content", "isError", "structuredContent", "_meta"];
        let extra_fields: Vec<Value> = record
            .keys()
            .filter(|key| !standard.contains(&key.as_str()))
            .take(KEY_PREVIEW_LIMIT)
            .map(|key| {
                json!({
                    "key": truncate_key(key),
                    "type": value_type_name(&record[key]),
                    "estimatedBytes": estimate_value_bytes(&record[key], 0),
                    "omitted": true,
                })
            })
            .collect();
        if !extra_fields.is_empty() {
            summary["extraFields"] = Value::Array(extra_fields);
        }
    }
    summary
}

/// `boundMcpResult` (mcp-output-guard.ts:268-273).
fn bound_mcp_result(result: &Value, details_max_bytes: usize) -> Value {
    let raw = safe_stringify(result);
    if byte_length(&raw) <= details_max_bytes {
        return result.clone();
    }
    summarize_mcp_result(result, &raw, byte_length(&raw))
}

fn reserve_budget(max_bytes: usize, max_lines: usize, notice: &str) -> (usize, usize) {
    let (notice_bytes, notice_lines) = text_stats(&format!("\n\n{notice}"));
    (
        max_bytes.saturating_sub(notice_bytes),
        max_lines.saturating_sub(notice_lines),
    )
}

/// `guardMcpOutput` (mcp-output-guard.ts:90-155). Synchronous here — the
/// only async work upstream is temp-file I/O, which `std::fs` covers.
pub fn guard_mcp_output(content: Vec<Value>, options: &GuardOptions) -> GuardedOutput {
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MCP_OUTPUT_MAX_BYTES);
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MCP_OUTPUT_MAX_LINES);
    let details_max_bytes = options
        .details_max_bytes
        .unwrap_or(DEFAULT_MCP_DETAILS_MAX_BYTES);
    let prefix = options.prefix.as_deref().unwrap_or("");
    let suffix = options.suffix.as_deref().unwrap_or("");

    let normalized = with_empty_text_fallback(
        if content.is_empty() {
            vec![json!({
                "type": "text",
                "text": options.empty_text_fallback.as_deref().unwrap_or("(empty result)"),
            })]
        } else {
            sanitize_content(content)
        },
        options.empty_text_fallback.as_deref(),
    );

    if options.enabled == Some(false) {
        return GuardedOutput {
            content: add_affixes(normalized, prefix, suffix),
            output_guard: None,
            mcp_result: options.raw_mcp_result.clone(),
        };
    }

    let image_blocks: Vec<Value> = normalized
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("image"))
        .cloned()
        .collect();
    let text_output: String = normalized
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let composed_output = format!("{prefix}{text_output}{suffix}");
    // #500: the host truncation classifies (never partial lines; a
    // first-line overflow empties the preview with
    // `firstLineExceedsLimit`).
    let truncation = truncate_head(
        &composed_output,
        Some(TruncateOptions {
            max_bytes,
            max_lines,
        }),
    );

    let mut guarded_content = add_affixes(normalized, prefix, suffix);
    let mut output_guard: Option<Value> = None;

    if truncation.truncated {
        let (full_output_path, write_error) = save_artifact("output", &composed_output);
        let initial_notice = format_truncation_notice(
            &truncation,
            full_output_path.as_deref(),
            write_error.as_deref(),
        );
        let (budget_bytes, budget_lines) = reserve_budget(max_bytes, max_lines, &initial_notice);
        let preview = truncate_head(
            &composed_output,
            Some(TruncateOptions {
                max_bytes: budget_bytes,
                max_lines: budget_lines,
            }),
        );
        // The FINAL notice reports the delivered preview's counts (upstream
        // recomposes with `{...truncation, outputLines: preview.outputLines,
        // outputBytes: preview.outputBytes}`).
        let mut notice_source = truncation.clone();
        notice_source.output_lines = preview.output_lines;
        notice_source.output_bytes = preview.output_bytes;
        let notice = format_truncation_notice(
            &notice_source,
            full_output_path.as_deref(),
            write_error.as_deref(),
        );
        let final_text = format!("{}\n\n{notice}", preview.content);
        let final_stats = text_stats(&final_text);

        guarded_content = vec![json!({ "type": "text", "text": final_text })];
        guarded_content.extend(image_blocks.iter().cloned());
        let mut guard_details = json!({
            "truncated": true,
            "originalBytes": truncation.total_bytes,
            "returnedBytes": final_stats.0,
            "originalLines": truncation.total_lines,
            "returnedLines": final_stats.1,
            // #500 host-classification fields.
            "truncatedBy": match truncation.truncated_by {
                Some(crate::truncate::TruncatedBy::Lines) => "lines",
                Some(crate::truncate::TruncatedBy::Bytes) => "bytes",
                None => Value::Null.as_str().unwrap_or("bytes"),
            },
            "totalLines": truncation.total_lines,
            "totalBytes": truncation.total_bytes,
            "outputLines": preview.output_lines,
            "outputBytes": preview.output_bytes,
            "lastLinePartial": truncation.last_line_partial,
            "firstLineExceedsLimit": truncation.first_line_exceeds_limit,
            "maxLines": truncation.max_lines,
            "maxBytes": truncation.max_bytes,
        });
        if !image_blocks.is_empty() {
            guard_details["imageBlocksPassedThrough"] = json!(image_blocks.len());
        }
        if let Some(path) = full_output_path {
            guard_details["fullOutputPath"] = json!(path);
        }
        if let Some(error) = write_error {
            guard_details["writeError"] = json!(error);
        }
        output_guard = Some(guard_details);
    }

    let mcp_result = options
        .raw_mcp_result
        .as_ref()
        .map(|raw| bound_mcp_result(raw, details_max_bytes));

    GuardedOutput {
        content: guarded_content,
        output_guard,
        mcp_result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(text: &str) -> Value {
        json!({ "type": "text", "text": text })
    }

    #[test]
    fn under_threshold_passes_through() {
        let guarded = guard_mcp_output(vec![text_block("hello")], &GuardOptions::default());
        assert_eq!(guarded.content, vec![text_block("hello")]);
        assert!(guarded.output_guard.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn spill_artifact_dir_and_file_modes() {
        let (Some(path), None) = save_artifact("probe", "secret") else {
            panic!("save_artifact failed");
        };
        use std::os::unix::fs::PermissionsExt;
        let dir = std::path::Path::new(&path).parent().expect("parent dir");
        let file_mode = std::fs::metadata(&path)
            .expect("file metadata")
            .permissions()
            .mode();
        let dir_mode = std::fs::metadata(dir)
            .expect("dir metadata")
            .permissions()
            .mode();
        assert_eq!(
            file_mode & 0o777,
            0o600,
            "spill file must be 0600 at creation"
        );
        assert_eq!(
            dir_mode & 0o777,
            0o700,
            "spill dir must be 0700 at creation"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn byte_threshold_boundary() {
        let exactly = "x".repeat(DEFAULT_MCP_OUTPUT_MAX_BYTES);
        let guarded = guard_mcp_output(vec![text_block(&exactly)], &GuardOptions::default());
        assert!(guarded.output_guard.is_none());

        let over = "x".repeat(DEFAULT_MCP_OUTPUT_MAX_BYTES + 1);
        let guarded = guard_mcp_output(vec![text_block(&over)], &GuardOptions::default());
        let guard = guarded.output_guard.expect("guard details");
        assert_eq!(guard["truncated"], json!(true));
        assert_eq!(
            guard["originalBytes"],
            json!(DEFAULT_MCP_OUTPUT_MAX_BYTES + 1)
        );
        let path = guard["fullOutputPath"].as_str().expect("spill path");
        assert_eq!(std::fs::read_to_string(path).expect("spill"), over);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "spill file must be 0600");
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn line_threshold_boundary() {
        let exactly = vec!["line"; DEFAULT_MCP_OUTPUT_MAX_LINES].join("\n");
        let guarded = guard_mcp_output(vec![text_block(&exactly)], &GuardOptions::default());
        assert!(guarded.output_guard.is_none());

        let over = vec!["line"; DEFAULT_MCP_OUTPUT_MAX_LINES + 1].join("\n");
        let guarded = guard_mcp_output(vec![text_block(&over)], &GuardOptions::default());
        let guard = guarded.output_guard.expect("guard details");
        assert_eq!(
            guard["originalLines"],
            json!(DEFAULT_MCP_OUTPUT_MAX_LINES + 1)
        );
        let text = guarded.content[0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("truncated: original 2,001 lines"));
        let _ = std::fs::remove_file(guard["fullOutputPath"].as_str().unwrap_or_default());
    }

    #[test]
    fn image_blocks_pass_through_uncounted() {
        let over = "x".repeat(DEFAULT_MCP_OUTPUT_MAX_BYTES + 1);
        let image = json!({ "type": "image", "data": "Zm9v", "mimeType": " image/png " });
        let guarded = guard_mcp_output(vec![text_block(&over), image], &GuardOptions::default());
        let guard = guarded.output_guard.expect("guard details");
        assert_eq!(guard["imageBlocksPassedThrough"], json!(1));
        // mimeType sanitized (trimmed); image block present after the text.
        assert_eq!(guarded.content.len(), 2);
        assert_eq!(guarded.content[1]["mimeType"], json!("image/png"));
        let _ = std::fs::remove_file(guard["fullOutputPath"].as_str().unwrap_or_default());
    }

    #[test]
    fn details_result_bounded_at_16kib() {
        let big = json!({ "content": [{ "type": "text", "text": "x".repeat(DEFAULT_MCP_DETAILS_MAX_BYTES + 1) }] });
        let guarded = guard_mcp_output(
            vec![text_block("ok")],
            &GuardOptions {
                raw_mcp_result: Some(big),
                ..Default::default()
            },
        );
        let summary = guarded.mcp_result.expect("mcp result");
        assert_eq!(summary["omitted"], json!(true));
        assert!(
            summary["rawResultBytes"].as_u64().unwrap_or(0) > DEFAULT_MCP_DETAILS_MAX_BYTES as u64
        );
        let path = summary["fullResultPath"].as_str().expect("spill path");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn disabled_guard_keeps_affixes_and_raw_result() {
        let big = json!({ "content": [{ "type": "text", "text": "x".repeat(DEFAULT_MCP_DETAILS_MAX_BYTES + 1) }] });
        let guarded = guard_mcp_output(
            vec![text_block("body")],
            &GuardOptions {
                enabled: Some(false),
                prefix: Some("Error: ".to_string()),
                suffix: Some("\ntail".to_string()),
                raw_mcp_result: Some(big.clone()),
                ..Default::default()
            },
        );
        assert_eq!(guarded.content[0]["text"], json!("Error: body\ntail"));
        assert_eq!(guarded.mcp_result, Some(big));
        assert!(guarded.output_guard.is_none());
    }

    #[test]
    fn empty_content_uses_fallback() {
        let guarded = guard_mcp_output(Vec::new(), &GuardOptions::default());
        assert_eq!(guarded.content, vec![text_block("(empty result)")]);
        let guarded = guard_mcp_output(
            Vec::new(),
            &GuardOptions {
                empty_text_fallback: Some("Tool execution failed".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(guarded.content, vec![text_block("Tool execution failed")]);
    }

    #[test]
    fn kill_switch_and_settings_resolution() {
        std::env::set_var("MCP_OUTPUT_GUARD", "0");
        let options = resolve_guard_options(None);
        assert_eq!(options.enabled, Some(false));
        std::env::set_var("MCP_OUTPUT_GUARD", "1");
        let options = resolve_guard_options(None);
        assert_eq!(options.enabled, Some(true));
        std::env::remove_var("MCP_OUTPUT_GUARD");

        let settings =
            json!({ "outputGuard": { "maxBytes": 100, "maxLines": 5, "detailsMaxBytes": 64 } });
        let options = resolve_guard_options(settings.as_object());
        assert_eq!(options.max_bytes, Some(100));
        assert_eq!(options.max_lines, Some(5));
        assert_eq!(options.details_max_bytes, Some(64));

        let settings = json!({ "outputGuard": false });
        let options = resolve_guard_options(settings.as_object());
        assert_eq!(options.enabled, Some(false));
    }

    #[test]
    fn locale_grouping_in_notice() {
        // TE24 (#500): the notice uses the HOST formatSize spelling
        // (G2: 旧期望 "512 B"/"50.0 KiB" → 新期望 "512B"/"50.0KB",
        // 依据 4444b49 reuse host truncation semantics)。
        assert_eq!(to_locale_string(2001), "2,001");
        assert_eq!(to_locale_string(999), "999");
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(50 * 1024), "50.0KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
    }
    #[test]
    fn truncation_notice_carries_host_reason_line() {
        // #500 (4444b49): the notice reports the host classification.
        let line_limit = TruncationResult {
            content: "a".into(),
            truncated: true,
            truncated_by: Some(crate::truncate::TruncatedBy::Lines),
            total_lines: 30,
            total_bytes: 300,
            output_lines: 7,
            output_bytes: 60,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines: 10,
            max_bytes: 10_000,
        };
        assert!(
            format_truncation_notice(&line_limit, Some("/tmp/x"), None)
                .contains("Truncated: showing 7 of 30 lines (10 line limit)"),
            "{:?}",
            format_truncation_notice(&line_limit, Some("/tmp/x"), None)
        );
        let byte_limit = TruncationResult {
            truncated_by: Some(crate::truncate::TruncatedBy::Bytes),
            max_bytes: 1500,
            ..line_limit.clone()
        };
        assert!(
            format_truncation_notice(&byte_limit, None, Some("disk full"))
                .contains("Truncated: 7 lines shown (1.5KB limit)")
        );
        let first_line = TruncationResult {
            content: String::new(),
            output_lines: 0,
            output_bytes: 0,
            first_line_exceeds_limit: true,
            max_bytes: 5,
            ..byte_limit.clone()
        };
        assert!(format_truncation_notice(&first_line, None, None)
            .contains("First line exceeds 5B limit"));
    }

    #[test]
    fn guard_first_line_overflow_leaves_no_partial_line() {
        // #500 test-vector port: the first line exceeding the byte cap
        // delivers an EMPTY preview with the dedicated notice.
        let options = GuardOptions {
            max_bytes: Some(5),
            max_lines: Some(10),
            ..GuardOptions::default()
        };
        let guarded = guard_mcp_output(
            vec![json!({ "type": "text", "text": format!("{}\nsmall", "x".repeat(10)) })],
            &options,
        );
        let guard = guarded.output_guard.expect("guard details");
        assert_eq!(guard["truncated"], json!(true));
        assert_eq!(guard["truncatedBy"], json!("bytes"));
        assert_eq!(guard["firstLineExceedsLimit"], json!(true));
        assert_eq!(guard["outputLines"], json!(0));
        assert_eq!(guard["outputBytes"], json!(0));
        let text = guarded.content[0]["text"].as_str().expect("text");
        assert!(!text.contains("xxxxxxxxxx"), "no partial first line");
        assert!(text.contains("First line exceeds 5B limit"));
    }

    #[test]
    fn guard_line_truncation_reports_delivered_preview_counts() {
        // #500: "Truncated: showing 7 of 30 lines (10 line limit)" — the
        // notice carries the DELIVERED preview line count (7 = 10 budget
        // minus the notice's own lines).
        let text = (0..30)
            .map(|i| format!("entry-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let options = GuardOptions {
            max_bytes: Some(10_000),
            max_lines: Some(10),
            ..GuardOptions::default()
        };
        let guarded = guard_mcp_output(vec![json!({ "type": "text", "text": text })], &options);
        let guard = guarded.output_guard.expect("guard details");
        assert_eq!(guard["truncatedBy"], json!("lines"));
        assert_eq!(guard["outputLines"], json!(7));
        let text = guarded.content[0]["text"].as_str().expect("text");
        assert!(text.contains("Truncated: showing 7 of 30 lines (10 line limit)"));
        let _ = std::fs::remove_file(guard["fullOutputPath"].as_str().unwrap_or_default());
    }

    #[test]
    fn structured_content_preserves_bounded_fields() {
        // #430 (summarizeStructuredContent, mcp-output-guard.ts:373-399):
        // small fields stay verbatim inside preservedFields; oversized
        // fields are summarized; the whole object keeps a 4 KiB budget.
        let small = json!({ "rows": [1, 2, 3] });
        let summarized = summarize_structured_content(&small);
        assert_eq!(
            summarized["preservedFields"]["rows"],
            json!([1, 2, 3]),
            "small field preserved verbatim"
        );
        assert_eq!(summarized["summary"]["omitted"], json!(true));

        // An oversized field (>512 B) is summarized inside preservedFields.
        let big_field = json!({ "blob": "x".repeat(600) });
        let summarized = summarize_structured_content(&big_field);
        assert!(
            summarized["preservedFields"]["blob"].get("omitted") == Some(&json!(true)),
            "oversized field summarized: {}",
            summarized["preservedFields"]["blob"]
        );

        // The 4 KiB total budget: many 100-B fields stop being preserved.
        let mut many = serde_json::Map::new();
        for i in 0..80 {
            many.insert(format!("k{i:02}"), json!(format!("{}", "v".repeat(90))));
        }
        let summarized = summarize_structured_content(&Value::Object(many));
        let preserved = summarized["preservedFields"].as_object().expect("object");
        assert!(preserved.len() < 80, "4 KiB budget caps preservation");
    }

    #[test]
    fn unique_bounded_keys_disambiguate_collisions() {
        // uniqueBoundedKeys (mcp-output-guard.ts:418-430): two distinct
        // long keys truncating to the same prefix get ~2/~3 suffixes.
        let key_a = format!("a{}z", "m".repeat(200));
        let key_b = format!("a{}y", "m".repeat(200));
        let keys = unique_bounded_keys(vec![key_a.clone(), key_b.clone(), key_a.clone() + "2"]);
        assert_eq!(keys.len(), 3);
        assert_ne!(keys[0], keys[1], "colliding prefixes disambiguated");
        assert!(keys.iter().all(|k| byte_length(k) <= KEY_MAX_BYTES));
        // Exact duplicates also get ordinals.
        let dupes = unique_bounded_keys(vec!["same".to_string(), "same".to_string()]);
        assert_eq!(dupes, vec!["same".to_string(), "same~2".to_string()]);
    }
}
