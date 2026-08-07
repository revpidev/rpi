//! Port of `packages/coding-agent/src/core/package-manager.ts` @ pi 0.82.1
//! (2efa728) — package sources (npm/git/local), install/remove, settings
//! persistence, identity dedupe, `package.json#pi` manifests, resource
//! filters, and the package slice of `resolve()`.
//!
//! Boundary with T09: top-level settings entries and auto-discovery of
//! `resolve()` (package-manager.ts:919-953, :2303-2467) live in
//! `resource_loader.rs` / `skills.rs`; this module resolves **package**
//! resources only (origin `"package"`, precedence rank 4) and feeds the
//! `PackageResourcePaths` input port of the resource loader. Session-startup
//! wiring of [`DefaultPackageManager::resolve`] is a later T14 wave.
//!
//! Update orchestration (`update`, `checkForAvailableUpdates`,
//! `updateConfiguredSources`, concurrency-4 scheduling) lands in W3; the
//! per-package update machinery (`update_git` / `ensure_git_ref` reconcile,
//! `should_update_npm_source`, `get_latest_npm_version` with
//! maxSatisfying) is implemented here already.
//!
//! Intentional differences:
//! - `.pi` → `.pir`, `PI_` → `PIR_` (ADR-0001).
//! - Command execution goes through the injectable [`PackageCommandRunner`]
//!   trait (tests use a fake runner; no network in tests). The default
//!   [`SystemPackageCommandRunner`] is synchronous `std::process`; upstream
//!   async/sync spawn pairs collapse into `run` / `run_capture`.
//! - `getEnv()` (package-manager.ts:6-23) is a Bun runtime workaround;
//!   `std::process::Command` inherits the parent environment by default.
//! - npm semver uses the `semver` crate with an npm-syntax translation
//!   layer (`||` unions, `x` wildcards, partial versions, hyphen ranges).
//!   Exotic range forms that do not translate are treated as "no valid
//!   range" (deviation D-04x).
//! - The legacy global npm root lookup (`npm root -g` / pnpm) is not
//!   cached; upstream's `globalNpmRoot` memo is a pure performance
//!   optimization.
//! - `markPathIgnoredByCloudSync` (paths.ts:103-118) runs best-effort
//!   directly (not through the runner) and ignores all errors, like
//!   upstream's fire-and-forget `spawnProcessSync` calls.
//! - Directory walks reuse the `ignore`-crate walker configuration of
//!   `resource_loader.rs`/`skills.rs` and yield sorted (deterministic)
//!   order; upstream emits raw `readdir` order (filesystem-dependent).
//! - Manifest glob entries (`collectFilesFromManifestEntries`,
//!   package-manager.ts:2263-2278) use the built-in glob matcher of
//!   `skills.rs` instead of the `glob` package's `globSync`.
//! - The managed npm root sentinel `package.json` keeps the upstream
//!   literal name `pi-extensions` (invisible implementation detail).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ignore::WalkBuilder;
use serde_json::Value;
use sha2::Digest;

use crate::config;
use crate::core::git_url::{parse_git_url, GitSource};
use crate::core::settings_manager::{
    PackageSource, PackageSourceFilter, Settings, SettingsManager,
};
use crate::core::skills::{
    self, apply_patterns, collect_skill_entries, glob_match, is_enabled_by_overrides,
    lexical_relative, match_candidates, matches_any_exact_pattern, matches_any_pattern,
    split_patterns, SkillDiscoveryMode, SourceOrigin, SourceScope,
};
use crate::tools::path_utils::resolve_path;

/// `NETWORK_TIMEOUT_MS` (package-manager.ts:38).
pub const NETWORK_TIMEOUT: Duration = Duration::from_millis(10_000);
/// `UPDATE_CHECK_CONCURRENCY` (package-manager.ts:39) — consumed by W3.
pub const UPDATE_CHECK_CONCURRENCY: usize = 4;
/// `GIT_UPDATE_CONCURRENCY` (package-manager.ts:40) — consumed by W3.
pub const GIT_UPDATE_CONCURRENCY: usize = 4;

/// `isOfflineModeEnabled` (package-manager.ts:42-46).
///
/// Divergence: upstream reads `Boolean(getEnv().PI_OFFLINE)` (any non-empty
/// value is offline), but `main.ts:476` gates the same flag through
/// `isTruthyEnvFlag` — upstream is internally inconsistent. We follow the
/// `main.ts` semantics (`1`/`true`/`yes`, case-insensitive) via the shared
/// [`environment::is_truthy_env_flag`] helper. (D-040 补记)
pub fn is_offline_mode_enabled() -> bool {
    crate::core::environment::is_truthy_env_flag(std::env::var(config::ENV_OFFLINE).ok().as_deref())
}

// ---------------------------------------------------------------------------
// npm semver helpers (semver package: valid / validRange / satisfies /
// maxSatisfying / rcompare)
// ---------------------------------------------------------------------------

/// `semver.valid` (strict): trims whitespace, allows one leading `v`.
fn parse_npm_version(version: &str) -> Option<semver::Version> {
    let trimmed = version.trim();
    let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);
    semver::Version::parse(stripped).ok()
}

/// `isExactNpmVersion` (package-manager.ts:48-50).
fn is_exact_npm_version(version: Option<&str>) -> bool {
    parse_npm_version(version.unwrap_or("")).is_some()
}

/// Translate one npm range comparator token to Rust `semver` syntax.
/// Returns `None` for forms that do not translate (treated as "no valid
/// range", matching upstream's `validRange(...) ?? undefined` fallback).
fn translate_range_token(token: &str) -> Option<String> {
    if token.is_empty() || token == "*" || token == "x" || token == "X" {
        return Some(String::new());
    }
    let index = token.find(|c: char| c.is_ascii_digit())?;
    let (op, rest) = (&token[..index], &token[index..]);
    if !matches!(op, "" | "=" | "v" | "^" | "~" | ">" | ">=" | "<" | "<=") {
        return None;
    }
    let segments: Vec<&str> = rest.split('.').collect();
    if segments.len() > 3 || segments.iter().any(|s| s.is_empty()) {
        return None;
    }
    let is_wildcard = |s: &str| s == "x" || s == "X" || s == "*";
    let wildcard_at = segments.iter().position(|s| is_wildcard(s));
    let numeric_ok = segments
        .iter()
        .all(|s| is_wildcard(s) || s.chars().all(|c| c.is_ascii_digit()));
    if !numeric_ok {
        // Prerelease/build forms pass through only for exact operators.
        if matches!(op, "" | "=" | "v") && parse_npm_version(rest).is_some() {
            return Some(format!("={}", rest.trim_start_matches('v')));
        }
        return None;
    }
    if let Some(wild) = wildcard_at {
        // `1.x` / `1.2.*` style. npm semantics: `^1.x` = `^1`, `~1.x` =
        // `~1`, `>=1.x` = `>=1.0.0`, bare `1.x` = `1.*`.
        let kept: Vec<&str> = segments[..wild].to_vec();
        match op {
            "" => {
                if kept.is_empty() {
                    Some(String::new())
                } else {
                    Some(format!("{}.*", kept.join(".")))
                }
            }
            "^" | "~" => Some(format!("{op}{}", kept.join("."))),
            ">=" | ">" | "<=" | "<" | "=" | "v" => {
                let mut padded: Vec<String> = kept.iter().map(|s| s.to_string()).collect();
                while padded.len() < 3 {
                    padded.push("0".to_string());
                }
                let op = if op == "v" { "=" } else { op };
                Some(format!("{}{}", op, padded.join(".")))
            }
            _ => None,
        }
    } else {
        match (op, segments.len()) {
            // Bare partials: npm `1.2` = `>=1.2.0 <1.3.0`, `1` = `>=1.0.0 <2.0.0`.
            ("", 1) | ("v", 1) => Some(format!(
                ">={}.0.0, <{}.0.0",
                segments[0],
                segments[0].parse::<u64>().ok()? + 1
            )),
            ("", 2) | ("v", 2) => Some(format!(
                ">={}.{}.0, <{}.{}.0",
                segments[0],
                segments[1],
                segments[0],
                segments[1].parse::<u64>().ok()? + 1
            )),
            ("", 3) | ("v", 3) => Some(format!("={}", segments.join("."))),
            _ => Some(format!("{op}{rest}")),
        }
    }
}

/// `validRange` (npm syntax) → Rust `semver::VersionReq` alternatives
/// (`||`-separated). `None` when the range does not translate.
fn npm_range_reqs(range: &str) -> Option<Vec<semver::VersionReq>> {
    let mut reqs = Vec::new();
    for alternative in range.split("||") {
        let tokens: Vec<&str> = alternative.split_whitespace().collect();
        let mut comparators: Vec<String> = Vec::new();
        let mut index = 0;
        while index < tokens.len() {
            if tokens[index] == "-" {
                return None;
            }
            // Hyphen range: `1.2.3 - 2.0.0` (full versions only).
            if index + 2 < tokens.len() && tokens[index + 1] == "-" {
                let from = parse_npm_version(tokens[index])?;
                let to = parse_npm_version(tokens[index + 2])?;
                comparators.push(format!(">={from}"));
                comparators.push(format!("<={to}"));
                index += 3;
                continue;
            }
            let comparator = translate_range_token(tokens[index])?;
            if !comparator.is_empty() {
                comparators.push(comparator);
            }
            index += 1;
        }
        let joined = if comparators.is_empty() {
            "*".to_string()
        } else {
            comparators.join(", ")
        };
        reqs.push(semver::VersionReq::parse(&joined).ok()?);
    }
    Some(reqs)
}

/// `getNpmVersionRange` (package-manager.ts:52-54): the raw range string
/// when it is a valid npm range.
fn get_npm_version_range(version: Option<&str>) -> Option<String> {
    let version = version?;
    if npm_range_reqs(version).is_some() {
        Some(version.to_string())
    } else {
        None
    }
}

/// `semver.satisfies(version, range)`.
fn npm_satisfies(version: &str, range: &str) -> bool {
    let Some(version) = parse_npm_version(version) else {
        return false;
    };
    match npm_range_reqs(range) {
        Some(reqs) => reqs.iter().any(|req| req.matches(&version)),
        None => false,
    }
}

/// `semver.maxSatisfying(versions, range)`; `None` range = highest version
/// (`[...versions].sort(rcompare)[0]`). Returns the original input string.
fn npm_max_satisfying(versions: &[String], range: Option<&str>) -> Option<String> {
    match range {
        Some(range) => {
            let reqs = npm_range_reqs(range)?;
            versions
                .iter()
                .filter_map(|v| parse_npm_version(v).map(|parsed| (v, parsed)))
                .filter(|(_, parsed)| reqs.iter().any(|req| req.matches(parsed)))
                .max_by(|(_, a), (_, b)| a.cmp(b))
                .map(|(original, _)| original.clone())
        }
        None => versions
            .iter()
            .filter_map(|v| parse_npm_version(v).map(|parsed| (v, parsed)))
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(original, _)| original.clone()),
    }
}

// ---------------------------------------------------------------------------
// Command runner (runCommand / runCommandCapture / runCommandSync,
// package-manager.ts:2555-2649)
// ---------------------------------------------------------------------------

/// One command invocation (upstream `{cwd, timeoutMs, env}` options).
#[derive(Debug, Clone)]
pub struct CommandRequest {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub timeout: Option<Duration>,
    /// Extra environment entries merged over the inherited environment
    /// (e.g. `GIT_TERMINAL_PROMPT=0`).
    pub extra_env: Vec<(String, String)>,
}

impl CommandRequest {
    pub fn new(command: &str, args: &[&str]) -> Self {
        CommandRequest {
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            timeout: None,
            extra_env: Vec::new(),
        }
    }

    pub fn with_cwd(mut self, cwd: &Path) -> Self {
        self.cwd = Some(cwd.to_path_buf());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.extra_env.push((key.to_string(), value.to_string()));
        self
    }

    /// `${command} ${args.join(" ")}` (upstream error prefix shape).
    ///
    /// URL userinfo (`scheme://user:token@host`) is redacted before display:
    /// upstream echoes the raw argv, but pir's red line forbids credentials
    /// in error messages (this string surfaces in CLI errors and could be
    /// persisted into logs). Execution still uses the unredacted args.
    pub fn display(&self) -> String {
        let args = self
            .args
            .iter()
            .map(|arg| redact_url_userinfo(arg))
            .collect::<Vec<_>>();
        format!("{} {}", self.command, args.join(" "))
    }
}

/// Replace `scheme://userinfo@` with `scheme://***@` in a single argument.
/// Only rewrites when the userinfo part actually contains a `:` (a password/
/// token is present); bare `user@host` logins are left untouched.
fn redact_url_userinfo(arg: &str) -> String {
    let Some(scheme_end) = arg.find("://") else {
        return arg.to_string();
    };
    let rest = &arg[scheme_end + 3..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let Some(at) = authority.rfind('@') else {
        return arg.to_string();
    };
    if !authority[..at].contains(':') {
        return arg.to_string();
    }
    let host_start = scheme_end + 3 + at + 1;
    format!("{}://***@{}", &arg[..scheme_end], &arg[host_start..])
}

/// Injectable command execution (upstream `spawnProcess` call sites). The
/// CLI uses [`SystemPackageCommandRunner`]; tests inject a fake.
pub trait PackageCommandRunner: Send + Sync {
    /// `runCommand`: stdio inherited; `Ok` on exit code 0.
    fn run(&self, request: &CommandRequest) -> Result<(), String>;
    /// `runCommandCapture` / `runCommandSync`: captured, trimmed stdout on
    /// exit code 0.
    fn run_capture(&self, request: &CommandRequest) -> Result<String, String>;
}

/// Default runner: real processes via `std::process`.
pub struct SystemPackageCommandRunner;

impl PackageCommandRunner for SystemPackageCommandRunner {
    fn run(&self, request: &CommandRequest) -> Result<(), String> {
        let mut command = std::process::Command::new(&request.command);
        command.args(&request.args);
        if let Some(cwd) = &request.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &request.extra_env {
            command.env(key, value);
        }
        // stdio inherited, like upstream's headless `"inherit"` branch.
        let status = command.spawn().map_err(|e| e.to_string())?;
        match status.wait_with_output() {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(format!(
                "{} failed with code {}",
                request.display(),
                output
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "null".to_string())
            )),
            Err(e) => Err(e.to_string()),
        }
    }

    fn run_capture(&self, request: &CommandRequest) -> Result<String, String> {
        use std::io::Read;
        let mut command = std::process::Command::new(&request.command);
        command
            .args(&request.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(cwd) = &request.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &request.extra_env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        // Drain both pipes on threads so a full pipe buffer cannot block
        // the child while we poll for the timeout.
        let stdout_reader = child.stdout.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = pipe.read_to_end(&mut buffer);
                buffer
            })
        });
        let stderr_reader = child.stderr.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = pipe.read_to_end(&mut buffer);
                buffer
            })
        });
        let deadline = request.timeout.map(|t| std::time::Instant::now() + t);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!(
                            "{} timed out after {}ms",
                            request.display(),
                            request.timeout.map(|t| t.as_millis()).unwrap_or(0)
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e.to_string()),
            }
        };
        let stdout = stdout_reader
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        let stderr = stderr_reader
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        if status.success() {
            return Ok(stdout.trim().to_string());
        }
        let exit_status = match status.code() {
            Some(code) => format!("code {code}"),
            None => "signal unknown".to_string(),
        };
        let detail = if stderr.is_empty() { stdout } else { stderr };
        Err(format!(
            "{} failed with {}: {}",
            request.display(),
            exit_status,
            detail
        ))
    }
}

// ---------------------------------------------------------------------------
// Progress events (package-manager.ts:78-85)
// ---------------------------------------------------------------------------

/// `ProgressEvent["type"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressKind {
    Start,
    Progress,
    Complete,
    Error,
}

/// `ProgressEvent["action"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressAction {
    Install,
    Remove,
    Update,
    Clone,
    Pull,
}

/// `ProgressEvent` (package-manager.ts:78-83).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressEvent {
    pub kind: ProgressKind,
    pub action: ProgressAction,
    pub source: String,
    pub message: Option<String>,
}

type ProgressCallback = Box<dyn Fn(&ProgressEvent) + Send + Sync>;

// ---------------------------------------------------------------------------
// Package sources (package-manager.ts:127-141, parseSource :1435-1460)
// ---------------------------------------------------------------------------

/// `NpmSource` (package-manager.ts:127-134).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmSource {
    pub spec: String,
    pub name: String,
    pub version: Option<String>,
    pub range: Option<String>,
    pub pinned: bool,
}

/// `ParsedSource` (package-manager.ts:141).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSource {
    Npm(NpmSource),
    Git(GitSource),
    Local(String),
}

/// `isLocalPath` (utils/paths.ts:41-56).
fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !(trimmed.starts_with("npm:")
        || trimmed.starts_with("git:")
        || trimmed.starts_with("github:")
        || trimmed.starts_with("http:")
        || trimmed.starts_with("https:")
        || trimmed.starts_with("ssh:"))
}

/// `parseNpmSpec` (package-manager.ts:1720-1728):
/// `/^(@?[^@]+(?:\/[^@]+)?)(?:@(.+))?$/`.
fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let separator = spec
        .char_indices()
        .skip(1)
        .find(|(_, c)| *c == '@')
        .map(|(i, _)| i);
    match separator {
        Some(index) if index + 1 < spec.len() => (
            spec[..index].to_string(),
            Some(spec[index + 1..].to_string()),
        ),
        // No match (or empty version): upstream falls back to the raw spec.
        _ => (spec.to_string(), None),
    }
}

/// `parseSource` (package-manager.ts:1435-1460): `npm:` prefix → local
/// path check → git URL → local fallback. Bare names are local paths.
pub fn parse_source(source: &str) -> ParsedSource {
    if let Some(spec) = source.strip_prefix("npm:") {
        let spec = spec.trim();
        let (name, version) = parse_npm_spec(spec);
        return ParsedSource::Npm(NpmSource {
            spec: spec.to_string(),
            name,
            pinned: is_exact_npm_version(version.as_deref()),
            range: get_npm_version_range(version.as_deref()),
            version,
        });
    }

    if is_local_path(source) {
        return ParsedSource::Local(source.to_string());
    }

    if let Some(git) = parse_git_url(source) {
        return ParsedSource::Git(git);
    }

    ParsedSource::Local(source.to_string())
}

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// `MissingSourceAction` (package-manager.ts:76).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingSourceAction {
    Install,
    Skip,
    Error,
}

/// `ConfiguredPackage` (package-manager.ts:94-99).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: SourceScope,
    pub filtered: bool,
    pub installed_path: Option<PathBuf>,
}

/// `PackageUpdate` (package-manager.ts:87-92) — produced by the W3 update
/// check; defined here so the per-package check functions have their type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageUpdate {
    pub source: String,
    pub display_name: String,
    pub kind: PackageUpdateKind,
    pub scope: SourceScope,
}

/// `PackageUpdate["type"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageUpdateKind {
    Npm,
    Git,
}

/// `ConfiguredUpdateSource` (package-manager.ts, `update` locals).
#[derive(Debug, Clone)]
struct ConfiguredUpdateSource {
    source: String,
    scope: SourceScope,
}

/// `NpmUpdateTarget` (package-manager.ts, `updateConfiguredSources` locals).
#[derive(Debug, Clone)]
struct NpmUpdateTarget {
    source: String,
    scope: SourceScope,
    parsed: NpmSource,
}

/// `GitUpdateTarget` entry shape (package-manager.ts,
/// `updateConfiguredSources` locals).
#[derive(Debug, Clone)]
struct GitUpdateEntry {
    source: String,
    scope: SourceScope,
    parsed: GitSource,
}

/// `runWithConcurrency` (package-manager.ts:1646-1668): a worker pool over
/// scoped threads (the manager methods are synchronous, unlike upstream's
/// promises). Results keep task order; the first error in task order
/// aborts (upstream rejects with the temporally first one).
fn run_with_concurrency<T, F>(tasks: Vec<F>, limit: usize) -> Result<Vec<T>, String>
where
    T: Send,
    F: FnOnce() -> Result<T, String> + Send,
{
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    if tasks.is_empty() {
        return Ok(Vec::new());
    }
    let worker_count = limit.max(1).min(tasks.len());
    let tasks: Vec<Mutex<Option<F>>> = tasks
        .into_iter()
        .map(|task| Mutex::new(Some(task)))
        .collect();
    let results: Vec<Mutex<Option<Result<T, String>>>> =
        (0..tasks.len()).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= tasks.len() {
                    return;
                }
                let task = tasks[index]
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                let Some(task) = task else {
                    continue;
                };
                let result = task();
                *results[index].lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            });
        }
    });
    let mut out = Vec::with_capacity(results.len());
    for slot in results {
        match slot.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(Ok(value)) => out.push(value),
            Some(Err(error)) => return Err(error),
            None => return Err("update worker panicked".to_string()),
        }
    }
    Ok(out)
}

/// Package-scoped `ResolvedResource` (package-manager.ts:63-67): `origin`
/// is always `"package"`; `source` is the configured package source
/// string (upstream `PathMetadata.source`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackageResource {
    pub path: PathBuf,
    pub enabled: bool,
    pub source: String,
    pub scope: SourceScope,
    pub base_dir: Option<PathBuf>,
}

/// The package slice of `ResolvedPaths` (package-manager.ts:69-74).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPackagePaths {
    pub extensions: Vec<ResolvedPackageResource>,
    pub skills: Vec<ResolvedPackageResource>,
    pub prompts: Vec<ResolvedPackageResource>,
    pub themes: Vec<ResolvedPackageResource>,
}

impl ResolvedPackagePaths {
    /// Convert into the resource loader's T14 input port
    /// (`resource_loader::PackageResourcePaths`).
    pub fn to_package_resource_paths(&self) -> crate::core::resource_loader::PackageResourcePaths {
        fn convert(
            entries: &[ResolvedPackageResource],
        ) -> Vec<crate::core::resource_loader::PackageResource> {
            entries
                .iter()
                .map(|entry| crate::core::resource_loader::PackageResource {
                    path: entry.path.clone(),
                    enabled: entry.enabled,
                    scope: entry.scope,
                    base_dir: entry.base_dir.clone(),
                })
                .collect()
        }
        crate::core::resource_loader::PackageResourcePaths {
            extension_paths: convert(&self.extensions),
            skill_paths: convert(&self.skills),
            prompt_paths: convert(&self.prompts),
            theme_paths: convert(&self.themes),
        }
    }
}

/// `onMissing` callback (package-manager.ts:102).
pub type OnMissing<'a> = Option<&'a mut dyn FnMut(&str) -> MissingSourceAction>;

