//! Rpi-specific (no upstream counterpart): extension registry / release
//! artifact channel (extension-distribution design, rpi-docs
//! `extension-distribution.md` §3 `.rpix` format, §5.2 index schema, §6
//! download mirror, §7 CLI flow).
//!
//! Two new package sources join the upstream `npm:` / `git:` / local set
//! (wired into [`crate::core::package_manager::parse_source`]):
//!
//! - `<name>` / `<name>@<range>` — registry source. The name resolves
//!   against `<registry>/api/extensions/<name>.json` (§5.2 schema), the
//!   highest non-yanked version matching `range` is picked, and the
//!   artifact sha256 is verified against the **index record** (the
//!   integrity anchor, design §8).
//! - `github:<owner>/<repo>[@<tag>]` — direct GitHub Release artifact,
//!   bypassing the index (private / unlisted extensions). Without an index
//!   anchor the integrity check degrades to the release's own
//!   `<file>.sha256` sidecar, and the install output says so (§7.1).
//!
//! Both channels download GitHub-direct first and fall back to the
//! official-site mirror `<registry>/extensions/download/…` (§6), and both
//! materialize the `.rpix` (a gzipped tar, §3.1) into the extension
//! discovery roots — `~/.rpi/agent/extensions/<name>/` or the trust-gated
//! `<cwd>/.rpi/extensions/<name>/` — via extract-to-temp → per-file
//! SHA256SUMS verification → manifest name/version check → atomic rename
//! (§3.3). Loading needs no wiring: the ext-host loader already discovers
//! those directories one level deep (`rpi_ext_host::loader`).
//!
//! Intentional differences from the upstream package manager shape:
//! - HTTP goes through the synchronous [`RegistryTransport`] seam (tests
//!   inject a fake; no network in tests). The production transport runs
//!   its reqwest call on a dedicated thread with its own current-thread
//!   tokio runtime, because the package manager is synchronous and may be
//!   invoked on a tokio worker thread (where `block_on` would panic).
//! - `github:` installs record their source string in a
//!   `.rpi-install-source` marker inside the installed directory so
//!   `remove` / `list` / `update` can find the directory without network
//!   access (the extension name is only known from the artifact itself).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::config;
use crate::core::self_update::sha256_hex;
use crate::core::version_check::rpi_user_agent;

/// Registry index / metadata requests (design §7.2 step 1).
pub const REGISTRY_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// `.rpix` artifacts can be large; same generosity as the binary
/// self-update download ([`crate::core::self_update`]).
pub const RPIX_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// GitHub Releases API base (release enumeration for the `github:`
/// channel).
pub const GITHUB_API_BASE_URL: &str = "https://api.github.com";
/// GitHub release asset download base (GitHub-direct download leg).
pub const GITHUB_DOWNLOAD_BASE_URL: &str = "https://github.com";

/// Marker file written into a `github:`-installed extension directory
/// holding the original source string (see the module docs).
pub const GITHUB_INSTALL_MARKER_FILE: &str = ".rpi-install-source";

// ---------------------------------------------------------------------------
// Source grammars (design §7.1)
// ---------------------------------------------------------------------------

/// Registry source: bare `<name>` or `<name>@<range>` (design §7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrySource {
    pub name: String,
    /// The raw range string (`*` semantics when `None`).
    pub range: Option<String>,
}

/// Direct GitHub Release source: `github:<owner>/<repo>[@<tag>]`
/// (design §7.1). `None` tag resolves to the latest release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubReleaseSource {
    pub owner: String,
    pub repo: String,
    pub tag: Option<String>,
}

/// The registry name grammar (design §5.3):
/// `^[a-z0-9][a-z0-9-]{1,63}$`.
pub fn is_registry_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 2 || bytes.len() > 64 {
        return false;
    }
    let is_name_char = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
    (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[1..].iter().all(|b| is_name_char(*b))
}

/// The registry range grammar (design §7.1): `*` (or absent), an exact
/// `x.y.z`, or a `^x.y[.z]` / `~x.y[.z]` range. Anything else is not a
/// registry range (the source string falls back to another source kind).
pub fn registry_range_req(range: &str) -> Option<semver::VersionReq> {
    let range = range.trim();
    if range.is_empty() || range == "*" {
        return Some(semver::VersionReq::STAR);
    }
    if semver::Version::parse(range).is_ok() {
        return semver::VersionReq::parse(&format!("={range}")).ok();
    }
    for prefix in ['^', '~'] {
        if let Some(rest) = range.strip_prefix(prefix) {
            let segments: Vec<&str> = rest.split('.').collect();
            let numeric = !segments.is_empty()
                && segments.len() <= 3
                && segments
                    .iter()
                    .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
            if !numeric {
                return None;
            }
            return semver::VersionReq::parse(&format!("{prefix}{rest}")).ok();
        }
    }
    None
}

/// [`registry_range_req`] validity predicate.
pub fn is_valid_registry_range(range: &str) -> bool {
    registry_range_req(range).is_some()
}

/// Parse a bare `<name>[@<range>]` registry source (pure grammar check;
/// the caller decides the precedence against existing local paths).
pub fn parse_registry_source(source: &str) -> Option<RegistrySource> {
    let trimmed = source.trim();
    let (name, range) = match trimmed.find('@') {
        Some(at) => (&trimmed[..at], Some(&trimmed[at + 1..])),
        None => (trimmed, None),
    };
    if !is_registry_name(name) {
        return None;
    }
    let range = match range {
        None => None,
        Some(range) if !range.is_empty() && is_valid_registry_range(range) => {
            Some(range.to_string())
        }
        // A bare name with an empty/invalid range tail is not a registry
        // source.
        Some(_) => return None,
    };
    Some(RegistrySource {
        name: name.to_string(),
        range,
    })
}

/// GitHub owner login: alphanumerics and single hyphens, no leading or
/// trailing hyphen, at most 39 characters.
fn is_github_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 39
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !owner.starts_with('-')
        && !owner.ends_with('-')
}

/// GitHub repository name: `[A-Za-z0-9._-]`, never `.` / `..`.
fn is_github_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo.len() <= 100
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && repo != "."
        && repo != ".."
}

/// Release tags are `v<semver>` (design §6 keeps the same strict shape on
/// the mirror proxy, so both download legs accept the tag).
fn is_valid_release_tag(tag: &str) -> bool {
    tag.strip_prefix('v')
        .is_some_and(|version| semver::Version::parse(version).is_ok())
}

