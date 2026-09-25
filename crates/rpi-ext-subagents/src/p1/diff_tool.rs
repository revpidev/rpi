//! Bounded working-tree diff tool for review children (#2333 /
//! cef12133): `watchdog_diff` gives a reviewer a read-only view of the
//! staged + unstaged working-tree delta against its launch `HEAD`, plus the
//! untracked-path inventory — with a byte budget, path validation, and a
//! HEAD-drift refusal (committed ranges are never shown; a task asking for
//! one must supply a diff artifact).
//!
//! The baseline (`root` + `ref`) is captured by the PARENT at child launch
//! (`captureWatchdogDiffBaseline`, diff-tool.ts:33-41) and carried in the
//! `RPI_SUBAGENT_DIFF_BASELINE` env; the child registers the tool only when
//! that env is present. This mirrors upstream's gating
//! (`config.requiredTools?.includes(WATCHDOG_DIFF_TOOL_NAME) && config.cwd`
//! → capture at registration, subagent-prompt-runtime.ts:456-462), with the
//! reviewer's tools line naming the tool playing the requiredTools role.

use serde_json::{json, Value};

/// `WATCHDOG_DIFF_TOOL_NAME` (diff-tool.ts:6).
pub const WATCHDOG_DIFF_TOOL_NAME: &str = "watchdog_diff";
/// `WATCHDOG_DIFF_MAX_CHARS` (diff-tool.ts:8).
pub const WATCHDOG_DIFF_MAX_CHARS: usize = 24_000;
/// `MAX_UNTRACKED_FILES` (diff-tool.ts:9).
const MAX_UNTRACKED_FILES: usize = 50;

/// Env carrying the parent-captured baseline: `<root>\t<ref>`.
pub const DIFF_BASELINE_ENV: &str = "RPI_SUBAGENT_DIFF_BASELINE";

/// The launch baseline (repo root + HEAD at reviewer launch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffBaseline {
    pub root: String,
    pub ref_: String,
}

impl DiffBaseline {
    /// `captureWatchdogDiffBaseline` (diff-tool.ts:33-41): resolve the repo
    /// toplevel and HEAD; any failure yields `None` (no tool registered).
    pub fn capture(cwd: &std::path::Path) -> Option<DiffBaseline> {
        let toplevel = run_git_at(cwd, &["rev-parse", "--show-toplevel"])?;
        let head = run_git_at(cwd, &["rev-parse", "HEAD"])?;
        let (root, ref_) = (toplevel.trim().to_string(), head.trim().to_string());
        if root.is_empty() || ref_.is_empty() {
            return None;
        }
        Some(DiffBaseline { root, ref_ })
    }

    /// Encode for the child env.
    pub fn to_env_value(&self) -> String {
        format!("{}\t{}", self.root, self.ref_)
    }

    /// Decode the env form (malformed → `None`; the tool stays unregistered).
    pub fn from_env_value(raw: &str) -> Option<DiffBaseline> {
        let (root, ref_) = raw.split_once('\t')?;
        let (root, ref_) = (root.trim(), ref_.trim());
        if root.is_empty() || ref_.is_empty() || root.contains('\t') || ref_.contains('\t') {
            return None;
        }
        Some(DiffBaseline {
            root: root.to_string(),
            ref_: ref_.to_string(),
        })
    }

    fn from_env() -> Option<DiffBaseline> {
        DiffBaseline::from_env_value(&std::env::var(DIFF_BASELINE_ENV).ok()?)
    }
}

struct GitOutput {
    ok: bool,
    stdout: String,
}