/// `PiManifest` (package-manager.ts:158-163).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PiManifest {
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

/// `ResourceType` (package-manager.ts:198-200).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

const RESOURCE_TYPES: [ResourceType; 4] = [
    ResourceType::Extensions,
    ResourceType::Skills,
    ResourceType::Prompts,
    ResourceType::Themes,
];

impl ResourceType {
    /// Convention directory name (`join(packageRoot, resourceType)`).
    fn dir_name(self) -> &'static str {
        match self {
            ResourceType::Extensions => "extensions",
            ResourceType::Skills => "skills",
            ResourceType::Prompts => "prompts",
            ResourceType::Themes => "themes",
        }
    }

    fn manifest_entries(self, manifest: &PiManifest) -> Option<&Vec<String>> {
        match self {
            ResourceType::Extensions => manifest.extensions.as_ref(),
            ResourceType::Skills => manifest.skills.as_ref(),
            ResourceType::Prompts => manifest.prompts.as_ref(),
            ResourceType::Themes => manifest.themes.as_ref(),
        }
    }

    fn filter_patterns(self, filter: &PackageSourceFilter) -> Option<&Vec<String>> {
        match self {
            ResourceType::Extensions => filter.extensions.as_ref(),
            ResourceType::Skills => filter.skills.as_ref(),
            ResourceType::Prompts => filter.prompts.as_ref(),
            ResourceType::Themes => filter.themes.as_ref(),
        }
    }
}

/// Insertion-ordered, first-write-wins accumulator (`addResource`,
/// package-manager.ts:2506-2516).
#[derive(Default)]
struct ResourceAccumulator {
    extensions: Vec<ResolvedPackageResource>,
    skills: Vec<ResolvedPackageResource>,
    prompts: Vec<ResolvedPackageResource>,
    themes: Vec<ResolvedPackageResource>,
    seen: HashSet<PathBuf>,
}

impl ResourceAccumulator {
    fn target_mut(&mut self, resource_type: ResourceType) -> &mut Vec<ResolvedPackageResource> {
        match resource_type {
            ResourceType::Extensions => &mut self.extensions,
            ResourceType::Skills => &mut self.skills,
            ResourceType::Prompts => &mut self.prompts,
            ResourceType::Themes => &mut self.themes,
        }
    }

    /// `addResource` (package-manager.ts:2506-2516).
    fn add(&mut self, resource_type: ResourceType, resource: ResolvedPackageResource) {
        if resource.path.as_os_str().is_empty() {
            return;
        }
        if self.seen.insert(resource.path.clone()) {
            self.target_mut(resource_type).push(resource);
        }
    }

    /// `toResolvedPaths` (package-manager.ts:2527-2553), package slice: all
    /// entries are rank 4 (origin `"package"`), so the stable rank sort is
    /// identity here; only the canonical-path dedupe remains. The dedupe set
    /// is per resource type, like upstream's per-type `mapToResolved`.
    fn into_resolved(self) -> ResolvedPackagePaths {
        fn dedupe(entries: Vec<ResolvedPackageResource>) -> Vec<ResolvedPackageResource> {
            let mut seen_canonical = HashSet::new();
            entries
                .into_iter()
                .filter(|entry| seen_canonical.insert(skills::canonicalize_path(&entry.path)))
                .collect::<Vec<_>>()
        }
        ResolvedPackagePaths {
            extensions: dedupe(self.extensions),
            skills: dedupe(self.skills),
            prompts: dedupe(self.prompts),
            themes: dedupe(self.themes),
        }
    }
}

/// `PathMetadata` (package-manager.ts:56-61) — the full-resolve metadata
/// covering package and top-level origins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePathMetadata {
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    pub base_dir: Option<PathBuf>,
}

/// `ResolvedResource` (package-manager.ts:63-67).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedResource {
    pub path: PathBuf,
    pub enabled: bool,
    pub metadata: ResourcePathMetadata,
}

/// `ResolvedPaths` (package-manager.ts:69-74) — the full `resolve()`
/// output (package + top-level + auto-discovered), backing `pir config`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub extensions: Vec<ResolvedResource>,
    pub skills: Vec<ResolvedResource>,
    pub prompts: Vec<ResolvedResource>,
    pub themes: Vec<ResolvedResource>,
}

/// `resourcePrecedenceRank` (package-manager.ts:184-188): lower rank wins
/// name collisions.
fn resource_precedence_rank(metadata: &ResourcePathMetadata) -> u8 {
    if metadata.origin == SourceOrigin::Package {
        return 4;
    }
    let scope_base = if metadata.scope == SourceScope::Project {
        0
    } else {
        2
    };
    scope_base + u8::from(metadata.source != "local")
}

/// The upstream `Map`-based `ResourceAccumulator` (package-manager.ts:
/// 165-170): insertion-ordered, first-write-wins on the raw path
/// (`addResource`, package-manager.ts:2506-2516).
#[derive(Default)]
struct FullResourceAccumulator {
    extensions: Vec<ResolvedResource>,
    skills: Vec<ResolvedResource>,
    prompts: Vec<ResolvedResource>,
    themes: Vec<ResolvedResource>,
    seen: HashSet<PathBuf>,
}

impl FullResourceAccumulator {
    fn target_mut(&mut self, resource_type: ResourceType) -> &mut Vec<ResolvedResource> {
        match resource_type {
            ResourceType::Extensions => &mut self.extensions,
            ResourceType::Skills => &mut self.skills,
            ResourceType::Prompts => &mut self.prompts,
            ResourceType::Themes => &mut self.themes,
        }
    }

    /// `addResource` (package-manager.ts:2506-2516).
    fn add(
        &mut self,
        resource_type: ResourceType,
        path: PathBuf,
        metadata: ResourcePathMetadata,
        enabled: bool,
    ) {
        if path.as_os_str().is_empty() {
            return;
        }
        if self.seen.insert(path.clone()) {
            self.target_mut(resource_type).push(ResolvedResource {
                path,
                enabled,
                metadata,
            });
        }
    }

    /// Drain the package-slice accumulator (already first-write-wins in
    /// upstream's add order) into this one with `origin: "package"`.
    fn add_package_slice(&mut self, packages: ResourceAccumulator) {
        for (resource_type, entries) in [
            (ResourceType::Extensions, packages.extensions),
            (ResourceType::Skills, packages.skills),
            (ResourceType::Prompts, packages.prompts),
            (ResourceType::Themes, packages.themes),
        ] {
            for entry in entries {
                self.add(
                    resource_type,
                    entry.path,
                    ResourcePathMetadata {
                        source: entry.source,
                        scope: entry.scope,
                        origin: SourceOrigin::Package,
                        base_dir: entry.base_dir,
                    },
                    entry.enabled,
                );
            }
        }
    }

    /// `toResolvedPaths` (package-manager.ts:2527-2553): stable rank sort,
    /// then per-type canonical-path dedupe.
    fn into_resolved_paths(self) -> ResolvedPaths {
        fn map_to_resolved(mut entries: Vec<ResolvedResource>) -> Vec<ResolvedResource> {
            entries.sort_by_key(|entry| resource_precedence_rank(&entry.metadata));
            let mut seen = HashSet::new();
            entries
                .into_iter()
                .filter(|entry| seen.insert(skills::canonicalize_path(&entry.path)))
                .collect()
        }
        ResolvedPaths {
            extensions: map_to_resolved(self.extensions),
            skills: map_to_resolved(self.skills),
            prompts: map_to_resolved(self.prompts),
            themes: map_to_resolved(self.themes),
        }
    }
}