/// Parse a `github:<owner>/<repo>[@<tag>]` source (design §7.1).
pub fn parse_github_release_source(source: &str) -> Option<GithubReleaseSource> {
    let rest = source.strip_prefix("github:")?.trim();
    let (path, tag) = match rest.find('@') {
        Some(at) => (&rest[..at], Some(&rest[at + 1..])),
        None => (rest, None),
    };
    let (owner, repo) = path.split_once('/')?;
    if repo.contains('/') || !is_github_owner(owner) || !is_github_repo(repo) {
        return None;
    }
    let tag = match tag {
        None => None,
        Some(tag) if is_valid_release_tag(tag) => Some(tag.to_string()),
        Some(_) => return None,
    };
    Some(GithubReleaseSource {
        owner: owner.to_string(),
        repo: repo.to_string(),
        tag,
    })
}

// ---------------------------------------------------------------------------
// Index schema (design §5.2) and version / artifact selection
// ---------------------------------------------------------------------------

/// Extension carrier kind (design §1.1): L0 native cdylib (no sandbox) or
/// L1 wasm (sandboxed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionKind {
    Native,
    Wasm,
}

impl ExtensionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ExtensionKind::Native => "native",
            ExtensionKind::Wasm => "wasm",
        }
    }

    pub fn parse(value: &str) -> Option<ExtensionKind> {
        match value {
            "native" => Some(ExtensionKind::Native),
            "wasm" => Some(ExtensionKind::Wasm),
            _ => None,
        }
    }
}

/// `GET /api/extensions/<name>.json` (design §5.2). Unknown fields are
/// ignored (additive evolution, ADR-0013).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryIndex {
    #[serde(default)]
    pub schema_version: Option<u32>,
    pub name: String,
    pub repository: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub versions: Vec<RegistryVersionEntry>,
}

/// One entry of the index `versions` matrix (design §5.2).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryVersionEntry {
    pub version: String,
    #[serde(default)]
    pub rpi_abi: Option<u32>,
    #[serde(default)]
    pub min_host_version: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub yanked: bool,
    /// Health-check marker (design §5.4): the artifact went missing.
    #[serde(default)]
    pub unavailable: bool,
    #[serde(default)]
    pub artifacts: Vec<RegistryArtifact>,
}

/// One artifact of a version entry (design §5.2): `target: null` marks the
/// universal wasm artifact.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RegistryArtifact {
    #[serde(default)]
    pub target: Option<String>,
    pub file: String,
    pub sha256: String,
    pub release: String,
    #[serde(default)]
    pub unavailable: bool,
}

/// Select the highest non-yanked version satisfying `range` (design §7.2
/// step 2; `None` range = `*`).
pub fn select_registry_version<'a>(
    index: &'a RegistryIndex,
    range: Option<&str>,
) -> Result<&'a RegistryVersionEntry, String> {
    let req = match range {
        None => None,
        Some(range) => match registry_range_req(range) {
            Some(req) => Some(req),
            None => {
                return Err(format!(
                    "Invalid version range \"{range}\" for \"{}\"",
                    index.name
                ))
            }
        },
    };
    let mut best: Option<(&RegistryVersionEntry, semver::Version)> = None;
    for entry in &index.versions {
        if entry.yanked {
            continue;
        }
        let Ok(version) = semver::Version::parse(&entry.version) else {
            continue;
        };
        if let Some(req) = &req {
            if !req.matches(&version) {
                continue;
            }
        }
        if best.as_ref().is_none_or(|(_, current)| version > *current) {
            best = Some((entry, version));
        }
    }
    best.map(|(entry, _)| entry).ok_or_else(|| {
        let available: Vec<&str> = index
            .versions
            .iter()
            .filter(|entry| !entry.yanked)
            .map(|entry| entry.version.as_str())
            .collect();
        let available = if available.is_empty() {
            "none".to_string()
        } else {
            available.join(", ")
        };
        match range {
            Some(range) => format!(
                "No version of \"{}\" matches \"{range}\" (available: {available})",
                index.name
            ),
            None => format!(
                "No installable version of \"{}\" (available: {available})",
                index.name
            ),
        }
    })
}

/// Compatibility precheck (design §7.2 step 3): `rpiAbi` must equal the
/// host ABI and `minHostVersion` must not exceed the running rpi version.
/// Absent fields impose no constraint (design §4); an unparseable
/// `minHostVersion` is ignored (the loader re-checks at load time).
pub fn precheck_version_compatibility(
    entry: &RegistryVersionEntry,
    host_abi: u32,
    host_version: &str,
) -> Result<(), String> {
    if let Some(required) = entry.rpi_abi {
        if required != host_abi {
            return Err(format!(
                "Version {} requires rpiAbi {required}, but this rpi supports ABI {host_abi}",
                entry.version
            ));
        }
    }
    if let Some(min) = &entry.min_host_version {
        if let (Ok(min), Ok(host)) = (
            semver::Version::parse(min),
            semver::Version::parse(host_version),
        ) {
            if min > host {
                return Err(format!(
                    "Version {} requires rpi ≥ {min} (current: {host})",
                    entry.version
                ));
            }
        }
    }
    Ok(())
}

