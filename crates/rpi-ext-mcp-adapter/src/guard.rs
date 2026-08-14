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
const KEY_MAX_CHARS: usize = 120;
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

/// `truncateHead` (mcp-output-guard.ts:218-241).
fn truncate_head(text: &str, max_bytes: usize, max_lines: usize) -> String {
    let mut output: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for line in text.split('\n') {
        if output.len() >= max_lines {
            break;
        }
        let separator_bytes = usize::from(!output.is_empty());
        let line_bytes = byte_length(line);
        if bytes + separator_bytes + line_bytes > max_bytes {
            let remaining = max_bytes.saturating_sub(bytes + separator_bytes);
            if remaining > 0 {
                output.push(truncate_string_to_bytes(line, remaining));
            }
            break;
        }
        output.push(line.to_string());
        bytes += separator_bytes + line_bytes;
    }
    output.join("\n")
}

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

/// `formatSize` (mcp-output-guard.ts:404-408).
fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// `formatTruncationNotice` (mcp-output-guard.ts:251-261).
fn format_truncation_notice(
    stats: (usize, usize),
    full_output_path: Option<&str>,
    write_error: Option<&str>,
) -> String {
    let base = format!(
        "[MCP text output truncated: original {} lines / {}.",
        to_locale_string(stats.1),
        format_size(stats.0)
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

fn truncate_key(key: &str) -> String {
    if key.chars().count() <= KEY_MAX_CHARS {
        key.to_string()
    } else {
        format!(
            "{}…",
            key.chars().take(KEY_MAX_CHARS - 1).collect::<String>()
        )
    }
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
            "keysPreview": keys.iter().take(KEY_PREVIEW_LIMIT).map(|k| truncate_key(k)).collect::<Vec<_>>(),
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
        "keysPreview": keys.iter().take(KEY_PREVIEW_LIMIT).map(|k| truncate_key(k)).collect::<Vec<_>>(),
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
            summary["structuredContent"] = summarize_value(&record["structuredContent"]);
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
    let stats = text_stats(&composed_output);

    let mut guarded_content = add_affixes(normalized, prefix, suffix);
    let mut output_guard: Option<Value> = None;

    if stats.0 > max_bytes || stats.1 > max_lines {
        let (full_output_path, write_error) = save_artifact("output", &composed_output);
        let notice =
            format_truncation_notice(stats, full_output_path.as_deref(), write_error.as_deref());
        let (budget_bytes, budget_lines) = reserve_budget(max_bytes, max_lines, &notice);
        let preview = truncate_head(&composed_output, budget_bytes, budget_lines);
        let final_text = format!("{preview}\n\n{notice}");
        let final_stats = text_stats(&final_text);

        guarded_content = vec![json!({ "type": "text", "text": final_text })];
        guarded_content.extend(image_blocks.iter().cloned());
        let mut guard_details = json!({
            "truncated": true,
            "originalBytes": stats.0,
            "returnedBytes": final_stats.0,
            "originalLines": stats.1,
            "returnedLines": final_stats.1,
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
        assert_eq!(to_locale_string(2001), "2,001");
        assert_eq!(to_locale_string(999), "999");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(50 * 1024), "50.0 KiB");
    }
}
