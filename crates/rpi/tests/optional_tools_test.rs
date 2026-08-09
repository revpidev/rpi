//! Integration tests for the optional tools grep / find / ls (T14 W1).
//!
//! Ports of the grep/find/ls sections of `test/tools.test.ts` @ pi 0.82.1
//! (2efa728) plus contract tests for the native Rust implementations
//! (ADR-0003 §2: `ignore`/`globset` instead of external rg/fd binaries).
//! All tests run against real temporary directories; no network access.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rpi::tools::find::{create_find_tool, FindToolOptions};
use rpi::tools::grep::{create_grep_tool, GrepToolOptions};
use rpi::tools::ls::{create_ls_tool, LsToolOptions};
use rpi::tools::ToolContext;
use rpi_agent::types::AgentTool;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rpi-opt-tools-test-{}-{nanos}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdirs");
        std::fs::write(&path, content).expect("write file");
        path
    }

    fn mkdir(&self, rel: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(&path).expect("mkdir");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ctx(cwd: &Path) -> ToolContext {
    ToolContext {
        cwd: cwd.to_path_buf(),
        session_env: None,
    }
}

/// Execute a tool and return the text output or the error message.
async fn run_tool(tool: &Arc<dyn AgentTool>, params: Value) -> Result<String, String> {
    match tool
        .execute("test-call", params, CancellationToken::new(), None)
        .await
    {
        Ok(result) => {
            let text = result
                .content
                .iter()
                .map(|c| match c {
                    rpi_ai::types::ToolResultContent::Text(t) => t.text.clone(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("");
            Ok(text)
        }
        Err(e) => Err(format!("{e}")),
    }
}

/// The result lines before the trailing `\n\n[...]` notice block.
fn result_lines(output: &str) -> Vec<&str> {
    let body = output.split("\n\n[").next().unwrap_or(output);
    body.split('\n').filter(|l| !l.is_empty()).collect()
}

// ---------------------------------------------------------------------------
// grep tool (tools.test.ts:772-823)
// ---------------------------------------------------------------------------

mod grep_tool {
    use super::*;

    fn tool(cwd: &Path) -> Arc<dyn AgentTool> {
        create_grep_tool(&ctx(cwd), GrepToolOptions::default())
    }

    // Port of "should include filename when searching a single file".
    #[tokio::test]
    async fn test_includes_filename_when_searching_single_file() {
        let tmp = TempDir::new();
        let file = tmp.write("example.txt", "first line\nmatch line\nlast line");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": file.to_string_lossy() }),
        )
        .await
        .expect("grep");
        assert!(out.contains("example.txt:2: match line"), "got: {out}");
    }

    // Port of "should respect global limit and include context lines".
    #[tokio::test]
    async fn test_respects_global_limit_and_includes_context_lines() {
        let tmp = TempDir::new();
        let file = tmp.write(
            "context.txt",
            "before\nmatch one\nafter\nmiddle\nmatch two\nafter two",
        );

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": file.to_string_lossy(), "limit": 1, "context": 1 }),
        )
        .await
        .expect("grep");
        assert!(out.contains("context.txt-1- before"), "got: {out}");
        assert!(out.contains("context.txt:2: match one"), "got: {out}");
        assert!(out.contains("context.txt-3- after"), "got: {out}");
        assert!(
            out.contains("[1 matches limit reached. Use limit=2 for more, or refine pattern]"),
            "got: {out}"
        );
        // Ensure second match is not present.
        assert!(!out.contains("match two"), "got: {out}");
    }

    // Port of "should treat flag-like patterns as search text".
    #[tokio::test]
    async fn test_treats_flag_like_patterns_as_search_text() {
        let tmp = TempDir::new();
        let marker = tmp.path().join("grep-injection-marker");
        let payload = tmp.write("payload.sh", "#!/bin/sh\necho executed\ncat \"$1\"\n");
        tmp.write("target.txt", "target\n");

        let out = run_tool(
            &tool(tmp.path()),
            json!({
                "pattern": format!("--pre={}", payload.to_string_lossy()),
                "path": tmp.path().to_string_lossy(),
            }),
        )
        .await
        .expect("grep");
        assert!(out.contains("No matches found"), "got: {out}");
        assert!(!marker.exists());
    }

    // Contract: default limit is 100 matches and stops there (grep.ts:39).
    #[tokio::test]
    async fn test_default_limit_100_stops_with_refine_notice() {
        let tmp = TempDir::new();
        let content: String = (1..=150)
            .map(|i| format!("match line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let file = tmp.write("many.txt", &content);

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": file.to_string_lossy() }),
        )
        .await
        .expect("grep");
        assert_eq!(result_lines(&out).len(), 100, "got: {out}");
        assert!(
            out.contains("[100 matches limit reached. Use limit=200 for more, or refine pattern]"),
            "got: {out}"
        );
    }

    // Contract: match lines use `path:lineno: text`, context lines use
    // `path-lineno- text` (grep.ts:264-265).
    #[tokio::test]
    async fn test_match_and_context_line_formats() {
        let tmp = TempDir::new();
        tmp.write("fmt.txt", "zero\none\ntarget here\nthree\nfour");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "target", "path": ".", "context": 2 }),
        )
        .await
        .expect("grep");
        let lines = result_lines(&out);
        assert_eq!(
            lines,
            vec![
                "fmt.txt-1- zero",
                "fmt.txt-2- one",
                "fmt.txt:3: target here",
                "fmt.txt-4- three",
                "fmt.txt-5- four",
            ],
            "got: {out}"
        );
    }

    // Contract: overlapping context blocks are not merged (grep.ts formats
    // every match independently).
    #[tokio::test]
    async fn test_overlapping_context_blocks_not_merged() {
        let tmp = TempDir::new();
        tmp.write("dup.txt", "hit a\nmid\nhit b");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "hit", "path": ".", "context": 1 }),
        )
        .await
        .expect("grep");
        let lines = result_lines(&out);
        assert_eq!(
            lines,
            vec![
                "dup.txt:1: hit a",
                "dup.txt-2- mid",
                "dup.txt-2- mid",
                "dup.txt:3: hit b",
            ],
            "got: {out}"
        );
    }

    // Contract: single lines are truncated to 500 chars (GREP_MAX_LINE_LENGTH).
    #[tokio::test]
    async fn test_long_line_truncated_to_500_chars() {
        let tmp = TempDir::new();
        let long_line = format!("match {}", "x".repeat(600));
        let file = tmp.write("long.txt", &long_line);

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": file.to_string_lossy() }),
        )
        .await
        .expect("grep");
        assert!(out.contains("... [truncated]"), "got: {out}");
        assert!(
            out.contains("Some lines truncated to 500 chars. Use read tool to see full lines"),
            "got: {out}"
        );
        // The truncated line holds 500 payload chars before the suffix.
        let line = result_lines(&out)[0];
        let payload = line.strip_prefix("long.txt:1: ").expect("prefix");
        // 500 UTF-16 units = "match " (6) + 494 x's, then the suffix.
        assert_eq!(
            payload,
            format!("match {}{}", "x".repeat(494), "... [truncated]"),
            "got: {out}"
        );
    }

    // Contract: total output is byte-truncated at 50KB (DEFAULT_MAX_BYTES).
    #[tokio::test]
    async fn test_output_truncated_at_50kb() {
        let tmp = TempDir::new();
        let line = format!("match {}", "y".repeat(480));
        let content: String = (1..=200)
            .map(|_| line.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let file = tmp.write("big.txt", &content);

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": file.to_string_lossy(), "limit": 200 }),
        )
        .await
        .expect("grep");
        assert!(
            out.contains("200 matches limit reached. Use limit=400 for more, or refine pattern"),
            "got: {out}"
        );
        assert!(out.contains("50.0KB limit reached"), "got: {out}");
        // Body stays within the 50KB byte budget.
        let body = out.split("\n\n[").next().expect("body");
        assert!(body.len() <= 50 * 1024, "body too large: {}", body.len());
    }

    // Contract: rg --hidden semantics — hidden files are searched.
    #[tokio::test]
    async fn test_hidden_files_are_searched() {
        let tmp = TempDir::new();
        tmp.write(".secret/hidden.txt", "match hidden\n");
        tmp.write("visible.txt", "match visible\n");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": "." }),
        )
        .await
        .expect("grep");
        assert!(
            out.contains(".secret/hidden.txt:1: match hidden"),
            "got: {out}"
        );
        assert!(out.contains("visible.txt:1: match visible"), "got: {out}");
    }

    // Contract: `--hidden` includes `.git` contents (rg searches inside
    // `.git/`; verified against rg 15 — T14 review anchor, distinct from
    // the find tool's fixed `.git` pruning).
    #[tokio::test]
    async fn test_git_contents_are_searched() {
        let tmp = TempDir::new();
        tmp.mkdir(".git");
        tmp.write(".git/config", "match inside git dir\n");
        tmp.write("tracked.txt", "match outside\n");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": "." }),
        )
        .await
        .expect("grep");
        assert!(
            out.contains(".git/config:1: match inside git dir"),
            "got: {out}"
        );
        assert!(out.contains("tracked.txt:1: match outside"), "got: {out}");
    }

    // Contract: rg gitignore semantics — respected inside a git repository.
    #[tokio::test]
    async fn test_gitignore_respected_inside_repo() {
        let tmp = TempDir::new();
        tmp.mkdir(".git");
        tmp.write(".gitignore", "ignored.txt\n");
        tmp.write("ignored.txt", "match ignored\n");
        tmp.write("kept.txt", "match kept\n");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": "." }),
        )
        .await
        .expect("grep");
        assert!(out.contains("kept.txt:1: match kept"), "got: {out}");
        assert!(!out.contains("ignored.txt"), "got: {out}");
    }

    // Contract: rg gitignore semantics — NOT respected outside a repository
    // (grep.ts passes no --no-require-git).
    #[tokio::test]
    async fn test_gitignore_ignored_outside_repo() {
        let tmp = TempDir::new();
        tmp.write(".gitignore", "ignored.txt\n");
        tmp.write("ignored.txt", "match ignored\n");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": "." }),
        )
        .await
        .expect("grep");
        assert!(out.contains("ignored.txt:1: match ignored"), "got: {out}");
    }

    // Contract: --glob filters walked files (rg gitignore-style glob).
    #[tokio::test]
    async fn test_glob_filters_files() {
        let tmp = TempDir::new();
        tmp.write("a.rs", "match rust\n");
        tmp.write("b.txt", "match text\n");
        tmp.write("sub/c.rs", "match sub rust\n");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": ".", "glob": "*.rs" }),
        )
        .await
        .expect("grep");
        assert!(out.contains("a.rs:1: match rust"), "got: {out}");
        assert!(out.contains("sub/c.rs:1: match sub rust"), "got: {out}");
        assert!(!out.contains("b.txt"), "got: {out}");
    }

    // Contract: a whitelist --glob overrides file-level gitignore rules (rg
    // override semantics), but gitignored directories are still pruned from
    // the walk (verified against ripgrep 15).
    #[tokio::test]
    async fn test_glob_overrides_gitignore() {
        let tmp = TempDir::new();
        tmp.mkdir(".git");
        tmp.write(".gitignore", "ignored.rs\nbuild/\n");
        tmp.write("ignored.rs", "match ignored\n");
        tmp.write("build/x.rs", "match build\n");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": ".", "glob": "*.rs" }),
        )
        .await
        .expect("grep");
        assert!(out.contains("ignored.rs:1: match ignored"), "got: {out}");
        assert!(!out.contains("build/x.rs"), "got: {out}");
    }

    // Contract: --ignore-case / --fixed-strings flags.
    #[tokio::test]
    async fn test_ignore_case_and_literal() {
        let tmp = TempDir::new();
        let file = tmp.write(
            "case.txt",
            "Match UPPER\nmatch lower\na.b literal\naxb regex\n",
        );

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": file.to_string_lossy(), "ignoreCase": true }),
        )
        .await
        .expect("grep");
        assert!(out.contains("case.txt:1: Match UPPER"), "got: {out}");
        assert!(out.contains("case.txt:2: match lower"), "got: {out}");

        // Regex: "a.b" also matches "axb".
        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "a.b", "path": file.to_string_lossy() }),
        )
        .await
        .expect("grep");
        assert!(out.contains("case.txt:3: a.b literal"), "got: {out}");
        assert!(out.contains("case.txt:4: axb regex"), "got: {out}");

        // Literal: "a.b" matches only the literal text.
        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "a.b", "path": file.to_string_lossy(), "literal": true }),
        )
        .await
        .expect("grep");
        assert!(out.contains("case.txt:3: a.b literal"), "got: {out}");
        assert!(!out.contains("axb regex"), "got: {out}");
    }

    // Contract: CRLF match lines are displayed without the \r (grep.ts:321-323).
    #[tokio::test]
    async fn test_crlf_match_lines_strip_carriage_return() {
        let tmp = TempDir::new();
        let file = tmp.write("crlf.txt", "line1\r\nmatch crlf\r\n");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": file.to_string_lossy() }),
        )
        .await
        .expect("grep");
        assert!(out.contains("crlf.txt:2: match crlf"), "got: {out}");
        assert!(!out.contains('\r'), "got: {out:?}");
    }

    // Contract: binary files are suppressed in directory walks but searched
    // when named explicitly (rg binary detection).
    #[tokio::test]
    async fn test_binary_file_suppressed_in_walk_but_searched_explicitly() {
        let tmp = TempDir::new();
        tmp.write("bin.dat", "match bin\nfoo\u{0}bar\nmatch after-nul\n");
        tmp.write("plain.txt", "match plain\n");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": "." }),
        )
        .await
        .expect("grep");
        assert!(out.contains("plain.txt:1: match plain"), "got: {out}");
        assert!(!out.contains("bin.dat"), "got: {out}");

        let bin = tmp.path().join("bin.dat");
        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "match", "path": bin.to_string_lossy() }),
        )
        .await
        .expect("grep");
        assert!(out.contains("bin.dat:1: match bin"), "got: {out}");
        assert!(out.contains("bin.dat:3: match after-nul"), "got: {out}");
    }

    // Contract: "No matches found" / "Path not found" outcomes.
    #[tokio::test]
    async fn test_no_matches_and_path_not_found() {
        let tmp = TempDir::new();
        tmp.write("a.txt", "hello\n");

        let out = run_tool(&tool(tmp.path()), json!({ "pattern": "zzz", "path": "." }))
            .await
            .expect("grep");
        assert_eq!(out, "No matches found");

        let missing = tmp.path().join("nope");
        let err = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "x", "path": missing.to_string_lossy() }),
        )
        .await
        .expect_err("should fail");
        assert!(err.contains("Path not found:"), "got: {err}");
    }

    // Invalid regex surfaces the parse error (rg exit code 2 upstream).
    #[tokio::test]
    async fn test_invalid_regex_surfaces_parse_error() {
        let tmp = TempDir::new();
        tmp.write("a.txt", "hello\n");

        let err = run_tool(&tool(tmp.path()), json!({ "pattern": "[", "path": "." }))
            .await
            .expect_err("should fail");
        assert!(err.contains("regex parse error"), "got: {err}");
    }
}