/// `(settings[key] ?? []) as string[]` — the settings schema constrains
/// these arrays to strings; non-string entries are dropped.
fn settings_string_array(settings: &Settings, key: &str) -> Vec<String> {
    settings
        .as_map()
        .get(key)
        .and_then(Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `collectAutoPromptEntries` / `collectAutoThemeEntries`
/// (package-manager.ts:462-525): direct children of `dir` with the given
/// extension; dotfiles and `node_modules` are skipped and the directory's
/// own ignore files are honored (the `configure_walk` walker config).
fn collect_auto_file_entries(dir: &Path, extension: &str) -> Vec<PathBuf> {
    walk_entries(dir, Some(1))
        .into_iter()
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .map(|name| name.to_string_lossy().ends_with(extension))
                    .unwrap_or(false)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// DefaultPackageManager
// ---------------------------------------------------------------------------

/// `PackageManagerOptions` (package-manager.ts:119-123) plus runner /
/// offline injection seams.
pub struct PackageManagerOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub settings_manager: SettingsManager,
    /// Defaults to [`SystemPackageCommandRunner`].
    pub runner: Option<Arc<dyn PackageCommandRunner>>,
    /// Defaults to [`is_offline_mode_enabled`] (env `PIR_OFFLINE`).
    pub offline: Option<bool>,
}

/// `DefaultPackageManager` (package-manager.ts:795).
pub struct DefaultPackageManager {
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings_manager: SettingsManager,
    runner: Arc<dyn PackageCommandRunner>,
    offline: bool,
    progress: Option<ProgressCallback>,
}

/// `getExtensionTempFolder` (package-manager.ts:221-226): create
/// `<agentDir>/tmp/extensions` with mode 0700.
pub fn get_extension_temp_folder(agent_dir: &Path) -> Result<PathBuf, String> {
    let temp_folder = agent_dir.join("tmp").join("extensions");
    std::fs::create_dir_all(&temp_folder).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp_folder, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    Ok(temp_folder)
}

impl DefaultPackageManager {
    /// Real-runner constructor (CLI path).
    pub fn new(cwd: PathBuf, agent_dir: PathBuf, settings_manager: SettingsManager) -> Self {
        Self::with_options(PackageManagerOptions {
            cwd,
            agent_dir,
            settings_manager,
            runner: None,
            offline: None,
        })
    }

    pub fn with_options(options: PackageManagerOptions) -> Self {
        let process_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        DefaultPackageManager {
            cwd: resolve_path(&options.cwd.to_string_lossy(), &process_cwd),
            agent_dir: resolve_path(&options.agent_dir.to_string_lossy(), &process_cwd),
            settings_manager: options.settings_manager,
            runner: options
                .runner
                .unwrap_or_else(|| Arc::new(SystemPackageCommandRunner)),
            offline: options.offline.unwrap_or_else(is_offline_mode_enabled),
            progress: None,
        }
    }

    /// `setProgressCallback` (package-manager.ts:809-811).
    pub fn set_progress_callback(&mut self, callback: Option<ProgressCallback>) {
        self.progress = callback;
    }

    fn emit_progress(&self, event: &ProgressEvent) {
        if let Some(callback) = &self.progress {
            callback(event);
        }
    }

    /// `withProgress` (package-manager.ts:884-899).
    fn with_progress(
        &self,
        action: ProgressAction,
        source: &str,
        message: &str,
        operation: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.emit_progress(&ProgressEvent {
            kind: ProgressKind::Start,
            action,
            source: source.to_string(),
            message: Some(message.to_string()),
        });
        match operation() {
            Ok(()) => {
                self.emit_progress(&ProgressEvent {
                    kind: ProgressKind::Complete,
                    action,
                    source: source.to_string(),
                    message: None,
                });
                Ok(())
            }
            Err(error) => {
                self.emit_progress(&ProgressEvent {
                    kind: ProgressKind::Error,
                    action,
                    source: source.to_string(),
                    message: Some(error.clone()),
                });
                Err(error)
            }
        }
    }

    // -------------------------------------------------------------------
    // Settings read/write (addSourceToSettings / removeSourceFromSettings,
    // package-manager.ts:813-860)
    // -------------------------------------------------------------------

    /// `[...(settings.packages ?? [])]` on a per-scope settings object.
    fn packages_of(settings: &Settings) -> Vec<PackageSource> {
        settings
            .as_map()
            .get("packages")
            .and_then(Value::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn settings_for_scope(&self, scope: SourceScope) -> Settings {
        if scope == SourceScope::Project {
            self.settings_manager.get_project_settings()
        } else {
            self.settings_manager.get_global_settings()
        }
    }

    fn write_packages(
        &mut self,
        scope: SourceScope,
        packages: Vec<PackageSource>,
    ) -> Result<(), String> {
        if scope == SourceScope::Project {
            self.settings_manager
                .set_project_packages(packages)
                .map_err(|e| e.to_string())
        } else {
            self.settings_manager.set_packages(packages);
            Ok(())
        }
    }

    fn package_source_string(pkg: &PackageSource) -> &str {
        match pkg {
            PackageSource::Source(source) => source,
            PackageSource::Filtered(filter) => &filter.source,
        }
    }

    /// `addSourceToSettings` (package-manager.ts:813-842).
    pub fn add_source_to_settings(&mut self, source: &str, local: bool) -> Result<bool, String> {
        let scope = if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        let current = DefaultPackageManager::packages_of(&self.settings_for_scope(scope));
        let normalized = self.normalize_package_source_for_settings(source, scope);
        let match_index = current
            .iter()
            .position(|existing| self.package_sources_match(existing, source, scope));
        if let Some(index) = match_index {
            let existing = &current[index];
            if Self::package_source_string(existing) == normalized {
                return Ok(false);
            }
            let mut next = current.clone();
            next[index] = match existing {
                PackageSource::Source(_) => PackageSource::Source(normalized),
                PackageSource::Filtered(filter) => {
                    let mut filter = filter.clone();
                    filter.source = normalized;
                    PackageSource::Filtered(filter)
                }
            };
            self.write_packages(scope, next)?;
            return Ok(true);
        }
        let mut next = current;
        next.push(PackageSource::Source(normalized));
        self.write_packages(scope, next)?;
        Ok(true)
    }

    /// `removeSourceFromSettings` (package-manager.ts:844-860).
    pub fn remove_source_from_settings(
        &mut self,
        source: &str,
        local: bool,
    ) -> Result<bool, String> {
        let scope = if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        let current = DefaultPackageManager::packages_of(&self.settings_for_scope(scope));
        let next: Vec<PackageSource> = current
            .iter()
            .filter(|existing| !self.package_sources_match(existing, source, scope))
            .cloned()
            .collect();
        if next.len() == current.len() {
            return Ok(false);
        }
        self.write_packages(scope, next)?;
        Ok(true)
    }

    // -------------------------------------------------------------------
    // Identity / matching / dedupe (package-manager.ts:1358-1433,
    // 1676-1718)
    // -------------------------------------------------------------------

    /// `getPackageIdentity` (package-manager.ts:1676-1690): npm by name,
    /// git by `host/path`, local by absolute path (scope-relative when a
    /// scope is given).
    pub fn get_package_identity(&self, source: &str, scope: Option<SourceScope>) -> String {
        match parse_source(source) {
            ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Local(path) => match scope {
                Some(scope) => {
                    let base_dir = self.get_base_dir_for_scope(scope);
                    format!(
                        "local:{}",
                        self.resolve_path_from_base(&path, &base_dir).display()
                    )
                }
                None => format!("local:{}", self.resolve_path(&path).display()),
            },
        }
    }

    /// `getSourceMatchKeyForInput` (package-manager.ts:1362-1371).
    fn source_match_key_for_input(&self, source: &str) -> String {
        match parse_source(source) {
            ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Local(path) => format!("local:{}", self.resolve_path(&path).display()),
        }
    }

    /// `getSourceMatchKeyForSettings` (package-manager.ts:1373-1383).
    fn source_match_key_for_settings(&self, source: &str, scope: SourceScope) -> String {
        match parse_source(source) {
            ParsedSource::Npm(npm) => format!("npm:{}", npm.name),
            ParsedSource::Git(git) => format!("git:{}/{}", git.host, git.path),
            ParsedSource::Local(path) => {
                let base_dir = self.get_base_dir_for_scope(scope);
                format!(
                    "local:{}",
                    self.resolve_path_from_base(&path, &base_dir).display()
                )
            }
        }
    }

    /// `packageSourcesMatch` (package-manager.ts:1418-1422).
    fn package_sources_match(
        &self,
        existing: &PackageSource,
        input_source: &str,
        scope: SourceScope,
    ) -> bool {
        let left = self.source_match_key_for_settings(Self::package_source_string(existing), scope);
        let right = self.source_match_key_for_input(input_source);
        left == right
    }

    /// `normalizePackageSourceForSettings` (package-manager.ts:1424-1433):
    /// local sources are stored relative to the scope's settings base dir.
    fn normalize_package_source_for_settings(&self, source: &str, scope: SourceScope) -> String {
        if !matches!(parse_source(source), ParsedSource::Local(_)) {
            return source.to_string();
        }
        let ParsedSource::Local(path) = parse_source(source) else {
            return source.to_string();
        };
        let base_dir = self.get_base_dir_for_scope(scope);
        let resolved = self.resolve_path(&path);
        let relative = lexical_relative(&base_dir, &resolved);
        let relative = relative.to_string_lossy();
        if relative.is_empty() {
            ".".to_string()
        } else {
            relative.into_owned()
        }
    }

    /// `dedupePackages` (package-manager.ts:1697-1718): project scope wins
    /// over user for the same identity; a project entry with
    /// `autoload: false` is a delta over the user entry, so both are kept.
    fn dedupe_packages(
        &self,
        packages: Vec<(PackageSource, SourceScope)>,
    ) -> Vec<(PackageSource, SourceScope)> {
        let mut result: Vec<(PackageSource, SourceScope)> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for entry in packages {
            let identity =
                self.get_package_identity(Self::package_source_string(&entry.0), Some(entry.1));
            match seen.get(&identity) {
                None => {
                    seen.insert(identity, result.len());
                    result.push(entry);
                }
                Some(&index) => {
                    let existing = &result[index];
                    if existing.1 == SourceScope::Project && entry.1 == SourceScope::User {
                        if matches!(
                            &existing.0,
                            PackageSource::Filtered(filter) if filter.autoload == Some(false)
                        ) {
                            result.push(entry);
                        }
                    } else if entry.1 == SourceScope::Project {
                        result[index] = entry;
                    }
                }
            }
        }
        result
    }

    // -------------------------------------------------------------------
    // Paths
    // -------------------------------------------------------------------

    /// `assertProjectTrustedForScope` (package-manager.ts:1730-1734).
    fn assert_project_trusted_for_scope(&self, scope: SourceScope) -> Result<(), String> {
        if scope == SourceScope::Project && !self.settings_manager.is_project_trusted() {
            return Err(
                "Project is not trusted; refusing to access project package storage".to_string(),
            );
        }
        Ok(())
    }

    /// `getBaseDirForScope` (package-manager.ts:2065-2074). The project
    /// trust gate surfaces as an empty-cwd fallback never taken: callers
    /// gate first. Kept total by returning cwd for temporary scope.
    fn get_base_dir_for_scope(&self, scope: SourceScope) -> PathBuf {
        match scope {
            SourceScope::Project => config::get_project_config_dir(&self.cwd),
            SourceScope::User => self.agent_dir.clone(),
            SourceScope::Temporary => self.cwd.clone(),
        }
    }

    /// `resolvePath` (package-manager.ts:2076-2078): trim, expand `~`,
    /// resolve against cwd, lexical cleanup.
    fn resolve_path(&self, input: &str) -> PathBuf {
        resolve_path(input.trim(), &self.cwd)
    }

    /// `resolvePathFromBase` (package-manager.ts:2080-2082).
    fn resolve_path_from_base(&self, input: &str, base_dir: &Path) -> PathBuf {
        resolve_path(input.trim(), base_dir)
    }

    /// `resolveManagedPath` (package-manager.ts:2056-2063): segments are
    /// resolved Node-style and must stay under `root`.
    fn resolve_managed_path(&self, root: &Path, parts: &[&str]) -> Result<PathBuf, String> {
        let resolved_root = resolve_path(&root.to_string_lossy(), &self.cwd);
        let mut resolved = resolved_root.clone();
        for part in parts {
            resolved = resolve_path(part, &resolved);
        }
        // Both inputs are lexically cleaned absolutes, so component-wise
        // `starts_with` matches upstream's `resolvedRoot + sep` check.
        if resolved != resolved_root && !resolved.starts_with(&resolved_root) {
            return Err(format!(
                "Refusing to use path outside package install root: {}",
                resolved.display()
            ));
        }
        Ok(resolved)
    }

    /// `getTemporaryDir` (package-manager.ts:2047-2054): sha256 of
    /// `{prefix}-{suffix}`, first 8 hex chars.
    fn get_temporary_dir(&self, prefix: &str, suffix: Option<&str>) -> Result<PathBuf, String> {
        let root =
            self.resolve_managed_path(&get_extension_temp_folder(&self.agent_dir)?, &[prefix])?;
        let hash = sha2::Sha256::digest(format!("{prefix}-{}", suffix.unwrap_or("")).as_bytes());
        let hash = hash[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        match suffix {
            Some(suffix) if !suffix.is_empty() => {
                self.resolve_managed_path(&root, &[&hash, suffix])
            }
            _ => self.resolve_managed_path(&root, &[&hash]),
        }
    }

    /// `getNpmInstallRoot` (package-manager.ts:1956-1965).
    fn get_npm_install_root(&self, scope: SourceScope, temporary: bool) -> Result<PathBuf, String> {
        if temporary {
            return self.get_temporary_dir("npm", None);
        }
        match scope {
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(config::get_project_config_dir(&self.cwd).join("npm"))
            }
            _ => Ok(self.agent_dir.join("npm")),
        }
    }

    /// `getManagedNpmInstallPath` (package-manager.ts:1997-2006).
    fn get_managed_npm_install_path(
        &self,
        source: &NpmSource,
        scope: SourceScope,
    ) -> Result<PathBuf, String> {
        match scope {
            SourceScope::Temporary => Ok(self
                .get_temporary_dir("npm", None)?
                .join("node_modules")
                .join(&source.name)),
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(config::get_project_config_dir(&self.cwd)
                    .join("npm")
                    .join("node_modules")
                    .join(&source.name))
            }
            SourceScope::User => Ok(self
                .agent_dir
                .join("npm")
                .join("node_modules")
                .join(&source.name)),
        }
    }

    /// `getNpmInstallPath` (package-manager.ts:2016-2023): managed path,
    /// falling back to a legacy global install for user scope.
    ///
    /// Divergence: the "Project is not trusted" error from
    /// `get_managed_npm_install_path` is swallowed here (`unwrap_or_default`)
    /// instead of propagating (upstream throws). Practically unreachable —
    /// the CLI never resolves project-scope npm paths for untrusted projects
    /// because SettingsManager yields empty settings there — so the function
    /// keeps its infallible `PathBuf` return shape. (D-040 补记)
    fn get_npm_install_path(&self, source: &NpmSource, scope: SourceScope) -> PathBuf {
        let managed = self
            .get_managed_npm_install_path(source, scope)
            .unwrap_or_default();
        if scope != SourceScope::User || managed.exists() {
            return managed;
        }
        match self.get_legacy_global_npm_install_path(source) {
            Some(legacy) if legacy.exists() => legacy,
            _ => managed,
        }
    }

    /// `getGitInstallRoot` (package-manager.ts:2036-2045).
    fn get_git_install_root(&self, scope: SourceScope) -> Result<Option<PathBuf>, String> {
        match scope {
            SourceScope::Temporary => Ok(None),
            SourceScope::Project => {
                self.assert_project_trusted_for_scope(scope)?;
                Ok(Some(config::get_project_config_dir(&self.cwd).join("git")))
            }
            SourceScope::User => Ok(Some(self.agent_dir.join("git"))),
        }
    }

    /// `getGitInstallPath` (package-manager.ts:2025-2034).
    fn get_git_install_path(
        &self,
        source: &GitSource,
        scope: SourceScope,
    ) -> Result<PathBuf, String> {
        if scope == SourceScope::Temporary {
            return self.get_temporary_dir(&format!("git-{}", source.host), Some(&source.path));
        }
        let install_root = self
            .get_git_install_root(scope)?
            .ok_or_else(|| "Missing git install root".to_string())?;
        self.resolve_managed_path(&install_root, &[&source.host, &source.path])
    }

    // -------------------------------------------------------------------
    // npm command wrapper (package-manager.ts:1736-1795)
    // -------------------------------------------------------------------

    /// `getNpmCommand` (package-manager.ts:1736-1746): the configured
    /// `npmCommand` argv wrapper, defaulting to plain `npm`.
    fn get_npm_command(&self) -> Result<(String, Vec<String>), String> {
        let configured = self.settings_manager.get_npm_command();
        match configured {
            None => Ok(("npm".to_string(), Vec::new())),
            Some(command) if command.is_empty() => Ok(("npm".to_string(), Vec::new())),
            Some(command) => {
                let first = command.first().map(String::as_str).unwrap_or("");
                if first.is_empty() {
                    return Err(
                        "Invalid npmCommand: first array entry must be a non-empty command"
                            .to_string(),
                    );
                }
                Ok((first.to_string(), command[1..].to_vec()))
            }
        }
    }

    /// `getPackageManagerName` (package-manager.ts:1748-1754): the part
    /// after the last `--`, else the command basename without `.cmd`/`.exe`.
    fn get_package_manager_name(&self) -> Result<String, String> {
        let (command, args) = self.get_npm_command()?;
        let mut parts = vec![command];
        parts.extend(args);
        let after_separator = parts
            .iter()
            .rposition(|part| part == "--")
            .and_then(|index| parts.get(index + 1));
        let package_manager_command = match after_separator {
            Some(part) => part.clone(),
            None => parts.first().cloned().unwrap_or_default(),
        };
        let base = Path::new(&package_manager_command)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let lower = base.to_lowercase();
        for suffix in [".cmd", ".exe"] {
            if lower.ends_with(suffix) {
                return Ok(base[..base.len() - suffix.len()].to_string());
            }
        }
        Ok(base)
    }

    /// `runNpmCommand` (package-manager.ts:1756-1759).
    fn run_npm_command(&self, args: &[String], cwd: Option<&Path>) -> Result<(), String> {
        let (command, prefix_args) = self.get_npm_command()?;
        let mut request = CommandRequest {
            command,
            args: prefix_args,
            cwd: cwd.map(Path::to_path_buf),
            timeout: None,
            extra_env: Vec::new(),
        };
        request.args.extend(args.iter().cloned());
        self.runner.run(&request)
    }

    /// `getGitDependencyInstallArgs` (package-manager.ts:1761-1767): with a
    /// configured `npmCommand` argv wrapper, git package dependency installs
    /// degrade to a bare `install`.
    fn get_git_dependency_install_args(&self) -> Vec<String> {
        match self.settings_manager.get_npm_command() {
            Some(command) if !command.is_empty() => vec!["install".to_string()],
            _ => vec!["install".to_string(), "--omit=dev".to_string()],
        }
    }

    /// `getNpmInstallArgs` (package-manager.ts:1774-1795): peer dependency
    /// resolution is disabled for managed installs (`--legacy-peer-deps`
    /// and the bun/pnpm equivalents).
    fn get_npm_install_args(
        &self,
        specs: &[String],
        install_root: &Path,
    ) -> Result<Vec<String>, String> {
        let package_manager_name = self.get_package_manager_name()?;
        let install_root = install_root.to_string_lossy().into_owned();
        let mut args: Vec<String> = vec!["install".to_string()];
        args.extend(specs.iter().cloned());
        match package_manager_name.as_str() {
            "bun" => {
                args.push("--cwd".to_string());
                args.push(install_root);
                args.push("--omit=peer".to_string());
            }
            "pnpm" => {
                args.push("--prefix".to_string());
                args.push(install_root);
                args.push("--config.auto-install-peers=false".to_string());
                args.push("--config.strict-peer-dependencies=false".to_string());
                args.push("--config.strict-dep-builds=false".to_string());
            }
            _ => {
                args.push("--prefix".to_string());
                args.push(install_root);
                args.push("--legacy-peer-deps".to_string());
            }
        }
        Ok(args)
    }

    // -------------------------------------------------------------------
    // Legacy global npm lookups (package-manager.ts:1967-2014)
    // -------------------------------------------------------------------

    fn run_npm_capture(&self, args: &[&str]) -> Result<String, String> {
        let (command, prefix_args) = self.get_npm_command()?;
        let mut request = CommandRequest::new(&command, &[]);
        request.args = prefix_args;
        request.args.extend(args.iter().map(|s| s.to_string()));
        self.runner.run_capture(&request)
    }

    /// `getGlobalNpmRoot` (package-manager.ts:1967-1981), without the
    /// upstream memo cache (pure performance optimization).
    fn get_global_npm_root(&self) -> Result<PathBuf, String> {
        if self.get_package_manager_name()? == "bun" {
            let bin_dir = self.run_npm_capture(&["pm", "bin", "-g"])?;
            return Ok(Path::new(bin_dir.trim())
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join("install")
                .join("global")
                .join("node_modules"));
        }
        Ok(PathBuf::from(self.run_npm_capture(&["root", "-g"])?.trim()))
    }

    /// `getPnpmGlobalPackagePath` (package-manager.ts:1983-1995).
    fn get_pnpm_global_package_path(&self, package_name: &str) -> Option<PathBuf> {
        if self.get_package_manager_name().ok()? != "pnpm" {
            return None;
        }
        let output = self
            .run_npm_capture(&["list", "-g", "--depth", "0", "--json"])
            .ok()?;
        let parsed: Value = serde_json::from_str(&output).ok()?;
        parsed.as_array()?.iter().find_map(|entry| {
            entry
                .get("dependencies")?
                .get(package_name)?
                .get("path")?
                .as_str()
                .map(PathBuf::from)
        })
    }

    /// `getLegacyGlobalNpmInstallPath` (package-manager.ts:2008-2014).
    fn get_legacy_global_npm_install_path(&self, source: &NpmSource) -> Option<PathBuf> {
        if let Some(path) = self.get_pnpm_global_package_path(&source.name) {
            return Some(path);
        }
        let root = self.get_global_npm_root().ok()?;
        Some(root.join(&source.name))
    }

    // -------------------------------------------------------------------
    // Install / remove (package-manager.ts:994-1046, 1797-1954)
    // -------------------------------------------------------------------

    /// `ensureNpmProject` (package-manager.ts:1933-1944).
    fn ensure_npm_project(&self, install_root: &Path) -> Result<(), String> {
        if !install_root.exists() {
            std::fs::create_dir_all(install_root).map_err(|e| e.to_string())?;
        }
        mark_path_ignored_by_cloud_sync(install_root);
        self.ensure_git_ignore(install_root)?;
        let package_json_path = install_root.join("package.json");
        if !package_json_path.exists() {
            let package_json = serde_json::json!({ "name": "pi-extensions", "private": true });
            let content = serde_json::to_string_pretty(&package_json).map_err(|e| e.to_string())?;
            std::fs::write(&package_json_path, content).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// `ensureGitIgnore` (package-manager.ts:1946-1954).
    fn ensure_git_ignore(&self, dir: &Path) -> Result<(), String> {
        if !dir.exists() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let ignore_path = dir.join(".gitignore");
        if !ignore_path.exists() {
            std::fs::write(&ignore_path, "*\n!.gitignore\n").map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// `installNpm` (package-manager.ts:1797-1801).
    fn install_npm(
        &self,
        source: &NpmSource,
        scope: SourceScope,
        temporary: bool,
    ) -> Result<(), String> {
        let install_root = self.get_npm_install_root(scope, temporary)?;
        self.ensure_npm_project(&install_root)?;
        let args = self.get_npm_install_args(std::slice::from_ref(&source.spec), &install_root)?;
        self.run_npm_command(&args, None)
    }

    /// `uninstallNpm` (package-manager.ts:1803-1818).
    fn uninstall_npm(&self, source: &NpmSource, scope: SourceScope) -> Result<(), String> {
        let install_root = self.get_npm_install_root(scope, false)?;
        if !install_root.exists() {
            return Ok(());
        }
        let package_manager_name = self.get_package_manager_name()?;
        if package_manager_name == "bun" {
            return self.run_npm_command(
                &[
                    "uninstall".to_string(),
                    source.name.clone(),
                    "--cwd".to_string(),
                    install_root.to_string_lossy().into_owned(),
                ],
                None,
            );
        }
        let mut args = vec![
            "uninstall".to_string(),
            source.name.clone(),
            "--prefix".to_string(),
            install_root.to_string_lossy().into_owned(),
        ];
        if package_manager_name != "pnpm" {
            args.push("--legacy-peer-deps".to_string());
        }
        self.run_npm_command(&args, None)
    }

    /// `installGit` (package-manager.ts:1820-1845).
    fn install_git(&self, source: &GitSource, scope: SourceScope) -> Result<(), String> {
        let target_dir = self.get_git_install_path(source, scope)?;
        if target_dir.exists() {
            if let Some(ref_) = &source.ref_ {
                return self.ensure_git_ref(&target_dir, &["fetch", "origin", ref_], "FETCH_HEAD");
            }
            let target = self.get_local_git_update_target(&target_dir)?;
            return self.ensure_git_ref(&target_dir, &target.fetch_args_str(), &target.ref_);
        }
        if let Some(git_root) = self.get_git_install_root(scope)? {
            self.ensure_git_ignore(&git_root)?;
        }
        if let Some(parent) = target_dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        self.runner.run(&CommandRequest::new(
            "git",
            &["clone", &source.repo, &target_dir.to_string_lossy()],
        ))?;
        if let Some(ref_) = &source.ref_ {
            self.runner
                .run(&CommandRequest::new("git", &["checkout", ref_]).with_cwd(&target_dir))?;
        }
        if target_dir.join("package.json").exists() {
            self.run_npm_command(&self.get_git_dependency_install_args(), Some(&target_dir))?;
        }
        Ok(())
    }

    /// `updateGit` (package-manager.ts:1847-1861): pinned refs do not move,
    /// but an existing clone is reconciled to the configured ref
    /// (reset + clean + dependency install) — W3 calls this from the
    /// update orchestration.
    pub fn update_git(&self, source: &GitSource, scope: SourceScope) -> Result<(), String> {
        let target_dir = self.get_git_install_path(source, scope)?;
        if !target_dir.exists() {
            return self.install_git(source, scope);
        }
        if let Some(ref_) = &source.ref_ {
            return self.ensure_git_ref(&target_dir, &["fetch", "origin", ref_], "FETCH_HEAD");
        }
        let target = self.get_local_git_update_target(&target_dir)?;
        self.ensure_git_ref(&target_dir, &target.fetch_args_str(), &target.ref_)
    }

    /// `ensureGitRef` (package-manager.ts:1863-1889): fetch only the target
    /// ref, then hard-reset + clean and reinstall dependencies when HEAD
    /// moves.
    fn ensure_git_ref(
        &self,
        target_dir: &Path,
        fetch_args: &[&str],
        ref_: &str,
    ) -> Result<(), String> {
        self.runner
            .run(&CommandRequest::new("git", fetch_args).with_cwd(target_dir))?;

        let local_head = self.runner.run_capture(
            &CommandRequest::new("git", &["rev-parse", "HEAD"])
                .with_cwd(target_dir)
                .with_timeout(NETWORK_TIMEOUT),
        )?;
        let commit_ref = format!("{ref_}^{{commit}}");
        let target_head = self.runner.run_capture(
            &CommandRequest::new("git", &["rev-parse", &commit_ref])
                .with_cwd(target_dir)
                .with_timeout(NETWORK_TIMEOUT),
        )?;
        if local_head.trim() == target_head.trim() {
            return Ok(());
        }

        self.runner.run(
            &CommandRequest::new("git", &["reset", "--hard", &commit_ref]).with_cwd(target_dir),
        )?;
        // Clean untracked files (extensions should be pristine).
        self.runner
            .run(&CommandRequest::new("git", &["clean", "-fdx"]).with_cwd(target_dir))?;

        if target_dir.join("package.json").exists() {
            self.run_npm_command(&self.get_git_dependency_install_args(), Some(target_dir))?;
        }
        Ok(())
    }

    /// `removeGit` (package-manager.ts:1904-1909).
    fn remove_git(&self, source: &GitSource, scope: SourceScope) -> Result<(), String> {
        let target_dir = self.get_git_install_path(source, scope)?;
        if !target_dir.exists() {
            return Ok(());
        }
        std::fs::remove_dir_all(&target_dir).map_err(|e| e.to_string())?;
        self.prune_empty_git_parents(&target_dir, self.get_git_install_root(scope)?);
        Ok(())
    }

    /// `pruneEmptyGitParents` (package-manager.ts:1911-1931).
    fn prune_empty_git_parents(&self, target_dir: &Path, install_root: Option<PathBuf>) {
        let Some(install_root) = install_root else {
            return;
        };
        let resolved_root = resolve_path(&install_root.to_string_lossy(), &self.cwd);
        let mut current = target_dir.parent().map(Path::to_path_buf);
        while let Some(dir) = current {
            if !dir.starts_with(&resolved_root) || dir == resolved_root {
                break;
            }
            if dir.exists() {
                let is_empty = std::fs::read_dir(&dir)
                    .map(|mut entries| entries.next().is_none())
                    .unwrap_or(false);
                if !is_empty {
                    break;
                }
                if std::fs::remove_dir_all(&dir).is_err() {
                    break;
                }
            }
            current = dir.parent().map(Path::to_path_buf);
        }
    }

    /// `installParsedSource` (package-manager.ts:1347-1356).
    fn install_parsed_source(
        &self,
        parsed: &ParsedSource,
        scope: SourceScope,
    ) -> Result<(), String> {
        match parsed {
            ParsedSource::Npm(npm) => self.install_npm(npm, scope, scope == SourceScope::Temporary),
            ParsedSource::Git(git) => self.install_git(git, scope),
            ParsedSource::Local(_) => Ok(()),
        }
    }

    /// `install` (package-manager.ts:994-1016).
    pub fn install(&self, source: &str, local: bool) -> Result<(), String> {
        let parsed = parse_source(source);
        let scope = if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        self.assert_project_trusted_for_scope(scope)?;
        self.with_progress(
            ProgressAction::Install,
            source,
            &format!("Installing {source}..."),
            || match &parsed {
                ParsedSource::Npm(npm) => self.install_npm(npm, scope, false),
                ParsedSource::Git(git) => self.install_git(git, scope),
                ParsedSource::Local(path) => {
                    let resolved = self.resolve_path(path);
                    if !resolved.exists() {
                        return Err(format!("Path does not exist: {}", resolved.display()));
                    }
                    Ok(())
                }
            },
        )
    }

    /// `installAndPersist` (package-manager.ts:1018-1021).
    pub fn install_and_persist(&mut self, source: &str, local: bool) -> Result<(), String> {
        self.install(source, local)?;
        self.add_source_to_settings(source, local)?;
        Ok(())
    }

    /// `remove` (package-manager.ts:1023-1041).
    pub fn remove(&self, source: &str, local: bool) -> Result<(), String> {
        let parsed = parse_source(source);
        let scope = if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        self.assert_project_trusted_for_scope(scope)?;
        self.with_progress(
            ProgressAction::Remove,
            source,
            &format!("Removing {source}..."),
            || match &parsed {
                ParsedSource::Npm(npm) => self.uninstall_npm(npm, scope),
                ParsedSource::Git(git) => self.remove_git(git, scope),
                ParsedSource::Local(_) => Ok(()),
            },
        )
    }

    /// `removeAndPersist` (package-manager.ts:1043-1046).
    pub fn remove_and_persist(&mut self, source: &str, local: bool) -> Result<bool, String> {
        self.remove(source, local)?;
        self.remove_source_from_settings(source, local)
    }

    /// `getInstalledPath` (package-manager.ts:862-878).
    pub fn get_installed_path(&self, source: &str, scope: SourceScope) -> Option<PathBuf> {
        match parse_source(source) {
            ParsedSource::Npm(npm) => {
                let path = self.get_npm_install_path(&npm, scope);
                path.exists().then_some(path)
            }
            ParsedSource::Git(git) => {
                let path = self.get_git_install_path(&git, scope).ok()?;
                path.exists().then_some(path)
            }
            ParsedSource::Local(local) => {
                let base_dir = self.get_base_dir_for_scope(scope);
                let path = self.resolve_path_from_base(&local, &base_dir);
                path.exists().then_some(path)
            }
        }
    }

    /// `listConfiguredPackages` (package-manager.ts:966-992).
    pub fn list_configured_packages(&self) -> Vec<ConfiguredPackage> {
        let mut configured = Vec::new();
        for pkg in DefaultPackageManager::packages_of(&self.settings_manager.get_global_settings())
        {
            let source = Self::package_source_string(&pkg).to_string();
            configured.push(ConfiguredPackage {
                filtered: matches!(pkg, PackageSource::Filtered(_)),
                installed_path: self.get_installed_path(&source, SourceScope::User),
                source,
                scope: SourceScope::User,
            });
        }
        for pkg in DefaultPackageManager::packages_of(&self.settings_manager.get_project_settings())
        {
            let source = Self::package_source_string(&pkg).to_string();
            configured.push(ConfiguredPackage {
                filtered: matches!(pkg, PackageSource::Filtered(_)),
                installed_path: self.get_installed_path(&source, SourceScope::Project),
                source,
                scope: SourceScope::Project,
            });
        }
        configured
    }

    // -------------------------------------------------------------------
    // Update orchestration (package-manager.ts:1048-1238, 1385-1416)
    // -------------------------------------------------------------------

    /// `update` (package-manager.ts:1048-1078): update every configured
    /// package, or only the one whose identity matches `source`.
    pub fn update(&self, source: Option<&str>) -> Result<(), String> {
        let global_settings = self.settings_manager.get_global_settings();
        let project_settings = self.settings_manager.get_project_settings();
        let identity = source.map(|source| self.get_package_identity(source, None));
        let mut matched = false;
        let mut update_sources: Vec<ConfiguredUpdateSource> = Vec::new();

        for (settings, scope) in [
            (&global_settings, SourceScope::User),
            (&project_settings, SourceScope::Project),
        ] {
            for pkg in Self::packages_of(settings) {
                let source_str = Self::package_source_string(&pkg);
                if let Some(identity) = &identity {
                    if &self.get_package_identity(source_str, Some(scope)) != identity {
                        continue;
                    }
                }
                matched = true;
                update_sources.push(ConfiguredUpdateSource {
                    source: source_str.to_string(),
                    scope,
                });
            }
        }

        if let Some(source) = source {
            if !matched {
                let mut configured = Self::packages_of(&global_settings);
                configured.extend(Self::packages_of(&project_settings));
                return Err(self.build_no_matching_package_message(source, &configured));
            }
        }

        self.update_configured_sources(&update_sources)
    }

    /// `updateConfiguredSources` (package-manager.ts:1080-1137): npm
    /// candidates are update-checked with concurrency 4, then the per-scope
    /// npm batches and the git updates (own concurrency-4 pool) run in
    /// parallel. Pinned npm versions are skipped; pinned git refs still
    /// reconcile. Offline / empty input is a no-op.
    fn update_configured_sources(&self, sources: &[ConfiguredUpdateSource]) -> Result<(), String> {
        if self.offline || sources.is_empty() {
            return Ok(());
        }

        let mut npm_candidates: Vec<NpmUpdateTarget> = Vec::new();
        let mut git_candidates: Vec<GitUpdateEntry> = Vec::new();
        for entry in sources {
            match parse_source(&entry.source) {
                ParsedSource::Npm(parsed) if !parsed.pinned => {
                    npm_candidates.push(NpmUpdateTarget {
                        source: entry.source.clone(),
                        scope: entry.scope,
                        parsed,
                    })
                }
                ParsedSource::Git(parsed) => git_candidates.push(GitUpdateEntry {
                    source: entry.source.clone(),
                    scope: entry.scope,
                    parsed,
                }),
                _ => {}
            }
        }

        let should_updates = run_with_concurrency(
            npm_candidates
                .iter()
                .map(|entry| {
                    let manager = &*self;
                    move || manager.should_update_npm_source(&entry.parsed, entry.scope)
                })
                .collect(),
            UPDATE_CHECK_CONCURRENCY,
        )?;
        let mut user_npm_updates: Vec<NpmUpdateTarget> = Vec::new();
        let mut project_npm_updates: Vec<NpmUpdateTarget> = Vec::new();
        for (entry, should_update) in npm_candidates.into_iter().zip(should_updates) {
            if !should_update {
                continue;
            }
            match entry.scope {
                SourceScope::User => user_npm_updates.push(entry),
                _ => project_npm_updates.push(entry),
            }
        }

        // `Promise.all(tasks)` (package-manager.ts:1119-1136): the npm
        // batches and the git pool run concurrently. On multiple failures
        // the first error in group order returns (upstream rejects with the
        // temporally first one).
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            if !user_npm_updates.is_empty() {
                handles.push(
                    scope.spawn(|| self.update_npm_batch(&user_npm_updates, SourceScope::User)),
                );
            }
            if !project_npm_updates.is_empty() {
                handles.push(
                    scope.spawn(|| {
                        self.update_npm_batch(&project_npm_updates, SourceScope::Project)
                    }),
                );
            }
            if !git_candidates.is_empty() {
                handles.push(scope.spawn(|| {
                    run_with_concurrency(
                        git_candidates
                            .iter()
                            .map(|entry| {
                                let manager = &*self;
                                move || {
                                    manager.with_progress(
                                        ProgressAction::Update,
                                        &entry.source,
                                        &format!("Updating {}...", entry.source),
                                        || manager.update_git(&entry.parsed, entry.scope),
                                    )
                                }
                            })
                            .collect(),
                        GIT_UPDATE_CONCURRENCY,
                    )
                    .map(|_| ())
                }));
            }
            let mut first_error: Option<String> = None;
            for handle in handles {
                let result = handle
                    .join()
                    .unwrap_or_else(|_| Err("update worker panicked".to_string()));
                if let Err(error) = result {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
            match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }

    /// `updateNpmBatch` (package-manager.ts:1155-1167).
    fn update_npm_batch(
        &self,
        sources: &[NpmUpdateTarget],
        scope: SourceScope,
    ) -> Result<(), String> {
        if sources.is_empty() {
            return Ok(());
        }
        let scope_name = match scope {
            SourceScope::Project => "project",
            _ => "user",
        };
        let (source_label, message) = if sources.len() == 1 {
            let source = sources[0].source.clone();
            (source.clone(), format!("Updating {source}..."))
        } else {
            (
                format!("{scope_name} npm packages"),
                format!("Updating {scope_name} npm packages..."),
            )
        };
        let specs: Vec<String> = sources
            .iter()
            .map(|entry| match &entry.parsed.version {
                Some(_) => entry.parsed.spec.clone(),
                None => format!("{}@latest", entry.parsed.name),
            })
            .collect();
        self.with_progress(ProgressAction::Update, &source_label, &message, || {
            self.install_npm_batch(&specs, scope)
        })
    }

    /// `installNpmBatch` (package-manager.ts:1169-1173).
    fn install_npm_batch(&self, specs: &[String], scope: SourceScope) -> Result<(), String> {
        let install_root = self.get_npm_install_root(scope, false)?;
        self.ensure_npm_project(&install_root)?;
        let args = self.get_npm_install_args(specs, &install_root)?;
        self.run_npm_command(&args, None)
    }

    /// `checkForAvailableUpdates` (package-manager.ts:1175-1238): local and
    /// pinned sources never report updates; the per-package checks run with
    /// concurrency 4. Offline returns an empty list.
    pub fn check_for_available_updates(&self) -> Result<Vec<PackageUpdate>, String> {
        if self.offline {
            return Ok(Vec::new());
        }

        let mut all_packages: Vec<(PackageSource, SourceScope)> = Vec::new();
        for pkg in Self::packages_of(&self.settings_manager.get_project_settings()) {
            all_packages.push((pkg, SourceScope::Project));
        }
        for pkg in Self::packages_of(&self.settings_manager.get_global_settings()) {
            all_packages.push((pkg, SourceScope::User));
        }
        let package_sources = self.dedupe_packages(all_packages);

        // Settings only carry user/project entries; the temporary-scope
        // filter is upstream parity (package-manager.ts:1191-1195).
        let checks: Vec<(String, SourceScope)> = package_sources
            .iter()
            .filter(|(_, scope)| *scope != SourceScope::Temporary)
            .map(|(pkg, scope)| (Self::package_source_string(pkg).to_string(), *scope))
            .collect();
        let results = run_with_concurrency(
            checks
                .iter()
                .map(|(source, scope)| {
                    let manager = &*self;
                    move || manager.check_one_for_available_update(source, *scope)
                })
                .collect(),
            UPDATE_CHECK_CONCURRENCY,
        )?;
        Ok(results.into_iter().flatten().collect())
    }

    /// One `checkForAvailableUpdates` probe (package-manager.ts:1196-1234).
    fn check_one_for_available_update(
        &self,
        source: &str,
        scope: SourceScope,
    ) -> Result<Option<PackageUpdate>, String> {
        match parse_source(source) {
            ParsedSource::Local(_) => Ok(None),
            ParsedSource::Npm(parsed) => {
                if parsed.pinned {
                    return Ok(None);
                }
                let installed_path = self.get_npm_install_path(&parsed, scope);
                if !installed_path.exists() {
                    return Ok(None);
                }
                if !self.npm_has_available_update(&parsed, &installed_path) {
                    return Ok(None);
                }
                Ok(Some(PackageUpdate {
                    source: source.to_string(),
                    display_name: parsed.name,
                    kind: PackageUpdateKind::Npm,
                    scope,
                }))
            }
            ParsedSource::Git(parsed) => {
                if parsed.pinned {
                    return Ok(None);
                }
                let installed_path = self.get_git_install_path(&parsed, scope)?;
                if !installed_path.exists() {
                    return Ok(None);
                }
                if !self.git_has_available_update(&installed_path) {
                    return Ok(None);
                }
                Ok(Some(PackageUpdate {
                    source: source.to_string(),
                    display_name: format!("{}/{}", parsed.host, parsed.path),
                    kind: PackageUpdateKind::Git,
                    scope,
                }))
            }
        }
    }

    /// `buildNoMatchingPackageMessage` (package-manager.ts:1385-1391).
    pub fn build_no_matching_package_message(
        &self,
        source: &str,
        configured_packages: &[PackageSource],
    ) -> String {
        match self.find_suggested_configured_source(source, configured_packages) {
            None => format!("No matching package found for {source}"),
            Some(suggestion) => {
                format!("No matching package found for {source}. Did you mean {suggestion}?")
            }
        }
    }

    /// `findSuggestedConfiguredSource` (package-manager.ts:1393-1416): an
    /// npm name/spec or git `host/path[@ref]` shorthand suggests the
    /// configured source string.
    fn find_suggested_configured_source(
        &self,
        source: &str,
        configured_packages: &[PackageSource],
    ) -> Option<String> {
        let trimmed = source.trim();
        for pkg in configured_packages {
            let source_str = Self::package_source_string(pkg);
            match parse_source(source_str) {
                ParsedSource::Npm(npm) => {
                    if trimmed == npm.name || trimmed == npm.spec {
                        return Some(source_str.to_string());
                    }
                }
                ParsedSource::Git(git) => {
                    let shorthand = format!("{}/{}", git.host, git.path);
                    let matches_shorthand = trimmed == shorthand
                        || git
                            .ref_
                            .as_ref()
                            .is_some_and(|ref_| trimmed == format!("{shorthand}@{ref_}"));
                    if matches_shorthand {
                        return Some(source_str.to_string());
                    }
                }
                ParsedSource::Local(_) => {}
            }
        }
        None
    }

    // -------------------------------------------------------------------
    // npm version queries (package-manager.ts:1462-1519)
    // -------------------------------------------------------------------

    /// `getInstalledNpmVersion` (package-manager.ts:1488-1498).
    fn get_installed_npm_version(&self, installed_path: &Path) -> Option<String> {
        let package_json_path = installed_path.join("package.json");
        if !package_json_path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(package_json_path).ok()?;
        let parsed: Value = serde_json::from_str(&content).ok()?;
        parsed.get("version")?.as_str().map(str::to_string)
    }

    /// `getLatestNpmVersion` (package-manager.ts:1500-1519):
    /// `npm view <spec> version --json` with the 10s network timeout;
    /// range specs resolve through maxSatisfying.
    pub fn get_latest_npm_version(
        &self,
        package_spec: &str,
        range: Option<&str>,
    ) -> Result<String, String> {
        let (command, prefix_args) = self.get_npm_command()?;
        let mut request =
            CommandRequest::new(&command, &["view", package_spec, "version", "--json"])
                .with_cwd(&self.cwd)
                .with_timeout(NETWORK_TIMEOUT);
        let mut args = prefix_args;
        args.append(&mut request.args);
        request.args = args;
        let stdout = self.runner.run_capture(&request)?;
        let raw = stdout.trim();
        if raw.is_empty() {
            return Err("Empty response from npm view".to_string());
        }
        let parsed: Value = serde_json::from_str(raw)
            .map_err(|_| "Unexpected response from npm view".to_string())?;
        if let Some(version) = parsed.as_str() {
            return Ok(version.to_string());
        }
        if let Some(versions) = parsed.as_array() {
            let versions: Vec<String> = versions
                .iter()
                .filter_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string))
                .collect();
            if let Some(latest) = npm_max_satisfying(&versions, range) {
                return Ok(latest);
            }
        }
        Err("Unexpected response from npm view".to_string())
    }

    /// `installedNpmMatchesConfiguredVersion`
    /// (package-manager.ts:1462-1468).
    fn installed_npm_matches_configured_version(
        &self,
        source: &NpmSource,
        installed_path: &Path,
    ) -> bool {
        let Some(installed_version) = self.get_installed_npm_version(installed_path) else {
            return false;
        };
        match &source.range {
            Some(range) => npm_satisfies(&installed_version, range),
            None => true,
        }
    }

    /// `shouldUpdateNpmSource` (package-manager.ts:1139-1153) — W3 update
    /// check entry point.
    pub fn should_update_npm_source(
        &self,
        source: &NpmSource,
        scope: SourceScope,
    ) -> Result<bool, String> {
        let installed_path = self.get_managed_npm_install_path(source, scope)?;
        let installed_version = if installed_path.exists() {
            self.get_installed_npm_version(&installed_path)
        } else {
            None
        };
        let Some(installed_version) = installed_version else {
            return Ok(true);
        };
        let spec = match &source.version {
            Some(_) => source.spec.clone(),
            None => source.name.clone(),
        };
        match self.get_latest_npm_version(&spec, source.range.as_deref()) {
            Ok(target_version) => Ok(target_version != installed_version),
            // Preserve existing update behavior when version lookup fails.
            Err(_) => Ok(true),
        }
    }

    /// `npmHasAvailableUpdate` (package-manager.ts:1470-1486) — W3 update
    /// check entry point.
    pub fn npm_has_available_update(&self, source: &NpmSource, installed_path: &Path) -> bool {
        if self.offline {
            return false;
        }
        let Some(installed_version) = self.get_installed_npm_version(installed_path) else {
            return false;
        };
        let spec = match &source.version {
            Some(_) => source.spec.clone(),
            None => source.name.clone(),
        };
        match self.get_latest_npm_version(&spec, source.range.as_deref()) {
            Ok(target_version) => target_version != installed_version,
            Err(_) => false,
        }
    }

    // -------------------------------------------------------------------
    // git update queries (package-manager.ts:1521-1644)
    // -------------------------------------------------------------------

    /// `runGitRemoteCommand` (package-manager.ts:1636-1644):
    /// `GIT_TERMINAL_PROMPT=0` so credential prompts never block.
    fn run_git_remote_command(
        &self,
        installed_path: &Path,
        args: &[&str],
    ) -> Result<String, String> {
        self.runner.run_capture(
            &CommandRequest::new("git", args)
                .with_cwd(installed_path)
                .with_timeout(NETWORK_TIMEOUT)
                .with_env("GIT_TERMINAL_PROMPT", "0"),
        )
    }

    /// `getGitUpstreamRef` (package-manager.ts:1619-1634).
    fn get_git_upstream_ref(&self, installed_path: &Path) -> Option<String> {
        let upstream = self
            .runner
            .run_capture(
                &CommandRequest::new("git", &["rev-parse", "--abbrev-ref", "@{upstream}"])
                    .with_cwd(installed_path)
                    .with_timeout(NETWORK_TIMEOUT),
            )
            .ok()?;
        let trimmed = upstream.trim();
        let branch = trimmed.strip_prefix("origin/")?;
        if branch.is_empty() {
            None
        } else {
            Some(format!("refs/heads/{branch}"))
        }
    }

    /// `getRemoteGitHead` (package-manager.ts:1538-1554).
    fn get_remote_git_head(&self, installed_path: &Path) -> Result<String, String> {
        if let Some(upstream_ref) = self.get_git_upstream_ref(installed_path) {
            let remote_head = self
                .run_git_remote_command(installed_path, &["ls-remote", "origin", &upstream_ref])?;
            if let Some(head) = extract_ls_remote_head(&remote_head, None) {
                return Ok(head);
            }
        }
        let remote_head =
            self.run_git_remote_command(installed_path, &["ls-remote", "origin", "HEAD"])?;
        extract_ls_remote_head(&remote_head, Some("HEAD"))
            .ok_or_else(|| "Failed to determine remote HEAD".to_string())
    }

    /// `gitHasAvailableUpdate` (package-manager.ts:1521-1536) — W3 update
    /// check entry point.
    pub fn git_has_available_update(&self, installed_path: &Path) -> bool {
        if self.offline {
            return false;
        }
        let local_head = self.runner.run_capture(
            &CommandRequest::new("git", &["rev-parse", "HEAD"])
                .with_cwd(installed_path)
                .with_timeout(NETWORK_TIMEOUT),
        );
        let remote_head = self.get_remote_git_head(installed_path);
        match (local_head, remote_head) {
            (Ok(local), Ok(remote)) => local.trim() != remote.trim(),
            _ => false,
        }
    }

    /// `getLocalGitUpdateTarget` (package-manager.ts:1556-1617).
    fn get_local_git_update_target(
        &self,
        installed_path: &Path,
    ) -> Result<GitUpdateTarget, String> {
        let upstream = self.runner.run_capture(
            &CommandRequest::new("git", &["rev-parse", "--abbrev-ref", "@{upstream}"])
                .with_cwd(installed_path)
                .with_timeout(NETWORK_TIMEOUT),
        );
        if let Ok(upstream) = upstream {
            let trimmed = upstream.trim();
            let Some(branch) = trimmed.strip_prefix("origin/") else {
                return Err(format!("Unsupported upstream remote: {trimmed}"));
            };
            if branch.is_empty() {
                return Err("Missing upstream branch name".to_string());
            }
            let head = self.runner.run_capture(
                &CommandRequest::new("git", &["rev-parse", "@{upstream}"])
                    .with_cwd(installed_path)
                    .with_timeout(NETWORK_TIMEOUT),
            )?;
            return Ok(GitUpdateTarget {
                ref_: "@{upstream}".to_string(),
                head,
                fetch_args: vec![
                    "fetch".to_string(),
                    "--prune".to_string(),
                    "--no-tags".to_string(),
                    "origin".to_string(),
                    format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
                ],
            });
        }

        // No upstream configured: fall back to origin/HEAD.
        let _ = self.runner.run(
            &CommandRequest::new("git", &["remote", "set-head", "origin", "-a"])
                .with_cwd(installed_path),
        );
        let head = self.runner.run_capture(
            &CommandRequest::new("git", &["rev-parse", "origin/HEAD"])
                .with_cwd(installed_path)
                .with_timeout(NETWORK_TIMEOUT),
        )?;
        let origin_head_ref = self
            .runner
            .run_capture(
                &CommandRequest::new("git", &["symbolic-ref", "refs/remotes/origin/HEAD"])
                    .with_cwd(installed_path)
                    .with_timeout(NETWORK_TIMEOUT),
            )
            .unwrap_or_default();
        let branch = origin_head_ref
            .trim()
            .strip_prefix("refs/remotes/origin/")
            .unwrap_or("")
            .to_string();
        if !branch.is_empty() {
            return Ok(GitUpdateTarget {
                ref_: "origin/HEAD".to_string(),
                head,
                fetch_args: vec![
                    "fetch".to_string(),
                    "--prune".to_string(),
                    "--no-tags".to_string(),
                    "origin".to_string(),
                    format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
                ],
            });
        }
        Ok(GitUpdateTarget {
            ref_: "origin/HEAD".to_string(),
            head,
            fetch_args: vec![
                "fetch".to_string(),
                "--prune".to_string(),
                "--no-tags".to_string(),
                "origin".to_string(),
                "+HEAD:refs/remotes/origin/HEAD".to_string(),
            ],
        })
    }

    /// `refreshTemporaryGitSource` (package-manager.ts:1891-1902): keep the
    /// cached temporary checkout when the refresh fails.
    fn refresh_temporary_git_source(&self, source: &GitSource, source_str: &str) {
        if self.offline {
            return;
        }
        let _ = self.with_progress(
            ProgressAction::Pull,
            source_str,
            &format!("Refreshing {source_str}..."),
            || self.update_git(source, SourceScope::Temporary),
        );
    }

    // -------------------------------------------------------------------
    // resolve (package slice): package-manager.ts:901-953, 1240-1345
    // -------------------------------------------------------------------

    /// `resolve` (package-manager.ts:901-953), package slice only: dedupe
    /// configured packages (project wins) and resolve their resources.
    /// Missing npm/git sources are installed unless offline; `on_missing`
    /// intercepts the decision (install/skip/error).
    pub fn resolve(&self, on_missing: OnMissing<'_>) -> Result<ResolvedPackagePaths, String> {
        let mut accumulator = ResourceAccumulator::default();
        let global_settings = self.settings_manager.get_global_settings();
        let project_settings = self.settings_manager.get_project_settings();

        // Project first so cwd resources win collisions.
        let mut all_packages: Vec<(PackageSource, SourceScope)> = Vec::new();
        for pkg in DefaultPackageManager::packages_of(&project_settings) {
            all_packages.push((pkg, SourceScope::Project));
        }
        for pkg in DefaultPackageManager::packages_of(&global_settings) {
            all_packages.push((pkg, SourceScope::User));
        }

        let package_sources = self.dedupe_packages(all_packages);
        self.resolve_package_sources(&package_sources, &mut accumulator, on_missing)?;
        Ok(accumulator.into_resolved())
    }

    /// `resolveExtensionSources` (package-manager.ts:955-964) — backs the
    /// `-e` temporary scope (`~/.pir/agent/tmp/extensions`).
    pub fn resolve_extension_sources(
        &self,
        sources: &[String],
        local: bool,
        temporary: bool,
    ) -> Result<ResolvedPackagePaths, String> {
        let mut accumulator = ResourceAccumulator::default();
        let scope = if temporary {
            SourceScope::Temporary
        } else if local {
            SourceScope::Project
        } else {
            SourceScope::User
        };
        let package_sources: Vec<(PackageSource, SourceScope)> = sources
            .iter()
            .map(|source| (PackageSource::Source(source.clone()), scope))
            .collect();
        self.resolve_package_sources(&package_sources, &mut accumulator, None)?;
        Ok(accumulator.into_resolved())
    }

    /// `resolve` (package-manager.ts:901-953) in full: package resources,
    /// then top-level settings entries, then auto-discovered resources.
    /// [`DefaultPackageManager::resolve`] is the package slice feeding the
    /// resource loader; this full form backs `pir config`.
    pub fn resolve_all(&self, on_missing: OnMissing<'_>) -> Result<ResolvedPaths, String> {
        let mut accumulator = FullResourceAccumulator::default();
        let global_settings = self.settings_manager.get_global_settings();
        let project_settings = self.settings_manager.get_project_settings();

        // Project first so cwd resources win collisions.
        let mut all_packages: Vec<(PackageSource, SourceScope)> = Vec::new();
        for pkg in DefaultPackageManager::packages_of(&project_settings) {
            all_packages.push((pkg, SourceScope::Project));
        }
        for pkg in DefaultPackageManager::packages_of(&global_settings) {
            all_packages.push((pkg, SourceScope::User));
        }
        let package_sources = self.dedupe_packages(all_packages);
        let mut package_accumulator = ResourceAccumulator::default();
        self.resolve_package_sources(&package_sources, &mut package_accumulator, on_missing)?;
        accumulator.add_package_slice(package_accumulator);

        let global_base_dir = self.agent_dir.clone();
        let project_base_dir = config::get_project_config_dir(&self.cwd);

        for resource_type in RESOURCE_TYPES {
            let key = resource_type.dir_name();
            self.resolve_local_entries(
                &settings_string_array(&project_settings, key),
                resource_type,
                &mut accumulator,
                &ResourcePathMetadata {
                    source: "local".to_string(),
                    scope: SourceScope::Project,
                    origin: SourceOrigin::TopLevel,
                    base_dir: None,
                },
                &project_base_dir,
            );
            self.resolve_local_entries(
                &settings_string_array(&global_settings, key),
                resource_type,
                &mut accumulator,
                &ResourcePathMetadata {
                    source: "local".to_string(),
                    scope: SourceScope::User,
                    origin: SourceOrigin::TopLevel,
                    base_dir: None,
                },
                &global_base_dir,
            );
        }

        self.add_auto_discovered_resources(
            &mut accumulator,
            &global_settings,
            &project_settings,
            &global_base_dir,
            &project_base_dir,
        );

        Ok(accumulator.into_resolved_paths())
    }

    /// `resolveLocalEntries` (package-manager.ts:2280-2301).
    fn resolve_local_entries(
        &self,
        entries: &[String],
        resource_type: ResourceType,
        accumulator: &mut FullResourceAccumulator,
        metadata: &ResourcePathMetadata,
        base_dir: &Path,
    ) {
        if entries.is_empty() {
            return;
        }
        let (plain, patterns) = split_patterns(entries);
        let resolved_plain: Vec<PathBuf> = plain
            .iter()
            .map(|p| self.resolve_path_from_base(p, base_dir))
            .collect();
        let all_files = self.collect_files_from_paths(&resolved_plain, resource_type);
        let enabled_paths = apply_patterns(&all_files, &patterns, base_dir);
        for file in all_files {
            let enabled = enabled_paths.contains(&file);
            accumulator.add(resource_type, file, metadata.clone(), enabled);
        }
    }

    /// `addAutoDiscoveredResources` (package-manager.ts:2303-2467).
    fn add_auto_discovered_resources(
        &self,
        accumulator: &mut FullResourceAccumulator,
        global_settings: &Settings,
        project_settings: &Settings,
        global_base_dir: &Path,
        project_base_dir: &Path,
    ) {
        let user_metadata = ResourcePathMetadata {
            source: "auto".to_string(),
            scope: SourceScope::User,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(global_base_dir.to_path_buf()),
        };
        let project_metadata = ResourcePathMetadata {
            source: "auto".to_string(),
            scope: SourceScope::Project,
            origin: SourceOrigin::TopLevel,
            base_dir: Some(project_base_dir.to_path_buf()),
        };
        let overrides = |settings: &Settings, resource_type: ResourceType| {
            settings_string_array(settings, resource_type.dir_name())
        };

        let user_agents_skills_dir = config::get_global_agents_skills_dir();
        let project_trusted = self.settings_manager.is_project_trusted();
        let project_agents_skill_dirs = if project_trusted {
            skills::collect_ancestor_agents_skill_dirs(&self.cwd)
                .into_iter()
                .filter(|dir| user_agents_skills_dir.as_ref() != Some(dir))
                .collect()
        } else {
            Vec::new()
        };

        if project_trusted {
            // Project extensions / skills from `.pir/`.
            self.add_auto_resources(
                ResourceType::Extensions,
                collect_auto_extension_entries(&project_base_dir.join("extensions")),
                &project_metadata,
                &overrides(project_settings, ResourceType::Extensions),
                project_base_dir,
                accumulator,
            );
            self.add_auto_resources(
                ResourceType::Skills,
                collect_skill_entries(&project_base_dir.join("skills"), SkillDiscoveryMode::Pir),
                &project_metadata,
                &overrides(project_settings, ResourceType::Skills),
                project_base_dir,
                accumulator,
            );
        }

        // Project skills from ancestor `.agents/` (each with its own baseDir).
        for agents_skills_dir in project_agents_skill_dirs {
            let agents_base_dir = agents_skills_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| agents_skills_dir.clone());
            let agents_metadata = ResourcePathMetadata {
                base_dir: Some(agents_base_dir.clone()),
                ..project_metadata.clone()
            };
            self.add_auto_resources(
                ResourceType::Skills,
                collect_skill_entries(&agents_skills_dir, SkillDiscoveryMode::Agents),
                &agents_metadata,
                &overrides(project_settings, ResourceType::Skills),
                &agents_base_dir,
                accumulator,
            );
        }

        if project_trusted {
            self.add_auto_resources(
                ResourceType::Prompts,
                collect_auto_file_entries(&project_base_dir.join("prompts"), ".md"),
                &project_metadata,
                &overrides(project_settings, ResourceType::Prompts),
                project_base_dir,
                accumulator,
            );
            self.add_auto_resources(
                ResourceType::Themes,
                collect_auto_file_entries(&project_base_dir.join("themes"), ".json"),
                &project_metadata,
                &overrides(project_settings, ResourceType::Themes),
                project_base_dir,
                accumulator,
            );
        }

        // User extensions / skills from the agent dir.
        self.add_auto_resources(
            ResourceType::Extensions,
            collect_auto_extension_entries(&global_base_dir.join("extensions")),
            &user_metadata,
            &overrides(global_settings, ResourceType::Extensions),
            global_base_dir,
            accumulator,
        );
        self.add_auto_resources(
            ResourceType::Skills,
            collect_skill_entries(&global_base_dir.join("skills"), SkillDiscoveryMode::Pir),
            &user_metadata,
            &overrides(global_settings, ResourceType::Skills),
            global_base_dir,
            accumulator,
        );

        // User skills from `~/.agents/` (with its own baseDir).
        if let Some(user_agents_skills_dir) = &user_agents_skills_dir {
            let user_agents_base_dir = user_agents_skills_dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| user_agents_skills_dir.clone());
            let user_agents_metadata = ResourcePathMetadata {
                base_dir: Some(user_agents_base_dir.clone()),
                ..user_metadata.clone()
            };
            self.add_auto_resources(
                ResourceType::Skills,
                collect_skill_entries(user_agents_skills_dir, SkillDiscoveryMode::Agents),
                &user_agents_metadata,
                &overrides(global_settings, ResourceType::Skills),
                &user_agents_base_dir,
                accumulator,
            );
        }

        self.add_auto_resources(
            ResourceType::Prompts,
            collect_auto_file_entries(&global_base_dir.join("prompts"), ".md"),
            &user_metadata,
            &overrides(global_settings, ResourceType::Prompts),
            global_base_dir,
            accumulator,
        );
        self.add_auto_resources(
            ResourceType::Themes,
            collect_auto_file_entries(&global_base_dir.join("themes"), ".json"),
            &user_metadata,
            &overrides(global_settings, ResourceType::Themes),
            global_base_dir,
            accumulator,
        );
    }

    /// The `addResources` closure of `addAutoDiscoveredResources`
    /// (package-manager.ts:2354-2366).
    fn add_auto_resources(
        &self,
        resource_type: ResourceType,
        paths: Vec<PathBuf>,
        metadata: &ResourcePathMetadata,
        overrides: &[String],
        base_dir: &Path,
        accumulator: &mut FullResourceAccumulator,
    ) {
        for path in paths {
            let enabled = is_enabled_by_overrides(&path, overrides, base_dir);
            accumulator.add(resource_type, path, metadata.clone(), enabled);
        }
    }

    /// `resolvePackageSources` (package-manager.ts:1240-1299).
    fn resolve_package_sources(
        &self,
        sources: &[(PackageSource, SourceScope)],
        accumulator: &mut ResourceAccumulator,
        mut on_missing: OnMissing<'_>,
    ) -> Result<(), String> {
        for (pkg, scope) in sources {
            let source_str = Self::package_source_string(pkg).to_string();
            let filter = match pkg {
                PackageSource::Filtered(filter) => Some(filter.clone()),
                PackageSource::Source(_) => None,
            };
            let delta_base = self.find_autoload_delta_base(pkg, *scope, sources);
            let (resolved_source, resolved_scope) = delta_base
                .as_ref()
                .map(|(source, scope)| (source.clone(), *scope))
                .unwrap_or_else(|| (source_str.clone(), *scope));
            let parsed = parse_source(&resolved_source);
            let mut metadata = PackageMetadata {
                source: source_str.clone(),
                scope: *scope,
                base_dir: None,
            };

            if let ParsedSource::Local(local) = &parsed {
                let base_dir = self.get_base_dir_for_scope(resolved_scope);
                self.resolve_local_extension_source(
                    local,
                    accumulator,
                    filter.as_ref(),
                    metadata,
                    &base_dir,
                );
                continue;
            }

            let install_missing = |on_missing: &mut OnMissing<'_>| -> Result<bool, String> {
                if self.offline {
                    return Ok(false);
                }
                match on_missing {
                    None => {
                        self.install_parsed_source(&parsed, resolved_scope)?;
                        Ok(true)
                    }
                    Some(callback) => match callback(&resolved_source) {
                        MissingSourceAction::Skip => Ok(false),
                        MissingSourceAction::Error => {
                            Err(format!("Missing source: {resolved_source}"))
                        }
                        MissingSourceAction::Install => {
                            self.install_parsed_source(&parsed, resolved_scope)?;
                            Ok(true)
                        }
                    },
                }
            };

            match &parsed {
                ParsedSource::Npm(npm) => {
                    let mut installed_path = self.get_npm_install_path(npm, resolved_scope);
                    let needs_install = !installed_path.exists()
                        || !self.installed_npm_matches_configured_version(npm, &installed_path);
                    if needs_install {
                        if !install_missing(&mut on_missing)? {
                            continue;
                        }
                        installed_path = self.get_npm_install_path(npm, resolved_scope);
                    }
                    metadata.base_dir = Some(installed_path.clone());
                    self.collect_package_resources(
                        &installed_path,
                        accumulator,
                        filter.as_ref(),
                        metadata,
                    );
                }
                ParsedSource::Git(git) => {
                    let installed_path = self.get_git_install_path(git, resolved_scope)?;
                    if !installed_path.exists() {
                        if !install_missing(&mut on_missing)? {
                            continue;
                        }
                    } else if resolved_scope == SourceScope::Temporary
                        && !git.pinned
                        && !self.offline
                    {
                        self.refresh_temporary_git_source(git, &resolved_source);
                    }
                    metadata.base_dir = Some(installed_path.clone());
                    self.collect_package_resources(
                        &installed_path,
                        accumulator,
                        filter.as_ref(),
                        metadata,
                    );
                }
                ParsedSource::Local(_) => {
                    // Invariant: local sources are handled (and continued)
                    // before the install-missing branch above.
                }
            }
        }
        Ok(())
    }

    /// `findAutoloadDeltaBase` (package-manager.ts:1301-1314): a project
    /// entry with `autoload: false` resolves against the user-scope copy of
    /// the same package.
    fn find_autoload_delta_base(
        &self,
        pkg: &PackageSource,
        scope: SourceScope,
        sources: &[(PackageSource, SourceScope)],
    ) -> Option<(String, SourceScope)> {
        if scope != SourceScope::Project {
            return None;
        }
        let PackageSource::Filtered(filter) = pkg else {
            return None;
        };
        if filter.autoload != Some(false) {
            return None;
        }
        let identity = self.get_package_identity(&filter.source, Some(scope));
        sources.iter().find_map(|(entry, entry_scope)| {
            if *entry_scope != SourceScope::User {
                return None;
            }
            let entry_source = Self::package_source_string(entry);
            if self.get_package_identity(entry_source, Some(SourceScope::User)) == identity {
                Some((entry_source.to_string(), SourceScope::User))
            } else {
                None
            }
        })
    }

    /// `resolveLocalExtensionSource` (package-manager.ts:1316-1345).
    fn resolve_local_extension_source(
        &self,
        path: &str,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageSourceFilter>,
        mut metadata: PackageMetadata,
        base_dir: &Path,
    ) {
        let resolved = self.resolve_path_from_base(path, base_dir);
        if !resolved.exists() {
            return;
        }
        if resolved.is_file() {
            metadata.base_dir = resolved.parent().map(Path::to_path_buf);
            accumulator.add(
                ResourceType::Extensions,
                metadata.into_resource(resolved, true),
            );
            return;
        }
        if resolved.is_dir() {
            metadata.base_dir = Some(resolved.clone());
            let found =
                self.collect_package_resources(&resolved, accumulator, filter, metadata.clone());
            if !found {
                accumulator.add(
                    ResourceType::Extensions,
                    metadata.into_resource(resolved, true),
                );
            }
        }
    }

    // -------------------------------------------------------------------
    // Package resource collection (package-manager.ts:2084-2278)
    // -------------------------------------------------------------------

    /// `collectPackageResources` (package-manager.ts:2084-2133). Returns
    /// whether any resource was found/handled.
    fn collect_package_resources(
        &self,
        package_root: &Path,
        accumulator: &mut ResourceAccumulator,
        filter: Option<&PackageSourceFilter>,
        metadata: PackageMetadata,
    ) -> bool {
        if let Some(filter) = filter {
            for resource_type in RESOURCE_TYPES {
                let patterns = resource_type.filter_patterns(filter);
                if filter.autoload == Some(false) {
                    let patterns = patterns.cloned().unwrap_or_default();
                    self.apply_package_delta_filter(
                        package_root,
                        &patterns,
                        resource_type,
                        accumulator,
                        &metadata,
                    );
                } else if let Some(patterns) = patterns {
                    self.apply_package_filter(
                        package_root,
                        patterns,
                        resource_type,
                        accumulator,
                        &metadata,
                    );
                } else {
                    self.collect_default_resources(
                        package_root,
                        resource_type,
                        accumulator,
                        &metadata,
                    );
                }
            }
            return true;
        }

        if let Some(manifest) = read_pi_manifest(package_root) {
            for resource_type in RESOURCE_TYPES {
                self.add_manifest_entries(
                    resource_type.manifest_entries(&manifest),
                    package_root,
                    resource_type,
                    accumulator,
                    &metadata,
                );
            }
            return true;
        }

        let mut has_any_dir = false;
        for resource_type in RESOURCE_TYPES {
            let dir = package_root.join(resource_type.dir_name());
            if dir.exists() {
                for file in collect_resource_files(&dir, resource_type) {
                    accumulator.add(resource_type, metadata.clone().into_resource(file, true));
                }
                has_any_dir = true;
            }
        }
        has_any_dir
    }

    /// `collectDefaultResources` (package-manager.ts:2135-2155).
    fn collect_default_resources(
        &self,
        package_root: &Path,
        resource_type: ResourceType,
        accumulator: &mut ResourceAccumulator,
        metadata: &PackageMetadata,
    ) {
        let manifest = read_pi_manifest(package_root);
        let entries = manifest
            .as_ref()
            .and_then(|m| resource_type.manifest_entries(m));
        if let Some(entries) = entries {
            self.add_manifest_entries(
                Some(entries),
                package_root,
                resource_type,
                accumulator,
                metadata,
            );
            return;
        }
        let dir = package_root.join(resource_type.dir_name());
        if dir.exists() {
            for file in collect_resource_files(&dir, resource_type) {
                accumulator.add(resource_type, metadata.clone().into_resource(file, true));
            }
        }
    }

    /// `applyPackageFilter` (package-manager.ts:2157-2181): an empty
    /// pattern array explicitly disables all resources of the type.
    fn apply_package_filter(
        &self,
        package_root: &Path,
        user_patterns: &[String],
        resource_type: ResourceType,
        accumulator: &mut ResourceAccumulator,
        metadata: &PackageMetadata,
    ) {
        let all_files = self.collect_manifest_files(package_root, resource_type);
        if user_patterns.is_empty() {
            for file in all_files {
                accumulator.add(resource_type, metadata.clone().into_resource(file, false));
            }
            return;
        }
        let enabled_by_user = apply_patterns(&all_files, user_patterns, package_root);
        for file in all_files {
            let enabled = enabled_by_user.contains(&file);
            accumulator.add(resource_type, metadata.clone().into_resource(file, enabled));
        }
    }

    /// `applyPackageDeltaFilter` (package-manager.ts:2183-2199): with
    /// `autoload: false`, only the explicitly matched entries are written
    /// (overriding the user-scope base copy's enablement).
    fn apply_package_delta_filter(
        &self,
        package_root: &Path,
        user_patterns: &[String],
        resource_type: ResourceType,
        accumulator: &mut ResourceAccumulator,
        metadata: &PackageMetadata,
    ) {
        if user_patterns.is_empty() {
            return;
        }
        let all_files = self.collect_manifest_files(package_root, resource_type);
        let enabled_by_user =
            apply_autoload_disabled_patterns(&all_files, user_patterns, package_root);
        for (file, enabled) in enabled_by_user {
            accumulator.add(resource_type, metadata.clone().into_resource(file, enabled));
        }
    }

    /// `collectManifestFiles` (package-manager.ts:2206-2226): all files of
    /// a resource type that pass the manifest's own patterns.
    fn collect_manifest_files(
        &self,
        package_root: &Path,
        resource_type: ResourceType,
    ) -> Vec<PathBuf> {
        let manifest = read_pi_manifest(package_root);
        let entries = manifest
            .as_ref()
            .and_then(|m| resource_type.manifest_entries(m));
        if let Some(entries) = entries {
            if !entries.is_empty() {
                let all_files =
                    self.collect_files_from_manifest_entries(entries, package_root, resource_type);
                let manifest_patterns: Vec<String> = entries
                    .iter()
                    .filter(|entry| is_override_pattern(entry))
                    .cloned()
                    .collect();
                if manifest_patterns.is_empty() {
                    return all_files;
                }
                let enabled = apply_patterns(&all_files, &manifest_patterns, package_root);
                // Keep the walk order (upstream Set preserves insertion
                // order; our matcher returns a set, so re-filter).
                return all_files
                    .into_iter()
                    .filter(|file| enabled.contains(file))
                    .collect();
            }
        }

        let convention_dir = package_root.join(resource_type.dir_name());
        if !convention_dir.exists() {
            return Vec::new();
        }
        collect_resource_files(&convention_dir, resource_type)
    }

    // `readPiManifest` (package-manager.ts:2228-2241) is the free
    // `read_pi_manifest`.

    /// `addManifestEntries` (package-manager.ts:2243-2261).
    fn add_manifest_entries(
        &self,
        entries: Option<&Vec<String>>,
        root: &Path,
        resource_type: ResourceType,
        accumulator: &mut ResourceAccumulator,
        metadata: &PackageMetadata,
    ) {
        let Some(entries) = entries else {
            return;
        };
        let all_files = self.collect_files_from_manifest_entries(entries, root, resource_type);
        let patterns: Vec<String> = entries
            .iter()
            .filter(|entry| is_override_pattern(entry))
            .cloned()
            .collect();
        let enabled = apply_patterns(&all_files, &patterns, root);
        for file in all_files {
            if enabled.contains(&file) {
                accumulator.add(resource_type, metadata.clone().into_resource(file, true));
            }
        }
    }

    /// `collectFilesFromManifestEntries` (package-manager.ts:2263-2278):
    /// plain entries resolve package-relative; glob entries expand against
    /// the package root (no dot entries, files and directories).
    fn collect_files_from_manifest_entries(
        &self,
        entries: &[String],
        root: &Path,
        resource_type: ResourceType,
    ) -> Vec<PathBuf> {
        let mut resolved = Vec::new();
        for entry in entries.iter().filter(|entry| !is_override_pattern(entry)) {
            if !has_glob_pattern(entry) {
                resolved.push(self.resolve_path_from_base(entry, root));
                continue;
            }
            resolved.extend(glob_expand(root, entry));
        }
        self.collect_files_from_paths(&resolved, resource_type)
    }

    /// `collectFilesFromPaths` (package-manager.ts:2469-2486).
    fn collect_files_from_paths(
        &self,
        paths: &[PathBuf],
        resource_type: ResourceType,
    ) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for path in paths {
            if !path.exists() {
                continue;
            }
            if path.is_file() {
                files.push(path.clone());
            } else if path.is_dir() {
                files.extend(collect_resource_files(path, resource_type));
            }
        }
        files
    }
}

/// `getLocalGitUpdateTarget` result (package-manager.ts:1556-1558).
struct GitUpdateTarget {
    ref_: String,
    #[allow(dead_code)] // Returned for parity; `ensureGitRef` recomputes HEAD.
    head: String,
    fetch_args: Vec<String>,
}

impl GitUpdateTarget {
    fn fetch_args_str(&self) -> Vec<&str> {
        self.fetch_args.iter().map(String::as_str).collect()
    }
}

/// Package-resource metadata (`PathMetadata` with `origin: "package"`).
#[derive(Debug, Clone)]
struct PackageMetadata {
    source: String,
    scope: SourceScope,
    base_dir: Option<PathBuf>,
}

impl PackageMetadata {
    fn into_resource(self, path: PathBuf, enabled: bool) -> ResolvedPackageResource {
        ResolvedPackageResource {
            path,
            enabled,
            source: self.source,
            scope: self.scope,
            base_dir: self.base_dir,
        }
    }
}

/// `/^([0-9a-f]{40})\s+/m` on `git ls-remote` output; with `suffix` the
/// line must end with ` {suffix}` (`/^([0-9a-f]{40})\s+HEAD$/m`).
fn extract_ls_remote_head(output: &str, suffix: Option<&str>) -> Option<String> {
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let Some(head) = parts.next() else {
            continue;
        };
        if head.len() != 40
            || !head
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            continue;
        }
        match suffix {
            Some(suffix) => {
                if parts.next() == Some(suffix) && parts.next().is_none() {
                    return Some(head.to_string());
                }
            }
            None => return Some(head.to_string()),
        }
    }
    None
}

/// `markPathIgnoredByCloudSync` (utils/paths.ts:103-118): best-effort
/// xattr marker; all errors ignored, exactly like upstream.
fn mark_path_ignored_by_cloud_sync(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        for attr in ["com.dropbox.ignored", "com.apple.fileprovider.ignore#P"] {
            let _ = std::process::Command::new("xattr")
                .args(["-w", attr, "1", &path.to_string_lossy()])
                .output();
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("setfattr")
            .args([
                "-n",
                "user.com.dropbox.ignored",
                "-v",
                "1",
                &path.to_string_lossy(),
            ])
            .output();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = path;
}

/// `isOverridePattern` (package-manager.ts:275-277).
fn is_override_pattern(s: &str) -> bool {
    s.starts_with('!') || s.starts_with('+') || s.starts_with('-')
}

/// `hasGlobPattern` (package-manager.ts:279-281).
fn has_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// `readPiManifest` / `readPiManifestFile` (package-manager.ts:536-544,
/// 2228-2241): `package.json#pi`; malformed JSON or a missing/non-object
/// `pi` yields `None` — except a present non-null non-object `pi`, which
/// upstream treats as a truthy manifest with no entries.
fn read_pi_manifest(package_root: &Path) -> Option<PiManifest> {
    let package_json_path = package_root.join("package.json");
    if !package_json_path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(package_json_path).ok()?;
    let parsed: Value = serde_json::from_str(&content).ok()?;
    let pi = parsed.get("pi")?;
    if pi.is_null() {
        return None;
    }
    let string_array = |key: &str| -> Option<Vec<String>> {
        pi.get(key).and_then(Value::as_array).map(|array| {
            array
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
    };
    Some(PiManifest {
        extensions: string_array("extensions"),
        skills: string_array("skills"),
        prompts: string_array("prompts"),
        themes: string_array("themes"),
    })
}

/// `resolveExtensionEntries` (package-manager.ts:546-574): explicit
/// `pi.extensions` manifest entries, else `index.ts` / `index.js`.
fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let package_json_path = dir.join("package.json");
    if package_json_path.exists() {
        if let Some(manifest) = read_pi_manifest(dir) {
            if let Some(entries) = &manifest.extensions {
                if !entries.is_empty() {
                    let resolved: Vec<PathBuf> = entries
                        .iter()
                        .map(|entry| resolve_path(entry, dir))
                        .filter(|path| path.exists())
                        .collect();
                    if !resolved.is_empty() {
                        return Some(resolved);
                    }
                }
            }
        }
    }
    let index_ts = dir.join("index.ts");
    if index_ts.exists() {
        return Some(vec![index_ts]);
    }
    let index_js = dir.join("index.js");
    if index_js.exists() {
        return Some(vec![index_js]);
    }
    None
}

/// `collectAutoExtensionEntries` (package-manager.ts:576-628): smart
/// discovery — the directory itself, its direct `.ts`/`.js` files, and one
/// level of subdirectory entry points.
fn collect_auto_extension_entries(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    if let Some(root_entries) = resolve_extension_entries(dir) {
        return root_entries;
    }
    let mut entries = Vec::new();
    for path in walk_entries(dir, Some(1)) {
        if path.is_file() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.ends_with(".ts") || name.ends_with(".js") {
                entries.push(path);
            }
        } else if path.is_dir() {
            if let Some(resolved) = resolve_extension_entries(&path) {
                entries.extend(resolved);
            }
        }
    }
    entries
}

/// `collectResourceFiles` (package-manager.ts:634-642).
fn collect_resource_files(dir: &Path, resource_type: ResourceType) -> Vec<PathBuf> {
    match resource_type {
        ResourceType::Skills => collect_skill_entries(dir, SkillDiscoveryMode::Pir),
        ResourceType::Extensions => collect_auto_extension_entries(dir),
        ResourceType::Prompts => walk_files(dir, ".md"),
        ResourceType::Themes => walk_files(dir, ".json"),
    }
}

/// Shared walker configuration (see `resource_loader::file_walk`):
/// hidden entries skipped, `.gitignore`/`.ignore`/`.fdignore` honored,
/// `node_modules` pruned, symlinks followed, deterministic sorted output.
fn configure_walk(dir: &Path, max_depth: Option<usize>) -> WalkBuilder {
    let mut builder = WalkBuilder::new(dir);
    builder
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .require_git(false)
        .follow_links(true)
        .add_custom_ignore_filename(".fdignore")
        .filter_entry(|entry| entry.file_name() != "node_modules");
    if let Some(depth) = max_depth {
        builder.max_depth(Some(depth));
    }
    builder
}

/// All direct/recursive entries (files and directories) under `dir`.
fn walk_entries(dir: &Path, max_depth: Option<usize>) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if !dir.is_dir() {
        return entries;
    }
    for result in configure_walk(dir, max_depth).build() {
        let Ok(entry) = result else {
            continue;
        };
        if entry.path() == dir {
            continue;
        }
        entries.push(entry.path().to_path_buf());
    }
    entries.sort();
    entries
}

/// `collectFiles` (package-manager.ts:296-345): recursive scan collecting
/// files with the given extension.
fn walk_files(dir: &Path, extension: &str) -> Vec<PathBuf> {
    walk_entries(dir, None)
        .into_iter()
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .map(|name| name.to_string_lossy().ends_with(extension))
                    .unwrap_or(false)
        })
        .collect()
}

