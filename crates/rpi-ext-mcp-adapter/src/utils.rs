//! Environment interpolation, path/URL resolution and JS-compatible JSON
//! scalar serialization helpers.
//!
//! Port of the pure subset of `utils.ts` @ pi-mcp-adapter v2.24.0 (3d953f90):
//! `interpolateEnvVars` / `getMissingEnvVars` / `interpolateEnvRecord` /
//! `resolveServerUrl` / `resolveConfigPath` / `resolveBearerToken` /
//! `truncateAtWord`.
//!
//! Intentional differences:
//! - `resolveCommandSecret` (`!command` execution) is NOT part of this wave;
//!   it lands with the transport wave that actually spawns processes.
//! - Home directory resolution is `HOME` (unix) / `USERPROFILE` (windows)
//!   only; no passwd-entry fallback (rpi's own `config.rs` does the libc
//!   fallback, but the plugin stays dependency-minimal).

use serde_json::Value;

use crate::error::AdapterError;

/// The three interpolation marker forms (upstream regexes `\$\{(\w+)\}`,
/// `\$env:(\w+)`, `\{env:(\w+)\}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterpolationForm {
    /// `${VAR}`
    DollarBrace,
    /// `$env:VAR`
    DollarEnv,
    /// `{env:VAR}`
    BraceEnv,
}

const FORMS: [InterpolationForm; 3] = [
    InterpolationForm::DollarBrace,
    InterpolationForm::DollarEnv,
    InterpolationForm::BraceEnv,
];

impl InterpolationForm {
    fn markers(self) -> (&'static str, Option<char>) {
        match self {
            InterpolationForm::DollarBrace => ("${", Some('}')),
            InterpolationForm::DollarEnv => ("$env:", None),
            InterpolationForm::BraceEnv => ("{env:", Some('}')),
        }
    }
}

/// Scan for one marker form; yields `(start, name, end)` spans of every
/// valid occurrence (JS `\w+` names = ASCII `[A-Za-z0-9_]`).
fn find_markers(input: &str, form: InterpolationForm) -> Vec<(usize, String, usize)> {
    let (open, close) = form.markers();
    let mut spans = Vec::new();
    let mut rest = input;
    let mut base = 0;
    while let Some(start) = rest.find(open) {
        let after = &rest[start + open.len()..];
        let name_len = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .map(char::len_utf8)
            .sum::<usize>();
        let name = &after[..name_len];
        let has_close = match close {
            Some(c) => after[name_len..].starts_with(c),
            None => true,
        };
        if !name.is_empty() && has_close {
            let close_len = close.map_or(0, |c| c.len_utf8());
            let abs_start = base + start;
            let abs_end = abs_start + open.len() + name_len + close_len;
            spans.push((abs_start, name.to_string(), abs_end));
            base = abs_end;
            rest = &input[base..];
        } else {
            base += start + open.len();
            rest = after;
        }
    }
    spans
}

/// `interpolateEnvVars` (utils.ts:74-80): three sequential replacement
/// passes — `${VAR}`, then `$env:VAR`, then `{env:VAR}`; unset variables
/// expand to the empty string (`process.env[name] ?? ""`).
pub fn interpolate_env_vars(value: &str) -> String {
    let mut current = value.to_string();
    for form in FORMS {
        let spans = find_markers(&current, form);
        if spans.is_empty() {
            continue;
        }
        let mut out = String::with_capacity(current.len());
        let mut cursor = 0;
        for (start, name, end) in spans {
            out.push_str(&current[cursor..start]);
            out.push_str(std::env::var(&name).as_deref().unwrap_or(""));
            cursor = end;
        }
        out.push_str(&current[cursor..]);
        current = out;
    }
    current
}

/// `getMissingEnvVars` (utils.ts:83-92): names referenced by any of the
/// three forms that are unset, deduplicated.
pub fn get_missing_env_vars(value: &str) -> Vec<String> {
    let mut missing: Vec<String> = Vec::new();
    for form in FORMS {
        for (_, name, _) in find_markers(value, form) {
            if std::env::var_os(&name).is_none() && !missing.contains(&name) {
                missing.push(name);
            }
        }
    }
    missing
}