// ---------------------------------------------------------------------------
// find tool (tools.test.ts:825-878)
// ---------------------------------------------------------------------------

mod find_tool {
    use super::*;

    fn tool(cwd: &Path) -> Arc<dyn AgentTool> {
        create_find_tool(&ctx(cwd), FindToolOptions::default())
    }

    // Port of "should include hidden files that are not gitignored".
    #[tokio::test]
    async fn test_includes_hidden_files_not_gitignored() {
        let tmp = TempDir::new();
        tmp.write(".secret/hidden.txt", "hidden");
        tmp.write("visible.txt", "visible");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "**/*.txt", "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("find");
        let lines = result_lines(&out);
        assert!(lines.contains(&"visible.txt"), "got: {out}");
        assert!(lines.contains(&".secret/hidden.txt"), "got: {out}");
    }

    // Port of "should respect .gitignore" (outside a repository, upstream
    // adds --no-require-git).
    #[tokio::test]
    async fn test_respects_gitignore_outside_repo() {
        let tmp = TempDir::new();
        tmp.write(".gitignore", "ignored.txt\n");
        tmp.write("ignored.txt", "ignored");
        tmp.write("kept.txt", "kept");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "**/*.txt", "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("find");
        assert!(out.contains("kept.txt"), "got: {out}");
        assert!(!out.contains("ignored.txt"), "got: {out}");
    }