/// Pick the artifact for this platform (design §7.2 step 4): native
/// extensions match the build-time target triple, wasm extensions take the
/// single `target: null` artifact.
pub fn select_artifact<'a>(
    entry: &'a RegistryVersionEntry,
    kind: ExtensionKind,
    target: Option<&str>,
) -> Result<&'a RegistryArtifact, String> {
    let wanted = match kind {
        ExtensionKind::Wasm => None,
        ExtensionKind::Native => match target {
            Some(target) => Some(target),
            None => {
                return Err(format!(
                    "Version {} is a native extension, but this build does not know its \
                     target triple, so the correct artifact cannot be determined",
                    entry.version
                ))
            }
        },
    };
    if let Some(artifact) = entry
        .artifacts
        .iter()
        .find(|artifact| artifact.target.as_deref() == wanted && !artifact.unavailable)
    {
        return Ok(artifact);
    }
    if entry
        .artifacts
        .iter()
        .any(|artifact| artifact.target.as_deref() == wanted)
    {
        return Err(format!(
            "The artifact for version {} is marked unavailable (the release may have been \
             pulled by its author)",
            entry.version
        ));
    }
    match kind {
        ExtensionKind::Wasm => Err(format!(
            "Version {} has no wasm artifact recorded in the index",
            entry.version
        )),
        ExtensionKind::Native => {
            let targets: Vec<&str> = entry
                .artifacts
                .iter()
                .filter_map(|artifact| artifact.target.as_deref())
                .collect();
            Err(format!(
                "Version {} was not published for platform {} (published: {})",
                entry.version,
                wanted.unwrap_or("<unknown>"),
                if targets.is_empty() {
                    "none".to_string()
                } else {
                    targets.join(", ")
                }
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// URLs (design §5.2 / §6)
// ---------------------------------------------------------------------------

/// `<base>/api/extensions/<name>.json` (design §7.2 step 1).
pub fn registry_index_url(registry_base: &str, name: &str) -> String {
    format!(
        "{}/api/extensions/{name}.json",
        registry_base.trim_end_matches('/')
    )
}

/// GitHub Releases API URL: `releases/tags/<tag>` or `releases/latest`.
pub fn github_release_api_url(owner: &str, repo: &str, tag: Option<&str>) -> String {
    match tag {
        Some(tag) => format!("{GITHUB_API_BASE_URL}/repos/{owner}/{repo}/releases/tags/{tag}"),
        None => format!("{GITHUB_API_BASE_URL}/repos/{owner}/{repo}/releases/latest"),
    }
}

/// GitHub-direct asset URL (design §6).
pub fn github_download_url(owner: &str, repo: &str, tag: &str, file: &str) -> String {
    format!("{GITHUB_DOWNLOAD_BASE_URL}/{owner}/{repo}/releases/download/{tag}/{file}")
}

/// Official-site mirror asset URL (design §6:
/// `<site_base>/extensions/download/<owner>/<repo>/<tag>/<file>`). The
/// site base is the registry base, mirroring the install.sh semantics.
pub fn mirror_download_url(
    registry_base: &str,
    owner: &str,
    repo: &str,
    tag: &str,
    file: &str,
) -> String {
    format!(
        "{}/extensions/download/{owner}/{repo}/{tag}/{file}",
        registry_base.trim_end_matches('/')
    )
}

// ---------------------------------------------------------------------------
// Transport seam
// ---------------------------------------------------------------------------

/// Injectable synchronous HTTP GET (mirrors the
/// [`crate::core::self_update::BinaryDownloadTransport`] pattern, but
/// synchronous because the package manager is). `Ok` on a 2xx response,
/// `Err` (with the status) otherwise. Tests inject a fake; no network in
/// tests.
pub trait RegistryTransport: Send + Sync {
    fn get(&self, url: &str, timeout: Duration) -> Result<Vec<u8>, String>;
}

/// Production transport: reqwest with rustls (the project's HTTP stack,
/// same trust model as the version endpoint). The request runs on a
/// dedicated thread with its own current-thread tokio runtime — the
/// package manager is synchronous and can be called from within a tokio
/// runtime thread (`run_app`), where `block_on` would panic.
pub struct ReqwestRegistryTransport;

impl RegistryTransport for ReqwestRegistryTransport {
    fn get(&self, url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
        let url = url.to_string();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(async move {
                let client = reqwest::Client::builder()
                    .timeout(timeout)
                    .build()
                    .map_err(|error| error.to_string())?;
                let response = client
                    .get(&url)
                    .header(reqwest::header::USER_AGENT, rpi_user_agent(config::VERSION))
                    .send()
                    .await
                    .map_err(|error| format!("request failed: {error}"))?;
                if !response.status().is_success() {
                    return Err(format!("request failed: HTTP {}", response.status()));
                }
                response
                    .bytes()
                    .await
                    .map(|bytes| bytes.to_vec())
                    .map_err(|error| format!("request failed: {error}"))
            })
        })
        .join()
        .unwrap_or_else(|_| Err("registry request worker panicked".to_string()))
    }
}

/// Download the first URL that succeeds (design §6: GitHub direct, then
/// the official-site mirror). Returns the winning URL plus the body; when
/// every URL fails the error keeps each one's failure reason (same shape
/// as the binary self-update).
pub fn download_with_fallback(
    transport: &dyn RegistryTransport,
    urls: &[String],
    timeout: Duration,
) -> Result<(String, Vec<u8>), String> {
    let mut failures: Vec<String> = Vec::new();
    for url in urls {
        match transport.get(url, timeout) {
            Ok(bytes) => return Ok((url.clone(), bytes)),
            Err(error) => failures.push(format!("{url}: {error}")),
        }
    }
    Err(format!(
        "Could not download from any source: {}",
        failures.join("; ")
    ))
}

// ---------------------------------------------------------------------------
// github: release resolution
// ---------------------------------------------------------------------------

/// The subset of a GitHub Releases API response the `github:` channel
/// consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubReleaseInfo {
    pub tag: String,
    pub assets: Vec<String>,
}

/// Parse a Releases API response (`tag_name` + asset `name`s).
pub fn parse_github_release_json(body: &[u8]) -> Result<GithubReleaseInfo, String> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| format!("invalid release response: {error}"))?;
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| "invalid release response: missing tag_name".to_string())?;
    let assets = value
        .get("assets")
        .and_then(Value::as_array)
        .map(|assets| {
            assets
                .iter()
                .filter_map(|asset| {
                    asset
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(GithubReleaseInfo {
        tag: tag.to_string(),
        assets,
    })
}

/// Split a `<name>-<version>` artifact stem: the version is the rightmost
/// `-`-separated suffix that parses as semver (kebab-case names contain
/// hyphens themselves; a prerelease tag like `1.0.0-beta.1` still splits
/// correctly).
pub fn split_name_version(stem: &str) -> Option<(String, String)> {
    for (index, ch) in stem.char_indices().rev() {
        if ch != '-' {
            continue;
        }
        let (name, version) = (&stem[..index], &stem[index + 1..]);
        if !name.is_empty() && semver::Version::parse(version).is_ok() {
            return Some((name.to_string(), version.to_string()));
        }
    }
    None
}

/// The selected `.rpix` asset of a GitHub release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubAssetSelection {
    pub file: String,
    pub name: String,
    pub version: String,
    pub kind: ExtensionKind,
}

/// Pick the `.rpix` asset of a release (design §3.2 naming): the native
/// `<name>-<version>-<target>.rpix` for this platform wins; otherwise the
/// universal wasm `<name>-<version>.rpix` (a plain semver suffix — a
/// target suffix would parse as a prerelease tag, so those are excluded).
/// A release carrying several different extensions is ambiguous and
/// rejected.
pub fn select_github_asset(
    assets: &[String],
    target: Option<&str>,
) -> Result<GithubAssetSelection, String> {
    let rpix: Vec<&str> = assets
        .iter()
        .map(String::as_str)
        .filter(|name| name.ends_with(".rpix"))
        .collect();
    if rpix.is_empty() {
        return Err("The release has no .rpix asset".to_string());
    }

    let mut native: Vec<GithubAssetSelection> = Vec::new();
    if let Some(target) = target {
        let suffix = format!("-{target}.rpix");
        for asset in &rpix {
            let Some(stem) = asset.strip_suffix(&suffix) else {
                continue;
            };
            if let Some((name, version)) = split_name_version(stem) {
                native.push(GithubAssetSelection {
                    file: asset.to_string(),
                    name,
                    version,
                    kind: ExtensionKind::Native,
                });
            }
        }
    }
    let mut wasm: Vec<GithubAssetSelection> = Vec::new();
    for asset in &rpix {
        let stem = asset.strip_suffix(".rpix").unwrap_or(asset);
        if let Some((name, version)) = split_name_version(stem) {
            let plain = semver::Version::parse(&version)
                .map(|parsed| parsed.pre.is_empty() && parsed.build.is_empty())
                .unwrap_or(false);
            if plain {
                wasm.push(GithubAssetSelection {
                    file: asset.to_string(),
                    name,
                    version,
                    kind: ExtensionKind::Wasm,
                });
            }
        }
    }

    let candidates = if !native.is_empty() { native } else { wasm };
    let mut names: Vec<&str> = candidates
        .iter()
        .map(|selection| selection.name.as_str())
        .collect();
    names.sort();
    names.dedup();
    if names.len() > 1 {
        return Err(format!(
            "The release carries several extensions ({}); install one per repository or use \
             the registry name form",
            names.join(", ")
        ));
    }
    match candidates.into_iter().next() {
        Some(selection) => Ok(selection),
        None => {
            let mut listed: Vec<&str> = rpix.to_vec();
            listed.sort_unstable();
            Err(format!(
                "The release carries no installable artifact for this platform (found: {})",
                listed.join(", ")
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// .rpix materialization (design §3.1 / §3.3)
// ---------------------------------------------------------------------------

/// The `name`/`version` pair of a package-internal `rpi-extension.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpixManifest {
    pub name: String,
    pub version: String,
    /// The raw manifest object (capabilities / author / native / wasm are
    /// read from it by the caller).
    pub raw: Value,
}

/// Join-safety for archive entry and SHA256SUMS paths: only normal
/// relative components survive (no absolute paths, no `..`).
fn sanitize_entry_path(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Extract a `.rpix` (gzipped tar, design §3.1: flat root, no wrapping
/// top-level directory) into `dest`. Only regular files and directories
/// are accepted — symlinks/hardlinks are rejected outright. Returns the
/// relative paths of the extracted regular files.
pub fn extract_rpix(archive: &[u8], dest: &Path) -> Result<Vec<PathBuf>, String> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let mut files = Vec::new();
    let entries = tar
        .entries()
        .map_err(|error| format!("could not read the .rpix archive: {error}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("corrupt .rpix archive: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("corrupt .rpix archive path: {error}"))?
            .into_owned();
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() && path.as_os_str().is_empty() {
            continue;
        }
        let Some(relative) = sanitize_entry_path(&path) else {
            return Err(format!(
                "archive entry escapes the install directory: {}",
                path.display()
            ));
        };
        let target = dest.join(&relative);
        if entry_type.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
        } else if entry_type.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            entry
                .unpack(&target)
                .map_err(|error| format!("could not extract {}: {error}", relative.display()))?;
            files.push(relative);
        } else {
            return Err(format!(
                "unsupported archive entry type for {} (only regular files are allowed)",
                relative.display()
            ));
        }
    }
    Ok(files)
}

/// Parse coreutils-format SHA256SUMS lines (`<64-hex> <space><space|*><path>`).
fn parse_sha256sums(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if line.len() < 66 || !line.as_bytes()[64].is_ascii_whitespace() {
            return Err(format!("SHA256SUMS line {} is malformed", index + 1));
        }
        let (hash, rest) = line.split_at(64);
        if !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("SHA256SUMS line {} is malformed", index + 1));
        }
        let path = rest.trim_start_matches([' ', '*']);
        if path.is_empty() {
            return Err(format!("SHA256SUMS line {} is malformed", index + 1));
        }
        entries.push((hash.to_ascii_lowercase(), path.to_string()));
    }
    Ok(entries)
}

/// Per-file verification against the package-internal `SHA256SUMS`
/// (design §3.1): every listed file must exist and match, and every
/// extracted file (except `SHA256SUMS` itself) must be listed.
pub fn verify_sha256sums(dir: &Path, files: &[PathBuf]) -> Result<(), String> {
    let sums_path = dir.join("SHA256SUMS");
    let text = std::fs::read_to_string(&sums_path)
        .map_err(|_| "the .rpix archive does not contain a SHA256SUMS file".to_string())?;
    let entries = parse_sha256sums(&text)?;
    let mut listed: HashSet<PathBuf> = HashSet::new();
    for (expected, name) in &entries {
        let Some(relative) = sanitize_entry_path(Path::new(name)) else {
            return Err(format!("SHA256SUMS lists an unsafe path: {name}"));
        };
        let bytes = std::fs::read(dir.join(&relative))
            .map_err(|_| format!("SHA256SUMS lists a missing file: {name}"))?;
        let actual = sha256_hex(&bytes);
        if actual != *expected {
            return Err(format!(
                "Checksum mismatch for {name}: expected {expected}, got {actual}"
            ));
        }
        listed.insert(relative);
    }
    for file in files {
        if file == Path::new("SHA256SUMS") {
            continue;
        }
        if !listed.contains(file) {
            return Err(format!("{} is not covered by SHA256SUMS", file.display()));
        }
    }
    Ok(())
}

/// Read and minimally validate the package-internal `rpi-extension.json`
/// (design §3.1: required; name/version must match the expectation).
pub fn read_rpix_manifest(dir: &Path) -> Result<RpixManifest, String> {
    let path = dir.join("rpi-extension.json");
    let content = std::fs::read_to_string(&path)
        .map_err(|_| "the .rpix archive does not contain an rpi-extension.json".to_string())?;
    let raw: Value = serde_json::from_str(&content)
        .map_err(|error| format!("invalid rpi-extension.json: {error}"))?;
    let string_field = |key: &str| -> Result<String, String> {
        raw.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("invalid rpi-extension.json: missing \"{key}\""))
    };
    Ok(RpixManifest {
        name: string_field("name")?,
        version: string_field("version")?,
        raw,
    })
}

/// The version recorded in an installed extension directory's manifest
/// (update skip check); `None` when absent or unreadable.
pub fn installed_extension_version(dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join("rpi-extension.json")).ok()?;
    let raw: Value = serde_json::from_str(&content).ok()?;
    raw.get("version")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The `name` recorded in an installed extension directory's manifest
/// (untracked-install discovery); `None` when absent or unreadable.
pub fn installed_extension_name(dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join("rpi-extension.json")).ok()?;
    let raw: Value = serde_json::from_str(&content).ok()?;
    raw.get("name").and_then(Value::as_str).map(str::to_string)
}