fn run_git(root: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_git_at(cwd: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_git_checked(root: &str, args: &[&str]) -> GitOutput {
    match std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
    {
        Ok(output) => GitOutput {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        },
        Err(_) => GitOutput {
            ok: false,
            stdout: String::new(),
        },
    }
}

/// `validatePath` (diff-tool.ts:45-51): optional relative path filter.
fn validate_path(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.starts_with('-') {
        return Err("watchdog_diff path must not start with '-'.".to_string());
    }
    if std::path::Path::new(trimmed).is_absolute() {
        return Err("watchdog_diff path must be relative to the repo root.".to_string());
    }
    if trimmed.split(['\\', '/']).any(|segment| segment == "..") {
        return Err("watchdog_diff path must not contain '..'.".to_string());
    }
    Ok(Some(trimmed.to_string()))
}

/// `bound` (diff-tool.ts:53-57): keep the head of the text within the
/// budget and append the exact omission marker.
fn bound(text: &str) -> String {
    if text.len() <= WATCHDOG_DIFF_MAX_CHARS {
        return text.to_string();
    }
    let marker = format!(
        "\n\n[... {} characters omitted; call again with a narrower path ...]",
        text.len() - WATCHDOG_DIFF_MAX_CHARS
    );
    // Cut on a char boundary within the budget.
    let mut cut = WATCHDOG_DIFF_MAX_CHARS.saturating_sub(marker.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{}", &text[..cut], marker)
}

/// Whether the child should register the tool (baseline env present).
pub fn baseline_from_env() -> Option<DiffBaseline> {
    DiffBaseline::from_env()
}

/// Tool execution (diff-tool.ts:59-96, workingTreeAtLaunch mode).
pub fn execute(params: &Value) -> Value {
    let Some(baseline) = DiffBaseline::from_env() else {
        return error_result(
            "watchdog_diff is unavailable: no diff baseline was configured for this session.",
        );
    };
    let path_filter = match validate_path(params.get("path").and_then(Value::as_str)) {
        Ok(filter) => filter,
        Err(message) => return error_result(&message),
    };
    let stat = params.get("stat").and_then(Value::as_bool) == Some(true);

    let mut diff_args: Vec<&str> = vec!["diff", "--no-color", "--no-ext-diff"];
    if stat {
        diff_args.push("--stat");
    }
    diff_args.push(&baseline.ref_);
    diff_args.push("--");
    if let Some(filter) = path_filter.as_deref() {
        diff_args.push(filter);
    }
    let diff = run_git_checked(&baseline.root, &diff_args);
    if !diff.ok {
        return error_result("git diff failed: unknown error");
    }
    let mut untracked_args: Vec<&str> =
        vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
    if let Some(filter) = path_filter.as_deref() {
        untracked_args.push(filter);
    }
    let untracked_result = run_git_checked(&baseline.root, &untracked_args);
    // workingTreeAtLaunch (#2333): if HEAD moved since launch, committed
    // changes would masquerade as working-tree deltas — refuse.
    let current_head = run_git(&baseline.root, &["rev-parse", "HEAD"]);
    if current_head.as_deref().map(str::trim) != Some(baseline.ref_.as_str()) {
        let text = "Reviewer-launch HEAD changed or is unavailable. Committed changes are unsupported; relaunch the reviewer or supply a diff artifact.";
        return json!({
            "content": [{ "type": "text", "text": text }],
            "details": { "chars": text.len() },
            "isError": false,
        });
    }
    let untracked: Vec<&str> = untracked_result
        .stdout
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .collect();
    let mut sections: Vec<String> = Vec::new();
    let diff_text = diff.stdout.trim_end();
    if !diff_text.is_empty() {
        sections.push(diff_text.to_string());
    }
    if !untracked.is_empty() {
        let shown = &untracked[..untracked.len().min(MAX_UNTRACKED_FILES)];
        let mut block = String::from("Untracked files (use read to inspect):");
        for file in shown {
            block.push_str(&format!("\n {file}"));
        }
        sections.push(block);
        if untracked.len() > shown.len() {
            sections.push(format!(
                "... {} more untracked files",
                untracked.len() - shown.len()
            ));
        }
    }
    let joined = sections.join("\n\n");
    let text = if joined.is_empty() {
        format!(
            "No working-tree changes against reviewer-launch HEAD {}. Committed changes are not included.",
            &baseline.ref_[..baseline.ref_.len().min(12)]
        )
    } else {
        bound(&joined)
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "details": { "chars": text.len() },
        "isError": false,
    })
}

fn error_result(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "details": { "chars": message.len() },
        "isError": true,
    })
}

/// The registration description (upstream `createWatchdogDiffTool`
/// description in workingTreeAtLaunch mode, diff-tool.ts:70-73).
pub fn tool_description() -> String {
    "Show the current staged and unstaged working-tree delta against reviewer-launch HEAD, plus untracked file paths. Committed ranges are not included. Optional path narrows it; stat:true returns per-file counts only.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_env_round_trip_and_malformed() {
        let baseline = DiffBaseline {
            root: "/repo".to_string(),
            ref_: "abc123".to_string(),
        };
        assert_eq!(
            DiffBaseline::from_env_value(&baseline.to_env_value()),
            Some(baseline)
        );
        assert_eq!(DiffBaseline::from_env_value(""), None);
        assert_eq!(DiffBaseline::from_env_value("only-root"), None);
        assert_eq!(DiffBaseline::from_env_value("\tref"), None);
    }

    #[test]
    fn path_validation_rejects_flag_absolute_and_dotdot() {
        assert_eq!(
            validate_path(Some("src/lib.rs")).unwrap().as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(validate_path(Some("  ")).unwrap(), None);
        assert_eq!(validate_path(None).unwrap(), None);
        assert!(validate_path(Some("-flag")).is_err());
        assert!(validate_path(Some("/abs/path")).is_err());
        assert!(validate_path(Some("a/../b")).is_err());
        assert!(validate_path(Some("a\\..\\b")).is_err());
    }

    #[test]
    fn bound_matches_upstream_marker() {
        let text = "x".repeat(WATCHDOG_DIFF_MAX_CHARS + 100);
        let bounded = bound(&text);
        assert!(bounded.len() <= WATCHDOG_DIFF_MAX_CHARS);
        assert!(bounded.contains(" characters omitted; call again with a narrower path ...]"));
        // Within budget: unchanged.
        assert_eq!(bound("short"), "short");
    }
}
