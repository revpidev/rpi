//! Port of `packages/tui/src/autocomplete.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - `getSuggestions` is synchronous (upstream async). The `AbortSignal` is
//!   replaced by an [`Arc<AtomicBool>`] abort flag (`GetSuggestionsOptions`),
//!   which the fd walk polls between read chunks and before spawning; the
//!   caller (Editor) sets it when the request is superseded. `getArgumentCompletions`
//!   is synchronous too (upstream `Awaitable`, i.e. also allowed to return a
//!   plain array — a rejected/settled promise is unrepresentable in Rust).
//! - `SlashCommand.getArgumentCompletions` returning a non-array is
//!   unrepresentable (the Rust type is `Option<Vec<AutocompleteItem>>`);
//!   upstream treats it as "no suggestions".
//! - `triggerCharacters` is `&[char]` (upstream `string[] | undefined`).
//! - The stderr pipe of the spawned fd is drained on a helper thread;
//!   upstream never reads it (observable output is identical, but a child
//!   blocked on a full stderr pipe can no longer hang the request).
//! - `entry.name.localeCompare` becomes bytewise `cmp` (identical for the
//!   ASCII labels used in practice).
//! - `os.homedir()` becomes `$HOME` (same value on Unix).

use std::fs;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::fuzzy::fuzzy_filter;

/// Path delimiters that terminate a completion token (`PATH_DELIMITERS`,
/// autocomplete.ts:7).
const PATH_DELIMITERS: [char; 5] = [' ', '\t', '"', '\'', '='];

/// `toDisplayPath` (autocomplete.ts:9-11).
fn to_display_path(value: &str) -> String {
    value.replace('\\', "/")
}

/// `escapeRegex` (autocomplete.ts:13-15).
fn escape_regex(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ".*+?^${}()|[]\\".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// `buildFdPathQuery` (autocomplete.ts:17-43).
fn build_fd_path_query(query: &str) -> String {
    let normalized = to_display_path(query);
    if !normalized.contains('/') {
        return normalized;
    }

    let has_trailing_separator = normalized.ends_with('/');
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return normalized;
    }

    let separator_pattern = "[\\\\/]";
    let segments: Vec<String> = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(escape_regex)
        .collect();
    if segments.is_empty() {
        return normalized;
    }

    let mut pattern = segments.join(separator_pattern);
    if has_trailing_separator {
        pattern.push_str(separator_pattern);
    }
    pattern
}

/// `findLastDelimiter` (autocomplete.ts:45-52): char index of the last path
/// delimiter, or `None`.
fn find_last_delimiter(text: &str) -> Option<usize> {
    text.char_indices()
        .rev()
        .find(|(_, ch)| PATH_DELIMITERS.contains(ch))
        .map(|(byte, _)| text[..byte].chars().count())
}

/// `findUnclosedQuoteStart` (autocomplete.ts:54-68): char index of the
/// opening `"` of an unclosed quoted section, or `None`.
fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut quote_start = 0usize;
    for (index, ch) in text.chars().enumerate() {
        if ch == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                quote_start = index;
            }
        }
    }
    in_quotes.then_some(quote_start)
}

/// `isTokenStart` (autocomplete.ts:70-72).
fn is_token_start(text: &str, index: usize) -> bool {
    index == 0
        || text
            .chars()
            .nth(index - 1)
            .is_none_or(|c| PATH_DELIMITERS.contains(&c))
}

/// `extractQuotedPrefix` (autocomplete.ts:74-92): the quoted (or `@`-quoted)
/// prefix at the end of `text`, or `None` when the text does not end inside
/// an unclosed quote.
fn extract_quoted_prefix(text: &str) -> Option<String> {
    let quote_start = find_unclosed_quote_start(text)?;

    if quote_start > 0 && text.chars().nth(quote_start - 1) == Some('@') {
        if !is_token_start(text, quote_start - 1) {
            return None;
        }
        return Some(text.chars().skip(quote_start - 1).collect());
    }

    if !is_token_start(text, quote_start) {
        return None;
    }
    Some(text.chars().skip(quote_start).collect())
}

/// `parsePathPrefix` (autocomplete.ts:94-105).
struct PathPrefix {
    raw_prefix: String,
    is_at_prefix: bool,
    is_quoted_prefix: bool,
}

fn parse_path_prefix(prefix: &str) -> PathPrefix {
    if let Some(raw) = prefix.strip_prefix("@\"") {
        return PathPrefix {
            raw_prefix: raw.to_string(),
            is_at_prefix: true,
            is_quoted_prefix: true,
        };
    }
    if let Some(raw) = prefix.strip_prefix('"') {
        return PathPrefix {
            raw_prefix: raw.to_string(),
            is_at_prefix: false,
            is_quoted_prefix: true,
        };
    }
    if let Some(raw) = prefix.strip_prefix('@') {
        return PathPrefix {
            raw_prefix: raw.to_string(),
            is_at_prefix: true,
            is_quoted_prefix: false,
        };
    }
    PathPrefix {
        raw_prefix: prefix.to_string(),
        is_at_prefix: false,
        is_quoted_prefix: false,
    }
}

/// `buildCompletionValue` (autocomplete.ts:107-121).
fn build_completion_value(path: &str, options: CompletionValueOptions) -> String {
    let needs_quotes = options.is_quoted_prefix || path.contains(' ');
    let prefix = if options.is_at_prefix { "@" } else { "" };

    if !needs_quotes {
        return format!("{prefix}{path}");
    }

    let open_quote = format!("{prefix}\"");
    format!("{open_quote}{path}\"")
}

#[derive(Clone, Copy)]
struct CompletionValueOptions {
    /// Carried for interface parity with the upstream options object
    /// (`buildCompletionValue` signature, autocomplete.ts:108); like
    /// upstream, the value is never read by the function.
    #[allow(dead_code)]
    is_directory: bool,
    is_at_prefix: bool,
    is_quoted_prefix: bool,
}

/// Node.js `path.join` semantics (POSIX): split on `/`, drop empty and `.`
/// segments, resolve `..` against the accumulated result (keeping leading
/// `..` for relative paths, clamping at root for absolute ones), then
/// re-join. Returns `"."` for the empty path.
fn node_join(parts: &[&str]) -> String {
    let is_absolute = parts.first().is_some_and(|part| part.starts_with('/'));
    let mut resolved: Vec<&str> = Vec::new();
    for part in parts {
        for segment in part.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    if resolved.is_empty() {
                        if !is_absolute {
                            resolved.push("..");
                        }
                    } else {
                        resolved.pop();
                    }
                }
                other => resolved.push(other),
            }
        }
    }
    let joined = if resolved.is_empty() {
        ".".to_string()
    } else {
        resolved.join("/")
    };
    if is_absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Node.js `path.dirname` semantics (POSIX).