/// A verified, still-temporary extraction awaiting activation.
#[derive(Debug)]
pub struct ExtractedRpix {
    /// `<extensions_root>/<name>.tmp-<pid>/`.
    pub temp_dir: PathBuf,
    pub manifest: RpixManifest,
}

impl ExtractedRpix {
    /// Best-effort cleanup of the temporary directory.
    pub fn cleanup(self) {
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

/// Design §7.2 step 7, first half: extract to `<name>.tmp-<pid>/`, verify
/// the per-file SHA256SUMS, and check the manifest name/version against
/// the expectation. Any failure removes the temporary directory.
pub fn extract_and_verify_rpix(
    archive: &[u8],
    extensions_root: &Path,
    expected_name: &str,
    expected_version: &str,
) -> Result<ExtractedRpix, String> {
    std::fs::create_dir_all(extensions_root).map_err(|e| e.to_string())?;
    let temp_dir = extensions_root.join(format!("{expected_name}.tmp-{}", std::process::id()));
    if temp_dir.exists() {
        std::fs::remove_dir_all(&temp_dir).map_err(|e| e.to_string())?;
    }
    let result = (|| -> Result<RpixManifest, String> {
        let files = extract_rpix(archive, &temp_dir)?;
        verify_sha256sums(&temp_dir, &files)?;
        let manifest = read_rpix_manifest(&temp_dir)?;
        if manifest.name != expected_name {
            return Err(format!(
                "Manifest name \"{}\" does not match \"{expected_name}\"",
                manifest.name
            ));
        }
        if manifest.version != expected_version {
            return Err(format!(
                "Manifest version \"{}\" does not match \"{expected_version}\"",
                manifest.version
            ));
        }
        Ok(manifest)
    })();
    match result {
        Ok(manifest) => Ok(ExtractedRpix { temp_dir, manifest }),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&temp_dir);
            Err(error)
        }
    }
}