/// `globSync(entry, {cwd: root, absolute: true, dot: false, nodir: false})`
/// (package-manager.ts:2270-2275): walk the package root (no ignore files,
/// hidden entries skipped, symlinks not followed) and match the
/// root-relative posix path with the built-in glob matcher.
fn glob_expand(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut matches = Vec::new();
    if !root.is_dir() {
        return matches;
    }
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .require_git(false)
        .follow_links(false);
    for result in builder.build() {
        let Ok(entry) = result else {
            continue;
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative_posix = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if glob_match(pattern, &relative_posix) {
            matches.push(path.to_path_buf());
        }
    }
    matches.sort();
    matches
}

/// `applyAutoloadDisabledPatterns` (package-manager.ts:776-793).
fn apply_autoload_disabled_patterns(
    all_paths: &[PathBuf],
    patterns: &[String],
    base_dir: &Path,
) -> Vec<(PathBuf, bool)> {
    let mut result: Vec<(PathBuf, bool)> = Vec::new();
    for pattern in patterns {
        let exact = pattern.starts_with('+') || pattern.starts_with('-');
        let target = if exact || pattern.starts_with('!') {
            &pattern[1..]
        } else {
            pattern.as_str()
        };
        let enabled = !pattern.starts_with('-') && !pattern.starts_with('!');
        for file_path in all_paths {
            let candidates = match_candidates(file_path, base_dir);
            let matched = if exact {
                matches_any_exact_pattern(&candidates, &[target.to_string()])
            } else {
                matches_any_pattern(&candidates, &[target.to_string()])
            };
            if matched {
                // Later patterns overwrite earlier ones (Map.set semantics).
                if let Some(entry) = result.iter_mut().find(|(path, _)| path == file_path) {
                    entry.1 = enabled;
                } else {
                    result.push((file_path.clone(), enabled));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    //! Port of the W2-relevant intent of
    //! `packages/coding-agent/test/package-manager.test.ts`,
    //! `package-manager-ssh.test.ts` and `package-command-paths.test.ts`:
    //! source parsing, install paths, settings normalization, npmCommand
    //! argv wrapping, git clone/reconcile, offline behavior, package
    //! filters/manifests/dedupe and `listConfiguredPackages`.

    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn command_request_display_redacts_url_userinfo() {
        let request = CommandRequest::new(
            "git",
            &[
                "clone",
                "https://user:secret-token@example.com/org/repo.git",
                "target",
            ],
        );
        let shown = request.display();
        assert_eq!(
            shown,
            "git clone https://***@example.com/org/repo.git target"
        );
        assert!(!shown.contains("secret-token"));

        // No userinfo → unchanged (upstream error prefix shape).
        let plain = CommandRequest::new("git", &["fetch", "origin", "v1.0"]);
        assert_eq!(plain.display(), "git fetch origin v1.0");

        // Bare `user@host` (no password) is left untouched.
        let user_only = CommandRequest::new("git", &["clone", "https://user@example.com/r.git"]);
        assert_eq!(
            user_only.display(),
            "git clone https://user@example.com/r.git"
        );
    }

    struct TestDirs {
        root: PathBuf,
        cwd: PathBuf,
        agent_dir: PathBuf,
    }

    impl TestDirs {
        fn new() -> Self {
            let unique = format!(
                "pir-pm-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            );
            let root = std::env::temp_dir().join(unique);
            let cwd = root.join("cwd");
            let agent_dir = root.join("agent");
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::create_dir_all(&agent_dir).unwrap();
            TestDirs {
                root,
                cwd,
                agent_dir,
            }
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[derive(Debug, Clone)]
    struct RecordedCall {
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    }

    /// Fake command runner: records every invocation and delegates the
    /// capture result to a scripted handler. No process ever spawns.
    type CaptureHandler = Box<dyn Fn(&CommandRequest) -> Result<String, String> + Send + Sync>;

    struct FakeRunner {
        calls: Mutex<Vec<RecordedCall>>,
        handler: CaptureHandler,
    }

    impl FakeRunner {
        fn new(
            handler: impl Fn(&CommandRequest) -> Result<String, String> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(FakeRunner {
                calls: Mutex::new(Vec::new()),
                handler: Box::new(handler),
            })
        }

        fn ok() -> Arc<Self> {
            Self::new(|_| Ok(String::new()))
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().clone()
        }

        fn find_calls(&self, command: &str, first_arg: &str) -> Vec<RecordedCall> {
            self.calls()
                .into_iter()
                .filter(|call| {
                    call.command == command
                        && call.args.first().map(String::as_str) == Some(first_arg)
                })
                .collect()
        }
    }

    impl PackageCommandRunner for FakeRunner {
        fn run(&self, request: &CommandRequest) -> Result<(), String> {
            self.calls.lock().unwrap().push(RecordedCall {
                command: request.command.clone(),
                args: request.args.clone(),
                cwd: request.cwd.clone(),
            });
            (self.handler)(request).map(|_| ())
        }

        fn run_capture(&self, request: &CommandRequest) -> Result<String, String> {
            self.calls.lock().unwrap().push(RecordedCall {
                command: request.command.clone(),
                args: request.args.clone(),
                cwd: request.cwd.clone(),
            });
            (self.handler)(request)
        }
    }

    fn test_manager_with(
        dirs: &TestDirs,
        runner: Arc<dyn PackageCommandRunner>,
        offline: bool,
        project_trusted: bool,
    ) -> DefaultPackageManager {
        let settings_manager = SettingsManager::create(
            &dirs.cwd,
            Some(&dirs.agent_dir),
            crate::core::settings_manager::SettingsManagerCreateOptions { project_trusted },
        );
        DefaultPackageManager::with_options(PackageManagerOptions {
            cwd: dirs.cwd.clone(),
            agent_dir: dirs.agent_dir.clone(),
            settings_manager,
            runner: Some(runner),
            offline: Some(offline),
        })
    }

    fn test_manager(
        dirs: &TestDirs,
        runner: Arc<dyn PackageCommandRunner>,
    ) -> DefaultPackageManager {
        test_manager_with(dirs, runner, false, true)
    }

    fn write_file(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Build a local package directory with convention-layout resources.
    fn make_local_package(dir: &Path) {
        write_file(
            &dir.join("extensions/index.ts"),
            "export default function() {}",
        );
        write_file(&dir.join("skills/foo/SKILL.md"), "# Foo\n");
        write_file(&dir.join("prompts/review.md"), "# Review\n");
        write_file(&dir.join("themes/dark.json"), "{}");
    }

    fn git_source(repo: &str, host: &str, path: &str, ref_: Option<&str>) -> GitSource {
        GitSource {
            repo: repo.to_string(),
            host: host.to_string(),
            path: path.to_string(),
            ref_: ref_.map(str::to_string),
            pinned: ref_.is_some(),
        }
    }

    // -----------------------------------------------------------------
    // Source parsing (package-manager-ssh.test.ts, "source parsing")
    // -----------------------------------------------------------------

    #[test]
    fn test_parse_source_npm_pinned_and_range() {
        let ParsedSource::Npm(npm) = parse_source("npm:@scope/pkg@1.2.3") else {
            panic!("expected npm source");
        };
        assert!(npm.pinned);
        assert_eq!(npm.name, "@scope/pkg");
        assert_eq!(npm.version.as_deref(), Some("1.2.3"));

        let ParsedSource::Npm(npm) = parse_source("npm:@scope/pkg@^1.2.3") else {
            panic!("expected npm source");
        };
        assert!(!npm.pinned);
        assert_eq!(npm.range.as_deref(), Some("^1.2.3"));

        let ParsedSource::Npm(npm) = parse_source("npm:pkg") else {
            panic!("expected npm source");
        };
        assert!(!npm.pinned);
        assert_eq!(npm.range, None);
    }

    #[test]
    fn test_parse_source_git_forms_from_docs_examples() {
        for source in [
            "git:github.com/user/repo@v1",
            "https://github.com/user/repo@v1",
            "git:git@github.com:user/repo@v1",
            "ssh://git@github.com/user/repo@v1",
        ] {
            let ParsedSource::Git(git) = parse_source(source) else {
                panic!("expected git source: {source}");
            };
            assert_eq!(git.host, "github.com", "{source}");
            assert_eq!(git.path, "user/repo", "{source}");
            assert_eq!(git.ref_.as_deref(), Some("v1"), "{source}");
            assert!(git.pinned, "{source}");
        }
    }

    #[test]
    fn test_parse_source_local_paths_and_bare_names() {
        for source in [
            "/absolute/path/to/package",
            "./relative/path/to/package",
            "../relative/path/to/package",
            "bare-name",
        ] {
            assert!(
                matches!(parse_source(source), ParsedSource::Local(_)),
                "{source} should be local"
            );
        }
        // `github.com/user/repo` shorthand is local without the git: prefix.
        assert!(matches!(
            parse_source("github.com/user/repo"),
            ParsedSource::Local(_)
        ));
        assert!(matches!(
            parse_source("git@github.com:user/repo"),
            ParsedSource::Local(_)
        ));
    }

    #[test]
    fn test_parse_source_https_hosts() {
        for (url, host) in [
            ("https://github.com/user/repo", "github.com"),
            ("https://github.com/user/repo.git", "github.com"),
            ("git:https://github.com/user/repo", "github.com"),
            ("https://gitlab.com/user/repo", "gitlab.com"),
            ("https://bitbucket.org/user/repo", "bitbucket.org"),
            ("https://codeberg.org/user/repo", "codeberg.org"),
        ] {
            let ParsedSource::Git(git) = parse_source(url) else {
                panic!("expected git source: {url}");
            };
            assert_eq!(git.host, host, "{url}");
            assert_eq!(git.path, "user/repo", "{url}");
            assert!(!git.pinned, "{url}");
        }
    }

    #[test]
    fn test_parse_source_https_refs() {
        let ParsedSource::Git(git) = parse_source("https://github.com/user/repo@v1.2.3") else {
            panic!("expected git source");
        };
        assert_eq!(git.ref_.as_deref(), Some("v1.2.3"));
        assert!(git.pinned);

        let ParsedSource::Git(git) = parse_source("https://github.com/user/repo@feature/branch")
        else {
            panic!("expected git source");
        };
        assert_eq!(git.ref_.as_deref(), Some("feature/branch"));
    }

    #[test]
    fn test_parse_source_never_parses_dot_relative_as_git() {
        assert_eq!(
            parse_source("./packages/agent-timers"),
            ParsedSource::Local("./packages/agent-timers".to_string())
        );
        assert_eq!(
            parse_source("../packages/agent-timers"),
            ParsedSource::Local("../packages/agent-timers".to_string())
        );
    }

    #[test]
    fn test_git_identity_normalizes_url_formats() {
        let dirs = TestDirs::new();
        let manager = test_manager(&dirs, FakeRunner::ok());
        for source in [
            "https://github.com/user/repo",
            "https://github.com/user/repo@v1.0.0",
            "git:github.com/user/repo",
            "https://github.com/user/repo.git",
            "git:git@github.com:user/repo",
            "ssh://git@github.com/user/repo",
        ] {
            assert_eq!(
                manager.get_package_identity(source, None),
                "git:github.com/user/repo",
                "{source}"
            );
        }
        // Different repos stay separate (SSH vs HTTPS).
        assert_ne!(
            manager.get_package_identity("git:git@github.com:user/repo-a", None),
            manager.get_package_identity("git:git@github.com:user/repo-b", None),
        );
    }

    // -----------------------------------------------------------------
    // Install paths
    // -----------------------------------------------------------------

    #[test]
    fn test_git_install_path_rejects_traversal() {
        let dirs = TestDirs::new();
        let manager = test_manager(&dirs, FakeRunner::ok());
        let traversal = git_source(
            "git@evil.example:../../victim/repo",
            "evil.example",
            "../../victim/repo",
            None,
        );
        for scope in [
            SourceScope::User,
            SourceScope::Project,
            SourceScope::Temporary,
        ] {
            let error = manager
                .get_git_install_path(&traversal, scope)
                .expect_err("traversal must be rejected");
            assert!(
                error.contains("outside package install root"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn test_temporary_npm_install_path_under_agent_temp_folder() {
        let dirs = TestDirs::new();
        let manager = test_manager(&dirs, FakeRunner::ok());
        let ParsedSource::Npm(npm) = parse_source("npm:left-pad") else {
            panic!("expected npm source");
        };
        let install_path = manager
            .get_managed_npm_install_path(&npm, SourceScope::Temporary)
            .unwrap();
        let temp_root = dirs.agent_dir.join("tmp").join("extensions");
        assert!(install_path.starts_with(&temp_root));
        assert!(install_path.ends_with("node_modules/left-pad"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&temp_root).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    // -----------------------------------------------------------------
    // Settings source normalization
    // -----------------------------------------------------------------

    #[test]
    fn test_add_source_stores_global_local_relative_to_agent_dir() {
        let dirs = TestDirs::new();
        let package_dir = dirs.root.join("packages/local-global-pkg");
        make_local_package(&package_dir);
        let mut manager = test_manager(&dirs, FakeRunner::ok());

        // Source is resolved against cwd; stored relative to agentDir.
        let source = format!(
            "./{}",
            lexical_relative(&dirs.cwd, &package_dir).to_string_lossy()
        );
        assert!(manager.add_source_to_settings(&source, false).unwrap());
        let settings = manager.settings_manager.get_global_settings();
        let packages = settings
            .as_map()
            .get("packages")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(
            packages[0].as_str().unwrap(),
            lexical_relative(&dirs.agent_dir, &package_dir)
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn test_add_source_stores_project_local_relative_to_pir_dir() {
        let dirs = TestDirs::new();
        let package_dir = dirs.cwd.join("project-local-pkg");
        make_local_package(&package_dir);
        let mut manager = test_manager(&dirs, FakeRunner::ok());

        assert!(manager
            .add_source_to_settings("./project-local-pkg", true)
            .unwrap());
        let settings = manager.settings_manager.get_project_settings();
        let packages = settings
            .as_map()
            .get("packages")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(packages[0].as_str().unwrap(), "../project-local-pkg");
    }

    #[test]
    fn test_remove_local_package_with_equivalent_path_form() {
        let dirs = TestDirs::new();
        let package_dir = dirs.cwd.join("remove-local-pkg");
        make_local_package(&package_dir);
        let mut manager = test_manager(&dirs, FakeRunner::ok());

        manager
            .add_source_to_settings("./remove-local-pkg", false)
            .unwrap();
        let with_trailing_slash = format!("{}/", package_dir.display());
        assert!(manager
            .remove_source_from_settings(&with_trailing_slash, false)
            .unwrap());
        assert!(DefaultPackageManager::packages_of(
            &manager.settings_manager.get_global_settings()
        )
        .is_empty());
    }

    #[test]
    fn test_add_same_git_source_twice_returns_false() {
        let dirs = TestDirs::new();
        let mut manager = test_manager(&dirs, FakeRunner::ok());
        assert!(manager
            .add_source_to_settings("git:github.com/user/repo@v1", false)
            .unwrap());
        assert!(!manager
            .add_source_to_settings("git:github.com/user/repo@v1", false)
            .unwrap());
    }

    #[test]
    fn test_add_same_git_source_with_new_ref_updates_entry() {
        let dirs = TestDirs::new();
        let mut manager = test_manager(&dirs, FakeRunner::ok());
        manager
            .add_source_to_settings("git:github.com/user/repo@v1", false)
            .unwrap();
        assert!(manager
            .add_source_to_settings("git:github.com/user/repo@v2", false)
            .unwrap());
        let packages =
            DefaultPackageManager::packages_of(&manager.settings_manager.get_global_settings());
        assert_eq!(
            packages,
            vec![PackageSource::Source(
                "git:github.com/user/repo@v2".to_string()
            )]
        );
    }

    #[test]
    fn test_add_source_preserves_filters_when_replacing_ref() {
        let dirs = TestDirs::new();
        let mut manager = test_manager(&dirs, FakeRunner::ok());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Filtered(PackageSourceFilter {
                source: "git:github.com/user/repo@v1".to_string(),
                autoload: None,
                extensions: Some(vec!["extensions/main.ts".to_string()]),
                skills: Some(Vec::new()),
                prompts: Some(vec!["prompts/review.md".to_string()]),
                themes: None,
            })]);

        assert!(manager
            .add_source_to_settings("git:github.com/user/repo@v2", false)
            .unwrap());
        let packages =
            DefaultPackageManager::packages_of(&manager.settings_manager.get_global_settings());
        assert_eq!(
            packages,
            vec![PackageSource::Filtered(PackageSourceFilter {
                source: "git:github.com/user/repo@v2".to_string(),
                autoload: None,
                extensions: Some(vec!["extensions/main.ts".to_string()]),
                skills: Some(Vec::new()),
                prompts: Some(vec!["prompts/review.md".to_string()]),
                themes: None,
            })]
        );
    }

    // -----------------------------------------------------------------
    // npm install / uninstall command lines
    // -----------------------------------------------------------------

    #[test]
    fn test_install_npm_uses_default_install_args() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::ok();
        let manager = test_manager(&dirs, runner.clone());
        manager.install("npm:left-pad", false).unwrap();

        let calls = runner.find_calls("npm", "install");
        assert_eq!(calls.len(), 1, "calls: {:?}", runner.calls());
        assert_eq!(
            calls[0].args,
            vec![
                "install",
                "left-pad",
                "--prefix",
                dirs.agent_dir.join("npm").to_string_lossy().as_ref(),
                "--legacy-peer-deps",
            ]
        );
        // `ensureNpmProject` side effects.
        let package_json: Value = serde_json::from_str(
            &std::fs::read_to_string(dirs.agent_dir.join("npm/package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            package_json,
            serde_json::json!({"name": "pi-extensions", "private": true})
        );
        assert_eq!(
            std::fs::read_to_string(dirs.agent_dir.join("npm/.gitignore")).unwrap(),
            "*\n!.gitignore\n"
        );
    }

    #[test]
    fn test_install_npm_with_npm_command_argv_wrapper() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::ok();
        let mut manager = test_manager(&dirs, runner.clone());
        // Wrapper argv including an entry with spaces and a `--` separator;
        // the package manager name resolves to `pnpm`.
        manager.settings_manager.set_npm_command(Some(vec![
            "my wrapper".to_string(),
            "--with flag".to_string(),
            "--".to_string(),
            "pnpm".to_string(),
        ]));
        manager.install("npm:left-pad", false).unwrap();

        let matching: Vec<_> = runner
            .calls()
            .into_iter()
            .filter(|call| call.command == "my wrapper")
            .collect();
        assert_eq!(matching.len(), 1, "calls: {:?}", runner.calls());
        let call = &matching[0];
        assert_eq!(
            call.args,
            vec![
                "--with flag",
                "--",
                "pnpm",
                "install",
                "left-pad",
                "--prefix",
                dirs.agent_dir.join("npm").to_string_lossy().as_ref(),
                "--config.auto-install-peers=false",
                "--config.strict-peer-dependencies=false",
                "--config.strict-dep-builds=false",
            ]
        );
    }

    #[test]
    fn test_install_npm_bun_uses_cwd_flag() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::ok();
        let mut manager = test_manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_npm_command(Some(vec!["bun".to_string()]));
        manager.install("npm:left-pad", false).unwrap();

        let call = runner
            .calls()
            .into_iter()
            .find(|c| c.command == "bun")
            .unwrap();
        assert_eq!(
            call.args,
            vec![
                "install",
                "left-pad",
                "--cwd",
                dirs.agent_dir.join("npm").to_string_lossy().as_ref(),
                "--omit=peer",
            ]
        );
    }

    #[test]
    fn test_uninstall_npm_passes_legacy_peer_deps() {
        let dirs = TestDirs::new();
        // The managed npm root must exist for uninstall to run.
        std::fs::create_dir_all(dirs.agent_dir.join("npm")).unwrap();
        let runner = FakeRunner::ok();
        let manager = test_manager(&dirs, runner.clone());
        manager.remove("npm:left-pad", false).unwrap();

        let call = runner
            .calls()
            .into_iter()
            .find(|c| c.command == "npm" && c.args.first().map(String::as_str) == Some("uninstall"))
            .unwrap();
        assert_eq!(
            call.args,
            vec![
                "uninstall",
                "left-pad",
                "--prefix",
                dirs.agent_dir.join("npm").to_string_lossy().as_ref(),
                "--legacy-peer-deps",
            ]
        );
    }

    #[test]
    fn test_uninstall_npm_skips_missing_install_root() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::ok();
        let manager = test_manager(&dirs, runner.clone());
        manager.remove("npm:left-pad", false).unwrap();
        assert!(runner.calls().is_empty());
    }

    // -----------------------------------------------------------------
    // git install / reconcile / remove
    // -----------------------------------------------------------------

    #[test]
    fn test_install_git_clones_checks_out_ref_and_installs_deps() {
        let dirs = TestDirs::new();
        let target = dirs.agent_dir.join("git/github.com/user/repo");
        let runner = FakeRunner::new(move |request| {
            if request.command == "git" && request.args.first().map(String::as_str) == Some("clone")
            {
                let target = PathBuf::from(request.args[2].clone());
                write_file(&target.join("package.json"), "{}");
            }
            Ok(String::new())
        });
        let manager = test_manager(&dirs, runner.clone());
        manager
            .install("git:github.com/user/repo@v1", false)
            .unwrap();

        let calls = runner.calls();
        assert_eq!(
            calls[0].args,
            vec![
                "clone",
                "https://github.com/user/repo",
                target.to_string_lossy().as_ref()
            ]
        );
        assert_eq!(calls[0].command, "git");
        assert_eq!(calls[1].command, "git");
        assert_eq!(calls[1].args, vec!["checkout", "v1"]);
        assert_eq!(calls[1].cwd, Some(target.clone()));
        // Dependencies install with `--omit=dev` when no npmCommand wrapper
        // is configured.
        let npm_call = calls.iter().find(|c| c.command == "npm").unwrap();
        assert_eq!(npm_call.args, vec!["install", "--omit=dev"]);
        assert_eq!(npm_call.cwd, Some(target));
    }

    #[test]
    fn test_install_git_deps_use_plain_install_with_npm_command() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::new(|request| {
            if request.command == "git" && request.args.first().map(String::as_str) == Some("clone")
            {
                let target = PathBuf::from(request.args[2].clone());
                write_file(&target.join("package.json"), "{}");
            }
            Ok(String::new())
        });
        let mut manager = test_manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_npm_command(Some(vec!["pnpm".to_string()]));
        manager.install("git:github.com/user/repo", false).unwrap();

        let npm_call = runner
            .calls()
            .into_iter()
            .find(|c| c.command == "pnpm")
            .unwrap();
        assert_eq!(npm_call.args, vec!["install"]);
    }

    #[test]
    fn test_install_git_existing_pinned_checkout_reconciles() {
        let dirs = TestDirs::new();
        let target = dirs.agent_dir.join("git/github.com/user/repo");
        write_file(&target.join("package.json"), "{}");
        let head = "a".repeat(40);
        let fetched = "b".repeat(40);
        let runner = FakeRunner::new(move |request| {
            if request.command == "git"
                && request.args.first().map(String::as_str) == Some("rev-parse")
            {
                if request.args.iter().any(|a| a == "HEAD") {
                    return Ok(head.clone());
                }
                return Ok(fetched.clone());
            }
            Ok(String::new())
        });
        let manager = test_manager(&dirs, runner.clone());
        manager
            .install("git:github.com/user/repo@v1", false)
            .unwrap();

        let git_args: Vec<Vec<String>> = runner
            .calls()
            .into_iter()
            .filter(|c| c.command == "git")
            .map(|c| c.args)
            .collect();
        assert_eq!(git_args[0], vec!["fetch", "origin", "v1"]);
        assert!(git_args.contains(&vec![
            "reset".to_string(),
            "--hard".to_string(),
            "FETCH_HEAD^{commit}".to_string()
        ]));
        assert!(git_args.contains(&vec!["clean".to_string(), "-fdx".to_string()]));
        // Dependencies reinstall after the reset.
        let npm_call = runner
            .calls()
            .into_iter()
            .find(|c| c.command == "npm")
            .unwrap();
        assert_eq!(npm_call.args, vec!["install", "--omit=dev"]);
    }

    #[test]
    fn test_install_git_existing_checkout_at_target_head_skips_reset() {
        let dirs = TestDirs::new();
        let target = dirs.agent_dir.join("git/github.com/user/repo");
        write_file(&target.join("package.json"), "{}");
        let head = "a".repeat(40);
        let runner = FakeRunner::new(move |request| {
            if request.command == "git"
                && request.args.first().map(String::as_str) == Some("rev-parse")
            {
                return Ok(head.clone());
            }
            Ok(String::new())
        });
        let manager = test_manager(&dirs, runner.clone());
        manager
            .install("git:github.com/user/repo@v1", false)
            .unwrap();

        let git_args: Vec<Vec<String>> = runner
            .calls()
            .into_iter()
            .filter(|c| c.command == "git")
            .map(|c| c.args)
            .collect();
        assert_eq!(git_args[0], vec!["fetch", "origin", "v1"]);
        assert!(!git_args
            .iter()
            .any(|args| args.first().map(String::as_str) == Some("reset")));
        // No dependency reinstall when HEAD did not move.
        assert!(runner.calls().iter().all(|c| c.command != "npm"));
    }

    #[test]
    fn test_remove_git_prunes_empty_parent_dirs() {
        let dirs = TestDirs::new();
        let target = dirs.agent_dir.join("git/github.com/user/repo");
        write_file(&target.join("package.json"), "{}");
        let runner = FakeRunner::ok();
        let manager = test_manager(&dirs, runner);

        manager.remove("git:github.com/user/repo", false).unwrap();
        assert!(!target.exists());
        assert!(!dirs.agent_dir.join("git/github.com/user").exists());
        assert!(!dirs.agent_dir.join("git/github.com").exists());
        assert!(dirs.agent_dir.join("git").exists());
    }

    #[test]
    fn test_remove_local_source_only_touches_settings() {
        let dirs = TestDirs::new();
        let package_dir = dirs.cwd.join("local-pkg");
        make_local_package(&package_dir);
        let runner = FakeRunner::ok();
        let mut manager = test_manager(&dirs, runner.clone());
        manager
            .add_source_to_settings("./local-pkg", false)
            .unwrap();

        assert!(manager.remove_and_persist("./local-pkg", false).unwrap());
        assert!(package_dir.exists());
        assert!(runner.calls().is_empty());
        // Removing again finds no matching settings entry.
        assert!(!manager.remove_and_persist("./local-pkg", false).unwrap());
    }

    // -----------------------------------------------------------------
    // Progress events / local existence / offline
    // -----------------------------------------------------------------

    #[test]
    fn test_progress_events_on_install_attempt() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::new(|_| Err("simulated npm install failure".to_string()));
        let mut manager = test_manager(&dirs, runner);
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        manager.set_progress_callback(Some(Box::new(move |event| {
            events_clone.lock().unwrap().push(event.clone());
        })));

        let error = manager
            .install("npm:nonexistent-package@1.0.0", false)
            .expect_err("install must fail");
        assert!(error.contains("simulated npm install failure"));
        let events = events.lock().unwrap();
        assert!(events
            .iter()
            .any(|e| e.kind == ProgressKind::Start && e.action == ProgressAction::Install));
        assert!(events.iter().any(|e| e.kind == ProgressKind::Error));
    }

    #[test]
    fn test_install_local_missing_path_errors() {
        let dirs = TestDirs::new();
        let manager = test_manager(&dirs, FakeRunner::ok());
        let error = manager.install("./missing", false).expect_err("must fail");
        assert!(error.starts_with("Path does not exist: "), "{error}");
    }

    #[test]
    fn test_resolve_offline_skips_missing_sources() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::new(|_| Err("unexpected install".to_string()));
        let mut manager = test_manager_with(&dirs, runner.clone(), true, true);
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source("npm:foo".to_string())]);

        let resolved = manager.resolve(None).unwrap();
        assert!(resolved.extensions.is_empty());
        // The legacy global npm root lookup still runs (upstream
        // `getNpmInstallPath` fallback); only installs must be skipped.
        assert!(runner.find_calls("npm", "install").is_empty());
    }

    #[test]
    fn test_resolve_on_missing_skip_and_error() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::new(|_| Err("unexpected install".to_string()));
        let mut manager = test_manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source("npm:foo".to_string())]);

        let mut skip = |_: &str| MissingSourceAction::Skip;
        let resolved = manager.resolve(Some(&mut skip)).unwrap();
        assert!(resolved.extensions.is_empty());
        assert!(runner.find_calls("npm", "install").is_empty());

        let mut error = |_: &str| MissingSourceAction::Error;
        let err = manager
            .resolve(Some(&mut error))
            .expect_err("missing source must error");
        assert_eq!(err, "Missing source: npm:foo");
    }

    // -----------------------------------------------------------------
    // resolve: local packages, manifest, filters, dedupe
    // -----------------------------------------------------------------

    #[test]
    fn test_resolve_local_package_convention_dirs() {
        let dirs = TestDirs::new();
        let package_dir = dirs.cwd.join("pkg");
        make_local_package(&package_dir);
        let runner = FakeRunner::ok();
        let manager = test_manager(&dirs, runner);

        let resolved = manager
            .resolve_extension_sources(&[package_dir.to_string_lossy().into_owned()], false, false)
            .unwrap();
        assert_eq!(
            resolved
                .extensions
                .iter()
                .map(|r| &r.path)
                .collect::<Vec<_>>(),
            vec![&package_dir.join("extensions/index.ts")]
        );
        assert_eq!(
            resolved.skills.iter().map(|r| &r.path).collect::<Vec<_>>(),
            vec![&package_dir.join("skills/foo/SKILL.md")]
        );
        assert_eq!(
            resolved.prompts.iter().map(|r| &r.path).collect::<Vec<_>>(),
            vec![&package_dir.join("prompts/review.md")]
        );
        assert_eq!(
            resolved.themes.iter().map(|r| &r.path).collect::<Vec<_>>(),
            vec![&package_dir.join("themes/dark.json")]
        );
        assert!(resolved.extensions[0].enabled);
        assert_eq!(resolved.extensions[0].base_dir, Some(package_dir.clone()));
    }

    #[test]
    fn test_resolve_local_file_source_loads_single_extension() {
        let dirs = TestDirs::new();
        let extension = dirs.cwd.join("single.ts");
        write_file(&extension, "export default function() {}");
        let manager = test_manager(&dirs, FakeRunner::ok());

        let resolved = manager
            .resolve_extension_sources(&[extension.to_string_lossy().into_owned()], false, false)
            .unwrap();
        assert_eq!(resolved.extensions.len(), 1);
        assert_eq!(resolved.extensions[0].path, extension);
        assert_eq!(resolved.extensions[0].base_dir, Some(dirs.cwd.clone()));
    }

    #[test]
    fn test_resolve_package_json_pi_manifest_entries() {
        let dirs = TestDirs::new();
        let package_dir = dirs.cwd.join("pkg");
        write_file(
            &package_dir.join("package.json"),
            r#"{"name": "pkg", "pi": {"extensions": ["src/main.ts"], "skills": ["skills/*"]}}"#,
        );
        write_file(
            &package_dir.join("src/main.ts"),
            "export default function() {}",
        );
        write_file(&package_dir.join("skills/alpha/SKILL.md"), "# Alpha\n");
        write_file(&package_dir.join("skills/beta/SKILL.md"), "# Beta\n");
        // Convention dirs are ignored when a manifest is present.
        write_file(&package_dir.join("themes/ignored.json"), "{}");
        let manager = test_manager(&dirs, FakeRunner::ok());

        let resolved = manager
            .resolve_extension_sources(&[package_dir.to_string_lossy().into_owned()], false, false)
            .unwrap();
        assert_eq!(
            resolved
                .extensions
                .iter()
                .map(|r| &r.path)
                .collect::<Vec<_>>(),
            vec![&package_dir.join("src/main.ts")]
        );
        assert_eq!(
            resolved.skills.iter().map(|r| &r.path).collect::<Vec<_>>(),
            vec![
                &package_dir.join("skills/alpha/SKILL.md"),
                &package_dir.join("skills/beta/SKILL.md"),
            ]
        );
        assert!(resolved.themes.is_empty());
    }

    #[test]
    fn test_resolve_manifest_override_patterns() {
        let dirs = TestDirs::new();
        let package_dir = dirs.cwd.join("pkg");
        write_file(
            &package_dir.join("package.json"),
            r#"{"name": "pkg", "pi": {"extensions": ["extensions", "!extensions/draft.ts"]}}"#,
        );
        write_file(&package_dir.join("extensions/main.ts"), "");
        write_file(&package_dir.join("extensions/draft.ts"), "");
        let manager = test_manager(&dirs, FakeRunner::ok());

        let resolved = manager
            .resolve_extension_sources(&[package_dir.to_string_lossy().into_owned()], false, false)
            .unwrap();
        assert_eq!(
            resolved
                .extensions
                .iter()
                .map(|r| &r.path)
                .collect::<Vec<_>>(),
            vec![&package_dir.join("extensions/main.ts")]
        );
    }

    /// Configure a filtered user-scope local package (source relative to
    /// the agent dir) and resolve it.
    fn resolve_filtered_user_package(
        dirs: &TestDirs,
        filter: PackageSourceFilter,
    ) -> ResolvedPackagePaths {
        let runner = FakeRunner::new(|_| Err("unexpected install".to_string()));
        let mut manager = test_manager(dirs, runner);
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Filtered(filter)]);
        manager.resolve(None).unwrap()
    }

    #[test]
    fn test_package_filter_exclude_pattern() {
        let dirs = TestDirs::new();
        let package_dir = dirs.root.join("pkg");
        write_file(&package_dir.join("extensions/a.ts"), "");
        write_file(&package_dir.join("extensions/a.test.ts"), "");
        let source = lexical_relative(&dirs.agent_dir, &package_dir)
            .to_string_lossy()
            .into_owned();

        let resolved = resolve_filtered_user_package(
            &dirs,
            PackageSourceFilter {
                source,
                extensions: Some(vec!["!**/*.test.ts".to_string()]),
                ..PackageSourceFilter::default()
            },
        );
        let enabled = |name: &str| {
            resolved
                .extensions
                .iter()
                .find(|r| r.path.ends_with(name))
                .map(|r| r.enabled)
        };
        assert_eq!(enabled("extensions/a.ts"), Some(true));
        assert_eq!(enabled("extensions/a.test.ts"), Some(false));
    }

    #[test]
    fn test_package_filter_empty_array_disables_type() {
        let dirs = TestDirs::new();
        let package_dir = dirs.root.join("pkg");
        write_file(&package_dir.join("extensions/a.ts"), "");
        write_file(&package_dir.join("themes/dark.json"), "{}");
        let source = lexical_relative(&dirs.agent_dir, &package_dir)
            .to_string_lossy()
            .into_owned();

        let resolved = resolve_filtered_user_package(
            &dirs,
            PackageSourceFilter {
                source,
                extensions: Some(Vec::new()),
                ..PackageSourceFilter::default()
            },
        );
        assert_eq!(resolved.extensions.len(), 1);
        assert!(!resolved.extensions[0].enabled);
        // Unmentioned types collect their defaults (all enabled).
        assert_eq!(resolved.themes.len(), 1);
        assert!(resolved.themes[0].enabled);
    }

    #[test]
    fn test_package_filter_include_glob() {
        let dirs = TestDirs::new();
        let package_dir = dirs.root.join("pkg");
        write_file(&package_dir.join("extensions/a.ts"), "");
        write_file(&package_dir.join("extensions/b.ts"), "");
        let source = lexical_relative(&dirs.agent_dir, &package_dir)
            .to_string_lossy()
            .into_owned();

        let resolved = resolve_filtered_user_package(
            &dirs,
            PackageSourceFilter {
                source,
                extensions: Some(vec!["extensions/a.ts".to_string()]),
                ..PackageSourceFilter::default()
            },
        );
        let enabled = |name: &str| {
            resolved
                .extensions
                .iter()
                .find(|r| r.path.ends_with(name))
                .map(|r| r.enabled)
        };
        assert_eq!(enabled("extensions/a.ts"), Some(true));
        assert_eq!(enabled("extensions/b.ts"), Some(false));
    }

    #[test]
    fn test_package_filter_force_include_and_force_exclude() {
        let dirs = TestDirs::new();
        let package_dir = dirs.root.join("pkg");
        write_file(&package_dir.join("extensions/keep.ts"), "");
        write_file(&package_dir.join("extensions/drop.ts"), "");
        let source = lexical_relative(&dirs.agent_dir, &package_dir)
            .to_string_lossy()
            .into_owned();

        // `!` excludes everything, `+` force-includes one file back.
        let resolved = resolve_filtered_user_package(
            &dirs,
            PackageSourceFilter {
                source: source.clone(),
                extensions: Some(vec![
                    "!extensions/*".to_string(),
                    "+extensions/keep.ts".to_string(),
                ]),
                ..PackageSourceFilter::default()
            },
        );
        let enabled = |resolved: &ResolvedPackagePaths, name: &str| {
            resolved
                .extensions
                .iter()
                .find(|r| r.path.ends_with(name))
                .map(|r| r.enabled)
        };
        assert_eq!(enabled(&resolved, "extensions/keep.ts"), Some(true));
        assert_eq!(enabled(&resolved, "extensions/drop.ts"), Some(false));

        // `-` force-excludes even from a default-enabled set.
        let resolved = resolve_filtered_user_package(
            &dirs,
            PackageSourceFilter {
                source,
                extensions: Some(vec!["-extensions/drop.ts".to_string()]),
                ..PackageSourceFilter::default()
            },
        );
        assert_eq!(enabled(&resolved, "extensions/keep.ts"), Some(true));
        assert_eq!(enabled(&resolved, "extensions/drop.ts"), Some(false));
    }

    #[test]
    fn test_package_filter_skill_parent_dir_pattern() {
        let dirs = TestDirs::new();
        let package_dir = dirs.root.join("pkg");
        write_file(&package_dir.join("skills/foo/SKILL.md"), "# Foo\n");
        write_file(&package_dir.join("skills/bar/SKILL.md"), "# Bar\n");
        let source = lexical_relative(&dirs.agent_dir, &package_dir)
            .to_string_lossy()
            .into_owned();

        let resolved = resolve_filtered_user_package(
            &dirs,
            PackageSourceFilter {
                source,
                skills: Some(vec!["!foo".to_string()]),
                ..PackageSourceFilter::default()
            },
        );
        let enabled = |name: &str| {
            resolved
                .skills
                .iter()
                .find(|r| r.path.ends_with(name))
                .map(|r| r.enabled)
        };
        assert_eq!(enabled("skills/foo/SKILL.md"), Some(false));
        assert_eq!(enabled("skills/bar/SKILL.md"), Some(true));
    }

    #[test]
    fn test_resolve_autoload_disabled_project_delta_over_user_package() {
        let dirs = TestDirs::new();
        let package_dir = dirs.agent_dir.join("npm/node_modules/pi-tools");
        write_file(
            &package_dir.join("package.json"),
            r#"{"name": "pi-tools", "version": "1.0.0"}"#,
        );
        write_file(&package_dir.join("extensions/foo.ts"), "");
        write_file(&package_dir.join("extensions/bar.ts"), "");
        let runner = FakeRunner::new(|_| Err("unexpected install".to_string()));
        let mut manager = test_manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source("npm:pi-tools".to_string())]);
        manager
            .settings_manager
            .set_project_packages(vec![PackageSource::Filtered(PackageSourceFilter {
                source: "npm:pi-tools".to_string(),
                autoload: Some(false),
                extensions: Some(vec!["-extensions/foo.ts".to_string()]),
                ..PackageSourceFilter::default()
            })])
            .unwrap();

        let resolved = manager.resolve(None).unwrap();
        assert!(runner.calls().is_empty(), "no install expected");
        let state = |name: &str| {
            resolved
                .extensions
                .iter()
                .find(|r| r.path.ends_with(name))
                .map(|r| (r.enabled, r.scope))
        };
        assert_eq!(
            state("extensions/foo.ts"),
            Some((false, SourceScope::Project))
        );
        assert_eq!(state("extensions/bar.ts"), Some((true, SourceScope::User)));
    }

    #[test]
    fn test_resolve_autoload_disabled_positive_only_without_user_package() {
        let dirs = TestDirs::new();
        let package_dir = dirs.cwd.join("positive-only-pkg");
        write_file(&package_dir.join("extensions/foo.ts"), "");
        write_file(&package_dir.join("extensions/bar.ts"), "");
        write_file(&package_dir.join("skills/foo/SKILL.md"), "# Foo\n");
        let runner = FakeRunner::ok();
        let mut manager = test_manager(&dirs, runner);
        manager
            .settings_manager
            .set_project_packages(vec![PackageSource::Filtered(PackageSourceFilter {
                source: "../positive-only-pkg".to_string(),
                autoload: Some(false),
                extensions: Some(vec!["+extensions/foo.ts".to_string()]),
                ..PackageSourceFilter::default()
            })])
            .unwrap();

        let resolved = manager.resolve(None).unwrap();
        assert_eq!(
            resolved
                .extensions
                .iter()
                .map(|r| &r.path)
                .collect::<Vec<_>>(),
            vec![&package_dir.join("extensions/foo.ts")]
        );
        assert!(resolved.skills.is_empty());
    }

    #[test]
    fn test_dedupe_project_wins_over_user_for_same_identity() {
        let dirs = TestDirs::new();
        let user_pkg = dirs.root.join("same-pkg");
        make_local_package(&user_pkg);
        let runner = FakeRunner::ok();
        let mut manager = test_manager(&dirs, runner);
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source(
                lexical_relative(&dirs.agent_dir, &user_pkg)
                    .to_string_lossy()
                    .into_owned(),
            )]);
        manager
            .settings_manager
            .set_project_packages(vec![PackageSource::Source(
                lexical_relative(&crate::config::get_project_config_dir(&dirs.cwd), &user_pkg)
                    .to_string_lossy()
                    .into_owned(),
            )])
            .unwrap();

        let resolved = manager.resolve(None).unwrap();
        // One entry per resource, owned by the project scope.
        assert_eq!(resolved.extensions.len(), 1);
        assert_eq!(resolved.extensions[0].scope, SourceScope::Project);
    }

    #[test]
    fn test_installed_npm_package_resolves_without_install() {
        let dirs = TestDirs::new();
        let package_dir = dirs.agent_dir.join("npm/node_modules/left-pad");
        write_file(
            &package_dir.join("package.json"),
            r#"{"name": "left-pad", "version": "1.0.0"}"#,
        );
        write_file(&package_dir.join("extensions/index.ts"), "");
        let runner = FakeRunner::new(|_| Err("unexpected install".to_string()));
        let mut manager = test_manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source(
                "npm:left-pad@^1.0.0".to_string(),
            )]);

        let resolved = manager.resolve(None).unwrap();
        assert_eq!(
            resolved
                .extensions
                .iter()
                .map(|r| &r.path)
                .collect::<Vec<_>>(),
            vec![&package_dir.join("extensions/index.ts")]
        );
        assert!(runner.calls().is_empty(), "range satisfied by 1.0.0");
    }

    #[test]
    fn test_installed_npm_package_outside_range_reinstalls() {
        let dirs = TestDirs::new();
        let package_dir = dirs.agent_dir.join("npm/node_modules/left-pad");
        write_file(
            &package_dir.join("package.json"),
            r#"{"name": "left-pad", "version": "0.9.0"}"#,
        );
        write_file(&package_dir.join("extensions/index.ts"), "");
        let runner = FakeRunner::ok();
        let mut manager = test_manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source(
                "npm:left-pad@^1.0.0".to_string(),
            )]);

        manager.resolve(None).unwrap();
        let install_calls = runner.find_calls("npm", "install");
        assert_eq!(install_calls.len(), 1, "0.9.0 does not satisfy ^1.0.0");
    }

    // -----------------------------------------------------------------
    // npm version queries (maxSatisfying / update checks)
    // -----------------------------------------------------------------

    #[test]
    fn test_get_latest_npm_version_max_satisfying() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::new(|request| {
            assert_eq!(request.command, "npm");
            assert_eq!(
                request.args,
                vec!["view", "left-pad@^1.0.0", "version", "--json"]
            );
            assert_eq!(request.timeout, Some(NETWORK_TIMEOUT));
            Ok(r#"["0.9.0", "1.2.0", "1.9.1", "2.0.0"]"#.to_string())
        });
        let manager = test_manager(&dirs, runner);
        let ParsedSource::Npm(npm) = parse_source("npm:left-pad@^1.0.0") else {
            panic!("expected npm source");
        };
        let version = manager
            .get_latest_npm_version(&npm.spec, npm.range.as_deref())
            .unwrap();
        assert_eq!(version, "1.9.1");
    }

    #[test]
    fn test_get_latest_npm_version_without_range_takes_highest() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::new(|_| Ok(r#"["0.9.0", "1.2.0", "2.0.0"]"#.to_string()));
        let manager = test_manager(&dirs, runner);
        let version = manager.get_latest_npm_version("left-pad", None).unwrap();
        assert_eq!(version, "2.0.0");
    }

    #[test]
    fn test_should_update_npm_source_compares_versions() {
        let dirs = TestDirs::new();
        let package_dir = dirs.agent_dir.join("npm/node_modules/left-pad");
        write_file(
            &package_dir.join("package.json"),
            r#"{"name": "left-pad", "version": "1.0.0"}"#,
        );
        let runner = FakeRunner::new(|_| Ok(r#"["1.0.0", "1.1.0"]"#.to_string()));
        let manager = test_manager(&dirs, runner);
        let ParsedSource::Npm(npm) = parse_source("npm:left-pad") else {
            panic!("expected npm source");
        };
        assert!(manager
            .should_update_npm_source(&npm, SourceScope::User)
            .unwrap());
    }

    #[test]
    fn test_semver_range_translation() {
        assert!(npm_satisfies("1.2.3", "^1.0.0"));
        assert!(!npm_satisfies("2.0.0", "^1.0.0"));
        assert!(npm_satisfies("1.2.3", "~1.2.0"));
        assert!(!npm_satisfies("1.3.0", "~1.2.0"));
        assert!(npm_satisfies("1.2.3", "1.2.3"));
        assert!(npm_satisfies("1.2.3", "1.x"));
        assert!(npm_satisfies("1.2.3", ">=1.0.0 <2.0.0"));
        assert!(npm_satisfies("2.0.0", "^1.0.0 || ^2.0.0"));
        assert!(npm_satisfies("1.5.0", "1.2.3 - 2.0.0"));
        assert!(npm_satisfies("1.2.0", "1.2"));
        assert!(npm_satisfies("1.2.3", "*"));
        assert!(is_exact_npm_version(Some("v1.2.3")));
        assert!(!is_exact_npm_version(Some("^1.2.3")));
        assert!(!is_exact_npm_version(None));
    }

    // -----------------------------------------------------------------
    // listConfiguredPackages
    // -----------------------------------------------------------------

    #[test]
    fn test_list_configured_packages_groups_and_flags() {
        let dirs = TestDirs::new();
        let local_pkg = dirs.cwd.join("local-pkg");
        make_local_package(&local_pkg);
        let runner = FakeRunner::ok();
        let mut manager = test_manager(&dirs, runner);
        manager.settings_manager.set_packages(vec![
            PackageSource::Source("npm:left-pad".to_string()),
            PackageSource::Filtered(PackageSourceFilter {
                source: lexical_relative(&dirs.agent_dir, &local_pkg)
                    .to_string_lossy()
                    .into_owned(),
                ..PackageSourceFilter::default()
            }),
        ]);
        manager
            .settings_manager
            .set_project_packages(vec![PackageSource::Source(
                "git:github.com/user/repo".to_string(),
            )])
            .unwrap();

        let packages = manager.list_configured_packages();
        assert_eq!(packages.len(), 3);
        assert_eq!(packages[0].source, "npm:left-pad");
        assert_eq!(packages[0].scope, SourceScope::User);
        assert!(!packages[0].filtered);
        // Not installed and no legacy global install: no path.
        assert_eq!(packages[0].installed_path, None);
        assert!(packages[1].filtered);
        assert_eq!(packages[1].installed_path, Some(local_pkg));
        assert_eq!(packages[2].scope, SourceScope::Project);
        assert_eq!(packages[2].source, "git:github.com/user/repo");
        assert_eq!(packages[2].installed_path, None);
    }

    #[test]
    fn test_list_configured_packages_skips_untrusted_project() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::ok();
        let mut manager = test_manager_with(&dirs, runner, false, false);
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source("npm:left-pad".to_string())]);

        let packages = manager.list_configured_packages();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].scope, SourceScope::User);
    }
}

