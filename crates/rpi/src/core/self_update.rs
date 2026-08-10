//! T18: binary self-update executor and self-uninstall (ADR-0011 §4/§5).
//!
//! No upstream counterpart: upstream Pi resolves the standalone-binary
//! install shape to "print the releases page and exit 1" (config.ts:336,
//! D-041); rpi — whose only real install shape is the GitHub Releases
//! binary (ADR-0011 §1) — replaces that outcome with a real
//! download → sha256 integrity check → atomic replace flow, and adds
//! `rpi self-uninstall` (intentional behavioral difference D-054,
//! ADR-0011 §7).
//!
//! Network and filesystem are injectable: downloads go through
//! [`BinaryDownloadTransport`] (mirroring the `LatestVersionTransport`
//! pattern), and the executor takes the executable path / target triple as
//! parameters so tests never touch the real binary or the network.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use sha2::{Digest, Sha256};

use crate::config::{
    build_target, write_install_manifest_for, InstallManifest, APP_NAME, SELF_UPDATE_DOWNLOAD_URL,
    VERSION,
};
use crate::core::session_manager::now_iso8601;
use crate::core::version_check::rpi_user_agent;

/// Base URL for release asset downloads (ADR-0011 §4): asset URLs are
/// constructed from this built-in repository constant plus the probed
/// version — never from the version endpoint, whose `packageName` field
/// keeps its D-041 parity semantics and carries no distribution meaning.
pub const RELEASE_DOWNLOAD_BASE_URL: &str = "https://github.com/revpidev/rpi/releases/download";

/// Official-site mirror base for release asset downloads (ADR-0011
/// revision): the China mirror fallback, tried after
/// [`RELEASE_DOWNLOAD_BASE_URL`] when GitHub is unreachable. The URL shape
/// is identical to GitHub's (`<base>/v<version>/<asset>`).
pub const SITE_DOWNLOAD_BASE_URL: &str = "https://revpi.dev/releases/download";

/// Binary downloads are larger than the version probe; give them their own
/// (generous) timeout.
pub const SELF_UPDATE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Release asset file name (build.yml packaging):
/// `rpi-<version>-<target>.tar.gz` on unix targets, `.zip` on Windows.
pub fn asset_file_name(version: &str, target: &str) -> String {
    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("{APP_NAME}-{version}-{target}.{extension}")
}

/// `releases/download/v<version>/<asset>` (ADR-0011 §4).
pub fn asset_url(base_url: &str, version: &str, target: &str) -> String {
    format!("{base_url}/v{version}/{}", asset_file_name(version, target))
}

/// The `.sha256` sidecar URL for an asset (build.yml emits one per asset).
pub fn sha256_sidecar_url(asset_url: &str) -> String {
    format!("{asset_url}.sha256")
}

/// Parse a `sha256sum`-format sidecar (`<hex>  <filename>`; macOS `shasum`
/// prints the same shape, build.yml). Returns the lowercase digest; `None`
/// when the sidecar is malformed.
pub fn parse_sha256_sidecar(text: &str) -> Option<String> {
    let token = text.split_whitespace().next()?;
    if token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(token.to_ascii_lowercase())
    } else {
        None
    }
}

/// Lowercase hex sha256 of `bytes` (integrity check only — see the
/// ADR-0011 security boundary: this does not authenticate the release).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract the `rpi` executable from a `.tar.gz` release asset. build.yml
/// packages the bare binary at the archive root; a nested entry named `rpi`
/// is accepted as a fallback but the root entry wins.
pub fn extract_rpi_binary(tarball: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let decoder = flate2::read::GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("could not read the release archive: {error}"))?;
    let mut nested: Option<Vec<u8>> = None;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("corrupt release archive: {error}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|error| format!("corrupt release archive path: {error}"))?
            .into_owned();
        let is_rpi = path.file_name().is_some_and(|name| name == "rpi");
        if !is_rpi {
            continue;
        }
        let mut binary = Vec::new();
        entry
            .read_to_end(&mut binary)
            .map_err(|error| format!("could not extract the rpi binary: {error}"))?;
        if path == Path::new("rpi") || path == Path::new("./rpi") {
            return Ok(binary);
        }
        nested.get_or_insert(binary);
    }
    nested.ok_or_else(|| "the release archive does not contain an rpi binary".to_string())
}

/// Why an in-place executable replace failed (ADR-0011 §4 dev notes:
/// permission vs read-only-mount guidance).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceErrorKind {
    /// EPERM/EACCES — suggest re-running with sudo.
    PermissionDenied,
    /// EROFS — the executable lives on a read-only mount.
    ReadOnlyFilesystem,
    Other,
}

/// Classify the io error of a failed temp-write/replace.
pub fn classify_replace_error(error: &std::io::Error) -> ReplaceErrorKind {
    #[cfg(unix)]
    {
        match error.raw_os_error() {
            Some(code) if code == libc::EROFS => return ReplaceErrorKind::ReadOnlyFilesystem,
            Some(code) if code == libc::EPERM || code == libc::EACCES => {
                return ReplaceErrorKind::PermissionDenied;
            }
            _ => {}
        }
    }
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => ReplaceErrorKind::PermissionDenied,
        _ => ReplaceErrorKind::Other,
    }
}

