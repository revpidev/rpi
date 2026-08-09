//! Integration tests for `core::prompt_templates` (port of
//! `prompt-templates.ts` @ pi 0.82.1 (2efa728)): template discovery,
//! frontmatter handling, and the expansion entry point against a real
//! filesystem. The argument-expansion DSL itself is covered by the module
//! unit tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rpi::core::prompt_templates::{
    expand_prompt_template, load_prompt_templates, load_templates_from_dir,
    LoadPromptTemplatesOptions,
};

// ---------------------------------------------------------------------------
// Temp dir helper (mirrors crates/rpi/src/tools.rs test_helpers::TempDir,
// which is cfg(test)-only and not visible to integration tests)
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "rpi-prompt-templates-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("failed to create temp dir for test");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().expect("rel has a parent"))
            .expect("failed to create parent dirs");
        std::fs::write(&path, content).expect("failed to write test file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// load_templates_from_dir
// ---------------------------------------------------------------------------

#[test]
fn loads_md_files_named_by_basename() {
    let tmp = TempDir::new();
    tmp.write("review.md", "Review the code");
    tmp.write("commit.md", "Commit changes");
    tmp.write("notes.txt", "not a template");

    let templates = load_templates_from_dir(tmp.path());
    let mut names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["commit", "review"]);
}

#[test]
fn non_recursive_and_missing_dir() {
    let tmp = TempDir::new();
    tmp.write("top.md", "top level");
    tmp.write("sub/nested.md", "nested");

    let templates = load_templates_from_dir(tmp.path());
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].name, "top");

    // Missing directory → empty list (prompt-templates.ts:141-143).
    assert!(load_templates_from_dir(&tmp.path().join("does-not-exist")).is_empty());
}

#[cfg(unix)]
#[test]
fn follows_symlinks_and_skips_broken_ones() {
    let tmp = TempDir::new();
    let target = tmp.write("real.md", "real content");
    std::os::unix::fs::symlink(&target, tmp.path().join("linked.md"))
        .expect("failed to create symlink");
    std::os::unix::fs::symlink(tmp.path().join("gone.md"), tmp.path().join("broken.md"))
        .expect("failed to create broken symlink");
    // Symlink to a directory named *.md is not a file → skipped.
    std::fs::create_dir_all(tmp.path().join("adir")).expect("mkdir");
    std::os::unix::fs::symlink(tmp.path().join("adir"), tmp.path().join("dirlink.md"))
        .expect("failed to create dir symlink");

    let templates = load_templates_from_dir(tmp.path());
    let mut names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["linked", "real"]);
}

// ---------------------------------------------------------------------------
// frontmatter & description
// ---------------------------------------------------------------------------

#[test]
fn frontmatter_description_and_argument_hint() {
    let tmp = TempDir::new();
    tmp.write(
        "review.md",
        "---\ndescription: Review some code\nargument-hint: <file> [focus]\n---\nBody $1\n",
    );

    let templates = load_templates_from_dir(tmp.path());
    assert_eq!(templates.len(), 1);
    let t = &templates[0];
    assert_eq!(t.description, "Review some code");
    assert_eq!(t.argument_hint.as_deref(), Some("<file> [focus]"));
    assert_eq!(t.content, "Body $1");
    assert_eq!(t.file_path, tmp.path().join("review.md"));
}

#[test]
fn description_defaults_to_first_non_empty_line() {
    let tmp = TempDir::new();
    tmp.write("a.md", "\n\n  \nFirst real line\nsecond line\n");
    let templates = load_templates_from_dir(tmp.path());
    // Untrimmed first line (JS keeps leading whitespace).
    assert_eq!(templates[0].description, "First real line");
    assert_eq!(templates[0].argument_hint, None);

    // Frontmatter without description → same fallback over the body.
    tmp.write("b.md", "---\nargument-hint: x\n---\nBody line here\n");
    let templates = load_templates_from_dir(tmp.path());
    let b = templates
        .iter()
        .find(|t| t.name == "b")
        .expect("template b");
    assert_eq!(b.description, "Body line here");
}