    // Port of "should surface fd glob parse errors".
    #[tokio::test]
    async fn test_surfaces_glob_parse_errors() {
        let tmp = TempDir::new();
        let err = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "[", "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect_err("should fail");
        assert!(err.contains("error parsing glob"), "got: {err}");
    }

    // Port of "should treat flag-like patterns as search text".
    #[tokio::test]
    async fn test_treats_flag_like_patterns_as_search_text() {
        let tmp = TempDir::new();
        tmp.write("a.txt", "x");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "--help", "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("find");
        assert!(
            out.contains("No files found matching pattern"),
            "got: {out}"
        );
    }

    // Contract: default limit is 1000 results (find.ts:30).
    #[tokio::test]
    async fn test_default_limit_1000_with_refine_notice() {
        let tmp = TempDir::new();
        for i in 0..1005 {
            tmp.write(&format!("f{i:04}.txt"), "x");
        }

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "*.txt", "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("find");
        assert_eq!(result_lines(&out).len(), 1000, "got: {out}");
        assert!(
            out.contains(
                "[1000 results limit reached. Use limit=2000 for more, or refine pattern]"
            ),
            "got: {out}"
        );
    }

    // Contract: output is relative to the search directory; directories keep
    // a trailing slash (find.ts:307-320).
    #[tokio::test]
    async fn test_relative_output_and_directory_trailing_slash() {
        let tmp = TempDir::new();
        tmp.write("sub/file.txt", "x");
        tmp.write("top.txt", "x");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "*", "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("find");
        let lines = result_lines(&out);
        assert!(lines.contains(&"sub/"), "got: {out}");
        assert!(lines.contains(&"sub/file.txt"), "got: {out}");
        assert!(lines.contains(&"top.txt"), "got: {out}");
        assert!(lines.iter().all(|l| !l.starts_with('/')), "got: {out}");
    }