#[cfg(test)]
mod update_tests {
    //! Port of the update-orchestration intent of
    //! `packages/coding-agent/test/package-manager.test.ts` (T14-W3):
    //! `update` / `updateConfiguredSources` / `checkForAvailableUpdates` /
    //! `buildNoMatchingPackageMessage` and the full `resolve()` (package +
    //! top-level + auto-discovered) backing `pir config`.

    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirs {
        root: PathBuf,
        cwd: PathBuf,
        agent_dir: PathBuf,
    }

    impl TestDirs {
        fn new() -> Self {
            let unique = format!(
                "pir-pm-update-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            );
            let root = std::env::temp_dir().join(unique);
            let cwd = root.join("cwd");
            let agent_dir = root.join("agent");
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::create_dir_all(&agent_dir).unwrap();
            TestDirs {
                root,
                cwd,
                agent_dir,
            }
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[derive(Debug, Clone)]
    struct RecordedCall {
        command: String,
        args: Vec<String>,
    }

    type CaptureHandler = Box<dyn Fn(&CommandRequest) -> Result<String, String> + Send + Sync>;

    struct FakeRunner {
        calls: Mutex<Vec<RecordedCall>>,
        handler: CaptureHandler,
    }

    impl FakeRunner {
        fn new(
            handler: impl Fn(&CommandRequest) -> Result<String, String> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(FakeRunner {
                calls: Mutex::new(Vec::new()),
                handler: Box::new(handler),
            })
        }

        fn ok() -> Arc<Self> {
            Self::new(|_| Ok(String::new()))
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PackageCommandRunner for FakeRunner {
        fn run(&self, request: &CommandRequest) -> Result<(), String> {
            self.calls.lock().unwrap().push(RecordedCall {
                command: request.command.clone(),
                args: request.args.clone(),
            });
            (self.handler)(request).map(|_| ())
        }

        fn run_capture(&self, request: &CommandRequest) -> Result<String, String> {
            self.calls.lock().unwrap().push(RecordedCall {
                command: request.command.clone(),
                args: request.args.clone(),
            });
            (self.handler)(request)
        }
    }

    fn manager_with(
        dirs: &TestDirs,
        runner: Arc<dyn PackageCommandRunner>,
        offline: bool,
        project_trusted: bool,
    ) -> DefaultPackageManager {
        let settings_manager = SettingsManager::create(
            &dirs.cwd,
            Some(&dirs.agent_dir),
            crate::core::settings_manager::SettingsManagerCreateOptions { project_trusted },
        );
        DefaultPackageManager::with_options(PackageManagerOptions {
            cwd: dirs.cwd.clone(),
            agent_dir: dirs.agent_dir.clone(),
            settings_manager,
            runner: Some(runner),
            offline: Some(offline),
        })
    }

    fn manager(dirs: &TestDirs, runner: Arc<dyn PackageCommandRunner>) -> DefaultPackageManager {
        manager_with(dirs, runner, false, true)
    }

    fn write_installed_npm(dirs: &TestDirs, name: &str, version: &str) {
        let dir = dirs.agent_dir.join("npm/node_modules").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name": "{name}", "version": "{version}"}}"#),
        )
        .unwrap();
    }

    fn install_calls(runner: &FakeRunner) -> Vec<RecordedCall> {
        runner
            .calls()
            .into_iter()
            .filter(|call| call.args.first().map(String::as_str) == Some("install"))
            .collect()
    }

    // ---- update() (package-manager.ts:1048-1137) ----

    #[test]
    fn test_update_without_packages_is_noop() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::ok();
        let manager = manager(&dirs, runner.clone());
        manager.update(None).unwrap();
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn test_update_offline_skips_everything() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::ok();
        let mut manager = manager_with(&dirs, runner.clone(), true, true);
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source("npm:foo".to_string())]);
        manager.update(None).unwrap();
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn test_update_unversioned_npm_installs_latest() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::new(|request| {
            if request.args.first().map(String::as_str) == Some("view") {
                return Ok(r#""1.2.3""#.to_string());
            }
            Ok(String::new())
        });
        let mut manager = manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source("npm:foo".to_string())]);
        manager.update(None).unwrap();

