//! Port of `packages/coding-agent/src/core/prompt-templates.ts`
//! @ pi 0.82.1 (2efa728), together with the `utils/frontmatter.ts` parser it
//! depends on (`parseFrontmatter`, frontmatter.ts:28-37).
//!
//! Covers: non-recursive `*.md` template loading (filename minus `.md` →
//! template name, symlinks followed), frontmatter (`description` /
//! `argument-hint`, description falls back to the first non-empty body line
//! truncated to 60 chars + `...`), the full argument-expansion DSL of
//! `substituteArgs` (quote-aware bash-style tokenisation, non-recursive
//! replacement, missing `$N` → `""`), and the `expandPromptTemplate` entry
//! point.
//!
//! Intentional differences:
//! - `SourceInfo` attribution (`source-info.ts`) is not ported yet — it lands
//!   with `core::resource_loader`. `PromptTemplate` therefore drops the
//!   `sourceInfo` field, and [`load_prompt_templates`] drops the
//!   `isUnderPath` scope-classification callback (its only purpose was
//!   feeding `createSyntheticSourceInfo`).
//! - JS `length`/`slice` count UTF-16 code units; the description fallback
//!   truncation here counts Unicode scalar values (`char`s). Identical for
//!   BMP text; a line of astral characters may truncate up to one `char`
//!   earlier than pi.
//! - JS `\s` includes `U+FEFF` (unlike Rust's `char::is_whitespace` /
//!   regex `\s`); the tokeniser and the expand-entry regex use an explicit
//!   JS-`\s` character set to stay byte-faithful.
//! - Non-string YAML frontmatter values (numbers, booleans, nested maps) are
//!   dropped; upstream only *casts* the parsed map to
//!   `Record<string, string>`, so non-strings would flow through as
//!   non-strings. A YAML syntax error still fails the whole template load
//!   (upstream: `parse` throws, `loadTemplateFromFile` catches → `null`).
//! - `.pir` rename per ADR-0001 (`CONFIG_DIR_NAME`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::{Captures, Regex};

use crate::config;
use crate::error::PirError;
use crate::tools::path_utils::resolve_path;

/// `PromptTemplate` (prompt-templates.ts:11-18), minus `sourceInfo` (see
/// module header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    /// Basename of the file minus the `.md` suffix.
    pub name: String,
    /// Frontmatter `description`, else first non-empty body line truncated
    /// to 60 chars (`...` appended when the line is longer).
    pub description: String,
    /// Frontmatter `argument-hint` (hyphenated key), when non-empty.
    pub argument_hint: Option<String>,
    /// Template body (frontmatter stripped).
    pub content: String,
    /// Absolute path to the template file.
    pub file_path: PathBuf,
}

// ---------------------------------------------------------------------------
// JS `\s` character set
// ---------------------------------------------------------------------------

/// Returns `true` for every character JavaScript's `\s` matches
/// (White_Space plus `U+FEFF`).
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// Regex fragment matching one JS-`\s` character (for use inside `[...]`).
const JS_WS_CLASS: &str = " \\t\\n\\x0b\\x0c\\r\\u{a0}\\u{1680}\\u{2000}-\\u{200a}\\u{2028}\\u{2029}\\u{202f}\\u{205f}\\u{3000}\\u{feff}";

// ---------------------------------------------------------------------------
// Argument tokenisation & substitution (prompt-templates.ts:24-102)
// ---------------------------------------------------------------------------

/// `parseCommandArgs` (prompt-templates.ts:24-55): parse command arguments
/// respecting quoted strings (bash-style). `"`/`'` quotes pair up, JS-`\s`
/// separates tokens, quoted content is preserved without the quote
/// characters. An unterminated quote swallows the rest of the input.
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for c in args_string.chars() {
        match in_quote {
            Some(q) => {
                if c == q {
                    in_quote = None;
                } else {
                    current.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    in_quote = Some(c);
                } else if is_js_whitespace(c) {
                    if !current.is_empty() {
                        args.push(std::mem::take(&mut current));
                    }
                } else {
                    current.push(c);
                }
            }
        }
    }

    if !current.is_empty() {
        args.push(current);
    }

    args
}

