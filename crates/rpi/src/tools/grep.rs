//! Port of `packages/coding-agent/src/core/tools/grep.ts` @ pi 0.82.1 (2efa728).
//!
//! Native Rust implementation of the ripgrep-backed grep tool (ADR-0003 §2):
//! upstream shells out to `rg --json --line-number --color=never --hidden`;
//! this port reproduces the same observable behavior with the `ignore` +
//! `regex` crates and downloads no external binary (gate G4).
//!
//! rg semantics replicated (verified against ripgrep 15):
//! - `--hidden`: hidden files are searched, including `.git` contents.
//! - gitignore rules apply only inside git repositories (rg's implicit
//!   `--require-git` default); `.ignore` files always apply.
//! - A whitelist `--glob` overrides file-level ignore rules (rg implements
//!   `--glob` through the ignore crate's overrides); gitignored directories
//!   are still pruned from the walk (verified against ripgrep 15). Glob
//!   patterns are anchored at the tool cwd, matching rg's process-cwd
//!   anchoring.
//! - An explicitly named file bypasses ignore/glob/hidden filters entirely.
//! - In directory walks, files containing NUL bytes are treated as binary and
//!   their matches suppressed; explicitly named files are searched regardless.
//! - Walk/read errors fail the whole search (rg exit code 2), unless the match
//!   limit was already reached (`killedDueToLimit` upstream).
//!
//! TUI rendering methods (`renderCall`, `renderResult`) are intentionally
//! omitted — rendering lives in the TUI layer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, AgentToolUpdateCallback};
use rpi_ai::types::{TextContent, ToolResultContent};
use regex::RegexBuilder;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use crate::tools::path_utils::resolve_to_cwd;
use crate::tools::truncate::{
    format_size, truncate_head, truncate_line, TruncateOptions, DEFAULT_MAX_BYTES,
    GREP_MAX_LINE_LENGTH,
};
use crate::tools::ToolContext;

/// Default maximum number of matches (grep.ts:39).
const DEFAULT_LIMIT: f64 = 100.0;

// ---------------------------------------------------------------------------
// GrepOperations (grep.ts:51-61)
// ---------------------------------------------------------------------------

/// Pluggable operations for the grep tool.
///
/// Override these to delegate search to remote systems (for example SSH).
/// Only the directory check and the context-line file read are pluggable,
/// matching upstream — the search itself is upstream's rg spawn and is the
/// native walker here.
#[async_trait]
pub trait GrepOperations: Send + Sync {
    /// Check if path is a directory. Errors if the path does not exist.
    async fn is_directory(&self, absolute_path: &Path) -> std::io::Result<bool>;

    /// Read file contents for context lines.
    async fn read_file(&self, absolute_path: &Path) -> std::io::Result<String>;
}

/// Default local-filesystem implementation of [`GrepOperations`]
/// (grep.ts:58-61).
pub struct LocalGrepOperations;

#[async_trait]
impl GrepOperations for LocalGrepOperations {
    async fn is_directory(&self, absolute_path: &Path) -> std::io::Result<bool> {
        Ok(tokio::fs::metadata(absolute_path).await?.is_dir())
    }

