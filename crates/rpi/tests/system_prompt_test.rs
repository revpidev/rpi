//! Integration tests for `core::system_prompt` (port of `system-prompt.ts`
//! and the context-file parts of `resource-loader.ts` @ pi 0.82.1
//! (2efa728)): context-file candidate priority, ancestor-chain loading,
//! SYSTEM.md / APPEND_SYSTEM.md trust gating, `--system-prompt` file-vs-
//! inline resolution, and byte-exact prompt injection against a real
//! filesystem.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rpi::core::system_prompt::{
    build_system_prompt, discover_append_system_prompt_file, discover_system_prompt_file,
    load_context_file_from_dir, load_project_context_files, resolve_prompt_input,
    BuildSystemPromptOptions,
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
            "rpi-system-prompt-test-{}-{unique}",
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
// load_context_file_from_dir: candidate priority
// ---------------------------------------------------------------------------

#[test]
fn candidate_priority_agents_md_first() {
    let tmp = TempDir::new();
    tmp.write("AGENTS.md", "agents-md");
    tmp.write("AGENTS.MD", "agents-MD");
    tmp.write("CLAUDE.md", "claude-md");
    tmp.write("CLAUDE.MD", "claude-MD");

    let found = load_context_file_from_dir(tmp.path()).expect("a context file");
    assert_eq!(found.path, tmp.path().join("AGENTS.md"));
    assert_eq!(found.content, "agents-md");
}

#[test]
fn candidate_priority_full_order() {
    // Each case leaves exactly one candidate standing, in priority order.
    for (name, content) in [
        ("AGENTS.MD", "agents-MD"),
        ("CLAUDE.md", "claude-md"),
        ("CLAUDE.MD", "claude-MD"),
    ] {
        let tmp = TempDir::new();
        tmp.write(name, content);
        let found = load_context_file_from_dir(tmp.path()).expect("a context file");
        assert_eq!(found.path, tmp.path().join(name));
        assert_eq!(found.content, content);
    }

    // AGENTS.MD beats CLAUDE.md when both exist.
    let tmp = TempDir::new();
    tmp.write("AGENTS.MD", "agents-MD");
    tmp.write("CLAUDE.md", "claude-md");
    assert_eq!(
        load_context_file_from_dir(tmp.path())
            .expect("found")
            .content,
        "agents-MD"
    );

    // No candidates → None.
    let tmp = TempDir::new();
    assert!(load_context_file_from_dir(tmp.path()).is_none());
}

#[test]
fn candidate_that_is_a_directory_is_skipped() {
    let tmp = TempDir::new();
    // A directory named AGENTS.md exists but is not a file → next candidate.
    std::fs::create_dir_all(tmp.path().join("AGENTS.md")).expect("mkdir AGENTS.md");
    tmp.write("CLAUDE.md", "claude-md");

    let found = load_context_file_from_dir(tmp.path()).expect("a context file");
    assert_eq!(found.path, tmp.path().join("CLAUDE.md"));
}

// ---------------------------------------------------------------------------
// load_project_context_files: global + ancestor chain
// ---------------------------------------------------------------------------

#[test]
fn global_then_ancestors_root_side_first() {
    let tmp = TempDir::new();
    // Layout: tmp/agent (global), tmp/repo/{sub/deep} with context files at
    // repo and deep levels; a `.git` marker at repo proves the walk is not
    // bounded by the repo root (tmp itself also gets a context file).
    let agent_dir = tmp.path().join("agent");
    let cwd = tmp.path().join("repo/sub/deep");
    std::fs::create_dir_all(&cwd).expect("mkdir cwd");

    tmp.write("agent/AGENTS.md", "global");
    tmp.write("AGENTS.md", "tmp-root");
    tmp.write("repo/.git/HEAD", "fake git marker");
    tmp.write("repo/CLAUDE.md", "repo");
    tmp.write("repo/sub/deep/AGENTS.md", "deep");

    let files = load_project_context_files(&cwd, &agent_dir);
    let ours: Vec<(&str, &str)> = files
        .iter()
        .filter(|f| f.path.starts_with(tmp.path()))
        .map(|f| {
            (
                f.path
                    .strip_prefix(tmp.path())
                    .expect("under tmp")
                    .to_str()
                    .expect("utf8"),
                f.content.as_str(),
            )
        })
        .collect();

    // Order: global agent dir, then ancestors root-side-first (tmp before
    // repo before deep) — the `.git` marker does not stop the walk.
    assert_eq!(
        ours,
        [
            ("agent/AGENTS.md", "global"),
            ("AGENTS.md", "tmp-root"),
            ("repo/CLAUDE.md", "repo"),
            ("repo/sub/deep/AGENTS.md", "deep"),
        ]
    );
}

#[test]
fn agent_dir_overlapping_ancestor_is_deduplicated() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("repo");
    std::fs::create_dir_all(&cwd).expect("mkdir");
    // agent_dir IS tmp: its context file would also be found by the
    // ancestor walk — it must appear exactly once, in the global slot.
    tmp.write("AGENTS.md", "shared");
    tmp.write("repo/AGENTS.md", "repo");

    let files = load_project_context_files(&cwd, tmp.path());
    let shared: Vec<_> = files.iter().filter(|f| f.content == "shared").collect();
    assert_eq!(shared.len(), 1);
    assert_eq!(files.first().map(|f| f.content.as_str()), Some("shared"));
    assert_eq!(files.last().map(|f| f.content.as_str()), Some("repo"));
}

