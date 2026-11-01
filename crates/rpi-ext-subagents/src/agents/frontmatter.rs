//! Hand-written frontmatter parser for agent definition files.
//!
//! Byte-level port of pi-subagents `src/agents/frontmatter.ts` @ v0.48.0
//! (56f97234). Upstream deliberately does NOT use a YAML library here: keys are
//! line-parsed (`/^([\w-]+):\s*(.*)$/`), quoted scalars keep their quotes
//! stripped, `>`/`|-` block scalars fold with custom rules, comments and
//! non-matching lines are silently ignored, and a missing closing fence turns
//! the whole file into body. serde_yaml cannot reproduce any of that, so the
//! parser is ported literally (deviation TE-D19: design §2.2 mentioned
//! serde_yaml; byte-parity with upstream won — the crate does not depend on
//! serde_yaml at all).
//!
//! Intentional differences: none.

/// Fold a YAML folded block scalar while preserving more-indented lines and
/// every blank-line separator (`foldBlock`, frontmatter.ts:12-40).
fn fold_block(block: &str) -> String {
    let mut folded = String::new();
    let mut has_content = false;
    let mut previous_is_more_indented = false;
    let mut blank_lines = 0usize;

    for line in block.split('\n') {
        let current = line.trim_end();
        if current.trim().is_empty() {
            if has_content {
                blank_lines += 1;
            }
            continue;
        }
        let current_is_more_indented = current.len() > current.trim_start().len();
        if has_content {
            if blank_lines > 0 {
                let extra = usize::from(previous_is_more_indented || current_is_more_indented);
                folded.push_str(&"\n".repeat(blank_lines + extra));
            } else if previous_is_more_indented || current_is_more_indented {
                folded.push('\n');
            } else {
                folded.push(' ');
            }
        }
        folded.push_str(current);
        has_content = true;
        previous_is_more_indented = current_is_more_indented;
        blank_lines = 0;
    }
    folded.trim().to_string()
}