fn node_dirname(path: &str) -> String {
    match path.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(index) => path[..index].to_string(),
    }
}

/// Node.js `path.basename` semantics (POSIX): trailing separators are
/// stripped first, so `basename("a/")` is `"a"`; a path of only separators
/// stays `"/"`.
fn node_basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        None => trimmed.to_string(),
        Some(index) => trimmed[index + 1..].to_string(),
    }
}

/// `os.homedir()` (autocomplete.ts:3): `$HOME` on Unix.
fn home_dir() -> String {
    std::env::var_os("HOME")
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Use fd to walk directory tree (fast, respects .gitignore). Upstream
/// `walkDirectoryWithFd` (autocomplete.ts:124-217), made synchronous: the
/// caller's abort flag is polled between read chunks, and the child is
/// killed as soon as the abort is observed (upstream `child.kill("SIGKILL")`).
/// Returns `(display_path, is_directory)` pairs.
fn walk_directory_with_fd(
    base_dir: &str,
    fd_path: &str,
    query: &str,
    max_results: usize,
    abort: &AtomicBool,
) -> Vec<(String, bool)> {
    if abort.load(Ordering::Relaxed) {
        return Vec::new();
    }

    let mut command = Command::new(fd_path);
    command
        .arg("--base-directory")
        .arg(base_dir)
        .arg("--max-results")
        .arg(max_results.to_string())
        .arg("--type")
        .arg("f")
        .arg("--type")
        .arg("d")
        .arg("--follow")
        .arg("--hidden")
        .arg("--exclude")
        .arg(".git")
        .arg("--exclude")
        .arg(".git/*")
        .arg("--exclude")
        .arg(".git/**");

    if to_display_path(query).contains('/') {
        command.arg("--full-path");
    }

    if !query.is_empty() {
        command.arg(build_fd_path_query(query));
    }

    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Vec::new(), // upstream `child.on("error")` → finish([])
    };

    // Drain stderr on a helper thread so a chatty fd cannot block on a full
    // pipe (upstream pipes it and never reads it; output is identical).
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
        });
    }

    let mut stdout = String::new();
    let mut aborted = false;
    if let Some(mut out) = child.stdout.take() {
        let mut buffer = [0u8; 4096];
        loop {
            match out.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    stdout.push_str(&String::from_utf8_lossy(&buffer[..n]));
                    if abort.load(Ordering::Relaxed) {
                        let _ = child.kill();
                        aborted = true;
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }
    let exit_ok = child.wait().map(|status| status.success()).unwrap_or(false);

    if aborted || !exit_ok || stdout.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();
    for line in stdout.trim().split('\n').filter(|line| !line.is_empty()) {
        let display_line = to_display_path(line);
        let has_trailing_separator = display_line.ends_with('/');
        let normalized_path = if has_trailing_separator {
            &display_line[..display_line.len() - 1]
        } else {
            display_line.as_str()
        };
        if normalized_path == ".git"
            || normalized_path.starts_with(".git/")
            || normalized_path.contains("/.git/")
        {
            continue;
        }

        results.push((display_line, has_trailing_separator));
    }

    results
}

/// A completion item (upstream `AutocompleteItem`, autocomplete.ts:219-223).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// Slash command with optional argument completions (upstream `SlashCommand`,
/// autocomplete.ts:227-234). `get_argument_completions` returns `None` when
/// no argument completion is available.
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub get_argument_completions: Option<ArgumentCompletionsFn>,
}

/// Argument completion callback (upstream `(argumentPrefix: string) =>
/// Awaitable<AutocompleteItem[] | null>`; synchronous in this port).
pub type ArgumentCompletionsFn = Box<dyn Fn(&str) -> Option<Vec<AutocompleteItem>> + Send + Sync>;

/// Suggestions plus the prefix they were computed for (upstream
/// `AutocompleteSuggestions`, autocomplete.ts:236-239).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteSuggestions {
    pub items: Vec<AutocompleteItem>,
    /// What we're matching against (e.g., "/" or "src/").
    pub prefix: String,
}

/// Options for [`AutocompleteProvider::get_suggestions`] (upstream
/// `{ signal: AbortSignal; force?: boolean }`, autocomplete.ts:251-252).
pub struct GetSuggestionsOptions {
    /// Abort flag: set by the caller when the request is superseded; the
    /// provider should stop work and return `None` promptly.
    pub abort: Arc<AtomicBool>,
    /// Explicit (Tab) trigger.
    pub force: bool,
}

/// Result of applying a completion (upstream return shape of
/// `applyCompletion`, autocomplete.ts:255-266).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

/// Autocomplete provider interface (upstream `AutocompleteProvider`,
/// autocomplete.ts:241-270). Synchronous in this port; the editor runs
/// `get_suggestions` on a worker thread when a debounce applies.
pub trait AutocompleteProvider: Send + Sync {
    /// Characters that should naturally trigger this provider at token
    /// boundaries (upstream `triggerCharacters?`).
    fn trigger_characters(&self) -> &[char];

    /// Get autocomplete suggestions for the current text/cursor position.
    /// Returns `None` when no suggestions are available.
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: &GetSuggestionsOptions,
    ) -> Option<AutocompleteSuggestions>;

    /// Apply the selected item, returning the new text and cursor position.
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult;

    /// Check if file completion should trigger for explicit Tab completion
    /// (upstream `shouldTriggerFileCompletion?`; default `true`).
    fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        let _ = (lines, cursor_line, cursor_col);
        true
    }
}

/// A slash command or a plain autocomplete item accepted by
/// [`CombinedAutocompleteProvider`] (upstream
/// `(SlashCommand | AutocompleteItem)[]`, autocomplete.ts:274).
pub enum SlashCommandOrItem {
    Command(SlashCommand),
    Item(AutocompleteItem),
}

/// Combined provider that handles both slash commands and file paths
/// (upstream `CombinedAutocompleteProvider`, autocomplete.ts:273).
pub struct CombinedAutocompleteProvider {
    commands: Vec<SlashCommandOrItem>,
    base_path: String,
    fd_path: Option<String>,
}

impl CombinedAutocompleteProvider {
    pub fn new(
        commands: Vec<SlashCommandOrItem>,
        base_path: String,
        fd_path: Option<String>,
    ) -> Self {
        Self {
            commands,
            base_path,
            fd_path,
        }
    }