/// Manual-update instruction appended to failures: with a known target
/// triple it names the exact asset; without one it points at the releases
/// page (never guess glibc vs musl, ADR-0011 §4).
pub fn manual_download_instruction(version: &str, target: Option<&str>) -> String {
    match target {
        Some(target) => format!(
            "download {} plus its .sha256 sidecar, verify the checksum, and replace the executable with the extracted {APP_NAME} binary",
            asset_url(RELEASE_DOWNLOAD_BASE_URL, version, target),
        ),
        None => format!("download the matching binary from {SELF_UPDATE_DOWNLOAD_URL}"),
    }
}

/// Failure guidance for a failed in-place replace (ADR-0011 §4): sudo for
/// permission errors, mount explanation for read-only filesystems.
pub fn replace_failure_guidance(
    kind: ReplaceErrorKind,
    exe_path: &Path,
    version: &str,
    target: Option<&str>,
) -> String {
    let manual = manual_download_instruction(version, target);
    match kind {
        ReplaceErrorKind::PermissionDenied => format!(
            "Permission denied replacing {}. Re-run with sudo (`sudo {APP_NAME} update --self`), or {manual}.",
            exe_path.display(),
        ),
        ReplaceErrorKind::ReadOnlyFilesystem => format!(
            "{} is on a read-only filesystem and cannot be replaced in place. To update, {manual}.",
            exe_path.display(),
        ),
        ReplaceErrorKind::Other => format!("To update manually, {manual}."),
    }
}

/// Phase-1 platform gate (ADR-0011 §4): Windows cannot rename over a
/// running executable, so the binary branch degrades to instructions there.
pub fn binary_inplace_replace_supported() -> bool {
    !cfg!(windows)
}

/// The Windows phase-1 outcome for `update --self` on a binary install
/// (ADR-0011 §4): no automatic replace — point at install.ps1 / a manual
/// replace. Pure so the text is unit-tested on every platform.
pub fn windows_manual_update_instructions(version: &str, exe_path: &Path) -> String {
    let mut lines = vec![format!(
        "{APP_NAME} cannot replace the running executable on Windows."
    )];
    if let Some(target) = build_target() {
        lines.push(format!(
            "Download {} and its .sha256 sidecar, verify the checksum,",
            asset_url(RELEASE_DOWNLOAD_BASE_URL, version, target),
        ));
    } else {
        lines.push(format!(
            "Download the new binary from {SELF_UPDATE_DOWNLOAD_URL},"
        ));
    }
    lines.push(format!(
        "then exit {APP_NAME} and replace {} with the extracted {APP_NAME}.exe (or re-run install.ps1).",
        exe_path.display(),
    ));
    lines.join("\n")
}

/// Injectable binary download (upstream `fetch` analogue; mirrors
/// [`crate::core::version_check::LatestVersionTransport`]). `Err` on
/// transport failure and on non-success status.
pub trait BinaryDownloadTransport: Send + Sync {
    fn download<'a>(
        &'a self,
        url: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>>;
}

/// Production transport: reqwest with rustls (same trust model as the
/// version endpoint, ADR-0011 security boundary).
pub struct ReqwestBinaryDownloadTransport;

impl BinaryDownloadTransport for ReqwestBinaryDownloadTransport {
    fn download<'a>(
        &'a self,
        url: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .map_err(|error| error.to_string())?;
            let response = client
                .get(url)
                .header(reqwest::header::USER_AGENT, rpi_user_agent(VERSION))
                .send()
                .await
                .map_err(|error| format!("download failed: {error}"))?;
            if !response.status().is_success() {
                return Err(format!("download failed: HTTP {}", response.status()));
            }
            response
                .bytes()
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(|error| format!("download failed: {error}"))
        })
    }
}

/// One binary self-update (ADR-0011 §4). `target` is the build-time
/// injected triple ([`build_target`]); `None` aborts with manual guidance
/// instead of guessing.
pub struct BinarySelfUpdateRequest<'a> {
    pub exe_path: &'a Path,
    pub version: &'a str,
    pub target: Option<&'a str>,
}

/// Successful update details for the caller's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdateOutcome {
    /// The asset the new binary came from (also the manifest `sourceUrl`).
    pub source_url: String,
    /// The replace succeeded but the manifest could not be written.
    pub manifest_warning: Option<String>,
}