/// Normalize a simple-scalar frontmatter list from comma-separated or
/// block-list syntax (`parseFrontmatterList`, frontmatter.ts:46-69 @ 3a63e0ad).
/// Only the standard `- item` marker is removed; ordinary hyphenated values stay
/// intact. A trailing ` #...` comment is dropped from unquoted `- item` lines;
/// single-line values are left alone because [`parse_frontmatter`] has already
/// removed their quotes, so `"notes #1.md"` must survive (v0.83.0 / #2995).
pub fn parse_frontmatter_list(raw: Option<&str>) -> Option<Vec<String>> {
    let raw = raw?;
    let mut out = Vec::new();
    for line in raw.split('\n') {
        let value = line.trim();
        // `value.match(/^-[\s]+(.+)$/)` — dash, at least one whitespace, then
        // content; `-read` without whitespace does not match and stays intact.
        let list_item = match value.strip_prefix('-') {
            Some(rest) if rest.starts_with(char::is_whitespace) => Some(rest.trim_start()),
            _ => None,
        };
        // `listItem === undefined || /^["']/.test(listItem) ? listItem ?? value : strip`
        let item = match list_item {
            Some(item) if !item.starts_with(['"', '\'']) => strip_trailing_comment(item),
            Some(item) => item,
            None => value,
        };
        for part in item.split(',') {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
    Some(out)
}

/// `item.replace(/(?:^|\s)#.*$/, "")` — drop a trailing comment marker and
/// everything after it, but only when `#` starts the value or follows whitespace
/// (so `read#1` stays a name).
fn strip_trailing_comment(item: &str) -> &str {
    let mut search = 0usize;
    while let Some(offset) = item[search..].find('#') {
        let index = search + offset;
        let at_start = index == 0;
        let after_space = item[..index].ends_with(char::is_whitespace);
        if at_start || after_space {
            return item[..index].trim_end();
        }
        search = index + 1;
    }
    item
}

/// A tool or extension name list whose items may each be quoted, as in
/// `"read", "bash"` or `- "read"` (`parseUnquotedList`, frontmatter.ts:71-73
/// @ 3a63e0ad, #3017).
pub fn parse_unquoted_list(raw: Option<&str>) -> Option<Vec<String>> {
    parse_frontmatter_list(raw).map(|items| items.iter().map(|item| unquote_name(item)).collect())
}

/// Strip one matching pair of surrounding single or double quotes
/// (`unquoteName`, frontmatter.ts:61-68 @ 3a63e0ad).
fn unquote_name(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

/// `tools` frontmatter scalar forms (TE18 FR-A, R7.1.4.1 / #1047/#1049).
///
/// The scalar semantics come from the upstream override surface
/// (`parseToolsOverride`/`applyToolsOverride`, agents.ts:946-951/1349-1356
/// @ 0fc0eebb): `"inherit"` clears the allowlist back to "undeclared"
/// (host defaults, no tool flags) and `false` is an explicit empty allowlist
/// (`--no-tools`). Upstream applies them to override entries only; rpi also
/// recognizes them on agent frontmatter (task §2.1), where the hand-rolled
/// parser delivers raw strings — both `tools: false` and quoted
/// `tools: "false"` arrive as `"false"`. Matching upstream, the comparison
/// is trim + case-sensitive (no lowercasing; `Inherit`/`FALSE` stay ordinary
/// list values). Every other value keeps the plain-list parse unchanged.
pub fn parse_tools_field(raw: Option<&str>) -> Option<Vec<String>> {
    match raw.map(str::trim) {
        Some("inherit") => None,
        Some("false") => Some(Vec::new()),
        other => parse_unquoted_list(other),
    }
}

/// Escape regex special characters (`escapeRegex`, frontmatter.ts:4-6) — kept
/// inline because the block flush matches a literal whitespace prefix.
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Strip the common leading whitespace from a collected block value
/// (frontmatter.ts:99-114): find the first `^[ \t]+(?=\S)` prefix, remove it
/// from every line, then drop one leading empty line.
fn strip_block_prefix(raw_block: &str) -> String {
    let leading = raw_block
        .lines()
        .find_map(|line| {
            let trimmed_start = line.len() - line.trim_start().len();
            if trimmed_start > 0 && !line.trim().is_empty() {
                Some(&line[..trimmed_start])
            } else {
                None
            }
        })
        .unwrap_or("");
    if leading.is_empty() {
        return raw_block.to_string();
    }
    let escaped = format!("^{}", escape_regex(leading));
    let mut stripped_lines = Vec::new();
    let regex = regex_lite_matcher(&escaped);
    for line in raw_block.split('\n') {
        stripped_lines.push(regex.strip_prefix(line).unwrap_or(line).to_string());
    }
    let joined = stripped_lines.join("\n");
    joined.strip_prefix('\n').unwrap_or(&joined).to_string()
}

/// Minimal literal `^<literal>` prefix matcher (the upstream flush builds one
/// RegExp with the escaped prefix; only literal chars can appear in it).
struct RegexLiteMatcher<'a> {
    literal: &'a str,
}

fn regex_lite_matcher(pattern: &str) -> RegexLiteMatcher<'_> {
    let literal = pattern.strip_prefix('^').unwrap_or(pattern);
    RegexLiteMatcher { literal }
}

impl<'a> RegexLiteMatcher<'a> {
    fn strip_prefix<'b>(&self, line: &'b str) -> Option<&'b str> {
        // The escaped literal contains backslash escapes; decode them back for
        // the literal comparison (`\\.` → the escaped char itself).
        let mut decoded = String::new();
        let mut chars = self.literal.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                decoded.push(chars.next().unwrap_or('\\'));
            } else {
                decoded.push(c);
            }
        }
        line.strip_prefix(decoded.as_str())
    }
}

pub struct ParsedFrontmatter {
    pub frontmatter: std::collections::BTreeMap<String, String>,
    pub body: String,
}