    // Contract: patterns containing `/` switch to full-path matching with a
    // `**/` prefix (find.ts:243-252).
    #[tokio::test]
    async fn test_slash_pattern_uses_full_path_with_globstar_prefix() {
        let tmp = TempDir::new();
        tmp.write("sub/x.txt", "x");
        tmp.write("other/y.txt", "y");
        tmp.write("a/sub/b.txt", "b");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "sub/*.txt", "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("find");
        let lines = result_lines(&out);
        assert!(lines.contains(&"sub/x.txt"), "got: {out}");
        // The `**/` prefix lets the pattern match at any depth.
        assert!(lines.contains(&"a/sub/b.txt"), "got: {out}");
        assert!(!lines.contains(&"other/y.txt"), "got: {out}");
    }

    // Contract: node_modules and .git are always pruned (requirements §4.5).
    #[tokio::test]
    async fn test_node_modules_and_git_pruned() {
        let tmp = TempDir::new();
        tmp.write("node_modules/pkg/m.txt", "m");
        tmp.write(".git/g.txt", "g");
        tmp.write("src/main.txt", "s");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "*", "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("find");
        assert!(out.contains("src/main.txt"), "got: {out}");
        assert!(!out.contains("node_modules"), "got: {out}");
        assert!(!out.contains(".git"), "got: {out}");
    }