    /// `getSuggestions` (autocomplete.ts:284-373).
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: &GetSuggestionsOptions,
    ) -> Option<AutocompleteSuggestions> {
        let current_line = lines.get(cursor_line).cloned().unwrap_or_default();
        let text_before_cursor =
            current_line[..char_to_byte(&current_line, cursor_col)].to_string();

        if let Some(at_prefix) = self.extract_at_prefix(&text_before_cursor) {
            let (raw_prefix, is_quoted_prefix) = {
                let parsed = parse_path_prefix(&at_prefix);
                (parsed.raw_prefix, parsed.is_quoted_prefix)
            };
            let suggestions =
                self.get_fuzzy_file_suggestions(&raw_prefix, is_quoted_prefix, &options.abort);
            if suggestions.is_empty() {
                return None;
            }
            return Some(AutocompleteSuggestions {
                items: suggestions,
                prefix: at_prefix,
            });
        }

        if !options.force && text_before_cursor.starts_with('/') {
            let Some(space_index) = text_before_cursor.find(' ') else {
                let prefix = text_before_cursor[1..].to_string();
                let command_items: Vec<CommandItem> = self
                    .commands
                    .iter()
                    .map(|cmd| {
                        let name = match cmd {
                            SlashCommandOrItem::Command(command) => command.name.clone(),
                            SlashCommandOrItem::Item(item) => item.value.clone(),
                        };
                        let hint = match cmd {
                            SlashCommandOrItem::Command(command) => command.argument_hint.clone(),
                            SlashCommandOrItem::Item(_) => None,
                        };
                        let desc = match cmd {
                            SlashCommandOrItem::Command(command) => {
                                command.description.clone().unwrap_or_default()
                            }
                            SlashCommandOrItem::Item(item) => {
                                item.description.clone().unwrap_or_default()
                            }
                        };
                        let full_desc = match hint {
                            Some(hint) if !desc.is_empty() => format!("{hint} — {desc}"),
                            Some(hint) => hint,
                            None => desc,
                        };
                        CommandItem {
                            label: name.clone(),
                            name,
                            description: (!full_desc.is_empty()).then_some(full_desc),
                        }
                    })
                    .collect();

                let filtered: Vec<AutocompleteItem> =
                    fuzzy_filter(command_items, &prefix, |item| item.name.clone())
                        .into_iter()
                        .map(|item| AutocompleteItem {
                            value: item.name,
                            label: item.label,
                            description: item.description,
                        })
                        .collect();

                if filtered.is_empty() {
                    return None;
                }
                return Some(AutocompleteSuggestions {
                    items: filtered,
                    prefix: text_before_cursor,
                });
            };

            let command_name = text_before_cursor[1..space_index].to_string();
            let argument_text = text_before_cursor[space_index + 1..].to_string();

            let command = self.commands.iter().find(|cmd| {
                let name = match cmd {
                    SlashCommandOrItem::Command(command) => command.name.as_str(),
                    SlashCommandOrItem::Item(item) => item.value.as_str(),
                };
                name == command_name
            });
            let Some(SlashCommandOrItem::Command(command)) = command else {
                return None;
            };
            let Some(get_argument_completions) = &command.get_argument_completions else {
                return None;
            };
            let argument_suggestions = get_argument_completions(&argument_text);
            let argument_suggestions = match argument_suggestions {
                Some(suggestions) if !suggestions.is_empty() => suggestions,
                _ => return None,
            };

            return Some(AutocompleteSuggestions {
                items: argument_suggestions,
                prefix: argument_text,
            });
        }

        let path_match = self.extract_path_prefix(&text_before_cursor, options.force);
        let path_match = path_match?;

        let suggestions = self.get_file_suggestions(&path_match);
        if suggestions.is_empty() {
            return None;
        }
        Some(AutocompleteSuggestions {
            items: suggestions,
            prefix: path_match,
        })
    }

    /// `applyCompletion` (autocomplete.ts:375-460).
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        let current_line = lines.get(cursor_line).cloned().unwrap_or_default();
        let before_prefix = current_line[..char_to_byte(
            &current_line,
            cursor_col.saturating_sub(prefix.chars().count()),
        )]
            .to_string();
        let after_cursor = current_line[char_to_byte(&current_line, cursor_col)..].to_string();
        let is_quoted_prefix = prefix.starts_with('"') || prefix.starts_with("@\"");
        let has_leading_quote_after_cursor = after_cursor.starts_with('"');
        let has_trailing_quote_in_item = item.value.ends_with('"');
        let adjusted_after_cursor: String =
            if is_quoted_prefix && has_trailing_quote_in_item && has_leading_quote_after_cursor {
                after_cursor[1..].to_string()
            } else {
                after_cursor
            };

        // Check if we're completing a slash command (prefix starts with "/"
        // but NOT a file path): slash commands are at the start of the line
        // and don't contain path separators after the first `/`.
        let is_slash_command = prefix.starts_with('/')
            && before_prefix.trim().is_empty()
            && !prefix[1..].contains('/');
        if is_slash_command {
            // This is a command name completion: `${beforePrefix}/${item.value}
            // ${adjustedAfterCursor}` (autocomplete.ts:396).
            let new_line = format!(
                "{before_prefix}/{item_value} {adjusted}",
                item_value = item.value,
                adjusted = adjusted_after_cursor
            );
            let mut new_lines = lines.to_vec();
            new_lines[cursor_line] = new_line;

            return CompletionResult {
                lines: new_lines,
                cursor_line,
                // +2 for "/" and space
                cursor_col: before_prefix.chars().count() + item.value.chars().count() + 2,
            };
        }

        // Check if we're completing a file attachment (prefix starts with
        // "@").
        if prefix.starts_with('@') {
            // Don't add space after directories so the user can continue
            // autocompleting.
            let is_directory = item.label.ends_with('/');
            let suffix = if is_directory { "" } else { " " };
            let new_line = format!(
                "{before_prefix}{}{}{}",
                item.value, suffix, adjusted_after_cursor
            );
            let mut new_lines = lines.to_vec();
            new_lines[cursor_line] = new_line;

            let has_trailing_quote = item.value.ends_with('"');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.chars().count() - 1
            } else {
                item.value.chars().count()
            };

            return CompletionResult {
                lines: new_lines,
                cursor_line,
                cursor_col: before_prefix.chars().count() + cursor_offset + suffix.chars().count(),
            };
        }

        // Check if we're in a slash command context (beforePrefix contains
        // "/command ").
        let text_before_cursor =
            current_line[..char_to_byte(&current_line, cursor_col)].to_string();
        if text_before_cursor.contains('/') && text_before_cursor.contains(' ') {
            // This is likely a command argument completion.
            let new_line = format!("{before_prefix}{}{}", item.value, adjusted_after_cursor);
            let mut new_lines = lines.to_vec();
            new_lines[cursor_line] = new_line;

            let is_directory = item.label.ends_with('/');
            let has_trailing_quote = item.value.ends_with('"');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.chars().count() - 1
            } else {
                item.value.chars().count()
            };

            return CompletionResult {
                lines: new_lines,
                cursor_line,
                cursor_col: before_prefix.chars().count() + cursor_offset,
            };
        }

        // For file paths, complete the path.
        let new_line = format!("{before_prefix}{}{}", item.value, adjusted_after_cursor);
        let mut new_lines = lines.to_vec();
        new_lines[cursor_line] = new_line;

        let is_directory = item.label.ends_with('/');
        let has_trailing_quote = item.value.ends_with('"');
        let cursor_offset = if is_directory && has_trailing_quote {
            item.value.chars().count() - 1
        } else {
            item.value.chars().count()
        };

        CompletionResult {
            lines: new_lines,
            cursor_line,
            cursor_col: before_prefix.chars().count() + cursor_offset,
        }
    }

    /// Extract @ prefix for fuzzy file suggestions
    /// (`extractAtPrefix`, autocomplete.ts:463-477).
    fn extract_at_prefix(&self, text: &str) -> Option<String> {
        if let Some(quoted_prefix) = extract_quoted_prefix(text) {
            if quoted_prefix.starts_with("@\"") {
                return Some(quoted_prefix);
            }
        }

        let last_delimiter_index = find_last_delimiter(text);
        let token_start = last_delimiter_index.map_or(0, |index| index + 1);

        if text.chars().nth(token_start) == Some('@') {
            return Some(text.chars().skip(token_start).collect());
        }

        None
    }

    /// Extract a path-like prefix from the text before cursor
    /// (`extractPathPrefix`, autocomplete.ts:480-507).
    fn extract_path_prefix(&self, text: &str, force_extract: bool) -> Option<String> {
        if let Some(quoted_prefix) = extract_quoted_prefix(text) {
            return Some(quoted_prefix);
        }

        let last_delimiter_index = find_last_delimiter(text);
        let path_prefix: String = match last_delimiter_index {
            None => text.to_string(),
            Some(index) => text.chars().skip(index + 1).collect(),
        };

        // For forced extraction (Tab key), always return something.
        if force_extract {
            return Some(path_prefix);
        }

        // For natural triggers, return if it looks like a path, ends with /,
        // starts with ~/, or .
        if path_prefix.contains('/')
            || path_prefix.starts_with('.')
            || path_prefix.starts_with("~/")
        {
            return Some(path_prefix);
        }

        // Return empty string only after a space (not for completely empty
        // text). Empty text should not trigger file suggestions — that's for
        // forced Tab completion.
        if path_prefix.is_empty() && text.ends_with(' ') {
            return Some(path_prefix);
        }

        None
    }

    /// Expand home directory (~/) to the actual home path
    /// (`expandHomePath`, autocomplete.ts:510-519).
    fn expand_home_path(&self, path: &str) -> String {
        if let Some(rest) = path.strip_prefix("~/") {
            let expanded = node_join(&[&home_dir(), rest]);
            // Preserve trailing slash if the original path had one.
            if path.ends_with('/') && !expanded.ends_with('/') {
                format!("{expanded}/")
            } else {
                expanded
            }
        } else if path == "~" {
            home_dir()
        } else {
            path.to_string()
        }
    }

    /// `resolveScopedFuzzyQuery` (autocomplete.ts:521-549).
    fn resolve_scoped_fuzzy_query(&self, raw_query: &str) -> Option<ScopedFuzzyQuery> {
        let normalized_query = to_display_path(raw_query);
        let slash_index = normalized_query.rfind('/')?;

        let display_base = normalized_query[..=slash_index].to_string();
        let query = normalized_query[slash_index + 1..].to_string();

        let base_dir = if display_base.starts_with("~/") {
            self.expand_home_path(&display_base)
        } else if display_base.starts_with('/') {
            display_base.clone()
        } else {
            node_join(&[&self.base_path, &display_base])
        };

        if !std::path::Path::new(&base_dir).is_dir() {
            return None;
        }

        Some(ScopedFuzzyQuery {
            base_dir,
            query,
            display_base,
        })
    }

    /// `scopedPathForDisplay` (autocomplete.ts:551-557).
    fn scoped_path_for_display(&self, display_base: &str, relative_path: &str) -> String {
        let normalized_relative_path = to_display_path(relative_path);
        if display_base == "/" {
            return format!("/{normalized_relative_path}");
        }
        format!(
            "{}{}",
            to_display_path(display_base),
            normalized_relative_path
        )
    }

    /// Get file/directory suggestions for a given path prefix
    /// (`getFileSuggestions`, autocomplete.ts:560-693).
    fn get_file_suggestions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let path_prefix = parse_path_prefix(prefix);
        let mut expanded_prefix = path_prefix.raw_prefix.clone();

        // Handle home directory expansion.
        if expanded_prefix.starts_with('~') {
            expanded_prefix = self.expand_home_path(&expanded_prefix);
        }

        let is_root_prefix = path_prefix.raw_prefix.is_empty()
            || matches!(
                path_prefix.raw_prefix.as_str(),
                "./" | "../" | "~" | "~/" | "/"
            )
            || (path_prefix.is_at_prefix && path_prefix.raw_prefix.is_empty());

        let (search_dir, search_prefix): (String, String) = if is_root_prefix {
            // Complete from the specified position.
            if path_prefix.raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                (expanded_prefix, String::new())
            } else {
                (
                    node_join(&[&self.base_path, &expanded_prefix]),
                    String::new(),
                )
            }
        } else if path_prefix.raw_prefix.ends_with('/') {
            // If prefix ends with /, show contents of that directory.
            if path_prefix.raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                (expanded_prefix, String::new())
            } else {
                (
                    node_join(&[&self.base_path, &expanded_prefix]),
                    String::new(),
                )
            }
        } else {
            // Split into directory and file prefix.
            let dir = node_dirname(&expanded_prefix);
            let file = node_basename(&expanded_prefix);
            if path_prefix.raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                (dir, file)
            } else {
                (node_join(&[&self.base_path, &dir]), file)
            }
        };

        let entries = match fs::read_dir(&search_dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(), // upstream catch → []
        };
        let mut suggestions: Vec<AutocompleteItem> = Vec::new();

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name
                .to_lowercase()
                .starts_with(&search_prefix.to_lowercase())
            {
                continue;
            }

            // Check if entry is a directory (or a symlink pointing to a
            // directory).
            let mut is_directory = entry
                .file_type()
                .map(|file_type| file_type.is_dir())
                .unwrap_or(false);
            if !is_directory
                && entry
                    .file_type()
                    .map(|file_type| file_type.is_symlink())
                    .unwrap_or(false)
            {
                // Broken symlink or permission error — treat as file.
                is_directory = fs::metadata(entry.path())
                    .map(|metadata| metadata.is_dir())
                    .unwrap_or(false);
            }

            let display_prefix = &path_prefix.raw_prefix;
            let relative_path: String = if display_prefix.ends_with('/') {
                // If prefix ends with /, append entry to the prefix.
                format!("{display_prefix}{name}")
            } else if display_prefix.contains('/') || display_prefix.contains('\\') {
                // Preserve ~/ format for home directory paths.
                if let Some(home_relative_dir) = display_prefix.strip_prefix("~/") {
                    let dir = node_dirname(home_relative_dir);
                    relative_home_path(&dir, &name)
                } else if display_prefix.starts_with('/') {
                    // Absolute path — construct properly.
                    let dir = node_dirname(display_prefix);
                    if dir == "/" {
                        format!("/{name}")
                    } else {
                        format!("{dir}/{name}")
                    }
                } else {
                    let mut relative = node_join(&[&node_dirname(display_prefix), &name]);
                    // path.join normalizes away ./ prefix, preserve it.
                    if display_prefix.starts_with("./") && !relative.starts_with("./") {
                        relative = format!("./{relative}");
                    }
                    relative
                }
            } else if display_prefix.starts_with('~') {
                // For standalone entries, preserve ~/ if the original prefix
                // was ~/.
                format!("~/{name}")
            } else {
                // For standalone entries.
                name.clone()
            };

            let relative_path = to_display_path(&relative_path);
            let path_value = if is_directory {
                format!("{relative_path}/")
            } else {
                relative_path.clone()
            };
            let value = build_completion_value(
                &path_value,
                CompletionValueOptions {
                    is_directory,
                    is_at_prefix: path_prefix.is_at_prefix,
                    is_quoted_prefix: path_prefix.is_quoted_prefix,
                },
            );

            suggestions.push(AutocompleteItem {
                value,
                label: format!("{name}{}", if is_directory { "/" } else { "" }),
                description: None,
            });
        }

        // Sort directories first, then alphabetically.
        suggestions.sort_by(|a, b| {
            let a_is_dir = a.value.ends_with('/');
            let b_is_dir = b.value.ends_with('/');
            if a_is_dir && !b_is_dir {
                return std::cmp::Ordering::Less;
            }
            if !a_is_dir && b_is_dir {
                return std::cmp::Ordering::Greater;
            }
            a.label.cmp(&b.label)
        });

        suggestions
    }

    /// Score an entry against the query (higher = better match); directories
    /// get a bonus to prioritize folders (`scoreEntry`, autocomplete.ts:697-717).
    fn score_entry(&self, file_path: &str, query: &str, is_directory: bool) -> i32 {
        let file_name = node_basename(file_path);
        let lower_file_name = file_name.to_lowercase();
        let lower_query = query.to_lowercase();

        let mut score = 0;

        // Exact filename match (highest).
        if lower_file_name == lower_query {
            score = 100;
        // Filename starts with query.
        } else if lower_file_name.starts_with(&lower_query) {
            score = 80;
        // Substring match in filename.
        } else if lower_file_name.contains(&lower_query) {
            score = 50;
        // Substring match in full path.
        } else if file_path.to_lowercase().contains(&lower_query) {
            score = 30;
        }

        // Directories get a bonus to appear first.
        if is_directory && score > 0 {
            score += 10;
        }

        score
    }

    /// Fuzzy file search using fd (fast, respects .gitignore)
    /// (`getFuzzyFileSuggestions`, autocomplete.ts:720-772).
    fn get_fuzzy_file_suggestions(
        &self,
        query: &str,
        is_quoted_prefix: bool,
        abort: &AtomicBool,
    ) -> Vec<AutocompleteItem> {
        if self.fd_path.is_none() || abort.load(Ordering::Relaxed) {
            return Vec::new();
        }

        let scoped_query = self.resolve_scoped_fuzzy_query(query);
        let fd_base_dir = scoped_query
            .as_ref()
            .map(|scoped| scoped.base_dir.clone())
            .unwrap_or_else(|| self.base_path.clone());
        let fd_query = scoped_query
            .as_ref()
            .map(|scoped| scoped.query.clone())
            .unwrap_or_else(|| query.to_string());
        let entries = walk_directory_with_fd(
            &fd_base_dir,
            self.fd_path.as_ref().expect("fd_path checked above"),
            &fd_query,
            100,
            abort,
        );
        if abort.load(Ordering::Relaxed) {
            return Vec::new();
        }

        let mut scored_entries: Vec<(String, bool, i32)> = entries
            .into_iter()
            .map(|(path, is_directory)| {
                let score = if fd_query.is_empty() {
                    1
                } else {
                    self.score_entry(&path, &fd_query, is_directory)
                };
                (path, is_directory, score)
            })
            .filter(|(_, _, score)| *score > 0)
            .collect();

        scored_entries.sort_by_key(|entry| std::cmp::Reverse(entry.2));
        let top_entries: Vec<(String, bool, i32)> = scored_entries.into_iter().take(20).collect();

        let mut suggestions: Vec<AutocompleteItem> = Vec::new();
        for (entry_path, is_directory, _) in top_entries {
            let path_without_slash = if is_directory {
                entry_path[..entry_path.len() - 1].to_string()
            } else {
                entry_path.clone()
            };
            let display_path = match &scoped_query {
                Some(scoped) => {
                    self.scoped_path_for_display(&scoped.display_base, &path_without_slash)
                }
                None => path_without_slash.clone(),
            };
            let entry_name = node_basename(&path_without_slash);
            let completion_path = if is_directory {
                format!("{display_path}/")
            } else {
                display_path.clone()
            };
            let value = build_completion_value(
                &completion_path,
                CompletionValueOptions {
                    is_directory,
                    is_at_prefix: true,
                    is_quoted_prefix,
                },
            );

            suggestions.push(AutocompleteItem {
                value,
                label: format!("{entry_name}{}", if is_directory { "/" } else { "" }),
                description: Some(display_path),
            });
        }

        suggestions
    }
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn trigger_characters(&self) -> &[char] {
        // Upstream leaves `triggerCharacters` undefined, so the editor falls
        // back to its default trigger characters (editor.ts:244).
        &[]
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: &GetSuggestionsOptions,
    ) -> Option<AutocompleteSuggestions> {
        self.get_suggestions(lines, cursor_line, cursor_col, options)
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        self.apply_completion(lines, cursor_line, cursor_col, item, prefix)
    }

    /// `shouldTriggerFileCompletion` (autocomplete.ts:775-785).
    fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        let current_line = lines.get(cursor_line).cloned().unwrap_or_default();
        let text_before_cursor =
            current_line[..char_to_byte(&current_line, cursor_col)].to_string();

        // Don't trigger if we're typing a slash command at the start of the
        // line.
        if text_before_cursor.trim().starts_with('/') && !text_before_cursor.trim().contains(' ') {
            return false;
        }

        true
    }
}