/// Parse YAML frontmatter from agent files (`parseFrontmatter`,
/// frontmatter.ts:65-153). Flat `key: value` pairs plus nested block values
/// collected as single strings with embedded newlines.
pub fn parse_frontmatter(content: &str) -> ParsedFrontmatter {
    let mut frontmatter = std::collections::BTreeMap::new();
    let normalized = content.replace("\r\n", "\n");

    if !normalized.starts_with("---") {
        return ParsedFrontmatter {
            frontmatter,
            body: normalized,
        };
    }

    let Some(end_index) = find_substring(&normalized, "\n---", 3) else {
        return ParsedFrontmatter {
            frontmatter,
            body: normalized,
        };
    };

    let frontmatter_block = &normalized[4..end_index];
    let body = normalized[end_index + 4..].trim().to_string();

    let mut current_key: Option<String> = None;
    let mut current_block_lines: Option<Vec<String>> = None;
    let mut current_indent: usize = 0;
    let mut current_folded = false;
    let mut current_literal = false;

    let flush = |frontmatter: &mut std::collections::BTreeMap<String, String>,
                 key: &mut Option<String>,
                 lines: &mut Option<Vec<String>>,
                 folded: bool| {
        if let (Some(k), Some(block)) = (key.take(), lines.take()) {
            let raw_block = block.join("\n");
            let stripped = strip_block_prefix(&raw_block);
            frontmatter.insert(
                k,
                if folded {
                    fold_block(&stripped)
                } else {
                    stripped
                },
            );
        }
    };

    for line in frontmatter_block.split('\n') {
        let indent = line.chars().take_while(|c| c.is_whitespace()).count();
        let trimmed = line.trim();

        // Comment lines are content only inside a `|` or `>` block scalar.
        // Elsewhere they must neither end a pending list block nor become
        // part of its value (frontmatter.ts:104-109 @ 3a63e0ad, #2995).
        if trimmed.starts_with('#') && !current_folded && !current_literal {
            continue;
        }

        let in_block = current_key.is_some()
            && current_block_lines.is_some()
            && (indent > current_indent
                || ((current_folded || current_literal) && trimmed.is_empty()));
        if in_block {
            if let Some(lines) = current_block_lines.as_mut() {
                lines.push(line.to_string());
            }
            continue;
        }

        flush(
            &mut frontmatter,
            &mut current_key,
            &mut current_block_lines,
            current_folded,
        );
        current_folded = false;
        current_literal = false;
        current_indent = 0;

        if let Some((key, raw_value)) = split_key_line(line) {
            let raw_value = raw_value.trim();
            let quote = raw_value.chars().next();
            let name_list = matches!(
                key,
                "tools"
                    | "excludeTools"
                    | "mutationTools"
                    | "extensions"
                    | "subagentOnlyExtensions"
            );
            // `"read", "bash"` starts and ends with a quote, but it is a list.
            // Keep those item quotes for the splitter (frontmatter.ts:140-145
            // @ 3a63e0ad, #3017).
            let quoted_list = name_list
                && matches!(quote, Some('"') | Some('\''))
                && (raw_value.contains(&format!("{},", quote.unwrap_or(' ')))
                    || raw_value.contains(&format!(", {}", quote.unwrap_or(' '))));
            let closing = quote.unwrap_or(' ');
            let is_quoted = !quoted_list
                && matches!(quote, Some('"') | Some('\''))
                && raw_value.ends_with(closing);
            let value = if is_quoted && raw_value.len() >= 2 {
                &raw_value[1..raw_value.len() - 1]
            } else if is_quoted {
                ""
            } else {
                raw_value
            };
            let is_folded = !is_quoted && (raw_value == ">" || raw_value == ">-");
            let is_literal = !is_quoted && (raw_value == "|" || raw_value == "|-");

            if value.is_empty() || is_folded || is_literal {
                current_key = Some(key.to_string());
                current_block_lines = Some(Vec::new());
                current_indent = indent;
                current_folded = is_folded;
                current_literal = is_literal;
            } else {
                frontmatter.insert(key.to_string(), value.to_string());
            }
        }
        // Lines that don't match a key pattern (comments, empty lines) are ignored.
    }

    flush(
        &mut frontmatter,
        &mut current_key,
        &mut current_block_lines,
        current_folded,
    );

    ParsedFrontmatter { frontmatter, body }
}