    // Contract: total output is byte-truncated at 50KB.
    #[tokio::test]
    async fn test_output_truncated_at_50kb() {
        let tmp = TempDir::new();
        for i in 0..1200 {
            tmp.write(&format!("file-{i:04}-{}", "n".repeat(50)), "x");
        }

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "file-*", "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("find");
        assert!(out.contains("1000 results limit reached"), "got: {out}");
        assert!(out.contains("50.0KB limit reached"), "got: {out}");
    }

    // Contract: empty result and non-directory search path outcomes.
    #[tokio::test]
    async fn test_no_files_and_not_a_directory() {
        let tmp = TempDir::new();
        let file = tmp.write("a.txt", "x");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "*.md", "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("find");
        assert_eq!(out, "No files found matching pattern");

        let err = run_tool(
            &tool(tmp.path()),
            json!({ "pattern": "*", "path": file.to_string_lossy() }),
        )
        .await
        .expect_err("should fail");
        assert!(err.contains("is not a directory"), "got: {err}");
    }
}

// ---------------------------------------------------------------------------
// ls tool (tools.test.ts:880-893)
// ---------------------------------------------------------------------------

mod ls_tool {
    use super::*;

    fn tool(cwd: &Path) -> Arc<dyn AgentTool> {
        create_ls_tool(&ctx(cwd), LsToolOptions::default())
    }