#[test]
fn description_truncates_at_60_chars() {
    let tmp = TempDir::new();
    let exactly_60 = "x".repeat(60);
    let over_60 = "y".repeat(61);
    tmp.write("exact.md", &exactly_60);
    tmp.write("over.md", &over_60);

    let templates = load_templates_from_dir(tmp.path());
    let exact = templates.iter().find(|t| t.name == "exact").expect("exact");
    let over = templates.iter().find(|t| t.name == "over").expect("over");
    assert_eq!(exact.description, exactly_60);
    assert_eq!(over.description, format!("{}...", "y".repeat(60)));
}

#[test]
fn crlf_file_and_invalid_yaml() {
    let tmp = TempDir::new();
    tmp.write(
        "crlf.md",
        "---\r\ndescription: Windows\r\n---\r\nBody line\r\n",
    );
    // Invalid YAML frontmatter → the whole template load fails
    // (loadTemplateFromFile catch → null, prompt-templates.ts:130-132).
    tmp.write("bad.md", "---\nkey: [unclosed\n---\nbody\n");

    let templates = load_templates_from_dir(tmp.path());
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].name, "crlf");
    assert_eq!(templates[0].description, "Windows");
    assert_eq!(templates[0].content, "Body line");
}

#[test]
fn empty_body_yields_empty_description_and_content() {
    let tmp = TempDir::new();
    tmp.write("empty.md", "");

    let templates = load_templates_from_dir(tmp.path());
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].description, "");
    assert_eq!(templates[0].content, "");
}

// ---------------------------------------------------------------------------
// load_prompt_templates
// ---------------------------------------------------------------------------

#[test]
fn loads_defaults_then_explicit_paths() {
    let tmp = TempDir::new();
    let agent_dir = tmp.path().join("agent");
    let cwd = tmp.path().join("project");

    tmp.write("agent/prompts/global.md", "global template");
    tmp.write("project/.rpi/prompts/project.md", "project template");
    tmp.write("extra/extra.md", "extra template");
    tmp.write("single.md", "single template $1");

    let options = LoadPromptTemplatesOptions {
        cwd: cwd.clone(),
        agent_dir: agent_dir.clone(),
        prompt_paths: vec![
            tmp.path().join("extra").to_string_lossy().into_owned(),
            tmp.path().join("single.md").to_string_lossy().into_owned(),
            // Missing paths are skipped.
            tmp.path().join("missing").to_string_lossy().into_owned(),
            // Non-md files are skipped.
            tmp.path().join("single.txt").to_string_lossy().into_owned(),
        ],
        include_defaults: true,
    };
    tmp.write("single.txt", "not a template");

    let templates = load_prompt_templates(&options);
    let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["global", "project", "extra", "single"]);
}

#[test]
fn include_defaults_false_and_relative_explicit_path() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("project");
    tmp.write("agent/prompts/global.md", "global");
    tmp.write("project/.rpi/prompts/project.md", "project");
    tmp.write("project/rel/explicit.md", "explicit");

    let options = LoadPromptTemplatesOptions {
        cwd: cwd.clone(),
        agent_dir: tmp.path().join("agent"),
        // Resolved against cwd (resolvePath(raw, resolvedCwd, {trim:true})).
        prompt_paths: vec!["  rel/explicit.md  ".to_string()],
        include_defaults: false,
    };

    let templates = load_prompt_templates(&options);
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].name, "explicit");
}

// ---------------------------------------------------------------------------
// expand_prompt_template (end to end)
// ---------------------------------------------------------------------------

#[test]
fn expand_loaded_template_end_to_end() {
    let tmp = TempDir::new();
    tmp.write(
        "review.md",
        "---\ndescription: Review code\n---\nReview $1 with focus on ${2:-general quality}",
    );
    let templates = load_templates_from_dir(tmp.path());

    // Found: quote-aware tokenisation + DSL expansion.
    assert_eq!(
        expand_prompt_template(r#"/review "my file.rs" perf"#, &templates),
        "Review my file.rs with focus on perf"
    );
    // Missing optional arg → default.
    assert_eq!(
        expand_prompt_template("/review main.rs", &templates),
        "Review main.rs with focus on general quality"
    );
    // Not found: original text returned unchanged.
    assert_eq!(
        expand_prompt_template("/unknown a b", &templates),
        "/unknown a b"
    );
    // Not a slash command: unchanged.
    assert_eq!(
        expand_prompt_template("plain text", &templates),
        "plain text"
    );
}