/// `substituteArgs` regex (prompt-templates.ts:74), verbatim.
fn substitute_args_regex() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\$\{(\d+|ARGUMENTS|@):-([^}]*)\}|\$\{@:(\d+)(?::(\d+))?\}|\$(ARGUMENTS|@|\d+)")
            .ok()
    })
    .as_ref()
}

/// Parse a `\d+` capture the way `parseInt(_, 10)` behaves for the regex
/// above: always succeeds, overflowing values saturate (JS loses integer
/// precision past 2^53, which likewise lands far beyond any args slice).
fn parse_js_int(s: &str) -> usize {
    s.parse::<usize>().unwrap_or(usize::MAX)
}

/// `substituteArgs` (prompt-templates.ts:70-102).
///
/// Supports `$1..$N`, `$@`, `$ARGUMENTS`, `${N:-default}`,
/// `${@:-default}` / `${ARGUMENTS:-default}`, `${@:N}` and `${@:N:L}`
/// (1-indexed; `0` is treated as `1`). Replacement happens on the template
/// string only — argument and default values containing `$1`-style patterns
/// are NOT recursively substituted (single pass over the original content,
/// same as JS `String.replace` with a function). Missing positional args
/// expand to `""`.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");

    let Some(re) = substitute_args_regex() else {
        // Invariant: the pattern is a verified-valid literal; unreachable.
        return content.to_string();
    };

    re.replace_all(content, |caps: &Captures| {
        // ${N:-default} / ${@:-default} / ${ARGUMENTS:-default}
        if let Some(default_target) = caps.get(1) {
            let target = default_target.as_str();
            let value: Option<&str> = if target == "@" || target == "ARGUMENTS" {
                Some(all_args.as_str())
            } else {
                // JS args[parseInt(N)-1]: $0 indexes args[-1] → undefined.
                parse_js_int(target)
                    .checked_sub(1)
                    .and_then(|index| args.get(index))
                    .map(String::as_str)
            };
            let default_value = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            // JS `value ? value : defaultValue`: missing AND empty both fall
            // back to the default.
            return match value {
                Some(v) if !v.is_empty() => v.to_string(),
                _ => default_value.to_string(),
            };
        }

        // ${@:N} / ${@:N:L}
        if let Some(slice_start) = caps.get(3) {
            // Convert to 0-indexed (user provides 1-indexed); treat 0 as 1
            // (bash convention: args start at 1).
            let start = parse_js_int(slice_start.as_str()).saturating_sub(1);
            return match caps.get(4) {
                Some(slice_length) => {
                    let length = parse_js_int(slice_length.as_str());
                    args.iter()
                        .skip(start)
                        .take(length)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" ")
                }
                None => args
                    .iter()
                    .skip(start)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
            };
        }

        // $N / $@ / $ARGUMENTS
        if let Some(simple) = caps.get(5) {
            let simple = simple.as_str();
            if simple == "ARGUMENTS" || simple == "@" {
                return all_args.clone();
            }
            // JS args[parseInt(N)-1]: $0 indexes args[-1] → undefined → "".
            let value = parse_js_int(simple)
                .checked_sub(1)
                .and_then(|index| args.get(index));
            return value.cloned().unwrap_or_default();
        }

        String::new()
    })
    .into_owned()
}

// ---------------------------------------------------------------------------
// Frontmatter (utils/frontmatter.ts:8-37)
// ---------------------------------------------------------------------------

/// Result of [`parse_frontmatter`]: string-valued frontmatter entries and
/// the remaining body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedFrontmatter {
    /// String-valued YAML frontmatter entries (non-string values dropped,
    /// see module header).
    pub values: HashMap<String, String>,
    /// Body after the frontmatter block (trimmed), or the whole
    /// newline-normalised content when no frontmatter block exists
    /// (NOT trimmed — verbatim port of `extractFrontmatter`).
    pub body: String,
}

/// `normalizeNewlines` (frontmatter.ts:8).
fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