    // Port of "should list dotfiles and directories".
    #[tokio::test]
    async fn test_lists_dotfiles_and_directories() {
        let tmp = TempDir::new();
        tmp.write(".hidden-file", "secret");
        tmp.mkdir(".hidden-dir");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("ls");
        assert!(out.contains(".hidden-file"), "got: {out}");
        assert!(out.contains(".hidden-dir/"), "got: {out}");
    }

    // Contract: case-insensitive alphabetical sort (ls.ts:149-150).
    #[tokio::test]
    async fn test_case_insensitive_sort() {
        let tmp = TempDir::new();
        tmp.write("b.txt", "x");
        tmp.write("A.txt", "x");
        tmp.write("c.txt", "x");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("ls");
        assert_eq!(
            result_lines(&out),
            vec!["A.txt", "b.txt", "c.txt"],
            "got: {out}"
        );
    }

    // Contract: default limit is 500 entries (ls.ts:21).
    #[tokio::test]
    async fn test_default_limit_500_with_notice() {
        let tmp = TempDir::new();
        for i in 0..600 {
            tmp.write(&format!("f{i:04}.txt"), "x");
        }

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("ls");
        assert_eq!(result_lines(&out).len(), 500, "got: {out}");
        assert!(
            out.contains("[500 entries limit reached. Use limit=1000 for more]"),
            "got: {out}"
        );
    }

    // Contract: entries that cannot be stat'ed are skipped (ls.ts:165-168).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_broken_symlink_skipped() {
        let tmp = TempDir::new();
        tmp.write("real.txt", "x");
        std::os::unix::fs::symlink(tmp.path().join("missing"), tmp.path().join("broken"))
            .expect("symlink");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("ls");
        assert!(out.contains("real.txt"), "got: {out}");
        assert!(!out.contains("broken"), "got: {out}");
    }

    // Contract: total output is byte-truncated at 50KB.
    #[tokio::test]
    async fn test_output_truncated_at_50kb() {
        let tmp = TempDir::new();
        for i in 0..600 {
            tmp.write(&format!("file-{i:04}-{}", "n".repeat(100)), "x");
        }

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "path": tmp.path().to_string_lossy() }),
        )
        .await
        .expect("ls");
        assert!(out.contains("500 entries limit reached"), "got: {out}");
        assert!(out.contains("50.0KB limit reached"), "got: {out}");
    }

    // Contract: empty / not-a-directory / missing path outcomes.
    #[tokio::test]
    async fn test_empty_not_a_directory_and_missing() {
        let tmp = TempDir::new();
        let empty = tmp.mkdir("empty");
        let file = tmp.write("a.txt", "x");

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "path": empty.to_string_lossy() }),
        )
        .await
        .expect("ls");
        assert_eq!(out, "(empty directory)");

        let err = run_tool(&tool(tmp.path()), json!({ "path": file.to_string_lossy() }))
            .await
            .expect_err("should fail");
        assert!(err.contains("Not a directory:"), "got: {err}");

        let err = run_tool(
            &tool(tmp.path()),
            json!({ "path": tmp.path().join("nope").to_string_lossy() }),
        )
        .await
        .expect_err("should fail");
        assert!(err.contains("Path not found:"), "got: {err}");
    }

    // Contract: `limit` parameter caps the entry count (ls.ts:125).
    #[tokio::test]
    async fn test_explicit_limit() {
        let tmp = TempDir::new();
        for i in 0..10 {
            tmp.write(&format!("f{i}.txt"), "x");
        }

        let out = run_tool(
            &tool(tmp.path()),
            json!({ "path": tmp.path().to_string_lossy(), "limit": 3 }),
        )
        .await
        .expect("ls");
        assert_eq!(result_lines(&out).len(), 3, "got: {out}");
        assert!(
            out.contains("[3 entries limit reached. Use limit=6 for more]"),
            "got: {out}"
        );
    }
}