/// Design §3.3, second half: atomically swap the verified temporary
/// directory into `<extensions_root>/<name>/`. A previous install is
/// renamed aside first and dropped after the swap; a mid-swap failure
/// rolls the previous install back. `marker_source` (github: channel) is
/// written as [`GITHUB_INSTALL_MARKER_FILE`] inside the directory.
pub fn activate_rpix(
    extracted: ExtractedRpix,
    extensions_root: &Path,
    name: &str,
    marker_source: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(source) = marker_source {
        std::fs::write(
            extracted.temp_dir.join(GITHUB_INSTALL_MARKER_FILE),
            format!("{source}\n"),
        )
        .map_err(|e| e.to_string())?;
    }
    let dest = extensions_root.join(name);
    let backup = extensions_root.join(format!("{name}.old-{}", std::process::id()));
    if backup.exists() {
        std::fs::remove_dir_all(&backup).map_err(|e| e.to_string())?;
    }
    let had_previous = dest.exists();
    if had_previous {
        std::fs::rename(&dest, &backup)
            .map_err(|error| format!("could not replace {}: {error}", dest.display()))?;
    }
    if let Err(error) = std::fs::rename(&extracted.temp_dir, &dest) {
        if had_previous {
            let _ = std::fs::rename(&backup, &dest);
        }
        let _ = std::fs::remove_dir_all(&extracted.temp_dir);
        return Err(format!("could not activate {}: {error}", dest.display()));
    }
    if had_previous {
        let _ = std::fs::remove_dir_all(&backup);
    }
    Ok(dest)
}

/// Extract + verify + activate in one call (registry channel, design §7.2
/// step 7). Returns the install directory.
pub fn materialize_rpix(
    archive: &[u8],
    extensions_root: &Path,
    expected_name: &str,
    expected_version: &str,
) -> Result<PathBuf, String> {
    let extracted =
        extract_and_verify_rpix(archive, extensions_root, expected_name, expected_version)?;
    activate_rpix(extracted, extensions_root, expected_name, None)
}

// ---------------------------------------------------------------------------
// Install confirmation surface (design §7.2 step 5)
// ---------------------------------------------------------------------------

/// Where the artifact sha256 came from (design §7.1 / §8): the registry
/// index pins it; the `github:` channel only has the release's own
/// sidecar, and the install output must say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityLevel {
    RegistryIndex,
    ReleaseSidecar,
}

/// The confirmation table content shown before an install (design §7.2
/// step 5: name / version / author / capabilities, plus the kind and the
/// integrity level).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionInstallInfo {
    pub name: String,
    pub version: String,
    pub author: Option<String>,
    pub homepage: Option<String>,
    pub capabilities: Vec<String>,
    pub kind: ExtensionKind,
    pub integrity: IntegrityLevel,
}

/// The default registry base URL: `RPI_REGISTRY_URL` env, else the
/// built-in `https://revpi.dev`; the literal `off` disables the registry
/// channel (`None`), sharing the ADR-0002 §8 endpoint semantics.
pub fn default_registry_base_url() -> Option<String> {
    config::endpoint_from_env(config::ENV_REGISTRY_URL, None, config::DEFAULT_REGISTRY_URL)
}