/// `interpolateSecretExpression` (utils.ts:102-105): `!!` escapes a literal
/// leading `!` (strip one, interpolate the rest), a single leading `!` is a
/// command marker left untouched for `resolveCommandSecret`, anything else
/// is interpolated.
pub fn interpolate_secret_expression(value: &str) -> String {
    if value.starts_with("!!") {
        // `value.slice(1)`: strip ONE bang, leaving a literal `!` prefix.
        return interpolate_env_vars(&value[1..]);
    }
    if value.starts_with('!') {
        return value.to_string();
    }
    interpolate_env_vars(value)
}

/// `interpolateEnvRecord` (utils.ts:107-114).
///
/// The input must be a JSON object with string values; anything else mirrors
/// the upstream TypeError (callers treat it as a hash failure).
pub fn interpolate_env_record(
    values: Option<&Value>,
) -> Result<Option<serde_json::Map<String, Value>>, AdapterError> {
    let Some(values) = values else {
        return Ok(None);
    };
    // JS `if (!values) return undefined`: null (and absent) map to None.
    if values.is_null() {
        return Ok(None);
    }
    let Some(map) = values.as_object() else {
        return Err(AdapterError::InvalidConfigValue(
            "env/headers must be an object of strings".to_string(),
        ));
    };
    let mut out = serde_json::Map::with_capacity(map.len());
    for (key, value) in map {
        let Some(text) = value.as_str() else {
            return Err(AdapterError::InvalidConfigValue(format!(
                "env/headers value for {key:?} must be a string"
            )));
        };
        out.insert(
            key.clone(),
            Value::String(interpolate_secret_expression(text)),
        );
    }
    Ok(Some(out))
}

/// `resolveServerUrl` (utils.ts:167-185): interpolate, require all referenced
/// variables to be set, then WHATWG-validate the result.
pub fn resolve_server_url(url: Option<&Value>) -> Result<Option<String>, AdapterError> {
    // JS `definition.url == null`: both absent and explicit null.
    let Some(url) = url.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let Some(raw) = url.as_str() else {
        return Err(AdapterError::InvalidServerUrl(
            "MCP server URL must be a string".to_string(),
        ));
    };
    let missing = get_missing_env_vars(raw);
    if !missing.is_empty() {
        let plural = if missing.len() == 1 { "" } else { "s" };
        return Err(AdapterError::InvalidServerUrl(format!(
            "Missing environment variable{plural} in MCP server URL: {}",
            missing.join(", ")
        )));
    }
    let resolved = interpolate_env_vars(raw);
    if url::Url::parse(&resolved).is_err() {
        // Parity note: the upstream message embeds the interpolated URL
        // (potential credential material); callers must not forward this
        // message into tracing output (coding-standards §11.2).
        return Err(AdapterError::InvalidServerUrl(format!(
            "Invalid MCP server URL after environment interpolation: {resolved}"
        )));
    }
    Ok(Some(resolved))
}

/// `resolveConfigPath` (utils.ts:187-196): interpolate, then expand a leading
/// `~` / `~/` against the home directory. Other (relative) paths pass through
/// unchanged; upstream resolves them against the session cwd at use sites.
pub fn resolve_config_path(value: Option<&Value>) -> Result<Option<String>, AdapterError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let Some(raw) = value.as_str() else {
        return Err(AdapterError::InvalidConfigValue(
            "config path value must be a string".to_string(),
        ));
    };
    let resolved = interpolate_env_vars(raw);
    if resolved == "~" {
        return Ok(home_dir().map(|h| h.to_string_lossy().into_owned()));
    }
    if let Some(rest) = resolved
        .strip_prefix("~/")
        .or_else(|| resolved.strip_prefix("~\\"))
    {
        return Ok(home_dir().map(|h| h.join(rest).to_string_lossy().into_owned()));
    }
    Ok(Some(resolved))
}