/// `parseFrontmatter` (frontmatter.ts:28-37) specialised to string values.
///
/// A frontmatter block is `---` at the very start of the (newline
/// normalised) content, closed by the next `\n---`. The body starts one
/// character after the closing delimiter and is trimmed. A YAML syntax
/// error is reported as [`PirError::Resource`] (upstream: `parse` throws
/// and the caller's `try/catch` turns it into a failed load).
pub fn parse_frontmatter(content: &str) -> Result<ParsedFrontmatter, PirError> {
    let normalized = normalize_newlines(content);

    if !normalized.starts_with("---") {
        return Ok(ParsedFrontmatter {
            values: HashMap::new(),
            body: normalized,
        });
    }

    // JS: normalized.indexOf("\n---", 3)
    let end_index = normalized[3..].find("\n---").map(|i| i + 3);
    let Some(end_index) = end_index else {
        return Ok(ParsedFrontmatter {
            values: HashMap::new(),
            body: normalized,
        });
    };

    // JS: normalized.slice(4, endIndex) — empty when endIndex < 4. `get`
    // (not indexing) because byte 4 can sit mid-char right after "---"
    // (e.g. "---é\n---"); JS slices UTF-16 units there instead. The
    // resulting YAML differs only in such pathological inputs.
    let yaml_string = normalized.get(4..end_index).unwrap_or("");
    let body = normalized[end_index + 4..].trim().to_string();

    let parsed: serde_yaml::Value = serde_yaml::from_str(yaml_string)
        .map_err(|e| PirError::Resource(format!("failed to parse frontmatter YAML: {e}")))?;

    let mut values = HashMap::new();
    // Upstream `parsed ?? {}`: non-mapping scalars behave like an empty
    // record (property lookups return undefined).
    if let serde_yaml::Value::Mapping(map) = parsed {
        for (key, value) in map {
            if let (serde_yaml::Value::String(k), serde_yaml::Value::String(v)) = (key, value) {
                values.insert(k, v);
            }
        }
    }

    Ok(ParsedFrontmatter { values, body })
}

// ---------------------------------------------------------------------------
// Template loading (prompt-templates.ts:104-263)
// ---------------------------------------------------------------------------

/// `loadTemplateFromFile` (prompt-templates.ts:104-133). Returns `None` on
/// any read or frontmatter-parse failure (upstream `try/catch → null`).
fn load_template_from_file(file_path: &Path) -> Option<PromptTemplate> {
    let raw_content = std::fs::read_to_string(file_path).ok()?;
    let parsed = parse_frontmatter(&raw_content).ok()?;

    // basename(filePath).replace(/\.md$/, "")
    let name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_path.to_string_lossy().into_owned());
    let name = name.strip_suffix(".md").unwrap_or(&name).to_string();

    // Description from frontmatter or first non-empty line (JS: any truthy
    // frontmatter value wins, so a whitespace-only description is kept).
    let mut description = parsed
        .values
        .get("description")
        .cloned()
        .unwrap_or_default();
    if description.is_empty() {
        if let Some(first_line) = parsed.body.lines().find(|line| !line.trim().is_empty()) {
            description = first_line.chars().take(60).collect();
            if first_line.chars().count() > 60 {
                description.push_str("...");
            }
        }
    }

    // `...(frontmatter["argument-hint"] && { argumentHint: ... })`:
    // truthy (non-empty) strings only.
    let argument_hint = parsed
        .values
        .get("argument-hint")
        .filter(|hint| !hint.is_empty())
        .cloned();

    Some(PromptTemplate {
        name,
        description,
        argument_hint,
        content: parsed.body,
        file_path: file_path.to_path_buf(),
    })
}