#[cfg(test)]
mod tests {
    //! extension-distribution design §3/§5.2/§7 unit tests: source
    //! grammars, range/version/artifact selection, `.rpix` extraction and
    //! SHA256SUMS verification, atomic replacement and failure cleanup.
    //! No network: the transport seam is exercised by the package-manager
    //! tests.

    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let unique = format!(
                "rpi-ext-registry-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            );
            let root = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&root).unwrap();
            TestDir(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ---- source grammars ----

    #[test]
    fn test_registry_source_grammar() {
        let parsed = parse_registry_source("subagents").unwrap();
        assert_eq!(parsed.name, "subagents");
        assert_eq!(parsed.range, None);

        let parsed = parse_registry_source("subagents@^0.2").unwrap();
        assert_eq!(parsed.range.as_deref(), Some("^0.2"));
        assert!(parse_registry_source("subagents@~1.2.3").is_some());
        assert!(parse_registry_source("subagents@1.2.3").is_some());
        assert!(parse_registry_source("subagents@*").is_some());
        assert!(parse_registry_source("a0").is_some());

        // Name grammar `^[a-z0-9][a-z0-9-]{1,63}$`: min length 2, lowercase.
        assert!(parse_registry_source("a").is_none());
        assert!(parse_registry_source("Subagents").is_none());
        assert!(parse_registry_source("sub_agents").is_none());
        assert!(parse_registry_source("-sub").is_none());
        assert!(parse_registry_source(&"a".repeat(65)).is_none());
        // Invalid ranges are not registry sources.
        assert!(parse_registry_source("subagents@>=1.0").is_none());
        assert!(parse_registry_source("subagents@1.2.x").is_none());
        assert!(parse_registry_source("subagents@").is_none());
        assert!(parse_registry_source("subagents@bogus").is_none());
        // Path-shaped inputs never match.
        assert!(parse_registry_source("./subagents").is_none());
        assert!(parse_registry_source("foo/bar").is_none());
        assert!(parse_registry_source("/abs/path").is_none());
    }

    #[test]
    fn test_github_release_source_grammar() {
        let parsed = parse_github_release_source("github:revpidev/rpi-subagents").unwrap();
        assert_eq!(parsed.owner, "revpidev");
        assert_eq!(parsed.repo, "rpi-subagents");
        assert_eq!(parsed.tag, None);

        let parsed = parse_github_release_source("github:revpidev/rpi-subagents@v0.2.0").unwrap();
        assert_eq!(parsed.tag.as_deref(), Some("v0.2.0"));

        assert!(parse_github_release_source("github:owner").is_none());
        assert!(parse_github_release_source("github:owner/repo/extra").is_none());
        assert!(parse_github_release_source("github:-owner/repo").is_none());
        assert!(parse_github_release_source("github:owner/../x").is_none());
        // Tags keep the strict `v<semver>` shape (design §6).
        assert!(parse_github_release_source("github:owner/repo@1.2.3").is_none());
        assert!(parse_github_release_source("github:owner/repo@latest").is_none());
        assert!(parse_github_release_source("github:owner/repo@").is_none());
        assert!(parse_github_release_source("npm:foo").is_none());
    }

    // ---- range / version / artifact selection ----

    fn index_fixture() -> RegistryIndex {
        let json = serde_json::json!({
            "schemaVersion": 1,
            "name": "subagents",
            "repository": "revpidev/rpi-subagents",
            "author": "rpi authors",
            "kind": "native",
            "versions": [
                {
                    "version": "0.1.0",
                    "rpiAbi": 1,
                    "capabilities": ["tools"],
                    "artifacts": [
                        {
                            "target": "x86_64-unknown-linux-musl",
                            "file": "subagents-0.1.0-x86_64-unknown-linux-musl.rpix",
                            "sha256": "aa",
                            "release": "v0.1.0"
                        }
                    ]
                },
                {
                    "version": "0.2.0",
                    "rpiAbi": 1,
                    "minHostVersion": "0.11.0",
                    "capabilities": ["tools", "commands"],
                    "yanked": true,
                    "artifacts": []
                },
                {
                    "version": "0.2.1",
                    "rpiAbi": 1,
                    "minHostVersion": "0.11.0",
                    "capabilities": ["tools", "commands"],
                    "artifacts": [
                        {
                            "target": "x86_64-unknown-linux-musl",
                            "file": "subagents-0.2.1-x86_64-unknown-linux-musl.rpix",
                            "sha256": "bb",
                            "release": "v0.2.1"
                        },
                        {
                            "target": "aarch64-apple-darwin",
                            "file": "subagents-0.2.1-aarch64-apple-darwin.rpix",
                            "sha256": "cc",
                            "release": "v0.2.1"
                        }
                    ]
                }
            ]
        });
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_select_registry_version_range_and_yank() {
        let index = index_fixture();
        // Highest non-yanked.
        assert_eq!(
            select_registry_version(&index, None).unwrap().version,
            "0.2.1"
        );
        // Range excludes 0.2.x.
        assert_eq!(
            select_registry_version(&index, Some("~0.1.0"))
                .unwrap()
                .version,
            "0.1.0"
        );
        // The yanked 0.2.0 is skipped even when it would be the max match.
        assert_eq!(
            select_registry_version(&index, Some("^0.2"))
                .unwrap()
                .version,
            "0.2.1"
        );
        // Nothing matches.
        let error = select_registry_version(&index, Some("1.0.0")).unwrap_err();
        assert!(error.contains("available: 0.1.0, 0.2.1"), "{error}");
    }

    #[test]
    fn test_precheck_abi_and_min_host_version() {
        let index = index_fixture();
        let entry = select_registry_version(&index, Some("0.2.1")).unwrap();
        assert!(precheck_version_compatibility(entry, 1, "0.11.0").is_ok());
        let error = precheck_version_compatibility(entry, 2, "0.11.0").unwrap_err();
        assert!(error.contains("rpiAbi 1"), "{error}");
        let error = precheck_version_compatibility(entry, 1, "0.10.0").unwrap_err();
        assert!(error.contains("requires rpi ≥ 0.11.0"), "{error}");
        // Absent fields impose no constraint.
        let entry = select_registry_version(&index, Some("0.1.0")).unwrap();
        assert!(precheck_version_compatibility(entry, 1, "0.1.0").is_ok());
    }

    #[test]
    fn test_select_artifact_native_target_and_wasm() {
        let index = index_fixture();
        let entry = select_registry_version(&index, Some("0.2.1")).unwrap();
        let artifact =
            select_artifact(entry, ExtensionKind::Native, Some("aarch64-apple-darwin")).unwrap();
        assert_eq!(artifact.file, "subagents-0.2.1-aarch64-apple-darwin.rpix");
        // Platform not published → error lists the published platforms.
        let error = select_artifact(entry, ExtensionKind::Native, Some("x86_64-pc-windows-msvc"))
            .unwrap_err();
        assert!(error.contains("not published for platform"), "{error}");
        assert!(error.contains("x86_64-unknown-linux-musl"), "{error}");
        // Wasm picks the target-less artifact.
        let wasm_entry = RegistryVersionEntry {
            version: "1.0.0".to_string(),
            rpi_abi: Some(1),
            min_host_version: None,
            capabilities: Vec::new(),
            yanked: false,
            unavailable: false,
            artifacts: vec![RegistryArtifact {
                target: None,
                file: "smart-1.0.0.rpix".to_string(),
                sha256: "dd".to_string(),
                release: "v1.0.0".to_string(),
                unavailable: false,
            }],
        };
        let artifact = select_artifact(
            &wasm_entry,
            ExtensionKind::Wasm,
            Some("aarch64-apple-darwin"),
        )
        .unwrap();
        assert_eq!(artifact.file, "smart-1.0.0.rpix");
        // Unknown build target cannot select a native artifact.
        assert!(select_artifact(entry, ExtensionKind::Native, None).is_err());
    }

    #[test]
    fn test_select_artifact_unavailable() {
        let entry = RegistryVersionEntry {
            version: "1.0.0".to_string(),
            rpi_abi: None,
            min_host_version: None,
            capabilities: Vec::new(),
            yanked: false,
            unavailable: false,
            artifacts: vec![RegistryArtifact {
                target: Some("x86_64-unknown-linux-musl".to_string()),
                file: "f.rpix".to_string(),
                sha256: "aa".to_string(),
                release: "v1.0.0".to_string(),
                unavailable: true,
            }],
        };
        let error = select_artifact(
            &entry,
            ExtensionKind::Native,
            Some("x86_64-unknown-linux-musl"),
        )
        .unwrap_err();
        assert!(error.contains("unavailable"), "{error}");
    }

    // ---- github: asset selection ----

    #[test]
    fn test_split_name_version_kebab_and_prerelease() {
        assert_eq!(
            split_name_version("subagents-0.2.0"),
            Some(("subagents".to_string(), "0.2.0".to_string()))
        );
        assert_eq!(
            split_name_version("my-ext-1.0.0-beta.1"),
            Some(("my-ext".to_string(), "1.0.0-beta.1".to_string()))
        );
        assert_eq!(split_name_version("noversion"), None);
        assert_eq!(split_name_version("-1.0.0"), None);
    }

    #[test]
    fn test_select_github_asset_prefers_native_target() {
        let assets = vec![
            "subagents-0.2.0.rpix".to_string(),
            "subagents-0.2.0-x86_64-unknown-linux-musl.rpix".to_string(),
            "subagents-0.2.0-aarch64-apple-darwin.rpix".to_string(),
            "subagents-0.2.0-x86_64-unknown-linux-musl.rpix.sha256".to_string(),
        ];
        let selected = select_github_asset(&assets, Some("x86_64-unknown-linux-musl")).unwrap();
        assert_eq!(
            selected.file,
            "subagents-0.2.0-x86_64-unknown-linux-musl.rpix"
        );
        assert_eq!(selected.name, "subagents");
        assert_eq!(selected.version, "0.2.0");
        assert_eq!(selected.kind, ExtensionKind::Native);
        // A platform without a native artifact falls back to the wasm one.
        let selected = select_github_asset(&assets, Some("x86_64-pc-windows-msvc")).unwrap();
        assert_eq!(selected.file, "subagents-0.2.0.rpix");
        assert_eq!(selected.kind, ExtensionKind::Wasm);
        // No build target: only the wasm artifact is installable.
        let selected = select_github_asset(&assets, None).unwrap();
        assert_eq!(selected.kind, ExtensionKind::Wasm);
    }

    #[test]
    fn test_select_github_asset_ambiguous_or_missing() {
        let error = select_github_asset(&["README.md".to_string()], Some("x")).unwrap_err();
        assert!(error.contains("no .rpix asset"), "{error}");
        // Two different extensions in one release: ambiguous.
        let assets = vec!["one-1.0.0.rpix".to_string(), "two-1.0.0.rpix".to_string()];
        let error = select_github_asset(&assets, None).unwrap_err();
        assert!(error.contains("several extensions"), "{error}");
    }

    // ---- .rpix extraction / verification / activation ----

    /// Build a `.rpix` in memory: manifest + payload files + SHA256SUMS
    /// (coreutils format), gzipped tar with a flat root (design §3.1).
    fn build_rpix(name: &str, version: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let manifest = format!(
            r#"{{"name":"{name}","version":"{version}","native":"lib{name}.so","capabilities":["tools"],"rpiAbi":1}}"#
        );
        let mut all: Vec<(String, Vec<u8>)> = vec![];
        all.push(("rpi-extension.json".to_string(), manifest.into_bytes()));
        for (path, content) in files {
            all.push((path.to_string(), content.to_vec()));
        }
        let sums = all
            .iter()
            .map(|(path, content)| format!("{}  {path}\n", sha256_hex(content)))
            .collect::<String>();
        all.push(("SHA256SUMS".to_string(), sums.into_bytes()));

        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (path, content) in &all {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, &content[..])
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn test_materialize_rpix_happy_path() {
        let dir = TestDir::new();
        let root = dir.path().join("extensions");
        let archive = build_rpix("subagents", "0.2.0", &[("libsubagents.so", b"so-bytes")]);
        let dest = materialize_rpix(&archive, &root, "subagents", "0.2.0").unwrap();
        assert_eq!(dest, root.join("subagents"));
        assert_eq!(
            std::fs::read(dest.join("libsubagents.so")).unwrap(),
            b"so-bytes"
        );
        assert!(dest.join("SHA256SUMS").is_file());
        assert!(dest.join("rpi-extension.json").is_file());
        // No temp/backup residue.
        let entries: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_materialize_rpix_replaces_previous_install() {
        let dir = TestDir::new();
        let root = dir.path().join("extensions");
        let old = build_rpix("subagents", "0.1.0", &[("libsubagents.so", b"old")]);
        materialize_rpix(&old, &root, "subagents", "0.1.0").unwrap();
        let new = build_rpix("subagents", "0.2.0", &[("libsubagents.so", b"new")]);
        materialize_rpix(&new, &root, "subagents", "0.2.0").unwrap();
        assert_eq!(
            std::fs::read(root.join("subagents/libsubagents.so")).unwrap(),
            b"new"
        );
        assert_eq!(
            installed_extension_version(&root.join("subagents")).as_deref(),
            Some("0.2.0")
        );
        let entries: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "no .old-*/.tmp-* residue");
    }

    #[test]
    fn test_materialize_rpix_name_mismatch_rejected_and_cleaned() {
        let dir = TestDir::new();
        let root = dir.path().join("extensions");
        let archive = build_rpix("other", "0.2.0", &[("libother.so", b"x")]);
        let error = materialize_rpix(&archive, &root, "subagents", "0.2.0").unwrap_err();
        assert!(error.contains("does not match"), "{error}");
        // Temp dir cleaned up, nothing installed.
        assert!(!root.join("subagents").exists());
        let entries: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
        assert!(entries.is_empty(), "residue: {entries:?}");
    }

    #[test]
    fn test_materialize_rpix_version_mismatch_rejected() {
        let dir = TestDir::new();
        let root = dir.path().join("extensions");
        let archive = build_rpix("subagents", "0.9.9", &[("libsubagents.so", b"x")]);
        let error = materialize_rpix(&archive, &root, "subagents", "0.2.0").unwrap_err();
        assert!(error.contains("0.9.9"), "{error}");
        assert!(!root.join("subagents").exists());
    }

    #[test]
    fn test_materialize_rpix_tampered_file_rejected() {
        let dir = TestDir::new();
        let root = dir.path().join("extensions");
        // A payload whose SHA256SUMS entry does not match.
        let manifest = r#"{"name":"subagents","version":"0.2.0","native":"libsubagents.so"}"#;
        let sums = format!(
            "{}  rpi-extension.json\n{}  libsubagents.so\n",
            sha256_hex(manifest.as_bytes()),
            sha256_hex(b"different")
        );
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (path, content) in [
            ("rpi-extension.json", manifest.as_bytes().to_vec()),
            ("libsubagents.so", b"good".to_vec()),
            ("SHA256SUMS", sums.into_bytes()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, path, &content[..])
                .unwrap();
        }
        let tampered = builder.into_inner().unwrap().finish().unwrap();
        let error = materialize_rpix(&tampered, &root, "subagents", "0.2.0").unwrap_err();
        assert!(error.contains("Checksum mismatch"), "{error}");
        assert!(!root.join("subagents").exists());
    }

    #[test]
    fn test_verify_sha256sums_unlisted_and_missing_files() {
        let dir = TestDir::new();
        // File not covered by SHA256SUMS.
        let manifest = r#"{"name":"subagents","version":"0.2.0"}"#;
        let sums = format!("{}  rpi-extension.json\n", sha256_hex(manifest.as_bytes()));
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (path, content) in [
            ("rpi-extension.json", manifest.as_bytes().to_vec()),
            ("extra.txt", b"unlisted".to_vec()),
            ("SHA256SUMS", sums.into_bytes()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, path, &content[..])
                .unwrap();
        }
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let error = materialize_rpix(&archive, dir.path(), "subagents", "0.2.0").unwrap_err();
        assert!(error.contains("not covered by SHA256SUMS"), "{error}");

        // SHA256SUMS missing entirely.
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "rpi-extension.json", manifest.as_bytes())
            .unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let error = materialize_rpix(&archive, dir.path(), "subagents", "0.2.0").unwrap_err();
        assert!(error.contains("SHA256SUMS"), "{error}");
    }

    #[test]
    fn test_extract_rpix_rejects_path_traversal() {
        // The tar builder itself refuses `..` in entry paths, so the
        // malicious archive is hand-rolled: a 512-byte header (name
        // `../evil`, octal size/mode/checksum) + padded payload + EOF.
        let mut header = [0u8; 512];
        header[..7].copy_from_slice(b"../evil");
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(b"00000000001\0");
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        let checksum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        let mut tar_bytes = header.to_vec();
        tar_bytes.push(b'x');
        tar_bytes.resize(512 + 512, 0);
        tar_bytes.resize(512 + 512 + 1024, 0);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        encoder.write_all(&tar_bytes).unwrap();
        let archive = encoder.finish().unwrap();
        let dir = TestDir::new();
        let error = extract_rpix(&archive, dir.path()).unwrap_err();
        assert!(error.contains("escapes the install directory"), "{error}");
    }

    #[test]
    fn test_activate_rpix_writes_marker() {
        let dir = TestDir::new();
        let root = dir.path().join("extensions");
        let archive = build_rpix("subagents", "0.2.0", &[("libsubagents.so", b"x")]);
        let extracted = extract_and_verify_rpix(&archive, &root, "subagents", "0.2.0").unwrap();
        let dest = activate_rpix(
            extracted,
            &root,
            "subagents",
            Some("github:revpidev/rpi-subagents@v0.2.0"),
        )
        .unwrap();
        let marker = std::fs::read_to_string(dest.join(GITHUB_INSTALL_MARKER_FILE)).unwrap();
        assert_eq!(marker.trim(), "github:revpidev/rpi-subagents@v0.2.0");
    }

    // ---- URL builders / release JSON ----

    #[test]
    fn test_url_builders() {
        assert_eq!(
            registry_index_url("https://revpi.dev", "subagents"),
            "https://revpi.dev/api/extensions/subagents.json"
        );
        assert_eq!(
            registry_index_url("https://mirror.test/", "subagents"),
            "https://mirror.test/api/extensions/subagents.json"
        );
        assert_eq!(
            github_release_api_url("o", "r", None),
            "https://api.github.com/repos/o/r/releases/latest"
        );
        assert_eq!(
            github_release_api_url("o", "r", Some("v1.2.3")),
            "https://api.github.com/repos/o/r/releases/tags/v1.2.3"
        );
        assert_eq!(
            github_download_url("o", "r", "v1.0.0", "f.rpix"),
            "https://github.com/o/r/releases/download/v1.0.0/f.rpix"
        );
        assert_eq!(
            mirror_download_url("https://revpi.dev", "o", "r", "v1.0.0", "f.rpix"),
            "https://revpi.dev/extensions/download/o/r/v1.0.0/f.rpix"
        );
    }

    #[test]
    fn test_parse_github_release_json() {
        let body = serde_json::json!({
            "tag_name": "v0.2.0",
            "assets": [{"name": "a.rpix"}, {"name": "a.rpix.sha256"}, {"noName": 1}]
        });
        let release = parse_github_release_json(body.to_string().as_bytes()).unwrap();
        assert_eq!(release.tag, "v0.2.0");
        assert_eq!(release.assets, vec!["a.rpix", "a.rpix.sha256"]);
        assert!(parse_github_release_json(b"{}").is_err());
        assert!(parse_github_release_json(b"not json").is_err());
    }

    #[test]
    fn test_parse_sha256sums_coreutils_format() {
        let hash = "a".repeat(64);
        let entries = parse_sha256sums(&format!("{hash}  file.txt\n{hash} *bin.so\n")).unwrap();
        assert_eq!(
            entries,
            vec![
                (hash.clone(), "file.txt".to_string()),
                (hash, "bin.so".to_string())
            ]
        );
        assert!(parse_sha256sums("garbage").is_err());
    }
}