/// A scoped fuzzy query (upstream `resolveScopedFuzzyQuery` result shape).
struct ScopedFuzzyQuery {
    base_dir: String,
    query: String,
    display_base: String,
}

/// Slash command item for fuzzy filtering (`getSuggestions` mapping).
struct CommandItem {
    name: String,
    label: String,
    description: Option<String>,
}

/// `~/${dir === "." ? name : join(dir, name)}` (autocomplete.ts:640).
fn relative_home_path(dir: &str, name: &str) -> String {
    if dir == "." {
        format!("~/{name}")
    } else {
        format!("~/{}", node_join(&[dir, name]))
    }
}

/// Byte offset of the `chars`-th character; `chars` == char count maps to
/// `text.len()`.
fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// Resolve the `fd` executable like the upstream tests' `resolveFdPath()`
/// (autocomplete.test.ts:9-18). Used by the test suite to skip fd-dependent
/// cases when fd is not installed.
#[cfg(test)]
fn resolve_fd_path() -> Option<String> {
    let result = Command::new("which").arg("fd").output().ok()?;
    if !result.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&result.stdout);
    stdout.lines().next().map(|line| line.trim().to_string())
}

#[cfg(test)]
mod tests {
    //! Ports of `test/autocomplete.test.ts` @ pi 0.82.1 (2efa728), all 25
    //! cases. The 16 fd-backed cases run against the real `fd` binary and
    //! early-return when it is not installed, mirroring the upstream
    //! `describe("fd @ file suggestions", { skip: !isFdInstalled })`.