/// `loadTemplatesFromDir` (prompt-templates.ts:138-175): scan a directory
/// for `.md` files (non-recursive) and load them as prompt templates.
/// Symlinks are followed (a symlink whose target is a file is loaded; a
/// broken symlink is skipped). Missing/unreadable directories yield an
/// empty list.
pub fn load_templates_from_dir(dir: &Path) -> Vec<PromptTemplate> {
    let mut templates = Vec::new();

    if !dir.exists() {
        return templates;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return templates,
    };

    for entry in entries.flatten() {
        let full_path = entry.path();

        // For symlinks, check if they point to a file.
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        let is_file = if file_type.is_symlink() {
            match std::fs::metadata(&full_path) {
                Ok(stats) => stats.is_file(),
                Err(_) => continue, // Broken symlink, skip it.
            }
        } else {
            file_type.is_file()
        };

        if is_file && entry.file_name().to_string_lossy().ends_with(".md") {
            if let Some(template) = load_template_from_file(&full_path) {
                templates.push(template);
            }
        }
    }

    templates
}

/// `LoadPromptTemplatesOptions` (prompt-templates.ts:177-186).
#[derive(Debug, Clone, Default)]
pub struct LoadPromptTemplatesOptions {
    /// Working directory for project-local templates.
    pub cwd: PathBuf,
    /// Agent config directory for global templates.
    pub agent_dir: PathBuf,
    /// Explicit prompt template paths (files or directories).
    pub prompt_paths: Vec<String>,
    /// Include default prompt directories.
    pub include_defaults: bool,
}

fn process_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

/// `loadPromptTemplates` (prompt-templates.ts:194-263): load all prompt
/// templates from:
/// 1. Global: `agentDir/prompts/`
/// 2. Project: `cwd/.pir/prompts/`
/// 3. Explicit prompt paths
///
/// The `sourceInfo` scope classification is not ported (see module header).
pub fn load_prompt_templates(options: &LoadPromptTemplatesOptions) -> Vec<PromptTemplate> {
    let resolved_cwd = resolve_path(&options.cwd.to_string_lossy(), &process_cwd());
    let resolved_agent_dir = resolve_path(&options.agent_dir.to_string_lossy(), &process_cwd());

    let mut templates = Vec::new();

    if options.include_defaults {
        templates.extend(load_templates_from_dir(&resolved_agent_dir.join("prompts")));
        templates.extend(load_templates_from_dir(&config::get_project_prompts_dir(
            &resolved_cwd,
        )));
    }

    // 3. Load explicit prompt paths.
    for raw_path in &options.prompt_paths {
        // resolvePath(rawPath, resolvedCwd, { trim: true })
        let resolved_path = resolve_path(raw_path.trim(), &resolved_cwd);
        if !resolved_path.exists() {
            continue;
        }

        match std::fs::metadata(&resolved_path) {
            Ok(stats) if stats.is_dir() => {
                templates.extend(load_templates_from_dir(&resolved_path));
            }
            Ok(stats) if stats.is_file() && resolved_path.to_string_lossy().ends_with(".md") => {
                if let Some(template) = load_template_from_file(&resolved_path) {
                    templates.push(template);
                }
            }
            // Ignore read failures / non-md entries.
            _ => {}
        }
    }

    templates
}

// ---------------------------------------------------------------------------
// Expansion entry point (prompt-templates.ts:269-285)
// ---------------------------------------------------------------------------

/// `expandPromptTemplate` matcher (prompt-templates.ts:272):
/// `/^\/([^\s]+)(?:\s+([\s\S]*))?$/` with the JS-`\s` character set.
fn expand_template_regex() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            "^/([^{JS_WS_CLASS}]+)(?:[{JS_WS_CLASS}]+([\\s\\S]*))?$"
        ))
        .ok()
    })
    .as_ref()
}