// ---------------------------------------------------------------------------
// Optional-tool wiring (requirements §4.5; agent-session.ts _buildRuntime)
// ---------------------------------------------------------------------------

mod optional_tool_wiring {
    use super::*;
    use rpi::core::agent_session_services::{
        create_agent_session_services, CreateAgentSessionServicesOptions,
    };
    use rpi::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime, ModelsPathInput};
    use rpi::core::session_manager::{NewSessionOptions, SessionManager};
    use rpi_test_support::faux::{
        FauxAiProvider, FauxModelDefinition, FauxProvider, FauxProviderOptions,
    };
    use std::sync::Mutex;

    async fn wiring_session(
        tools: Option<Vec<String>>,
    ) -> (rpi::core::agent_session::AgentSession, TempDir) {
        let tmp = TempDir::new();
        let cwd = tmp.path().join("cwd");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");

        let provider = FauxProvider::new(FauxProviderOptions {
            models: Some(vec![FauxModelDefinition {
                id: "faux-1".to_owned(),
                name: Some("Faux One".to_owned()),
                reasoning: Some(true),
                input: None,
                cost: None,
                context_window: Some(200_000),
                max_tokens: Some(8192),
            }]),
            ..Default::default()
        });
        let model = provider.get_model(None).expect("faux model");

        let model_runtime = ModelRuntime::create(CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
            ..Default::default()
        })
        .await;
        model_runtime
            .register_native_provider(Arc::new(FauxAiProvider::new(provider)))
            .await
            .expect("register faux provider");

        let services = create_agent_session_services(CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        })
        .await
        .expect("services");

        let session_manager = Arc::new(Mutex::new(
            SessionManager::in_memory(Some(&cwd), NewSessionOptions::default())
                .expect("in-memory session"),
        ));
        let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
            cwd: Some(cwd),
            agent_dir: Some(agent_dir),
            model_runtime: Some(model_runtime),
            model: Some(model),
            services: Some(services),
            session_manager: Some(session_manager),
            tools,
            ..Default::default()
        })
        .await
        .expect("create session");
        (created.session, tmp)
    }

    // Optional tools are registered but inactive by default (requirements
    // §4.5: default active set is read/bash/edit/write).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_optional_tools_inactive_by_default() {
        let (session, _tmp) = wiring_session(None).await;
        assert_eq!(
            session.get_active_tool_names(),
            vec!["read", "bash", "edit", "write"]
        );
    }

    // --tools allowlist activates optional tools by name (sdk.ts:246-252).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_tools_allowlist_activates_optional_tools() {
        let names = vec![
            "read".to_owned(),
            "grep".to_owned(),
            "find".to_owned(),
            "ls".to_owned(),
        ];
        let (session, _tmp) = wiring_session(Some(names.clone())).await;
        assert_eq!(session.get_active_tool_names(), names);
    }

    // The registry holds the optional tools, so runtime activation
    // (setActiveToolsByName, e.g. /tools) can enable them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_optional_tools_activatable_at_runtime() {
        let (session, _tmp) = wiring_session(None).await;
        session.set_active_tools_by_name(vec![
            "read".to_owned(),
            "grep".to_owned(),
            "find".to_owned(),
            "ls".to_owned(),
        ]);
        assert_eq!(
            session.get_active_tool_names(),
            vec!["read", "grep", "find", "ls"]
        );
        // Unknown names are dropped, mirroring setActiveToolsByName.
        session.set_active_tools_by_name(vec!["grep".to_owned(), "nope".to_owned()]);
        assert_eq!(session.get_active_tool_names(), vec!["grep"]);
    }
}