#[test]
fn no_context_files_anywhere_under_tmp() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("a/b");
    std::fs::create_dir_all(&cwd).expect("mkdir");
    let files = load_project_context_files(&cwd, &tmp.path().join("agent"));
    assert!(files.iter().all(|f| !f.path.starts_with(tmp.path())));
}

// ---------------------------------------------------------------------------
// SYSTEM.md / APPEND_SYSTEM.md discovery (trust gate + priority)
// ---------------------------------------------------------------------------

#[test]
fn system_md_project_requires_trust_and_beats_global() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("repo");
    let agent_dir = tmp.path().join("agent");
    tmp.write("repo/.rpi/SYSTEM.md", "project system prompt");
    tmp.write("agent/SYSTEM.md", "global system prompt");

    // Trusted: project wins.
    assert_eq!(
        discover_system_prompt_file(&cwd, &agent_dir, true),
        Some(cwd.join(".rpi/SYSTEM.md"))
    );
    // Untrusted: project file exists but is gated → global.
    assert_eq!(
        discover_system_prompt_file(&cwd, &agent_dir, false),
        Some(agent_dir.join("SYSTEM.md"))
    );
}

#[test]
fn system_md_global_only_and_none() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("repo");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("mkdir");
    tmp.write("agent/SYSTEM.md", "global");

    // Trusted but no project file → global.
    assert_eq!(
        discover_system_prompt_file(&cwd, &agent_dir, true),
        Some(agent_dir.join("SYSTEM.md"))
    );

    // Neither → None.
    let bare = TempDir::new();
    std::fs::create_dir_all(bare.path().join("repo")).expect("mkdir");
    assert_eq!(
        discover_system_prompt_file(&bare.path().join("repo"), &bare.path().join("agent"), true),
        None
    );
}

#[test]
fn append_system_md_same_gate_and_priority() {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("repo");
    let agent_dir = tmp.path().join("agent");
    tmp.write("repo/.rpi/APPEND_SYSTEM.md", "project append");
    tmp.write("agent/APPEND_SYSTEM.md", "global append");

    assert_eq!(
        discover_append_system_prompt_file(&cwd, &agent_dir, true),
        Some(cwd.join(".rpi/APPEND_SYSTEM.md"))
    );
    assert_eq!(
        discover_append_system_prompt_file(&cwd, &agent_dir, false),
        Some(agent_dir.join("APPEND_SYSTEM.md"))
    );
}

// ---------------------------------------------------------------------------
// resolve_prompt_input: file path vs inline text
// ---------------------------------------------------------------------------

#[test]
fn existing_file_is_read() {
    let tmp = TempDir::new();
    let path = tmp.write("prompt.md", "file based prompt\n");
    assert_eq!(
        resolve_prompt_input(Some(&path.to_string_lossy()), "system prompt"),
        Some("file based prompt\n".to_string())
    );
}

#[test]
fn missing_path_is_inline_text() {
    let tmp = TempDir::new();
    let missing = tmp.path().join("nope.md");
    let input = missing.to_string_lossy().into_owned();
    assert_eq!(
        resolve_prompt_input(Some(&input), "system prompt"),
        Some(input)
    );
}

#[test]
fn unreadable_existing_path_falls_back_to_inline() {
    // A directory exists but cannot be read as a file → the input itself is
    // used as inline text (resource-loader.ts:56-61 catch branch).
    let tmp = TempDir::new();
    let input = tmp.path().to_string_lossy().into_owned();
    assert_eq!(
        resolve_prompt_input(Some(&input), "system prompt"),
        Some(input)
    );
}

// ---------------------------------------------------------------------------
// Byte-exact injection end to end
// ---------------------------------------------------------------------------

#[test]
fn context_files_injected_byte_exact_into_default_prompt() {
    let tmp = TempDir::new();
    let agent_dir = tmp.path().join("agent");
    let cwd = tmp.path().join("repo/sub");
    std::fs::create_dir_all(&cwd).expect("mkdir");
    tmp.write("agent/AGENTS.md", "global rules");
    tmp.write("repo/AGENTS.md", "repo rules");

    let context_files = load_project_context_files(&cwd, &agent_dir);
    let options = BuildSystemPromptOptions {
        custom_prompt: Some("BASE".to_string()),
        append_system_prompt: Some("APPEND".to_string()),
        cwd: cwd.clone(),
        context_files,
        ..Default::default()
    };
    let prompt = build_system_prompt(&options);

    let expected = format!(
        "BASE\n\nAPPEND\n\n<project_context>\n\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"{}\">\nglobal rules\n</project_instructions>\n\n<project_instructions path=\"{}\">\nrepo rules\n</project_instructions>\n\n</project_context>\n\nCurrent working directory: {}",
        agent_dir.join("AGENTS.md").display(),
        tmp.path().join("repo/AGENTS.md").display(),
        cwd.display(),
    );
    assert_eq!(prompt, expected);
}