        let installs = install_calls(&runner);
        assert_eq!(installs.len(), 1);
        assert!(installs[0].args.contains(&"foo@latest".to_string()));
        // Unversioned sources skip the version check (not installed →
        // shouldUpdate) but still consult npm view... no: not installed
        // short-circuits to true, so no `view` call is required. The
        // install must carry `--legacy-peer-deps` and the managed prefix.
        assert!(installs[0].args.contains(&"--legacy-peer-deps".to_string()));
        assert!(installs[0]
            .args
            .contains(&dirs.agent_dir.join("npm").to_string_lossy().into_owned()));
    }

    #[test]
    fn test_update_pinned_npm_is_skipped() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::ok();
        let mut manager = manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source("npm:foo@1.2.3".to_string())]);
        manager.update(None).unwrap();
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn test_update_up_to_date_npm_is_skipped() {
        let dirs = TestDirs::new();
        write_installed_npm(&dirs, "foo", "1.2.3");
        let runner = FakeRunner::new(|request| {
            if request.args.first().map(String::as_str) == Some("view") {
                return Ok(r#""1.2.3""#.to_string());
            }
            Ok(String::new())
        });
        let mut manager = manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source("npm:foo".to_string())]);
        manager.update(None).unwrap();
        assert!(install_calls(&runner).is_empty());
    }

    #[test]
    fn test_update_outdated_ranged_npm_reinstalls_the_spec() {
        let dirs = TestDirs::new();
        write_installed_npm(&dirs, "foo", "1.0.0");
        let runner = FakeRunner::new(|request| {
            if request.args.first().map(String::as_str) == Some("view") {
                // Range resolution goes through maxSatisfying.
                return Ok(r#"["1.0.0", "1.2.0"]"#.to_string());
            }
            Ok(String::new())
        });
        let mut manager = manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source("npm:foo@^1.0.0".to_string())]);
        manager.update(None).unwrap();

        let installs = install_calls(&runner);
        assert_eq!(installs.len(), 1);
        // Versioned (range) sources reinstall the configured spec, not
        // `name@latest` (package-manager.ts:1162).
        assert!(installs[0].args.contains(&"foo@^1.0.0".to_string()));
    }

    #[test]
    fn test_update_git_reconciles_existing_clone() {
        let dirs = TestDirs::new();
        let clone_dir = dirs.agent_dir.join("git/github.com/user/repo");
        std::fs::create_dir_all(&clone_dir).unwrap();
        let runner = FakeRunner::new(|request| {
            let args: Vec<&str> = request.args.iter().map(String::as_str).collect();
            match args.as_slice() {
                ["rev-parse", "--abbrev-ref", "@{upstream}"] => Ok("origin/main".to_string()),
                ["rev-parse", "@{upstream}"] => Ok("newhead".to_string()),
                ["rev-parse", "HEAD"] => Ok("oldhead".to_string()),
                _ => Ok(String::new()),
            }
        });
        let mut manager = manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source(
                "git:github.com/user/repo".to_string(),
            )]);
        manager.update(None).unwrap();

        let calls = runner.calls();
        let fetch = calls
            .iter()
            .find(|call| {
                call.command == "git" && call.args.first().map(String::as_str) == Some("fetch")
            })
            .expect("git fetch");
        assert!(fetch
            .args
            .contains(&"+refs/heads/main:refs/remotes/origin/main".to_string()));
        // HEAD moved → hard reset + clean + dependency install
        // (ensure_git_ref, package-manager.ts:1863-1889).
        assert!(calls
            .iter()
            .any(|call| call.args.first().map(String::as_str) == Some("reset")));
        assert!(calls
            .iter()
            .any(|call| call.args.first().map(String::as_str) == Some("clean")));
    }

    #[test]
    fn test_update_pinned_git_ref_still_reconciles() {
        // Pinned git refs are configured checkout targets: update
        // reconciles the clone (`fetch origin <ref>` + reset to
        // FETCH_HEAD) instead of skipping (package-manager.ts:1096-1098).
        let dirs = TestDirs::new();
        let clone_dir = dirs.agent_dir.join("git/github.com/user/repo");
        std::fs::create_dir_all(&clone_dir).unwrap();
        let runner = FakeRunner::new(|request| {
            let args: Vec<&str> = request.args.iter().map(String::as_str).collect();
            match args.as_slice() {
                ["rev-parse", "FETCH_HEAD"] => Ok("b".repeat(40)),
                ["rev-parse", "HEAD"] => Ok("a".repeat(40)),
                _ => Ok(String::new()),
            }
        });
        let mut manager = manager(&dirs, runner.clone());
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source(
                "git:github.com/user/repo#v1.0".to_string(),
            )]);
        manager.update(None).unwrap();

        let calls = runner.calls();
        let fetch = calls
            .iter()
            .find(|call| {
                call.command == "git" && call.args.first().map(String::as_str) == Some("fetch")
            })
            .expect("git fetch");
        assert_eq!(
            fetch.args,
            vec![
                "fetch".to_string(),
                "origin".to_string(),
                "v1.0".to_string()
            ]
        );
        assert!(calls
            .iter()
            .any(|call| call.args.first().map(String::as_str) == Some("reset")));
    }

    #[test]
    fn test_update_with_source_matches_by_identity() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::new(|request| {
            if request.args.first().map(String::as_str) == Some("view") {
                return Ok(r#""1.2.3""#.to_string());
            }
            Ok(String::new())
        });
        let mut manager = manager(&dirs, runner.clone());
        manager.settings_manager.set_packages(vec![
            PackageSource::Source("npm:foo".to_string()),
            PackageSource::Source("npm:bar".to_string()),
        ]);
        manager.update(Some("npm:foo")).unwrap();

        let installs = install_calls(&runner);
        assert_eq!(installs.len(), 1);
        assert!(installs[0].args.contains(&"foo@latest".to_string()));
        assert!(!installs[0].args.iter().any(|arg| arg.contains("bar")));
    }

    #[test]
    fn test_update_no_matching_package_message_and_suggestions() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::ok();
        let mut manager = manager(&dirs, runner);
        manager.settings_manager.set_packages(vec![
            PackageSource::Source("npm:foo".to_string()),
            PackageSource::Source("git:github.com/user/repo#v1".to_string()),
        ]);

        let error = manager.update(Some("nope")).unwrap_err();
        assert_eq!(error, "No matching package found for nope");

        // npm name / spec shorthand suggests the configured source
        // (findSuggestedConfiguredSource, package-manager.ts:1393-1416).
        let error = manager.update(Some("foo")).unwrap_err();
        assert_eq!(
            error,
            "No matching package found for foo. Did you mean npm:foo?"
        );
        // git host/path[@ref] shorthand.
        let error = manager.update(Some("github.com/user/repo")).unwrap_err();
        assert_eq!(
            error,
            "No matching package found for github.com/user/repo. Did you mean git:github.com/user/repo#v1?"
        );
        let error = manager.update(Some("github.com/user/repo@v1")).unwrap_err();
        assert!(error.contains("Did you mean git:github.com/user/repo#v1?"));
    }

    // ---- checkForAvailableUpdates (package-manager.ts:1175-1238) ----

    #[test]
    fn test_check_for_available_updates_npm() {
        let dirs = TestDirs::new();
        write_installed_npm(&dirs, "foo", "1.0.0");
        write_installed_npm(&dirs, "bar", "2.0.0");
        let runner = FakeRunner::new(|request| {
            if request.args.first().map(String::as_str) == Some("view") {
                let spec = request.args[1].as_str();
                return match spec {
                    "foo" => Ok(r#""1.1.0""#.to_string()),
                    _ => Ok(r#""2.0.0""#.to_string()),
                };
            }
            Ok(String::new())
        });
        let mut manager = manager(&dirs, runner);
        manager.settings_manager.set_packages(vec![
            PackageSource::Source("npm:foo".to_string()),
            PackageSource::Source("npm:bar".to_string()),
            PackageSource::Source("npm:pinned@1.0.0".to_string()),
            PackageSource::Source("npm:not-installed".to_string()),
        ]);

        let updates = manager.check_for_available_updates().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].source, "npm:foo");
        assert_eq!(updates[0].display_name, "foo");
        assert_eq!(updates[0].kind, PackageUpdateKind::Npm);
        assert_eq!(updates[0].scope, SourceScope::User);
    }

    #[test]
    fn test_check_for_available_updates_git() {
        let dirs = TestDirs::new();
        let clone_dir = dirs.agent_dir.join("git/github.com/user/repo");
        std::fs::create_dir_all(&clone_dir).unwrap();
        let runner = FakeRunner::new(|request| {
            let args: Vec<&str> = request.args.iter().map(String::as_str).collect();
            match args.as_slice() {
                ["rev-parse", "HEAD"] => Ok("a".repeat(40)),
                ["rev-parse", "--abbrev-ref", "@{upstream}"] => Ok("origin/main".to_string()),
                ["ls-remote", "origin", "refs/heads/main"] => {
                    Ok(format!("{}\trefs/heads/main", "b".repeat(40)))
                }
                _ => Ok(String::new()),
            }
        });
        let mut manager = manager(&dirs, runner);
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source(
                "git:github.com/user/repo".to_string(),
            )]);

        let updates = manager.check_for_available_updates().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].display_name, "github.com/user/repo");
        assert_eq!(updates[0].kind, PackageUpdateKind::Git);
    }

    #[test]
    fn test_check_for_available_updates_offline_is_empty() {
        let dirs = TestDirs::new();
        let runner = FakeRunner::ok();
        let mut manager = manager_with(&dirs, runner.clone(), true, true);
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source("npm:foo".to_string())]);
        assert_eq!(manager.check_for_available_updates().unwrap(), vec![]);
        assert!(runner.calls().is_empty());
    }

    // ---- run_with_concurrency (package-manager.ts:1646-1668) ----

    #[test]
    fn test_run_with_concurrency_preserves_order_and_limits() {
        use std::sync::atomic::AtomicUsize;
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..16)
            .map(|index| {
                let in_flight = Arc::clone(&in_flight);
                let max_seen = Arc::clone(&max_seen);
                move || {
                    let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(index * 2)
                }
            })
            .collect();
        let results = run_with_concurrency(tasks, 4).unwrap();
        assert_eq!(results, (0..16).map(|i| i * 2).collect::<Vec<_>>());
        assert!(max_seen.load(Ordering::SeqCst) <= 4);
    }

    #[test]
    fn test_run_with_concurrency_returns_first_error() {
        let tasks: Vec<Box<dyn FnOnce() -> Result<i32, String> + Send>> = vec![
            Box::new(|| Ok(1)),
            Box::new(|| Err("boom".to_string())),
            Box::new(|| Ok(3)),
        ];
        let error = run_with_concurrency(tasks, 2).unwrap_err();
        assert_eq!(error, "boom");
    }

    // ---- resolve_all (package-manager.ts:901-953 full form) ----

    fn write_skill(dir: &Path, name: &str) -> PathBuf {
        let skill = dir.join(name);
        std::fs::create_dir_all(&skill).unwrap();
        let file = skill.join("SKILL.md");
        std::fs::write(&file, "---\nname: x\n---\n").unwrap();
        file
    }

    #[test]
    fn test_resolve_all_includes_auto_discovered_with_enabled_flags() {
        let dirs = TestDirs::new();
        let user_skill = write_skill(&dirs.agent_dir.join("skills"), "foo");
        write_skill(&dirs.cwd.join(".pir/skills"), "bar");

        // Untrusted: project auto-discovery is skipped.
        let runner = FakeRunner::ok();
        let manager = manager_with(&dirs, runner.clone(), false, false);
        let resolved = manager.resolve_all(None).unwrap();
        assert_eq!(resolved.skills.len(), 1);
        assert_eq!(resolved.skills[0].path, user_skill);
        assert_eq!(resolved.skills[0].metadata.source, "auto");
        assert_eq!(resolved.skills[0].metadata.origin, SourceOrigin::TopLevel);
        assert_eq!(resolved.skills[0].metadata.scope, SourceScope::User);
        assert_eq!(
            resolved.skills[0].metadata.base_dir.as_deref(),
            Some(dirs.agent_dir.as_path())
        );
        assert!(resolved.skills[0].enabled);

        // Trusted: the project skill joins (project scope).
        let manager = manager_with(&dirs, runner, false, true);
        let resolved = manager.resolve_all(None).unwrap();
        assert_eq!(resolved.skills.len(), 2);
        let project = resolved
            .skills
            .iter()
            .find(|entry| entry.metadata.scope == SourceScope::Project)
            .expect("project skill");
        assert!(project.path.ends_with("bar/SKILL.md"));
    }

    #[test]
    fn test_resolve_all_settings_entries_outrank_auto_and_carry_patterns() {
        let dirs = TestDirs::new();
        let user_skill = write_skill(&dirs.agent_dir.join("skills"), "foo");
        let runner = FakeRunner::ok();
        let mut manager = manager_with(&dirs, runner, false, false);
        // A plain settings entry plus a disable pattern for the same file:
        // the settings ("local") entry outranks the auto-discovered copy.
        manager
            .settings_manager
            .set_skill_paths(vec!["skills".to_string(), "-skills/foo".to_string()]);

        let resolved = manager.resolve_all(None).unwrap();
        let entries: Vec<&ResolvedResource> = resolved
            .skills
            .iter()
            .filter(|entry| entry.path == user_skill)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].metadata.source, "local");
        assert!(!entries[0].enabled);
    }

    #[test]
    fn test_resolve_all_includes_package_slice_with_metadata() {
        let dirs = TestDirs::new();
        let package_root = dirs.cwd.join("pkg");
        std::fs::create_dir_all(package_root.join("themes")).unwrap();
        std::fs::write(package_root.join("themes/nord.json"), "{}").unwrap();
        let runner = FakeRunner::ok();
        let mut manager = manager(&dirs, runner);
        // Absolute local source (a relative one resolves against the user
        // scope base dir, not the cwd).
        let source = package_root.to_string_lossy().into_owned();
        manager
            .settings_manager
            .set_packages(vec![PackageSource::Source(source.clone())]);

        let resolved = manager.resolve_all(None).unwrap();
        assert_eq!(resolved.themes.len(), 1);
        let theme = &resolved.themes[0];
        assert_eq!(theme.metadata.origin, SourceOrigin::Package);
        assert_eq!(theme.metadata.source, source);
        assert_eq!(theme.metadata.scope, SourceScope::User);
        assert_eq!(
            theme.metadata.base_dir.as_deref(),
            Some(package_root.as_path())
        );
        assert!(theme.enabled);
    }
}