/// `os.homedir()` equivalent, restricted to process env (unix: `HOME`,
/// windows: `USERPROFILE`). `None` when unset.
pub fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    {
        match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => return Some(std::path::PathBuf::from(home)),
            _ => {}
        }
        None
    }
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// `resolveBearerToken` (utils.ts:198-203): explicit `bearerToken` is
/// interpolated; `bearerTokenEnv` reads the variable raw (no interpolation).
pub fn resolve_bearer_token(
    definition: &serde_json::Map<String, Value>,
) -> Result<Option<String>, AdapterError> {
    if let Some(token) = definition.get("bearerToken") {
        let Some(text) = token.as_str() else {
            return Err(AdapterError::InvalidConfigValue(
                "bearerToken must be a string".to_string(),
            ));
        };
        return Ok(Some(interpolate_secret_expression(text)));
    }
    match definition.get("bearerTokenEnv") {
        // JS truthiness: an empty bearerTokenEnv falls through to undefined.
        Some(Value::String(name)) if !name.is_empty() => Ok(std::env::var(name).ok()),
        _ => Ok(None),
    }
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Truncate to at most `target` UTF-16 code units (JS `slice(0, target)`).
fn truncate_utf16(text: &str, target: usize) -> &str {
    let mut units = 0;
    for (idx, ch) in text.char_indices() {
        if units + ch.len_utf16() > target {
            return &text[..idx];
        }
        units += ch.len_utf16();
    }
    text
}

/// `truncateAtWord` (utils.ts:264-275): cut at `target` UTF-16 units, prefer
/// the last space when it lies past 60% of the target.
pub fn truncate_at_word(text: &str, target: usize) -> String {
    if text.is_empty() || utf16_len(text) <= target {
        return text.to_string();
    }
    let truncated = truncate_utf16(text, target);
    // Position of the last space measured in UTF-16 units (JS lastIndexOf).
    let mut last_space_units: Option<usize> = None;
    let mut units = 0;
    for ch in truncated.chars() {
        if ch == ' ' {
            last_space_units = Some(units);
        }
        units += ch.len_utf16();
    }
    if let Some(pos) = last_space_units {
        if (pos as f64) > target as f64 * 0.6 {
            return format!("{}...", truncate_utf16(truncated, pos));
        }
    }
    format!("{truncated}...")
}

/// JS `JSON.stringify` for a single value — used where upstream byte output
/// depends on JS number formatting (`String(number)` in `renderLiteral`,
/// `JSON.stringify` in `stableStringify` / `formatSchema`).
///
/// Numbers: integers print identically in both languages; finite f64 values
/// with a zero fraction print without `.0` (JS `String(3.0) === "3"`), and
/// exponent notation carries an explicit `+` (JS `"1e+21"`). String escaping
/// matches `serde_json` (control chars, `"`, `\`; raw UTF-8 otherwise), which
/// is also `JSON.stringify`'s behavior for all non-lone-surrogate input.
pub fn js_json_stringify(value: &Value) -> String {
    match value {
        Value::Number(n) => js_number_to_string(n),
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
    }
}

fn js_number_to_string(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if !f.is_finite() {
        // JSON.stringify(NaN/Infinity) === "null".
        return "null".to_string();
    }
    let abs = f.abs();
    if f.fract() == 0.0 && abs < 1e21 {
        // Integer-valued doubles print without a fraction or exponent.
        return format!("{f:.0}");
    }
    if (1e-6..1e21).contains(&abs) {
        // Rust Display: shortest round-trip decimal, no exponent — matches
        // the JS fixed-notation range.
        return format!("{f}");
    }
    // Exponent range: JS emits e.g. "1e+21" / "1.5e-7"; Rust {:e} omits the
    // '+' for positive exponents.
    let s = format!("{f:e}");
    match s.split_once('e') {
        Some((mantissa, exp)) if !exp.starts_with('-') => format!("{mantissa}e+{exp}"),
        _ => s,
    }
}

/// `COMMAND_SECRET_TIMEOUT_MS` / `COMMAND_SECRET_MAX_OUTPUT_BYTES`
/// (utils.ts:116-117).
const COMMAND_SECRET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const COMMAND_SECRET_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// `resolveCommandSecret` (utils.ts:119-152): resolve a single leading `!`
/// command marker by executing it via the shell; `!!` escapes to a literal
/// `!` prefix (interpolated); anything else is plain interpolation.
///
/// The command comes from the user's own MCP config — this is the upstream
/// trust model (the adapter spawns arbitrary MCP servers anyway). Failures
/// (timeout / non-zero exit / empty output) raise, they never silently
/// degrade to a literal.
pub fn resolve_command_secret(value: &str, context: &str) -> Result<String, AdapterError> {
    if value.starts_with("!!") {
        return Ok(interpolate_env_vars(&value[1..]));
    }
    if !value.starts_with('!') {
        return Ok(interpolate_env_vars(value));
    }
    let command = &value[1..];
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let shell_arg = if cfg!(windows) { "/c" } else { "-c" };
    let mut child = std::process::Command::new(shell)
        .arg(shell_arg)
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| {
            AdapterError::InvalidConfigValue(format!(
                "Failed to resolve {context}: command failed to start"
            ))
        })?;

    // Reader thread capped at 1 MiB; exceeding the cap is an error
    // (upstream ENOBUFS via maxBuffer), never a silent truncation.
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut exceeded = false;
        if let Some(mut out) = stdout {
            use std::io::Read;
            let mut chunk = [0u8; 8192];
            loop {
                match out.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if buf.len() + n > COMMAND_SECRET_MAX_OUTPUT_BYTES {
                            exceeded = true;
                            // Drain so the child is not blocked on a full pipe.
                            while matches!(out.read(&mut chunk), Ok(m) if m > 0) {}
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
        (buf, exceeded)
    });
    let deadline = std::time::Instant::now() + COMMAND_SECRET_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                break Err(());
            }
            Err(_) => break Err(()),
        }
    };
    let _ = child.wait();
    let (output, exceeded) = reader.join().unwrap_or_default();
    if exceeded {
        return Err(AdapterError::InvalidConfigValue(format!(
            "Failed to resolve {context}: command output exceeded 1 MiB"
        )));
    }
    let status = status.map_err(|()| {
        AdapterError::InvalidConfigValue(format!(
            "Failed to resolve {context}: command timed out after 10 seconds"
        ))
    })?;
    if !status.success() {
        return Err(AdapterError::InvalidConfigValue(format!(
            "Failed to resolve {context}: command exited with code {}",
            status
                .code()
                .map_or_else(|| "unknown".to_string(), |c| c.to_string())
        )));
    }
    let resolved = String::from_utf8_lossy(&output).trim().to_string();
    if resolved.is_empty() {
        return Err(AdapterError::InvalidConfigValue(format!(
            "Failed to resolve {context}: command returned empty output"
        )));
    }
    Ok(resolved)
}