    use std::path::PathBuf;

    use super::*;

    /// `getSuggestions` helper (autocomplete.test.ts:49-55): a fresh
    /// never-aborted signal.
    fn get_suggestions(
        provider: &CombinedAutocompleteProvider,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<AutocompleteSuggestions> {
        provider.get_suggestions(
            lines,
            cursor_line,
            cursor_col,
            &GetSuggestionsOptions {
                abort: Arc::new(AtomicBool::new(false)),
                force,
            },
        )
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        /// `mkdtempSync(join(tmpdir(), prefix))`.
        fn new(prefix: &str) -> Self {
            let base = std::env::temp_dir();
            for attempt in 0..1000 {
                let candidate = base.join(format!("{prefix}-{}-{attempt}", std::process::id()));
                match fs::create_dir(&candidate) {
                    Ok(()) => return Self { path: candidate },
                    Err(_) if attempt < 999 => continue,
                    Err(error) => panic!("mkdtemp failed: {error}"),
                }
            }
            unreachable!()
        }

        /// `rmSync(root, { recursive: true, force: true })`.
        fn remove(&self) {
            let _ = fs::remove_dir_all(&self.path);
        }

        /// `setupFolder` (autocomplete.test.ts:25-37).
        fn setup_folder(&self, base_dir: &str, structure: &FolderStructure) {
            for dir in &structure.dirs {
                fs::create_dir_all(self.path.join(base_dir).join(dir))
                    .unwrap_or_else(|error| panic!("mkdir {dir}: {error}"));
            }
            for (file_path, contents) in &structure.files {
                let full_path = self.path.join(base_dir).join(file_path);
                fs::create_dir_all(full_path.parent().expect("file has a parent"))
                    .unwrap_or_else(|error| panic!("mkdir for {file_path}: {error}"));
                fs::write(&full_path, contents)
                    .unwrap_or_else(|error| panic!("write {file_path}: {error}"));
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            self.remove();
        }
    }

    #[derive(Default)]
    struct FolderStructure {
        dirs: Vec<String>,
        files: Vec<(String, String)>,
    }

    fn folder_structure(dirs: &[&str], files: &[(&str, &str)]) -> FolderStructure {
        FolderStructure {
            dirs: dirs.iter().map(|dir| dir.to_string()).collect(),
            files: files
                .iter()
                .map(|(path, contents)| (path.to_string(), contents.to_string()))
                .collect(),
        }
    }

    fn item_values(result: &Option<AutocompleteSuggestions>) -> Vec<String> {
        result
            .as_ref()
            .map(|result| result.items.iter().map(|item| item.value.clone()).collect())
            .unwrap_or_default()
    }

    // --- extractPathPrefix (no fd required) -------------------------------

    #[test]
    fn extracts_slash_from_hey_slash_when_forced() {
        let provider = CombinedAutocompleteProvider::new(Vec::new(), "/tmp".to_string(), None);
        let lines = vec!["hey /".to_string()];

        let result = get_suggestions(&provider, &lines, 0, 5, true);

        assert!(
            result.is_some(),
            "Should return suggestions for root directory"
        );
        if let Some(result) = result {
            assert_eq!(result.prefix, "/", "Prefix should be '/'");
        }
    }

    #[test]
    fn extracts_slash_a_from_slash_a_when_forced() {
        let provider = CombinedAutocompleteProvider::new(Vec::new(), "/tmp".to_string(), None);
        let lines = vec!["/A".to_string()];

        let result = get_suggestions(&provider, &lines, 0, 2, true);

        // This might be null if /A doesn't match anything, which is fine —
        // we're mainly testing that the prefix extraction works.
        if let Some(result) = result {
            assert_eq!(result.prefix, "/A", "Prefix should be '/A'");
        }
    }

    #[test]
    fn does_not_trigger_for_slash_commands() {
        let provider = CombinedAutocompleteProvider::new(Vec::new(), "/tmp".to_string(), None);
        let lines = vec!["/model".to_string()];

        let result = get_suggestions(&provider, &lines, 0, 6, true);

        assert!(result.is_none(), "Should not trigger for slash commands");
    }

    #[test]
    fn triggers_for_absolute_paths_after_slash_command_argument() {
        let provider = CombinedAutocompleteProvider::new(Vec::new(), "/tmp".to_string(), None);
        let lines = vec!["/command /".to_string()];

        let result = get_suggestions(&provider, &lines, 0, 10, true);

        assert!(
            result.is_some(),
            "Should trigger for absolute paths in command arguments"
        );
        if let Some(result) = result {
            assert_eq!(result.prefix, "/", "Prefix should be '/'");
        }
    }

    // --- fd @ file suggestions (skip when fd is not installed) ------------

    fn fd_path() -> Option<String> {
        resolve_fd_path()
    }

    #[test]
    fn returns_all_files_and_folders_for_empty_at_query() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(&["src"], &[("README.md", "readme")]),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let mut values = item_values(&result);
        values.sort();
        assert_eq!(values, vec!["@README.md".to_string(), "@src/".to_string()]);
    }

    #[test]
    fn matches_file_with_extension_in_query() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder("cwd", &folder_structure(&[], &[("file.txt", "content")]));

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@file.txt".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let values = item_values(&result);
        assert!(values.iter().any(|value| value == "@file.txt"));
    }