/// `expandPromptTemplate` (prompt-templates.ts:269-285): expand a prompt
/// template if `text` matches a template name (`/name args...`). Returns
/// the original text when it is not a `/`-command, does not parse, or names
/// no known template.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    if !text.starts_with('/') {
        return text.to_string();
    }

    let Some(re) = expand_template_regex() else {
        // Invariant: the pattern is a verified-valid literal; unreachable.
        return text.to_string();
    };
    let Some(caps) = re.captures(text) else {
        return text.to_string();
    };

    let template_name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let args_string = caps.get(2).map(|m| m.as_str()).unwrap_or("");

    match templates.iter().find(|t| t.name == template_name) {
        Some(template) => {
            let args = parse_command_args(args_string);
            substitute_args(&template.content, &args)
        }
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    // ---- parse_command_args ---------------------------------------------

    #[test]
    fn parse_args_whitespace_split() {
        assert_eq!(parse_command_args("a b  c"), args(&["a", "b", "c"]));
        assert_eq!(
            parse_command_args("  lead trail  "),
            args(&["lead", "trail"])
        );
        assert_eq!(parse_command_args(""), Vec::<String>::new());
        assert_eq!(parse_command_args("   "), Vec::<String>::new());
    }

    #[test]
    fn parse_args_quotes() {
        assert_eq!(parse_command_args(r#"a "b c" d"#), args(&["a", "b c", "d"]));
        assert_eq!(parse_command_args("'x y' z"), args(&["x y", "z"]));
        // Quotes preserved mid-token, removed from the result.
        assert_eq!(
            parse_command_args(r#"pre"mid dle"post"#),
            args(&["premid dlepost"])
        );
        // Mixed quote kinds: the opening kind closes.
        assert_eq!(parse_command_args(r#""it's" ok"#), args(&["it's", "ok"]));
        // Unterminated quote swallows the rest.
        assert_eq!(parse_command_args(r#"a "bc"#), args(&["a", "bc"]));
        // Empty quoted string produces no token.
        assert_eq!(parse_command_args(r#"a "" b"#), args(&["a", "b"]));
    }

    #[test]
    fn parse_args_js_whitespace_set() {
        // U+FEFF splits tokens in JS (\s) but is not char::is_whitespace.
        assert_eq!(parse_command_args("a\u{feff}b"), args(&["a", "b"]));
        assert_eq!(parse_command_args("a\u{3000}b"), args(&["a", "b"]));
    }

    // ---- substitute_args -------------------------------------------------

    #[test]
    fn substitute_positional() {
        let a = args(&["one", "two"]);
        assert_eq!(substitute_args("$1 $2", &a), "one two");
        // Missing positional → empty string.
        assert_eq!(substitute_args("<$3>", &a), "<>");
        // $0 is never a valid positional (JS: args[-1] → undefined → "").
        assert_eq!(substitute_args("<$0>", &a), "<>");
    }

    #[test]
    fn substitute_all_args() {
        let a = args(&["one", "two"]);
        assert_eq!(substitute_args("$@", &a), "one two");
        assert_eq!(substitute_args("$ARGUMENTS", &a), "one two");
        assert_eq!(substitute_args("$@", &[]), "");
    }

    #[test]
    fn substitute_defaults() {
        let a = args(&["one"]);
        assert_eq!(substitute_args("${1:-d}", &a), "one");
        assert_eq!(substitute_args("${2:-d}", &a), "d");
        assert_eq!(substitute_args("${@:-d}", &a), "one");
        assert_eq!(substitute_args("${@:-d}", &[]), "d");
        assert_eq!(substitute_args("${ARGUMENTS:-d}", &[]), "d");
        // Empty default.
        assert_eq!(substitute_args("${2:-}", &a), "");
        // Default containing spaces / dollar patterns (not re-expanded).
        assert_eq!(substitute_args("${2:-$1 d}", &a), "$1 d");
    }

    #[test]
    fn substitute_slices() {
        let a = args(&["a", "b", "c", "d"]);
        assert_eq!(substitute_args("${@:2}", &a), "b c d");
        assert_eq!(substitute_args("${@:1}", &a), "a b c d");
        // 0 treated as 1 (bash convention).
        assert_eq!(substitute_args("${@:0}", &a), "a b c d");
        assert_eq!(substitute_args("${@:2:2}", &a), "b c");
        assert_eq!(substitute_args("${@:3:99}", &a), "c d");
        // Length 0 → empty.
        assert_eq!(substitute_args("${@:1:0}", &a), "");
        // Start beyond the end → empty.
        assert_eq!(substitute_args("${@:99}", &a), "");
    }

    #[test]
    fn substitute_is_single_pass() {
        // Argument values containing placeholders are NOT re-expanded.
        let a = args(&["$2", "x"]);
        assert_eq!(substitute_args("$1", &a), "$2");
    }

    #[test]
    fn substitute_no_match_passthrough() {
        let a = args(&["a"]);
        // In `$$5` the second `$` still starts a `$5` placeholder (missing
        // → ""), matching JS regex-scan order; bare `$` and `${unknown}`
        // are not placeholders.
        assert_eq!(
            substitute_args("cost: $$5 and $ and ${foo}", &a),
            "cost: $ and $ and ${foo}"
        );
        // `${1:x}` (no dash) is not a default expression.
        assert_eq!(substitute_args("${1:x}", &a), "${1:x}");
    }

    // ---- parse_frontmatter ------------------------------------------------

    #[test]
    fn frontmatter_basic() {
        let parsed =
            parse_frontmatter("---\ndescription: hello\nargument-hint: <x>\n---\nbody text\n")
                .expect("valid frontmatter");
        assert_eq!(
            parsed.values.get("description").map(String::as_str),
            Some("hello")
        );
        assert_eq!(
            parsed.values.get("argument-hint").map(String::as_str),
            Some("<x>")
        );
        assert_eq!(parsed.body, "body text");
    }

    #[test]
    fn frontmatter_crlf_normalised() {
        let parsed = parse_frontmatter("---\r\ndescription: hi\r\n---\r\nbody\r\n")
            .expect("valid frontmatter");
        assert_eq!(
            parsed.values.get("description").map(String::as_str),
            Some("hi")
        );
        assert_eq!(parsed.body, "body");
        // No frontmatter: body is the normalised content, NOT trimmed.
        let plain = parse_frontmatter("\r\nbody\r\n").expect("no frontmatter");
        assert_eq!(plain.body, "\nbody\n");
        assert!(plain.values.is_empty());
    }

    #[test]
    fn frontmatter_unclosed_is_body() {
        // No closing "\n---": entire content is the body.
        let parsed = parse_frontmatter("---\ndescription: hi\nbody").expect("unclosed");
        assert!(parsed.values.is_empty());
        assert_eq!(parsed.body, "---\ndescription: hi\nbody");
    }

    #[test]
    fn frontmatter_empty_block() {
        // "---\n---\nbody": empty YAML parses as null → empty record.
        let parsed = parse_frontmatter("---\n---\nbody").expect("empty block");
        assert!(parsed.values.is_empty());
        assert_eq!(parsed.body, "body");
    }

    #[test]
    fn frontmatter_invalid_yaml_errors() {
        assert!(parse_frontmatter("---\nkey: [unclosed\n---\nbody").is_err());
    }

    #[test]
    fn frontmatter_non_string_values_dropped() {
        let parsed =
            parse_frontmatter("---\ndescription: 42\nother: true\n---\nbody").expect("map");
        assert!(!parsed.values.contains_key("description"));
        assert!(!parsed.values.contains_key("other"));
    }

    // ---- expand_prompt_template -------------------------------------------

    fn template(name: &str, content: &str) -> PromptTemplate {
        PromptTemplate {
            name: name.to_string(),
            description: String::new(),
            argument_hint: None,
            content: content.to_string(),
            file_path: PathBuf::from(format!("/tmp/{name}.md")),
        }
    }

    #[test]
    fn expand_matches_and_substitutes() {
        let templates = vec![template("review", "Review $1 for $2")];
        assert_eq!(
            expand_prompt_template("/review main.rs bugs", &templates),
            "Review main.rs for bugs"
        );
        // Quoted args survive tokenisation.
        assert_eq!(
            expand_prompt_template(r#"/review "my file.rs" bugs"#, &templates),
            "Review my file.rs for bugs"
        );
    }

    #[test]
    fn expand_passthrough_cases() {
        let templates = vec![template("review", "body")];
        // Not a slash command.
        assert_eq!(expand_prompt_template("review x", &templates), "review x");
        // Unknown template name.
        assert_eq!(
            expand_prompt_template("/unknown x", &templates),
            "/unknown x"
        );
        // Multiline args reach the template ([\s\S]* matches newlines).
        assert_eq!(expand_prompt_template("/review a\nb", &templates), "body");
    }
}