/// `String.prototype.indexOf(search, fromIndex)`.
fn find_substring(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let byte_from = char_boundary_index(haystack, from);
    haystack[byte_from..].find(needle).map(|i| i + byte_from)
}

/// Upstream operates on UTF-16 indices; for the ASCII delimiters this parser
/// uses (`---`, `\n---` at offset 3) byte and char indices coincide. The
/// helper keeps the intent explicit if content ever changes.
fn char_boundary_index(s: &str, char_index: usize) -> usize {
    s.char_indices()
        .nth(char_index)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// `line.match(/^([\w-]+):\s*(.*)$/)` — `\w` in JS regex is [A-Za-z0-9_].
fn split_key_line(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let key = &line[..colon];
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let rest = &line[colon + 1..];
    let trimmed_start = rest.trim_start();
    // `:\s*(.*)` — the value is everything after the first colon and optional
    // whitespace; caller trims.
    Some((key, trimmed_start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_returns_whole_content_as_body() {
        let parsed = parse_frontmatter("just a prompt\n");
        assert!(parsed.frontmatter.is_empty());
        assert_eq!(parsed.body, "just a prompt\n");
    }

    #[test]
    fn unterminated_frontmatter_returns_whole_content_as_body() {
        let parsed = parse_frontmatter("---\nname: x");
        assert!(parsed.frontmatter.is_empty());
        assert_eq!(parsed.body, "---\nname: x");
    }

    #[test]
    fn simple_fields_and_body() {
        let parsed = parse_frontmatter(
            "---\nname: scout\ndescription: Fast recon\n\nYou are a scout.\n---\nBody here.\n",
        );
        assert_eq!(parsed.frontmatter["name"], "scout");
        assert_eq!(parsed.frontmatter["description"], "Fast recon");
        assert_eq!(parsed.body, "Body here.");
    }

    #[test]
    fn quoted_values_strip_quotes() {
        let parsed = parse_frontmatter("---\nname: \"scout\"\nother: 'x y'\n---\n");
        assert_eq!(parsed.frontmatter["name"], "scout");
        assert_eq!(parsed.frontmatter["other"], "x y");
    }

    #[test]
    fn block_list_tools_and_comma_list_both_parse() {
        let block = parse_frontmatter("---\ntools:\n  - read\n  - bash, edit\n---\n");
        assert_eq!(
            parse_frontmatter_list(block.frontmatter.get("tools").map(String::as_str)),
            Some(vec![
                "read".to_string(),
                "bash".to_string(),
                "edit".to_string()
            ])
        );
        let comma = parse_frontmatter("---\ntools: read, bash ,edit\n---\n");
        assert_eq!(
            parse_frontmatter_list(comma.frontmatter.get("tools").map(String::as_str)),
            Some(vec![
                "read".to_string(),
                "bash".to_string(),
                "edit".to_string()
            ])
        );
    }

    #[test]
    fn folded_block_scalar_folds_like_upstream() {
        let parsed = parse_frontmatter("---\ndescription: >\n  line one\n  line two\n---\n");
        assert_eq!(parsed.frontmatter["description"], "line one line two");
    }

    #[test]
    fn comments_and_unknown_lines_are_ignored() {
        let parsed = parse_frontmatter("---\n# a comment\nname: x\nnot a pair\n---\nbody");
        assert_eq!(parsed.frontmatter.len(), 1);
        assert_eq!(parsed.body, "body");
    }

    // v0.83.0 (#2995/#3001): YAML comments in agent frontmatter lists.
    fn tools_of(frontmatter: &str) -> Option<Vec<String>> {
        let parsed = parse_frontmatter(&format!("---\n{frontmatter}\nname: worker\n---\nbody"));
        parse_frontmatter_list(parsed.frontmatter.get("tools").map(String::as_str))
    }

    #[test]
    fn ignores_yaml_comments_in_tool_lists() {
        assert_eq!(
            tools_of("tools:\n  - read\n  # note\n  - bash"),
            Some(vec!["read".into(), "bash".into()])
        );
        assert_eq!(
            tools_of("tools:\n  - read\n# note\n  - bash"),
            Some(vec!["read".into(), "bash".into()])
        );
        assert_eq!(
            tools_of("tools:\n  - read # why\n  - bash"),
            Some(vec!["read".into(), "bash".into()])
        );
        assert_eq!(
            tools_of("tools:\n  - read\n  # note, more\n  - bash"),
            Some(vec!["read".into(), "bash".into()])
        );
    }

    #[test]
    fn keeps_hash_inside_quoted_list_items_and_block_scalars() {
        assert_eq!(
            tools_of("tools:\n  - \"a # b\"\n  - bash"),
            Some(vec!["\"a # b\"".into(), "bash".into()])
        );
        let parsed = parse_frontmatter("---\ndefaultReads: \"notes #1.md\"\n---\nbody");
        assert_eq!(
            parse_frontmatter_list(parsed.frontmatter.get("defaultReads").map(String::as_str)),
            Some(vec!["notes #1.md".into()])
        );
        let parsed = parse_frontmatter(
            "---\ndescription: |\n  first\n  # kept\nnotes: >\n  one\n  # two\n---\nbody",
        );
        assert_eq!(parsed.frontmatter["description"], "first\n# kept");
        assert_eq!(parsed.frontmatter["notes"], "one # two");
    }

    // v0.83.0 (#3017): quoted names in tool and extension lists.
    fn unquoted_tools_of(value: &str) -> Option<Vec<String>> {
        let parsed = parse_frontmatter(&format!("---\ntools: {value}\nname: worker\n---\nbody"));
        parse_unquoted_list(parsed.frontmatter.get("tools").map(String::as_str))
    }

    #[test]
    fn reads_each_quoted_item_without_its_quote_marks() {
        assert_eq!(
            unquoted_tools_of("\"read\", \"bash\""),
            Some(vec!["read".into(), "bash".into()])
        );
        assert_eq!(
            unquoted_tools_of("'read', 'bash'"),
            Some(vec!["read".into(), "bash".into()])
        );
        assert_eq!(unquoted_tools_of("\"read\""), Some(vec!["read".into()]));
        let parsed =
            parse_frontmatter("---\ntools:\n  - \"read\"\n  - bash\nname: worker\n---\nbody");
        assert_eq!(
            parse_unquoted_list(parsed.frontmatter.get("tools").map(String::as_str)),
            Some(vec!["read".into(), "bash".into()])
        );
        assert_eq!(unquoted_tools_of("false"), Some(vec!["false".into()]));
        assert_eq!(parse_unquoted_list(None), None);
        let description = parse_frontmatter(
            "---\ndescription: \"Review \\\"a\\\", then continue\"\nname: worker\n---\nbody",
        );
        assert_eq!(
            description.frontmatter["description"],
            "Review \\\"a\\\", then continue"
        );
    }

    #[test]
    fn tools_field_still_maps_inherit_and_false_before_unquoting() {
        assert_eq!(parse_tools_field(Some("inherit")), None);
        assert_eq!(parse_tools_field(Some("false")), Some(vec![]));
        assert_eq!(
            parse_tools_field(Some("\"read\", \"bash\"")),
            Some(vec!["read".into(), "bash".into()])
        );
    }

    #[test]
    fn builtin_agent_files_parse() {
        let builtins: &[(&str, &str)] = &[
            ("delegate", include_str!("../../assets/agents/delegate.md")),
            ("oracle", include_str!("../../assets/agents/oracle.md")),
            (
                "researcher",
                include_str!("../../assets/agents/researcher.md"),
            ),
            ("reviewer", include_str!("../../assets/agents/reviewer.md")),
            ("scout", include_str!("../../assets/agents/scout.md")),
            ("worker", include_str!("../../assets/agents/worker.md")),
        ];
        for (name, content) in builtins {
            let parsed = parse_frontmatter(content);
            assert_eq!(parsed.frontmatter["name"], *name, "{name} name field");
            assert!(
                parsed.frontmatter["description"].len() > 10,
                "{name} description"
            );
            assert!(parsed.body.len() > 20, "{name} system prompt body");
        }
    }
}