/// Write `binary` to the same-directory temp file `rpi.new.<pid>`, fsync +
/// chmod 755, then atomically rename over `exe_path` (same filesystem, so
/// the rename is atomic; ADR-0011 §4 dev notes). The temp file is removed
/// on any failure.
fn install_binary(exe_path: &Path, binary: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write;
    let dir = exe_path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} has no parent directory", exe_path.display()),
        )
    })?;
    let temp = dir.join(format!("{APP_NAME}.new.{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(binary)?;
            // fsync before the rename so a crash cannot leave a truncated
            // executable in place.
            file.sync_all()?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&temp, exe_path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Run the real binary self-update (ADR-0011 §4): download the release
/// asset and its sha256 sidecar, verify the checksum (integrity check — a
/// mismatch aborts with no residue), extract the `rpi` binary, atomically
/// replace the executable, then refresh the install manifest.
///
/// `base_urls` are tried in order (ADR-0011 revision: GitHub first, then
/// the official-site mirror for networks where GitHub is unreachable). A
/// failed DOWNLOAD moves on to the next base; an integrity failure
/// (malformed sidecar or sha256 mismatch) aborts immediately — never
/// switch sources on a failed integrity check. When every base fails the
/// error keeps each base's failure reason.
pub async fn run_binary_self_update(
    request: &BinarySelfUpdateRequest<'_>,
    transport: &dyn BinaryDownloadTransport,
    base_urls: &[String],
) -> Result<SelfUpdateOutcome, String> {
    let Some(target) = request.target else {
        return Err(format!(
            "This build does not know its target triple, so the correct release asset cannot be determined. To update, {}.",
            manual_download_instruction(request.version, None),
        ));
    };
    let asset_name = asset_file_name(request.version, target);

    let mut download_failures: Vec<String> = Vec::new();
    let mut verified: Option<(String, Vec<u8>, String)> = None;
    for base_url in base_urls {
        let candidate_url = asset_url(base_url, request.version, target);
        let sidecar_url = sha256_sidecar_url(&candidate_url);
        let tarball = match transport
            .download(&candidate_url, SELF_UPDATE_DOWNLOAD_TIMEOUT)
            .await
        {
            Ok(bytes) => bytes,
            Err(error) => {
                download_failures.push(format!("{candidate_url}: {error}"));
                continue;
            }
        };
        let sidecar = match transport
            .download(&sidecar_url, SELF_UPDATE_DOWNLOAD_TIMEOUT)
            .await
        {
            Ok(bytes) => bytes,
            Err(error) => {
                download_failures.push(format!("{sidecar_url}: {error}"));
                continue;
            }
        };
        let expected =
            parse_sha256_sidecar(&String::from_utf8_lossy(&sidecar)).ok_or_else(|| {
                format!("The sha256 sidecar for {asset_name} is malformed; aborting the update.")
            })?;
        let actual = sha256_hex(&tarball);
        if actual != expected {
            // Nothing has been written yet: the abort leaves no residue.
            return Err(format!(
                "Integrity check failed for {asset_name}: expected sha256 {expected}, got {actual}. The download was discarded."
            ));
        }
        verified = Some((candidate_url, tarball, actual));
        break;
    }
    let Some((asset_url, tarball, actual)) = verified else {
        return Err(format!(
            "Could not download {asset_name} from any source: {}",
            download_failures.join("; "),
        ));
    };

    let binary = extract_rpi_binary(&tarball)?;
    install_binary(request.exe_path, &binary).map_err(|error| {
        let kind = classify_replace_error(&error);
        format!(
            "Could not replace {}: {error}. {}",
            request.exe_path.display(),
            replace_failure_guidance(kind, request.exe_path, request.version, Some(target)),
        )
    })?;

    let manifest = InstallManifest {
        version: request.version.to_string(),
        target: target.to_string(),
        installed_at: now_iso8601(),
        source_url: asset_url.clone(),
        sha256: actual,
        install_path: request.exe_path.to_string_lossy().into_owned(),
        method: InstallManifest::METHOD_BINARY.to_string(),
    };
    let manifest_warning = write_install_manifest_for(request.exe_path, &manifest)
        .err()
        .map(|error| format!("the install manifest could not be updated: {error}"));
    Ok(SelfUpdateOutcome {
        source_url: asset_url,
        manifest_warning,
    })
}

/// Test seam / explicit context for the binary self-update branch of
/// `rpi update` (production passes `None` and the branch resolves the
/// current executable, the built-in base URLs — GitHub plus the
/// official-site mirror — and the reqwest transport).
#[derive(Clone)]
pub struct BinarySelfUpdateSeam {
    pub exe_path: PathBuf,
    /// Ordered download bases: GitHub first, official-site mirror last.
    pub base_urls: Vec<String>,
    pub transport: Arc<dyn BinaryDownloadTransport>,
}

// ---------------------------------------------------------------------------
// self-uninstall (ADR-0011 §5)
// ---------------------------------------------------------------------------

/// One `rpi self-uninstall` run. `data_dir` is the `~/.rpi` root offered
/// for deletion ([`crate::config::get_uninstall_data_dir`]). The
/// `confirm_delete_data` seam decides interactively: `Some` asks (the
/// closure's return value is the answer), `None` is the non-interactive
/// path — never ask, keep the data.
pub struct SelfUninstallRequest<'a> {
    pub exe_path: &'a Path,
    pub data_dir: &'a Path,
    pub purge: bool,
    pub confirm_delete_data: Option<&'a dyn Fn(&Path) -> bool>,
}

/// Phase-1 platform gate (ADR-0011 §5): Windows cannot delete a running
/// executable, so uninstall skips the binary there and prints manual
/// commands instead.
pub fn binary_delete_supported() -> bool {
    !cfg!(windows)
}

/// The Windows phase-1 uninstall instructions (ADR-0011 §5): manual
/// deletion commands for the binary and the manifest. Pure so the text is
/// unit-tested on every platform.
pub fn windows_manual_uninstall_instructions(exe_path: &Path) -> String {
    let manifest_path = crate::config::install_manifest_path_for(exe_path);
    format!(
        "Windows cannot delete the running {APP_NAME} executable. After this command exits, delete it manually:\n  Remove-Item \"{}\"\n  Remove-Item \"{}\"",
        exe_path.display(),
        manifest_path.display(),
    )
}

/// Data paths that survive a keep-default uninstall (ADR-0011 §5: sessions
/// / auth.json / extension directories and everything else under the data
/// root). Empty when the data root is gone (purged or never existed).
pub fn leftover_paths(data_dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// `rpi self-uninstall [--purge]` (ADR-0011 §5). Returns the exit code.
/// Order: read the manifest → delete the binary (Windows: skip + manual
/// commands) → keep/delete the data root (default keep; `--purge` deletes;
/// non-interactive never asks and keeps) → delete the manifest → report
/// leftovers and the manual removal instructions.
pub fn run_self_uninstall_in(request: &SelfUninstallRequest<'_>) -> i32 {
    let manifest_path = crate::config::install_manifest_path_for(request.exe_path);
    match crate::config::read_install_manifest_for(request.exe_path) {
        Some(manifest) => println!(
            "Install manifest: {} (version {}, method {})",
            manifest_path.display(),
            manifest.version,
            manifest.method,
        ),
        None => println!(
            "No install manifest found at {}; continuing with manual-style removal.",
            manifest_path.display(),
        ),
    }

    // 1. Binary (skipped on Windows, phase 1).
    if binary_delete_supported() {
        match std::fs::remove_file(request.exe_path) {
            Ok(()) => println!("Removed {}", request.exe_path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => println!(
                "Executable {} was not found; nothing to remove.",
                request.exe_path.display(),
            ),
            Err(error) => {
                eprintln!(
                    "Error: could not remove {}: {error}",
                    request.exe_path.display(),
                );
                return 1;
            }
        }
    } else {
        println!(
            "{}",
            windows_manual_uninstall_instructions(request.exe_path)
        );
    }

    // 2. Data root (default: keep).
    let mut deleted_data = false;
    if request.data_dir.exists() {
        let delete = request.purge
            || request
                .confirm_delete_data
                .is_some_and(|confirm| confirm(request.data_dir));
        if delete {
            match std::fs::remove_dir_all(request.data_dir) {
                Ok(()) => {
                    deleted_data = true;
                    println!("Removed data directory {}", request.data_dir.display());
                }
                Err(error) => eprintln!(
                    "Warning: could not fully remove {}: {error}",
                    request.data_dir.display(),
                ),
            }
        } else {
            println!(
                "Kept data directory {} (re-run with --purge to delete it).",
                request.data_dir.display(),
            );
        }
    }

    // 3. Manifest.
    if binary_delete_supported() {
        match std::fs::remove_file(&manifest_path) {
            Ok(()) => println!("Removed {}", manifest_path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "Warning: could not remove {}: {error}",
                manifest_path.display(),
            ),
        }
    }

    // 4. Leftovers + manual instructions.
    let leftovers = leftover_paths(request.data_dir);
    if leftovers.is_empty() {
        if deleted_data {
            println!("No rpi data left behind.");
        }
    } else {
        println!();
        println!("Left in place (sessions, auth, extensions, and other rpi data):");
        for path in &leftovers {
            println!("  {}", path.display());
        }
        println!(
            "Remove them manually with: rm -rf {}",
            request.data_dir.display()
        );
    }
    println!("{APP_NAME} has been uninstalled.");
    0
}

#[cfg(test)]
mod tests {
    //! T18 self-update / self-uninstall tests (ADR-0011 §4/§5). Zero real
    //! network: downloads use a scripted transport or a loopback fake
    //! release server (axum, same pattern as the rpi-ai OAuth mocks).

    use super::*;
    use crate::config::{install_manifest_path_for, read_install_manifest_for};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let unique = format!(
                "rpi-self-update-test-{}-{}",
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
            // A permission-denied test may leave a read-only dir behind.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Build a release asset pair (`tar.gz` bytes + `sha256sum`-format
    /// sidecar) with `binary` as the `rpi` entry.
    fn fake_release(binary: &[u8]) -> (Vec<u8>, String) {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, "rpi", binary).unwrap();
        let encoder = builder.into_inner().unwrap();
        let tarball = encoder.finish().unwrap();
        let sidecar = format!("{}  rpi-1.2.3-test-target.tar.gz\n", sha256_hex(&tarball));
        (tarball, sidecar)
    }

    /// Scripted download transport: exact-URL map, `Err` for anything else.
    struct MapTransport {
        responses: std::collections::HashMap<String, Result<Vec<u8>, String>>,
        calls: Mutex<Vec<String>>,
    }

    impl MapTransport {
        fn new(responses: std::collections::HashMap<String, Result<Vec<u8>, String>>) -> Self {
            MapTransport {
                responses,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl BinaryDownloadTransport for MapTransport {
        fn download<'a>(
            &'a self,
            url: &'a str,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<Vec<u8>, String>> {
            self.calls.lock().unwrap().push(url.to_string());
            let response = self
                .responses
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err(format!("404 for {url}")));
            Box::pin(async move { response })
        }
    }

    // ---- asset naming / sidecar parsing ----

    #[test]
    fn test_asset_file_name_gnu_musl_windows() {
        // build.yml asset naming; glibc/musl must stay distinct (ADR-0011 §3).
        assert_eq!(
            asset_file_name("0.2.0", "x86_64-unknown-linux-gnu"),
            "rpi-0.2.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_file_name("0.2.0", "x86_64-unknown-linux-musl"),
            "rpi-0.2.0-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            asset_file_name("0.2.0", "aarch64-apple-darwin"),
            "rpi-0.2.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_file_name("0.2.0", "x86_64-pc-windows-msvc"),
            "rpi-0.2.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn test_asset_and_sidecar_urls() {
        let url = asset_url(
            RELEASE_DOWNLOAD_BASE_URL,
            "1.2.3",
            "aarch64-unknown-linux-musl",
        );
        assert_eq!(
            url,
            "https://github.com/revpidev/rpi/releases/download/v1.2.3/rpi-1.2.3-aarch64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(sha256_sidecar_url(&url), format!("{url}.sha256"));
    }

    #[test]
    fn test_parse_sha256_sidecar() {
        let digest = "a".repeat(64);
        assert_eq!(
            parse_sha256_sidecar(&format!("{digest}  rpi-1.0.0-x.tar.gz\n")),
            Some(digest.clone())
        );
        // BSD `shasum` prints the same shape; uppercase digests normalize.
        let upper = "A".repeat(64);
        assert_eq!(
            parse_sha256_sidecar(&format!("{upper}  file")),
            Some("a".repeat(64))
        );
        assert_eq!(parse_sha256_sidecar(""), None);
        assert_eq!(parse_sha256_sidecar("not-a-digest  file"), None);
        assert_eq!(parse_sha256_sidecar(&"ab".repeat(20)), None);
    }

    #[test]
    fn test_sha256_hex_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    // ---- archive extraction ----

    #[test]
    fn test_extract_rpi_binary_root_entry() {
        let (tarball, _) = fake_release(b"new-rpi-binary");
        assert_eq!(extract_rpi_binary(&tarball).unwrap(), b"new-rpi-binary");
    }

    #[test]
    fn test_extract_rpi_binary_missing_entry() {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_cksum();
        builder
            .append_data(&mut header, "other", &b"foo"[..])
            .unwrap();
        let tarball = builder.into_inner().unwrap().finish().unwrap();
        let error = extract_rpi_binary(&tarball).unwrap_err();
        assert!(error.contains("does not contain an rpi binary"), "{error}");
    }

    // ---- error classification / guidance ----

    #[cfg(unix)]
    #[test]
    fn test_classify_replace_error_kinds() {
        assert_eq!(
            classify_replace_error(&std::io::Error::from_raw_os_error(libc::EROFS)),
            ReplaceErrorKind::ReadOnlyFilesystem
        );
        assert_eq!(
            classify_replace_error(&std::io::Error::from_raw_os_error(libc::EACCES)),
            ReplaceErrorKind::PermissionDenied
        );
        assert_eq!(
            classify_replace_error(&std::io::Error::from_raw_os_error(libc::EPERM)),
            ReplaceErrorKind::PermissionDenied
        );
        assert_eq!(
            classify_replace_error(&std::io::Error::from_raw_os_error(libc::ENOENT)),
            ReplaceErrorKind::Other
        );
    }

    #[test]
    fn test_replace_failure_guidance_permission_vs_readonly() {
        let exe = Path::new("/usr/local/bin/rpi");
        let target = Some("x86_64-unknown-linux-gnu");
        let permission =
            replace_failure_guidance(ReplaceErrorKind::PermissionDenied, exe, "1.2.3", target);
        assert!(
            permission.contains("sudo rpi update --self"),
            "{permission}"
        );
        assert!(
            permission.contains(
                "https://github.com/revpidev/rpi/releases/download/v1.2.3/rpi-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
            ),
            "{permission}"
        );
        let readonly =
            replace_failure_guidance(ReplaceErrorKind::ReadOnlyFilesystem, exe, "1.2.3", target);
        assert!(readonly.contains("read-only filesystem"), "{readonly}");
        assert!(!readonly.contains("sudo"), "{readonly}");
        // Without a target triple the guidance points at the releases page.
        let no_target = replace_failure_guidance(ReplaceErrorKind::Other, exe, "1.2.3", None);
        assert!(no_target.contains(SELF_UPDATE_DOWNLOAD_URL), "{no_target}");
    }

    #[test]
    fn test_windows_manual_update_instructions_text() {
        let exe = Path::new("C:\\Users\\u\\AppData\\Local\\Programs\\rpi\\rpi.exe");
        let text = windows_manual_update_instructions("1.2.3", exe);
        assert!(text.contains("cannot replace the running"), "{text}");
        assert!(text.contains("install.ps1"), "{text}");
        assert!(text.contains("rpi.exe"), "{text}");
        // Under cargo the build target is injected, so the exact asset URL
        // is named (a .zip for Windows targets).
        let target = build_target().unwrap();
        if target.contains("windows") {
            assert!(text.contains(".zip"), "{text}");
        }
    }

    // ---- the executor ----

    #[tokio::test]
    async fn test_self_update_full_chain_replaces_and_writes_manifest() {
        let dir = TestDir::new();
        let exe = dir.path().join("rpi");
        std::fs::write(&exe, b"old-binary").unwrap();
        let (tarball, sidecar) = fake_release(b"new-binary");
        let target = "x86_64-unknown-linux-gnu";
        let asset_url = asset_url("https://releases.test", "9.9.9", target);
        let transport = MapTransport::new(std::collections::HashMap::from([
            (asset_url.clone(), Ok(tarball.clone())),
            (sha256_sidecar_url(&asset_url), Ok(sidecar.into_bytes())),
        ]));

        let outcome = run_binary_self_update(
            &BinarySelfUpdateRequest {
                exe_path: &exe,
                version: "9.9.9",
                target: Some(target),
            },
            &transport,
            &["https://releases.test".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(outcome.source_url, asset_url);
        assert_eq!(outcome.manifest_warning, None);
        assert_eq!(std::fs::read(&exe).unwrap(), b"new-binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        // No temp residue in the install dir.
        let residue: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".new."))
            .collect();
        assert!(residue.is_empty(), "residue: {residue:?}");
        // Manifest: camelCase wire shape with the update details.
        let manifest = read_install_manifest_for(&exe).unwrap();
        assert_eq!(manifest.version, "9.9.9");
        assert_eq!(manifest.target, target);
        assert_eq!(manifest.method, "binary");
        assert_eq!(manifest.source_url, outcome.source_url);
        assert_eq!(manifest.sha256, sha256_hex(&tarball));
        assert_eq!(manifest.install_path, exe.to_string_lossy());
        assert!(!manifest.installed_at.is_empty());
        let raw = std::fs::read_to_string(install_manifest_path_for(&exe)).unwrap();
        assert!(raw.contains("\"installedAt\""), "{raw}");
        assert!(raw.contains("\"sourceUrl\""), "{raw}");
        assert!(raw.contains("\"installPath\""), "{raw}");
    }

    #[tokio::test]
    async fn test_self_update_sha256_mismatch_aborts_without_residue() {
        let dir = TestDir::new();
        let exe = dir.path().join("rpi");
        std::fs::write(&exe, b"old-binary").unwrap();
        let (tarball, _) = fake_release(b"new-binary");
        let target = "aarch64-unknown-linux-musl";
        let asset_url = asset_url("https://releases.test", "9.9.9", target);
        let transport = MapTransport::new(std::collections::HashMap::from([
            (asset_url.clone(), Ok(tarball)),
            (
                sha256_sidecar_url(&asset_url),
                Ok(format!("{}  {asset_url}\n", "0".repeat(64)).into_bytes()),
            ),
        ]));

        let error = run_binary_self_update(
            &BinarySelfUpdateRequest {
                exe_path: &exe,
                version: "9.9.9",
                target: Some(target),
            },
            &transport,
            &["https://releases.test".to_string()],
        )
        .await
        .unwrap_err();
        assert!(error.contains("Integrity check failed"), "{error}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old-binary");
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "only the untouched executable remains");
        assert!(!install_manifest_path_for(&exe).exists());
    }

    /// Mirror fallback (ADR-0011 revision): the first base 404s and the
    /// official-site mirror base serves the asset — the update succeeds
    /// and the outcome records the mirror URL.
    #[tokio::test]
    async fn test_self_update_falls_back_to_mirror_base() {
        let dir = TestDir::new();
        let exe = dir.path().join("rpi");
        std::fs::write(&exe, b"old-binary").unwrap();
        let (tarball, sidecar) = fake_release(b"mirror-binary");
        let target = "x86_64-unknown-linux-gnu";
        let github_url = asset_url("https://github.test", "9.9.9", target);
        let mirror_url = asset_url("https://site.test", "9.9.9", target);
        let transport = MapTransport::new(std::collections::HashMap::from([
            (mirror_url.clone(), Ok(tarball)),
            (sha256_sidecar_url(&mirror_url), Ok(sidecar.into_bytes())),
        ]));

        let outcome = run_binary_self_update(
            &BinarySelfUpdateRequest {
                exe_path: &exe,
                version: "9.9.9",
                target: Some(target),
            },
            &transport,
            &[
                "https://github.test".to_string(),
                "https://site.test".to_string(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(outcome.source_url, mirror_url);
        assert_eq!(std::fs::read(&exe).unwrap(), b"mirror-binary");
        // GitHub was tried first (and 404'd) before the mirror.
        let calls = transport.calls.lock().unwrap();
        assert_eq!(
            calls.first().map(String::as_str),
            Some(github_url.as_str()),
            "calls: {calls:?}"
        );
        let manifest = read_install_manifest_for(&exe).unwrap();
        assert_eq!(manifest.source_url, mirror_url);
    }

    /// Every base failing reports each base's failure reason.
    #[tokio::test]
    async fn test_self_update_all_bases_fail_reports_each_reason() {
        let dir = TestDir::new();
        let exe = dir.path().join("rpi");
        std::fs::write(&exe, b"old-binary").unwrap();
        let target = "x86_64-unknown-linux-gnu";
        let transport = MapTransport::new(std::collections::HashMap::new());

        let error = run_binary_self_update(
            &BinarySelfUpdateRequest {
                exe_path: &exe,
                version: "9.9.9",
                target: Some(target),
            },
            &transport,
            &[
                "https://github.test".to_string(),
                "https://site.test".to_string(),
            ],
        )
        .await
        .unwrap_err();
        assert!(error.contains("Could not download"), "{error}");
        assert!(error.contains("https://github.test"), "{error}");
        assert!(error.contains("https://site.test"), "{error}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old-binary");
        assert!(!install_manifest_path_for(&exe).exists());
    }

    /// A failed integrity check never falls back to the next base.
    #[tokio::test]
    async fn test_self_update_checksum_failure_does_not_try_mirror() {
        let dir = TestDir::new();
        let exe = dir.path().join("rpi");
        std::fs::write(&exe, b"old-binary").unwrap();
        let (tarball, _) = fake_release(b"new-binary");
        let target = "x86_64-unknown-linux-gnu";
        let github_url = asset_url("https://github.test", "9.9.9", target);
        let mirror_url = asset_url("https://site.test", "9.9.9", target);
        let (mirror_tarball, mirror_sidecar) = fake_release(b"mirror-binary");
        let transport = MapTransport::new(std::collections::HashMap::from([
            (github_url.clone(), Ok(tarball)),
            (
                sha256_sidecar_url(&github_url),
                Ok(format!("{}  {github_url}\n", "0".repeat(64)).into_bytes()),
            ),
            (mirror_url.clone(), Ok(mirror_tarball)),
            (
                sha256_sidecar_url(&mirror_url),
                Ok(mirror_sidecar.into_bytes()),
            ),
        ]));

        let error = run_binary_self_update(
            &BinarySelfUpdateRequest {
                exe_path: &exe,
                version: "9.9.9",
                target: Some(target),
            },
            &transport,
            &[
                "https://github.test".to_string(),
                "https://site.test".to_string(),
            ],
        )
        .await
        .unwrap_err();
        assert!(error.contains("Integrity check failed"), "{error}");
        // The mirror was never requested.
        let calls = transport.calls.lock().unwrap();
        assert!(
            !calls.iter().any(|url| url.starts_with("https://site.test")),
            "calls: {calls:?}"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old-binary");
        assert!(!install_manifest_path_for(&exe).exists());
    }

    #[tokio::test]
    async fn test_self_update_without_target_triple_gives_manual_guidance() {
        let dir = TestDir::new();
        let exe = dir.path().join("rpi");
        std::fs::write(&exe, b"old-binary").unwrap();
        let transport = MapTransport::new(std::collections::HashMap::new());
        let error = run_binary_self_update(
            &BinarySelfUpdateRequest {
                exe_path: &exe,
                version: "9.9.9",
                target: None,
            },
            &transport,
            &["https://releases.test".to_string()],
        )
        .await
        .unwrap_err();
        assert!(error.contains("target triple"), "{error}");
        assert!(error.contains(SELF_UPDATE_DOWNLOAD_URL), "{error}");
        // No download was attempted.
        assert!(transport.calls.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_self_update_permission_denied_guides_sudo() {
        if unsafe { libc::geteuid() } == 0 {
            // Root ignores mode bits; the guidance mapping itself is covered
            // by test_replace_failure_guidance_permission_vs_readonly.
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new();
        let exe = dir.path().join("rpi");
        std::fs::write(&exe, b"old-binary").unwrap();
        let (tarball, sidecar) = fake_release(b"new-binary");
        let target = "x86_64-unknown-linux-gnu";
        let asset_url = asset_url("https://releases.test", "9.9.9", target);
        let transport = MapTransport::new(std::collections::HashMap::from([
            (asset_url.clone(), Ok(tarball)),
            (sha256_sidecar_url(&asset_url), Ok(sidecar.into_bytes())),
        ]));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        let error = run_binary_self_update(
            &BinarySelfUpdateRequest {
                exe_path: &exe,
                version: "9.9.9",
                target: Some(target),
            },
            &transport,
            &["https://releases.test".to_string()],
        )
        .await
        .unwrap_err();
        assert!(error.contains("sudo rpi update --self"), "{error}");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"old-binary");
        let residue: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".new."))
            .collect();
        assert!(residue.is_empty(), "residue: {residue:?}");
    }

    /// End-to-end through the production reqwest transport against a
    /// loopback fake release server (axum; no real network).
    #[tokio::test]
    async fn test_self_update_e2e_against_loopback_release_server() {
        let dir = TestDir::new();
        let exe = dir.path().join("rpi");
        std::fs::write(&exe, b"old-binary").unwrap();
        let (tarball, sidecar) = fake_release(b"loopback-new-binary");
        let target = build_target().unwrap().to_string();
        let asset_path = format!(
            "/releases/download/v2.0.0/{}",
            asset_file_name("2.0.0", &target)
        );
        let responses = std::collections::HashMap::from([
            (asset_path.clone(), tarball),
            (format!("{asset_path}.sha256"), sidecar.into_bytes()),
        ]);

        async fn serve(
            axum::extract::State(responses): axum::extract::State<
                std::collections::HashMap<String, Vec<u8>>,
            >,
            request: axum::http::Request<axum::body::Body>,
        ) -> Result<Vec<u8>, axum::http::StatusCode> {
            responses
                .get(request.uri().path())
                .cloned()
                .ok_or(axum::http::StatusCode::NOT_FOUND)
        }
        let app = axum::Router::new().fallback(serve).with_state(responses);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });

        let base_url = format!("http://{addr}/releases/download");
        let outcome = run_binary_self_update(
            &BinarySelfUpdateRequest {
                exe_path: &exe,
                version: "2.0.0",
                target: Some(&target),
            },
            &ReqwestBinaryDownloadTransport,
            std::slice::from_ref(&base_url),
        )
        .await
        .unwrap();
        assert!(outcome.source_url.starts_with(&base_url));
        assert_eq!(std::fs::read(&exe).unwrap(), b"loopback-new-binary");
        let manifest = read_install_manifest_for(&exe).unwrap();
        assert_eq!(manifest.version, "2.0.0");
        assert_eq!(manifest.target, target);
        let _ = tx.send(());
    }

    // ---- self-uninstall ----

    struct UninstallFixture {
        _dir: TestDir,
        exe: PathBuf,
        data_dir: PathBuf,
    }

    impl UninstallFixture {
        fn new() -> Self {
            let dir = TestDir::new();
            let bin_dir = dir.path().join("bin");
            std::fs::create_dir_all(&bin_dir).unwrap();
            let exe = bin_dir.join("rpi");
            std::fs::write(&exe, b"installed-binary").unwrap();
            let data_dir = dir.path().join("home").join(".rpi");
            std::fs::create_dir_all(data_dir.join("agent/sessions")).unwrap();
            std::fs::write(data_dir.join("agent/auth.json"), "{}").unwrap();
            UninstallFixture {
                _dir: dir,
                exe,
                data_dir,
            }
        }

        fn write_manifest(&self) {
            crate::config::write_install_manifest_for(
                &self.exe,
                &InstallManifest {
                    version: "1.0.0".to_string(),
                    target: "x86_64-unknown-linux-gnu".to_string(),
                    installed_at: "2026-08-10T00:00:00.000Z".to_string(),
                    source_url: "https://example.test/asset".to_string(),
                    sha256: "ab".to_string(),
                    install_path: self.exe.to_string_lossy().into_owned(),
                    method: "binary".to_string(),
                },
            )
            .unwrap();
        }

        fn run(&self, purge: bool, confirm: Option<&dyn Fn(&Path) -> bool>) -> i32 {
            run_self_uninstall_in(&SelfUninstallRequest {
                exe_path: &self.exe,
                data_dir: &self.data_dir,
                purge,
                confirm_delete_data: confirm,
            })
        }
    }

    #[test]
    fn test_self_uninstall_keeps_data_non_interactive() {
        let fixture = UninstallFixture::new();
        fixture.write_manifest();
        // `confirm: None` is the non-interactive path: never ask, keep.
        let code = fixture.run(false, None);
        assert_eq!(code, 0);
        assert!(!fixture.exe.exists(), "binary removed");
        assert!(!install_manifest_path_for(&fixture.exe).exists());
        assert!(fixture.data_dir.join("agent/sessions").exists());
        assert!(fixture.data_dir.join("agent/auth.json").exists());
        // Leftovers report sees the kept data.
        let leftovers = leftover_paths(&fixture.data_dir);
        assert_eq!(leftovers, vec![fixture.data_dir.join("agent")]);
    }

    #[test]
    fn test_self_uninstall_purge_removes_data() {
        let fixture = UninstallFixture::new();
        fixture.write_manifest();
        let code = fixture.run(true, None);
        assert_eq!(code, 0);
        assert!(!fixture.exe.exists());
        assert!(!fixture.data_dir.exists(), "--purge deletes the data root");
        assert!(leftover_paths(&fixture.data_dir).is_empty());
    }

    #[test]
    fn test_self_uninstall_interactive_confirm_decides() {
        // Answering "yes" deletes the data without --purge.
        let fixture = UninstallFixture::new();
        let code = fixture.run(false, Some(&|_| true));
        assert_eq!(code, 0);
        assert!(!fixture.data_dir.exists());
        // Answering "no" (or just Enter) keeps it.
        let fixture = UninstallFixture::new();
        let code = fixture.run(false, Some(&|_| false));
        assert_eq!(code, 0);
        assert!(fixture.data_dir.exists());
    }

    #[test]
    fn test_self_uninstall_without_manifest_still_removes_binary() {
        let fixture = UninstallFixture::new();
        let code = fixture.run(false, None);
        assert_eq!(code, 0);
        assert!(!fixture.exe.exists());
        assert!(fixture.data_dir.exists());
    }

    #[test]
    fn test_self_uninstall_without_binary_still_removes_manifest() {
        let fixture = UninstallFixture::new();
        fixture.write_manifest();
        std::fs::remove_file(&fixture.exe).unwrap();
        let code = fixture.run(false, None);
        assert_eq!(code, 0);
        assert!(!install_manifest_path_for(&fixture.exe).exists());
    }

    #[test]
    fn test_windows_manual_uninstall_instructions_text() {
        let exe = Path::new("C:\\Users\\u\\Programs\\rpi\\rpi.exe");
        let text = windows_manual_uninstall_instructions(exe);
        assert!(text.contains("cannot delete the running"), "{text}");
        assert!(text.contains("Remove-Item"), "{text}");
        assert!(text.contains("rpi.exe"), "{text}");
        assert!(text.contains("rpi.install.json"), "{text}");
    }
}