    async fn read_file(&self, absolute_path: &Path) -> std::io::Result<String> {
        // fsReadFile(p, "utf-8") — invalid sequences become U+FFFD.
        let bytes = tokio::fs::read(absolute_path).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

// ---------------------------------------------------------------------------
// GrepToolOptions (grep.ts:63-66)
// ---------------------------------------------------------------------------

/// Options for creating a grep tool instance.
#[derive(Default)]
pub struct GrepToolOptions {
    /// Custom operations for grep. Default: local filesystem.
    pub operations: Option<Arc<dyn GrepOperations>>,
}

// ---------------------------------------------------------------------------
// createGrepTool (grep.ts:123-135, 383-385)
// ---------------------------------------------------------------------------

/// Create a grep tool bound to the given context.
pub fn create_grep_tool(ctx: &ToolContext, options: GrepToolOptions) -> Arc<dyn AgentTool> {
    let operations = options
        .operations
        .unwrap_or_else(|| Arc::new(LocalGrepOperations));
    Arc::new(GrepTool {
        cwd: ctx.cwd.clone(),
        operations,
    })
}

// ---------------------------------------------------------------------------
// GrepTool
// ---------------------------------------------------------------------------

struct GrepTool {
    cwd: PathBuf,
    operations: Arc<dyn GrepOperations>,
}

/// Tool description with constants expanded (grep.ts:131).
const DESCRIPTION: &str = "Search file contents for a pattern. Returns matching lines with \
file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB \
(whichever is hit first). Long lines are truncated to 500 chars.";

/// Format a number for display in notices (JS `${limit}` semantics).
fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// A single matching line collected during the search phase.
struct GrepMatch {
    file_path: PathBuf,
    line_number: usize,
    /// Raw line text without the trailing `\n` (a trailing `\r` is kept, like
    /// rg's `lines.text` without `--crlf`).
    line_text: String,
}

/// Outcome of the blocking search phase.
struct SearchOutcome {
    matches: Vec<GrepMatch>,
    /// The match limit was reached and the search stopped early
    /// (`matchLimitReached` / `killedDueToLimit`, grep.ts:287-290).
    match_limit_reached: bool,
    /// Walk/read error messages (rg's stderr lines).
    errors: Vec<String>,
    /// Cancelled via the abort signal mid-walk.
    aborted: bool,
}

/// Search one file's contents (streaming; T14 review: the previous
/// whole-file `std::fs::read` + lossy decode held ~2× the file size in
/// memory — rg streams and so do we now). Lines are accumulated as raw
/// bytes and lossy-decoded whole, so a multi-byte character split across
/// read chunks decodes exactly like the whole-file decode did. A line
/// without a trailing newline is still searched (`split_terminator`
/// semantics); a trailing newline produces no phantom line. Note: the
/// longest single line is held in memory while it is searched (rg can
/// stream it) — a pathological newline-free file still needs its full
/// content buffered; output truncation caps what is returned either way.
///
/// `suppress_binary`: in a directory walk rg reports no matches for files
/// containing NUL bytes; an explicitly named file is searched as-is (no
/// binary suppression, grep.ts passes the file straight to rg).
///
/// Returns `true` when the limit was reached (search must stop).
fn search_stream(
    re: &regex::Regex,
    file_path: &Path,
    reader: &mut impl std::io::Read,
    limit: f64,
    suppress_binary: bool,
    matches: &mut Vec<GrepMatch>,
) -> std::io::Result<bool> {
    const CHUNK_SIZE: usize = 64 * 1024;
    let mut chunk = vec![0u8; CHUNK_SIZE];
    let mut line: Vec<u8> = Vec::new();
    let mut line_number = 0usize;
    let start_len = matches.len();
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        let data = &chunk[..n];
        // Binary suppression: rg reports no matches at all for a file with
        // any NUL byte (D-039 #4) — matches already collected for this file
        // are rolled back.
        if suppress_binary && data.contains(&0) {
            matches.truncate(start_len);
            return Ok(false);
        }
        let mut start = 0;
        for (index, byte) in data.iter().enumerate() {
            if *byte == b'\n' {
                line.extend_from_slice(&data[start..index]);
                line_number += 1;
                let text = String::from_utf8_lossy(&line);
                if re.is_match(&text) {
                    matches.push(GrepMatch {
                        file_path: file_path.to_path_buf(),
                        line_number,
                        line_text: text.into_owned(),
                    });
                    if matches.len() as f64 >= limit {
                        return Ok(true);
                    }
                }
                line.clear();
                start = index + 1;
            }
        }
        line.extend_from_slice(&data[start..]);
    }
    if !line.is_empty() {
        line_number += 1;
        let text = String::from_utf8_lossy(&line);
        if re.is_match(&text) {
            matches.push(GrepMatch {
                file_path: file_path.to_path_buf(),
                line_number,
                line_text: text.into_owned(),
            });
            if matches.len() as f64 >= limit {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Run the rg-equivalent search (blocking; call inside `spawn_blocking`).
fn run_search(
    re: &regex::Regex,
    search_path: &Path,
    is_directory: bool,
    cwd: &Path,
    glob: Option<&str>,
    limit: f64,
    signal: &CancellationToken,
) -> Result<SearchOutcome, AgentError> {
    let mut outcome = SearchOutcome {
        matches: Vec::new(),
        match_limit_reached: false,
        errors: Vec::new(),
        aborted: false,
    };

    if !is_directory {
        // Explicitly named file: rg searches it unconditionally — no
        // ignore/glob/hidden filtering, no binary suppression (grep.ts passes
        // the file straight to rg).
        match std::fs::File::open(search_path) {
            Ok(file) => {
                let mut reader = std::io::BufReader::new(file);
                match search_stream(
                    re,
                    search_path,
                    &mut reader,
                    limit,
                    false,
                    &mut outcome.matches,
                ) {
                    Ok(true) => outcome.match_limit_reached = true,
                    Ok(false) => {}
                    Err(e) => outcome
                        .errors
                        .push(format!("{}: {e}", search_path.display())),
                }
            }
            Err(e) => outcome
                .errors
                .push(format!("{}: {e}", search_path.display())),
        }
        return Ok(outcome);
    }

    let mut builder = WalkBuilder::new(search_path);
    builder
        // rg --hidden: search hidden files and directories.
        .hidden(false)
        // rg default: no symlink following.
        .follow_links(false)
        // rg default: gitignore rules apply only inside git repositories.
        .require_git(true);
    if let Some(glob) = glob {
        // rg --glob is implemented upstream through the same override
        // mechanism: a whitelist glob overrides every other ignore rule.
        // Patterns anchor at the process cwd; the tool cwd stands in for it.
        let mut overrides = OverrideBuilder::new(cwd);
        overrides
            .add(glob)
            .map_err(|e| AgentError::Message(e.to_string()))?;
        let overrides = overrides
            .build()
            .map_err(|e| AgentError::Message(e.to_string()))?;
        builder.overrides(overrides);
    }

    for result in builder.build() {
        if signal.is_cancelled() {
            outcome.aborted = true;
            return Ok(outcome);
        }
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => {
                outcome.errors.push(e.to_string());
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let is_file = entry.file_type().is_some_and(|ft| ft.is_file());
        if !is_file {
            continue;
        }
        match std::fs::File::open(entry.path()) {
            Ok(file) => {
                let mut reader = std::io::BufReader::new(file);
                match search_stream(
                    re,
                    entry.path(),
                    &mut reader,
                    limit,
                    true,
                    &mut outcome.matches,
                ) {
                    Ok(true) => {
                        outcome.match_limit_reached = true;
                        return Ok(outcome);
                    }
                    Ok(false) => {}
                    Err(e) => outcome
                        .errors
                        .push(format!("{}: {e}", entry.path().display())),
                }
            }
            Err(e) => outcome
                .errors
                .push(format!("{}: {e}", entry.path().display())),
        }
    }
    Ok(outcome)
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn label(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        // TypeBox Type.Object with additionalProperties: false (grep.ts:24-36).
        static PARAMETERS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        PARAMETERS.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Search pattern (regex or literal string)"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory or file to search (default: current directory)"
                    },
                    "glob": {
                        "type": "string",
                        "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"
                    },
                    "ignoreCase": {
                        "type": "boolean",
                        "description": "Case-insensitive search (default: false)"
                    },
                    "literal": {
                        "type": "boolean",
                        "description": "Treat pattern as literal string instead of regex (default: false)"
                    },
                    "context": {
                        "type": "number",
                        "description": "Number of lines to show before and after each match (default: 0)"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Maximum number of matches to return (default: 100)"
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            })
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, AgentError> {
        // --- Extract parameters (grep.ts:136-152) ---
        let pattern = params["pattern"].as_str().ok_or_else(|| {
            AgentError::Message("Missing required parameter: pattern".to_string())
        })?;
        let search_dir = params["path"].as_str();
        let glob = params["glob"].as_str();
        let ignore_case = params["ignoreCase"].as_bool().unwrap_or(false);
        let literal = params["literal"].as_bool().unwrap_or(false);
        let context = params["context"].as_f64();
        let limit = params["limit"].as_f64();

        // --- Abort check at entry (grep.ts:158-161) ---
        if signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }

        let search_path = resolve_to_cwd(search_dir.unwrap_or("."), &self.cwd);
        let is_directory = self
            .operations
            .is_directory(&search_path)
            .await
            .map_err(|_| {
                AgentError::Message(format!("Path not found: {}", search_path.display()))
            })?;

        // context && context > 0 ? context : 0 (grep.ts:188).
        let context_value = match context {
            Some(c) if c > 0.0 => c,
            _ => 0.0,
        };
        // Math.max(1, limit ?? DEFAULT_LIMIT) (grep.ts:189).
        let effective_limit = limit.unwrap_or(DEFAULT_LIMIT).max(1.0);

        // --- Build the matcher (rg argv: --ignore-case / --fixed-strings) ---
        let pattern = if literal {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|e| AgentError::Message(e.to_string()))?;

        // --- Search phase (blocking walk, mirrors the rg subprocess) ---
        let search_path_block = search_path.clone();
        let cwd = self.cwd.clone();
        let glob_owned = glob.map(str::to_owned);
        let signal_clone = signal.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            run_search(
                &re,
                &search_path_block,
                is_directory,
                &cwd,
                glob_owned.as_deref(),
                effective_limit,
                &signal_clone,
            )
        })
        .await
        .map_err(|e| AgentError::Message(format!("grep search task failed: {e}")))??;

        if outcome.aborted || signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }
        // rg exit code 2 → reject with stderr, unless the limit kill fired
        // (grep.ts:298-307).
        if !outcome.match_limit_reached && !outcome.errors.is_empty() {
            return Err(AgentError::Message(outcome.errors.join("\n")));
        }

        if outcome.matches.is_empty() {
            return Ok(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent {
                    text: "No matches found".to_string(),
                    text_signature: None,
                })],
                details: Value::Null,
                usage: None,
                added_tool_names: None,
                terminate: None,
            });
        }

        // --- formatPath (grep.ts:190-198) ---
        let format_path = |file_path: &Path| -> String {
            if is_directory {
                if let Ok(relative) = file_path.strip_prefix(&search_path) {
                    let relative = relative.to_string_lossy().replace('\\', "/");
                    if !relative.is_empty() && !relative.starts_with("..") {
                        return relative;
                    }
                }
            }
            file_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file_path.to_string_lossy().into_owned())
        };

        // --- Format matches after the search (grep.ts:316-331) ---
        let mut lines_truncated = false;
        let mut output_lines: Vec<String> = Vec::new();
        let mut file_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();

        for m in &outcome.matches {
            if context_value == 0.0 {
                let relative_path = format_path(&m.file_path);
                // grep.ts:320-323: strip \r\n → \n, then all \r, then a
                // trailing \n (our line_text never contains \n).
                let sanitized = m.line_text.replace('\r', "");
                let truncated = truncate_line(&sanitized, None);
                if truncated.was_truncated {
                    lines_truncated = true;
                }
                output_lines.push(format!(
                    "{relative_path}:{}: {}",
                    m.line_number, truncated.text
                ));
            } else {
                // formatBlock (grep.ts:250-268): re-read the file for context.
                let relative_path = format_path(&m.file_path);
                let lines = match file_cache.get(&m.file_path) {
                    Some(lines) => lines.clone(),
                    None => {
                        let lines = match self.operations.read_file(&m.file_path).await {
                            Ok(content) => content
                                .replace("\r\n", "\n")
                                .replace('\r', "\n")
                                .split('\n')
                                .map(str::to_owned)
                                .collect::<Vec<String>>(),
                            Err(_) => Vec::new(),
                        };
                        file_cache.insert(m.file_path.clone(), lines.clone());
                        lines
                    }
                };
                if lines.is_empty() {
                    output_lines.push(format!(
                        "{relative_path}:{}: (unable to read file)",
                        m.line_number
                    ));
                    continue;
                }
                let context = context_value as usize;
                let start = m.line_number.saturating_sub(context).max(1);
                let end = (m.line_number + context).min(lines.len());
                for current in start..=end {
                    let line_text = lines.get(current - 1).map_or("", String::as_str);
                    let sanitized = line_text.replace('\r', "");
                    let truncated = truncate_line(&sanitized, None);
                    if truncated.was_truncated {
                        lines_truncated = true;
                    }
                    if current == m.line_number {
                        output_lines.push(format!("{relative_path}:{current}: {}", truncated.text));
                    } else {
                        output_lines.push(format!("{relative_path}-{current}- {}", truncated.text));
                    }
                }
            }
        }

        // --- Byte truncation + notices (grep.ts:333-362) ---
        let raw_output = output_lines.join("\n");
        let truncation = truncate_head(
            &raw_output,
            Some(TruncateOptions {
                max_lines: usize::MAX,
                max_bytes: DEFAULT_MAX_BYTES,
            }),
        );
        let mut output = truncation.content.clone();
        let mut details = Map::new();
        let mut notices: Vec<String> = Vec::new();
        if outcome.match_limit_reached {
            notices.push(format!(
                "{} matches limit reached. Use limit={} for more, or refine pattern",
                format_number(effective_limit),
                format_number(effective_limit * 2.0)
            ));
            // JS numbers serialise without a trailing ".0" for integers.
            let limit_value = if effective_limit.fract() == 0.0 {
                json!(effective_limit as i64)
            } else {
                json!(effective_limit)
            };
            details.insert("matchLimitReached".to_string(), limit_value);
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            details.insert(
                "truncation".to_string(),
                serde_json::to_value(&truncation).unwrap_or(Value::Null),
            );
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
            details.insert("linesTruncated".to_string(), Value::Bool(true));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        if signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }

        Ok(AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent {
                text: output,
                text_signature: None,
            })],
            details: if details.is_empty() {
                Value::Null
            } else {
                Value::Object(details)
            },
            usage: None,
            added_tool_names: None,
            terminate: None,
        })
    }
}