/// `resolveCommandSecretsRecord` (utils.ts:157-165): resolve `!command`
/// markers in a configured record without mutating the input.
pub fn resolve_command_secrets_record(
    values: Option<&Value>,
    context: &dyn Fn(&str) -> String,
) -> Result<Option<serde_json::Map<String, Value>>, AdapterError> {
    let Some(values) = values else {
        return Ok(None);
    };
    if values.is_null() {
        return Ok(None);
    }
    let Some(map) = values.as_object() else {
        return Err(AdapterError::InvalidConfigValue(
            "env/headers must be an object of strings".to_string(),
        ));
    };
    let mut out = serde_json::Map::with_capacity(map.len());
    for (key, value) in map {
        let Some(text) = value.as_str() else {
            return Err(AdapterError::InvalidConfigValue(format!(
                "env/headers value for {key:?} must be a string"
            )));
        };
        out.insert(
            key.clone(),
            Value::String(resolve_command_secret(text, &context(key))?),
        );
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn interpolate_all_three_forms_and_unset_expands_empty() {
        std::env::set_var("RPI_MCP_TEST_SET", "yes");
        assert_eq!(
            interpolate_env_vars(
                "${RPI_MCP_TEST_SET}-$env:RPI_MCP_TEST_SET-{env:RPI_MCP_TEST_SET}-${RPI_MCP_TEST_UNSET}"
            ),
            "yes-yes-yes-"
        );
        std::env::remove_var("RPI_MCP_TEST_SET");
    }

    #[test]
    fn interpolate_leaves_partial_markers_alone() {
        // `\w+` includes digits, so `$env:1bad` IS a marker (unset -> "");
        // unclosed/empty markers pass through verbatim.
        assert_eq!(
            interpolate_env_vars("{env:} ${} $ {env:x"),
            "{env:} ${} $ {env:x"
        );
        assert_eq!(interpolate_env_vars("$env:RPI_MCP_TEST_UNSET_2"), "");
    }

    #[test]
    fn secret_expression_bang_semantics() {
        assert_eq!(interpolate_secret_expression("!!literal"), "!literal");
        assert_eq!(interpolate_secret_expression("!cmd arg"), "!cmd arg");
    }

    #[test]
    fn resolve_url_validates_after_interpolation() {
        assert!(resolve_server_url(Some(&json!("https://example.test/mcp"))).is_ok());
        let err = resolve_server_url(Some(&json!("not a url"))).unwrap_err();
        assert!(err.to_string().contains("Invalid MCP server URL"));
        let err = resolve_server_url(Some(&json!("https://${RPI_MCP_TEST_MISSING_HOST}/mcp")))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid MCP server URL: Missing environment variable in MCP server URL: RPI_MCP_TEST_MISSING_HOST"
        );
    }

    #[test]
    fn truncate_at_word_prefers_space_past_sixty_percent() {
        assert_eq!(
            truncate_at_word("hello world from mcp", 12),
            "hello world..."
        );
        assert_eq!(truncate_at_word("abcdefghijklmnop", 8), "abcdefgh...");
        assert_eq!(truncate_at_word("short", 50), "short");
    }

    #[test]
    fn js_numbers_format_like_js() {
        assert_eq!(js_json_stringify(&json!(3)), "3");
        assert_eq!(js_json_stringify(&json!(3.0)), "3");
        assert_eq!(js_json_stringify(&json!(1.5)), "1.5");
        assert_eq!(js_json_stringify(&json!(-0.25)), "-0.25");
        assert_eq!(js_json_stringify(&json!("a\"b\n")), "\"a\\\"b\\n\"");
    }

    #[test]
    fn resolve_bearer_token_precedence_and_env_reading() {
        // bearerToken (literal) takes priority over bearerTokenEnv.
        let map = json!({
            "bearerToken": "lit-token",
            "bearerTokenEnv": "RPI_MCP_TEST_BEARER_UNSET",
        })
        .as_object()
        .cloned()
        .unwrap_or_default();
        assert_eq!(
            resolve_bearer_token(&map).unwrap(),
            Some("lit-token".to_string())
        );

        // bearerTokenEnv reads the env var raw (no interpolation).
        std::env::set_var("RPI_MCP_TEST_BEARER_SET", "env-token-raw");
        let map = json!({ "bearerTokenEnv": "RPI_MCP_TEST_BEARER_SET" })
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            resolve_bearer_token(&map).unwrap(),
            Some("env-token-raw".to_string())
        );
        std::env::remove_var("RPI_MCP_TEST_BEARER_SET");

        // Unset env var → None (no error).
        let map = json!({ "bearerTokenEnv": "RPI_MCP_TEST_BEARER_UNSET2" })
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert_eq!(resolve_bearer_token(&map).unwrap(), None);

        // Empty bearerTokenEnv falls through (JS truthiness).
        let map = json!({ "bearerTokenEnv": "" })
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert_eq!(resolve_bearer_token(&map).unwrap(), None);

        // bearerToken with interpolation.
        std::env::set_var("RPI_MCP_TEST_TOKEN", "tok-123");
        let map = json!({ "bearerToken": "key-${RPI_MCP_TEST_TOKEN}" })
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            resolve_bearer_token(&map).unwrap(),
            Some("key-tok-123".to_string())
        );
        std::env::remove_var("RPI_MCP_TEST_TOKEN");

        // Non-string bearerToken is an error.
        let map = json!({ "bearerToken": 42 })
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert!(resolve_bearer_token(&map).is_err());
    }
}