    #[test]
    fn filters_are_case_insensitive() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(&["src"], &[("README.md", "readme")]),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@re".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let mut values = item_values(&result);
        values.sort();
        assert_eq!(values, vec!["@README.md".to_string()]);
    }

    #[test]
    fn ranks_directories_before_files() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder("cwd", &folder_structure(&["src"], &[("src.txt", "text")]));

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@src".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let first_value = result
            .as_ref()
            .and_then(|result| result.items.first())
            .map(|item| item.value.clone());
        let has_src_file = result
            .as_ref()
            .is_some_and(|result| result.items.iter().any(|item| item.value == "@src.txt"));
        assert_eq!(first_value.as_deref(), Some("@src/"));
        assert!(has_src_file);
    }

    #[test]
    fn returns_nested_file_paths() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(&[], &[("src/index.ts", "export {};\n")]),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@index".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let values = item_values(&result);
        assert!(values.iter().any(|value| value == "@src/index.ts"));
    }

    #[test]
    fn matches_deeply_nested_paths() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(
                &[],
                &[
                    ("packages/tui/src/autocomplete.ts", "export {};"),
                    ("packages/ai/src/autocomplete.ts", "export {};"),
                ],
            ),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@tui/src/auto".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let values = item_values(&result);
        assert!(values
            .iter()
            .any(|value| value == "@packages/tui/src/autocomplete.ts"));
        assert!(!values
            .iter()
            .any(|value| value == "@packages/ai/src/autocomplete.ts"));
    }

    #[test]
    fn matches_directory_in_middle_of_path_with_full_path() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(
                &[],
                &[
                    ("src/components/Button.tsx", "export {};"),
                    ("src/utils/helpers.ts", "export {};"),
                ],
            ),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@components/".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let values = item_values(&result);
        assert!(values
            .iter()
            .any(|value| value == "@src/components/Button.tsx"));
        assert!(!values.iter().any(|value| value == "@src/utils/helpers.ts"));
    }

    #[test]
    fn scopes_fuzzy_search_to_relative_directories_and_searches_recursively() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        let outside_dir = temp.path.join("outside").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        temp.setup_folder(
            "outside",
            &folder_structure(
                &[],
                &[
                    ("nested/alpha.ts", "export {};"),
                    ("nested/deeper/also-alpha.ts", "export {};"),
                    ("nested/deeper/zzz.ts", "export {};"),
                ],
            ),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@../outside/a".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let values = item_values(&result);
        assert!(values
            .iter()
            .any(|value| value == "@../outside/nested/alpha.ts"));
        assert!(values
            .iter()
            .any(|value| value == "@../outside/nested/deeper/also-alpha.ts"));
        assert!(!values
            .iter()
            .any(|value| value == "@../outside/nested/deeper/zzz.ts"));
    }

    #[test]
    fn quotes_paths_with_spaces_for_at_suggestions() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(&["my folder"], &[("my folder/test.txt", "content")]),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@my".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let values = item_values(&result);
        assert!(values.iter().any(|value| value == "@\"my folder/\""));
    }

    #[test]
    fn includes_hidden_paths_but_excludes_git() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(
                &[".pi", ".github", ".git"],
                &[
                    (".pi/config.json", "{}"),
                    (".github/workflows/ci.yml", "name: ci"),
                    (".git/config", "[core]"),
                ],
            ),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let values = item_values(&result);
        assert!(values.iter().any(|value| value == "@.pi/"));
        assert!(values.iter().any(|value| value == "@.github/"));
        assert!(!values
            .iter()
            .any(|value| value == "@.git" || value.starts_with("@.git/")));
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinked_directories_for_fuzzy_at_search() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        let outside_dir = temp.path.join("outside").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(&[], &[("dir/some_file.txt", "real")]),
        );
        temp.setup_folder(
            "outside",
            &folder_structure(&[], &[("some_file.txt", "symlinked")]),
        );
        std::os::unix::fs::symlink("../outside", temp.path.join("cwd/symlinked_dir")).unwrap();

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@some".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let values = item_values(&result);
        assert!(values.iter().any(|value| value == "@dir/some_file.txt"));
        assert!(values
            .iter()
            .any(|value| value == "@symlinked_dir/some_file.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn returns_symlinked_directories_when_matching_their_name() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        let outside_dir = temp.path.join("outside").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        temp.setup_folder(
            "outside",
            &folder_structure(&[], &[("nested/file.txt", "symlinked")]),
        );
        std::os::unix::fs::symlink("../outside", temp.path.join("cwd/symlinked_dir")).unwrap();

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@symlinked".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let values = item_values(&result);
        assert!(values.iter().any(|value| value == "@symlinked_dir/"));
    }

    #[cfg(unix)]
    #[test]
    fn returns_symlinked_files_without_requiring_type_l() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(&[], &[("original.txt", "content")]),
        );
        std::os::unix::fs::symlink("original.txt", temp.path.join("cwd/link.txt")).unwrap();

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@link".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            false,
        );

        let values = item_values(&result);
        assert!(values.iter().any(|value| value == "@link.txt"));
    }

    #[test]
    fn returns_the_same_at_suggestions_when_the_cwd_path_contains_the_query() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let normal_base_dir = temp.path.join("cwd-normal").to_string_lossy().into_owned();
        let query_in_path_base_dir = temp
            .path
            .join("cwd-plan-repro")
            .to_string_lossy()
            .into_owned();
        fs::create_dir_all(&normal_base_dir).unwrap();
        fs::create_dir_all(&query_in_path_base_dir).unwrap();

        let structure = folder_structure(
            &["packages/coding-agent/examples/extensions/plan-mode"],
            &[
                (
                    "packages/coding-agent/examples/extensions/plan-mode/README.md",
                    "readme",
                ),
                ("packages/tui/docs/plan.md", "plan"),
            ],
        );
        temp.setup_folder("cwd-normal", &structure);
        temp.setup_folder("cwd-plan-repro", &structure);

        let query = "@plan".to_string();
        let normal_provider =
            CombinedAutocompleteProvider::new(Vec::new(), normal_base_dir, Some(fd_path.clone()));
        let query_in_path_provider =
            CombinedAutocompleteProvider::new(Vec::new(), query_in_path_base_dir, Some(fd_path));

        let normal_result = get_suggestions(
            &normal_provider,
            std::slice::from_ref(&query),
            0,
            query.chars().count(),
            false,
        );
        let query_in_path_result = get_suggestions(
            &query_in_path_provider,
            std::slice::from_ref(&query),
            0,
            query.chars().count(),
            false,
        );

        let normalize = |result: &Option<AutocompleteSuggestions>| -> Vec<String> {
            let mut entries: Vec<String> = result
                .as_ref()
                .map(|result| {
                    result
                        .items
                        .iter()
                        .map(|item| {
                            format!(
                                "{} :: {}",
                                item.label,
                                item.description.clone().unwrap_or_default()
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            entries.sort();
            entries
        };

        assert_eq!(normalize(&query_in_path_result), normalize(&normal_result));
        let normal_entries = normalize(&normal_result);
        assert!(normal_entries
            .iter()
            .any(|entry| entry
                == "plan-mode/ :: packages/coding-agent/examples/extensions/plan-mode"));
        assert!(normal_entries
            .iter()
            .any(|entry| entry == "plan.md :: packages/tui/docs/plan.md"));
    }

    #[test]
    fn continues_autocomplete_inside_quoted_at_paths() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(
                &[],
                &[
                    ("my folder/test.txt", "content"),
                    ("my folder/other.txt", "content"),
                ],
            ),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@\"my folder/\"".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count() - 1,
            false,
        );

        assert!(
            result.is_some(),
            "Should return suggestions for quoted folder path"
        );
        let values = item_values(&result);
        assert!(values
            .iter()
            .any(|value| value == "@\"my folder/test.txt\""));
        assert!(values
            .iter()
            .any(|value| value == "@\"my folder/other.txt\""));
    }

    #[test]
    fn applies_quoted_at_completion_without_duplicating_closing_quote() {
        let Some(fd_path) = fd_path() else {
            eprintln!("skipping: fd is not installed");
            return;
        };
        let temp = TempDir::new("pi-autocomplete-root");
        let base_dir = temp.path.join("cwd").to_string_lossy().into_owned();
        fs::create_dir_all(&base_dir).unwrap();
        temp.setup_folder(
            "cwd",
            &folder_structure(&[], &[("my folder/test.txt", "content")]),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, Some(fd_path));
        let line = "@\"my folder/te\"".to_string();
        let cursor_col = line.chars().count() - 1;
        let result = get_suggestions(&provider, std::slice::from_ref(&line), 0, cursor_col, false);

        assert!(
            result.is_some(),
            "Should return suggestions for quoted @ path"
        );
        let item = result
            .as_ref()
            .and_then(|result| {
                result
                    .items
                    .iter()
                    .find(|entry| entry.value == "@\"my folder/test.txt\"")
            })
            .cloned();
        assert!(item.is_some(), "Should find test.txt suggestion");

        let applied = provider.apply_completion(
            std::slice::from_ref(&line),
            0,
            cursor_col,
            item.as_ref().expect("checked above"),
            &result.as_ref().expect("checked above").prefix,
        );
        assert_eq!(applied.lines[0], "@\"my folder/test.txt\" ");
    }

    // --- dot-slash path completion (no fd required) -----------------------

    #[test]
    fn preserves_dot_slash_prefix_when_completing_paths() {
        let temp = TempDir::new("pi-autocomplete");
        let base_dir = temp.path.to_string_lossy().into_owned();
        temp.setup_folder(
            "",
            &folder_structure(
                &[],
                &[("update.sh", "#!/bin/bash"), ("utils.ts", "export {};")],
            ),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, None);
        let line = "./up".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            true,
        );

        assert!(result.is_some(), "Should return suggestions for ./ path");
        let values = item_values(&result);
        assert!(
            values.iter().any(|value| value == "./update.sh"),
            "Expected ./update.sh in {values:?}"
        );
    }

    #[test]
    fn preserves_dot_slash_prefix_for_directory_completions() {
        let temp = TempDir::new("pi-autocomplete");
        let base_dir = temp.path.to_string_lossy().into_owned();
        temp.setup_folder(
            "",
            &folder_structure(&["src"], &[("src/index.ts", "export {};")]),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, None);
        let line = "./sr".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            true,
        );

        assert!(
            result.is_some(),
            "Should return suggestions for ./ directory path"
        );
        let values = item_values(&result);
        assert!(
            values.iter().any(|value| value == "./src/"),
            "Expected ./src/ in {values:?}"
        );
    }

    // --- quoted path completion (no fd required) --------------------------

    #[test]
    fn quotes_paths_with_spaces_for_direct_completion() {
        let temp = TempDir::new("pi-autocomplete");
        let base_dir = temp.path.to_string_lossy().into_owned();
        temp.setup_folder(
            "",
            &folder_structure(&["my folder"], &[("my folder/test.txt", "content")]),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, None);
        let line = "my".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count(),
            true,
        );

        assert!(
            result.is_some(),
            "Should return suggestions for path completion"
        );
        let values = item_values(&result);
        assert!(
            values.iter().any(|value| value == "\"my folder/\""),
            "Expected quoted path in {values:?}"
        );
    }

    #[test]
    fn continues_completion_inside_quoted_paths() {
        let temp = TempDir::new("pi-autocomplete");
        let base_dir = temp.path.to_string_lossy().into_owned();
        temp.setup_folder(
            "",
            &folder_structure(
                &[],
                &[
                    ("my folder/test.txt", "content"),
                    ("my folder/other.txt", "content"),
                ],
            ),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, None);
        let line = "\"my folder/\"".to_string();
        let result = get_suggestions(
            &provider,
            std::slice::from_ref(&line),
            0,
            line.chars().count() - 1,
            true,
        );

        assert!(
            result.is_some(),
            "Should return suggestions for quoted folder path"
        );
        let values = item_values(&result);
        assert!(values.iter().any(|value| value == "\"my folder/test.txt\""));
        assert!(values
            .iter()
            .any(|value| value == "\"my folder/other.txt\""));
    }

    #[test]
    fn applies_quoted_completion_without_duplicating_closing_quote() {
        let temp = TempDir::new("pi-autocomplete");
        let base_dir = temp.path.to_string_lossy().into_owned();
        temp.setup_folder(
            "",
            &folder_structure(&[], &[("my folder/test.txt", "content")]),
        );

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base_dir, None);
        let line = "\"my folder/te\"".to_string();
        let cursor_col = line.chars().count() - 1;
        let result = get_suggestions(&provider, std::slice::from_ref(&line), 0, cursor_col, true);

        assert!(
            result.is_some(),
            "Should return suggestions for quoted path"
        );
        let item = result
            .as_ref()
            .and_then(|result| {
                result
                    .items
                    .iter()
                    .find(|entry| entry.value == "\"my folder/test.txt\"")
            })
            .cloned();
        assert!(item.is_some(), "Should find test.txt suggestion");

        let applied = provider.apply_completion(
            std::slice::from_ref(&line),
            0,
            cursor_col,
            item.as_ref().expect("checked above"),
            &result.as_ref().expect("checked above").prefix,
        );
        assert_eq!(applied.lines[0], "\"my folder/test.txt\"");
    }
}
