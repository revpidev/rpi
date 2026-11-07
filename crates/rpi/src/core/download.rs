//! Shared resumable download engine (V18-12 FR-B, rpi-own).
//!
//! Self-update assets and extension `.rpix` artifacts both need the same
//! transfer semantics: a visible progress stream, bounded retries with
//! backoff, mirror fallback, and resumable `.part` files that are only
//! promoted after the complete file matches its sha256. The security
//! boundary (ADR-0011) is unchanged: sha256 is the only install credential,
//! an integrity failure aborts immediately and never switches mirrors.
//!
//! The transport is synchronous so the package manager (a synchronous
//! caller that may already be on a tokio worker) can use it directly.
//! [`ReqwestDownloadTransport`] runs its async reqwest call on a scoped
//! worker thread with a current-thread runtime, mirroring
//! [`crate::core::extension_registry::ReqwestRegistryTransport`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::VERSION;
use crate::core::version_check::rpi_user_agent;

/// Default connect budget (R3.10.7): only establishing the connection is
/// bounded; a download that keeps receiving bytes never times out.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default stall budget (R3.10.7): no bytes for this long triggers a retry.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-source attempt cap (R3.10.5).
pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;
/// Partial files older than this are discarded (R3.10.6).
pub const DEFAULT_PARTIAL_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Which artifact a transfer represents (drives user-facing messages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadAssetKind {
    Asset,
    Sidecar,
}

/// A single HTTP transfer request. Range headers are added by the engine.
#[derive(Debug, Clone)]
pub struct DownloadHttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub connect_timeout: Duration,
    pub stall_timeout: Duration,
}

/// Response head exposed before the body arrives.
#[derive(Debug, Clone, Default)]
pub struct DownloadResponseHead {
    pub status: u16,
    pub content_length: Option<u64>,
    /// `Content-Range: bytes */<total>` or `bytes <start>-<end>/<total>`.
    pub content_range_total: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Parsed `Retry-After` (429/503), when present.
    pub retry_after: Option<Duration>,
}

/// Events a transport emits while a response streams in.
#[derive(Debug)]
pub enum DownloadStreamEvent<'a> {
    Head(&'a DownloadResponseHead),
    Chunk(&'a [u8]),
}

/// Error classes (R3.10.5). Only the retryable classes trigger another
/// attempt; `Integrity` is terminal and must not switch mirrors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadErrorKind {
    Connect,
    Timeout,
    Reset,
    Http(u16),
    Io,
    Integrity,
    Policy,
}

impl DownloadErrorKind {
    pub fn is_retryable(self) -> bool {
        match self {
            DownloadErrorKind::Connect | DownloadErrorKind::Timeout | DownloadErrorKind::Reset => {
                true
            }
            DownloadErrorKind::Http(status) => {
                matches!(status, 408 | 425 | 429) || (500..600).contains(&status)
            }
            DownloadErrorKind::Io | DownloadErrorKind::Integrity | DownloadErrorKind::Policy => {
                false
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadError {
    pub kind: DownloadErrorKind,
    pub message: String,
    /// `Retry-After` (429/503), honored instead of the exponential delay.
    pub retry_after: Option<Duration>,
}

impl DownloadError {
    pub fn new(kind: DownloadErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    pub fn integrity(message: impl Into<String>) -> Self {
        Self::new(DownloadErrorKind::Integrity, message)
    }

    pub fn policy(message: impl Into<String>) -> Self {
        Self::new(DownloadErrorKind::Policy, message)
    }
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DownloadError {}

/// Transport seam: one request, streamed through `sink`. Implementations
/// must classify transport failures into [`DownloadError`]; HTTP non-2xx
/// responses may be returned as a head so the engine can apply the
/// 404/403-vs-retry rules (the default adapter reports them as `Http`).
pub trait DownloadTransport: Send + Sync {
    fn stream(
        &self,
        request: &DownloadHttpRequest,
        sink: &mut (dyn FnMut(DownloadStreamEvent<'_>) -> Result<(), DownloadError> + Send),
    ) -> Result<(), DownloadError>;
}

/// Progress/state callback consumed by the CLI renderer and test fakes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadEvent {
    Connecting {
        url: String,
        attempt: u32,
    },
    Progress {
        downloaded: u64,
        total: Option<u64>,
    },
    Retry {
        url: String,
        attempt: u32,
        reason: String,
        downloaded: u64,
        delay: Duration,
    },
    Switching {
        from: String,
        to: String,
        reason: String,
        kept_bytes: u64,
        attempt: u32,
    },
    Restarting {
        url: String,
        discarded_bytes: u64,
    },
    Verifying {
        file: PathBuf,
    },
    Installing {
        target: PathBuf,
    },
    Done {
        file: PathBuf,
        bytes: u64,
    },
    Failed {
        message: String,
    },
}

pub trait DownloadObserver: Send + Sync {
    fn on_download_event(&self, event: &DownloadEvent);
}

/// Observer for non-interactive callers/tests.
pub struct NoopDownloadObserver;

impl DownloadObserver for NoopDownloadObserver {
    fn on_download_event(&self, _event: &DownloadEvent) {}
}

/// Retry / timeout / partial policy (R3.10.5–R3.10.7).
#[derive(Debug, Clone)]
pub struct DownloadPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub connect_timeout: Duration,
    pub stall_timeout: Duration,
    pub partial_max_age: Duration,
    /// Disable ±20% jitter (tests).
    pub jitter: bool,
}

impl Default for DownloadPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
            partial_max_age: DEFAULT_PARTIAL_MAX_AGE,
            jitter: true,
        }
    }
}

impl DownloadPolicy {
    /// Deterministic policy for tests (no real sleeping, no jitter).
    pub fn for_tests() -> Self {
        Self {
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            jitter: false,
            ..Self::default()
        }
    }

    fn delay_for(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(6);
        let delay = self
            .base_delay
            .saturating_mul(1u32 << exponent)
            .min(self.max_delay);
        if !self.jitter || delay.is_zero() {
            return delay;
        }
        // ±20% jitter without an RNG dependency: derive from the clock's
        // subsecond nanos; exact distribution does not matter.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(0);
        let percent = 80 + (nanos % 41);
        delay.mul_f64(percent as f64 / 100.0)
    }
}

/// One download input.
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub url: String,
    pub dest_dir: PathBuf,
    pub file_name: String,
    pub version: String,
    pub expected_sha256: Option<String>,
    pub kind: DownloadAssetKind,
}

impl DownloadSpec {
    pub fn new(
        url: impl Into<String>,
        dest_dir: impl Into<PathBuf>,
        file_name: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            dest_dir: dest_dir.into(),
            file_name: file_name.into(),
            version: VERSION.to_owned(),
            expected_sha256: None,
            kind: DownloadAssetKind::Asset,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub url: String,
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PartialMetadata {
    url: String,
    version: String,
    file_name: String,
    etag: Option<String>,
    last_modified: Option<String>,
    total: Option<u64>,
    created_at: u64,
}

impl PartialMetadata {
    fn fresh(
        spec: &DownloadSpec,
        etag: Option<String>,
        last_modified: Option<String>,
        total: Option<u64>,
    ) -> Self {
        Self {
            url: spec.url.clone(),
            version: spec.version.clone(),
            file_name: spec.file_name.clone(),
            etag,
            last_modified,
            total,
            created_at: unix_now(),
        }
    }

    fn matches(&self, spec: &DownloadSpec) -> bool {
        // Mirrors serve the same artifact: version/file identity is enough
        // to resume across a fallback. `If-Range` protects against a
        // changed object and the final sha256 remains authoritative.
        self.version == spec.version && self.file_name == spec.file_name
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// A per-artifact lock (`create_new`) so two concurrent `rpi update`
/// processes cannot interleave appends. A lock older than an hour is
/// considered abandoned and replaced.
struct PartialLock {
    path: PathBuf,
}

impl PartialLock {
    fn acquire(path: &Path) -> Result<Self, DownloadError> {
        for _ in 0..2 {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age >= Duration::from_secs(3600));
                    if stale {
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    return Err(DownloadError::policy(format!(
                        "another download for {} is already running (lock: {})",
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("artifact"),
                        path.display()
                    )));
                }
                Err(error) => {
                    return Err(DownloadError::new(
                        DownloadErrorKind::Io,
                        format!(
                            "could not create the download lock {}: {error}",
                            path.display()
                        ),
                    ));
                }
            }
        }
        Err(DownloadError::policy(format!(
            "could not acquire the download lock {}",
            path.display()
        )))
    }
}

impl Drop for PartialLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Resumable download engine. Owns the policy/observer; `sleep` is
/// injectable so tests never wait on backoff.
pub struct Downloader<'a> {
    transport: &'a dyn DownloadTransport,
    policy: DownloadPolicy,
    observer: &'a dyn DownloadObserver,
    sleep: Box<dyn Fn(Duration) + 'a>,
}

impl<'a> Downloader<'a> {
    pub fn new(
        transport: &'a dyn DownloadTransport,
        policy: DownloadPolicy,
        observer: &'a dyn DownloadObserver,
    ) -> Self {
        Self {
            transport,
            policy,
            observer,
            sleep: Box::new(std::thread::sleep),
        }
    }

    /// Replace the backoff sleep (tests pass a no-op).
    pub fn with_sleeper(mut self, sleep: impl Fn(Duration) + 'a) -> Self {
        self.sleep = Box::new(sleep);
        self
    }

    pub fn policy(&self) -> &DownloadPolicy {
        &self.policy
    }

    pub fn observer(&self) -> &dyn DownloadObserver {
        self.observer
    }

    /// Download one URL, resuming `<file>.part` when possible. The final
    /// file is only created after the complete sha256 matches.
    pub fn download(&self, spec: &DownloadSpec) -> Result<DownloadOutcome, DownloadError> {
        if let Err(error) = std::fs::create_dir_all(&spec.dest_dir) {
            return Err(DownloadError::new(
                DownloadErrorKind::Io,
                format!(
                    "could not create the download directory {}: {error}",
                    spec.dest_dir.display()
                ),
            ));
        }
        gc_stale_partials(&spec.dest_dir, self.policy.partial_max_age);
        let _download_lock =
            PartialLock::acquire(&spec.dest_dir.join(format!("{}.part.lock", spec.file_name)))?;
        let final_path = spec.dest_dir.join(&spec.file_name);
        let partial_path = spec.dest_dir.join(format!("{}.part", spec.file_name));
        let metadata_path = spec.dest_dir.join(format!("{}.part.json", spec.file_name));

        let mut metadata = self.load_metadata(spec, &partial_path, &metadata_path);
        let mut failures: Vec<String> = Vec::new();
        for attempt in 1..=self.policy.max_attempts.max(1) {
            if attempt > 1 {
                // The head handler persists resume metadata as soon as the
                // response arrives, so a failed attempt can resume.
                metadata = self.load_metadata(spec, &partial_path, &metadata_path);
            }
            self.observer.on_download_event(&DownloadEvent::Connecting {
                url: spec.url.clone(),
                attempt,
            });
            let existing = std::fs::metadata(&partial_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            let resume = existing > 0 && metadata.is_some();
            let mut headers: Vec<(String, String)> = Vec::new();
            if resume {
                headers.push(("Range".to_string(), format!("bytes={existing}-")));
                if let Some(metadata) = &metadata {
                    if let Some(etag) = &metadata.etag {
                        headers.push(("If-Range".to_string(), etag.clone()));
                    } else if let Some(last_modified) = &metadata.last_modified {
                        headers.push(("If-Range".to_string(), last_modified.clone()));
                    }
                }
            }
            let request = DownloadHttpRequest {
                url: spec.url.clone(),
                headers,
                connect_timeout: self.policy.connect_timeout,
                stall_timeout: self.policy.stall_timeout,
            };

            let mut mode: Option<OpenMode> = None;
            let mut downloaded = existing;
            let mut total = metadata.as_ref().and_then(|metadata| metadata.total);
            let attempt_result = {
                let observer = self.observer;
                let partial_path_ref = &partial_path;
                let metadata_path_ref = &metadata_path;
                let metadata_snapshot = metadata.clone();
                let spec_snapshot = spec.clone();
                let mut write_error: Option<DownloadError> = None;
                let mut head: Option<DownloadResponseHead> = None;
                let mut sink = |event: DownloadStreamEvent<'_>| match event {
                    DownloadStreamEvent::Head(response) => {
                        head = Some(response.clone());
                        let status = response.status;
                        if !(200..300).contains(&status) && status != 416 {
                            return Err(DownloadError::new(
                                DownloadErrorKind::Http(status),
                                format!("download failed: HTTP {status}"),
                            )
                            .with_retry_after(retry_after_for(response)));
                        }
                        let (open_mode, resume_offset, new_total) =
                            decide_mode(status, existing, response);
                        total = new_total;
                        mode = Some(open_mode);
                        downloaded = resume_offset;
                        open_partial(partial_path_ref, open_mode)?;
                        if response.status == 200 && existing > 0 {
                            observer.on_download_event(&DownloadEvent::Restarting {
                                url: spec_snapshot.url.clone(),
                                discarded_bytes: existing,
                            });
                        }
                        if response.status != 416 {
                            // Persist resume metadata with the first bytes:
                            // the partial must be resumable even when this
                            // attempt is interrupted.
                            let updated = PartialMetadata::fresh(
                                &spec_snapshot,
                                response.etag.clone().or_else(|| {
                                    metadata_snapshot.as_ref().and_then(|m| m.etag.clone())
                                }),
                                response.last_modified.clone().or_else(|| {
                                    metadata_snapshot
                                        .as_ref()
                                        .and_then(|m| m.last_modified.clone())
                                }),
                                new_total.or(response.content_length),
                            );
                            write_metadata(metadata_path_ref, &updated)?;
                            observer
                                .on_download_event(&DownloadEvent::Progress { downloaded, total });
                        }
                        Ok(())
                    }
                    DownloadStreamEvent::Chunk(chunk) => {
                        if mode == Some(OpenMode::None) {
                            // 416 with a complete part: ignore any body.
                            return Ok(());
                        }
                        if write_error.is_some() {
                            return Ok(());
                        }
                        match append_partial(partial_path_ref, chunk) {
                            Ok(()) => {
                                downloaded += chunk.len() as u64;
                                observer.on_download_event(&DownloadEvent::Progress {
                                    downloaded,
                                    total,
                                });
                                Ok(())
                            }
                            Err(error) => {
                                let message = error.to_string();
                                write_error = Some(DownloadError::new(
                                    DownloadErrorKind::Io,
                                    format!("could not write the partial download: {message}"),
                                ));
                                Err(DownloadError::new(DownloadErrorKind::Io, message))
                            }
                        }
                    }
                };
                let transport_result = self.transport.stream(&request, &mut sink);
                if let Some(error) = write_error {
                    Err(error)
                } else if let Some(head) = head {
                    match transport_result {
                        Ok(()) => Ok(head),
                        Err(error) => Err(error),
                    }
                } else {
                    transport_result.map(|()| DownloadResponseHead::default())
                }
            };

            match attempt_result {
                Ok(head) => {
                    let status = head.status;
                    if status == 416 {
                        let complete = total.is_some_and(|total| total == existing)
                            || head
                                .content_range_total
                                .is_some_and(|total| total == existing);
                        if complete {
                            // The partial already holds the full body.
                        } else {
                            self.reset_partial(&partial_path, &metadata_path);
                            metadata = None;
                            let delay = self.policy.delay_for(attempt);
                            self.emit_retry(
                                spec,
                                attempt,
                                "server rejected the resume range (HTTP 416); restarting from zero",
                                0,
                                delay,
                            );
                            (self.sleep)(delay);
                            continue;
                        }
                    }
                    let Some(open_mode) = mode else {
                        // A transport that returned no head but wrote no
                        // body cannot happen for a 2xx production response;
                        // treat it as a policy failure.
                        return Err(DownloadError::policy(format!(
                            "the download transport returned no response for {}",
                            spec.url
                        )));
                    };
                    if open_mode == OpenMode::None {
                        // 416 complete: nothing to verify below unless the
                        // caller supplied a digest.
                    }
                    let updated = PartialMetadata::fresh(
                        spec,
                        head.etag
                            .clone()
                            .or_else(|| metadata.as_ref().and_then(|m| m.etag.clone())),
                        head.last_modified
                            .clone()
                            .or_else(|| metadata.as_ref().and_then(|m| m.last_modified.clone())),
                        total.or(head.content_length),
                    );
                    write_metadata(&metadata_path, &updated)?;
                    let actual = sha256_file(&partial_path)?;
                    if let Some(expected) = &spec.expected_sha256
                        && !actual.eq_ignore_ascii_case(expected)
                    {
                        // A poisoned partial must not survive: a
                        // Range-capable server would answer 416 "complete"
                        // and reproduce the same mismatch forever.
                        self.reset_partial(&partial_path, &metadata_path);
                        let error = DownloadError::integrity(format!(
                            "Integrity check failed for {}: expected sha256 {}, got {}. The download was discarded; re-run to download it again.",
                            spec.file_name, expected, actual,
                        ));
                        self.observer.on_download_event(&DownloadEvent::Failed {
                            message: error.message.clone(),
                        });
                        return Err(error);
                    }
                    self.observer.on_download_event(&DownloadEvent::Verifying {
                        file: final_path.clone(),
                    });
                    promote_partial(&partial_path, &final_path)?;
                    let _ = std::fs::remove_file(&metadata_path);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            &final_path,
                            std::fs::Permissions::from_mode(0o600),
                        );
                    }
                    let bytes = std::fs::metadata(&final_path)
                        .map(|metadata| metadata.len())
                        .unwrap_or(0);
                    self.observer.on_download_event(&DownloadEvent::Done {
                        file: final_path.clone(),
                        bytes,
                    });
                    return Ok(DownloadOutcome {
                        url: spec.url.clone(),
                        path: final_path,
                        sha256: actual,
                        bytes,
                        attempts: attempt,
                    });
                }
                Err(error) => {
                    failures.push(error.message.clone());
                    if error.kind == DownloadErrorKind::Integrity {
                        return Err(error);
                    }
                    if !error.kind.is_retryable() {
                        // 404/403 and other terminal statuses move to the
                        // next mirror immediately (the caller loops).
                        return Err(error);
                    }
                    if attempt == self.policy.max_attempts {
                        let message = format!(
                            "Could not download {} after {} attempts: {}",
                            spec.file_name, attempt, error.message
                        );
                        self.observer.on_download_event(&DownloadEvent::Failed {
                            message: message.clone(),
                        });
                        return Err(DownloadError::new(DownloadErrorKind::Policy, message));
                    }
                    let downloaded = std::fs::metadata(&partial_path)
                        .map(|metadata| metadata.len())
                        .unwrap_or(0);
                    let delay = error
                        .retry_after
                        .unwrap_or_else(|| self.policy.delay_for(attempt));
                    self.emit_retry(spec, attempt, &error.message, downloaded, delay);
                    (self.sleep)(delay);
                }
            }
        }
        Err(DownloadError::policy(format!(
            "Could not download {} from {}: {}",
            spec.file_name,
            spec.url,
            failures.last().cloned().unwrap_or_default()
        )))
    }

    fn load_metadata(
        &self,
        spec: &DownloadSpec,
        partial_path: &Path,
        metadata_path: &Path,
    ) -> Option<PartialMetadata> {
        let Ok(text) = std::fs::read_to_string(metadata_path) else {
            // No metadata: a stray partial cannot be trusted for resume.
            self.reset_partial(partial_path, metadata_path);
            return None;
        };
        let Ok(metadata) = serde_json::from_str::<PartialMetadata>(&text) else {
            self.reset_partial(partial_path, metadata_path);
            return None;
        };
        let fresh = metadata.matches(spec)
            && unix_now().saturating_sub(metadata.created_at)
                <= self.policy.partial_max_age.as_secs();
        if fresh {
            if !partial_path.exists() {
                self.reset_partial(partial_path, metadata_path);
            }
            return fresh.then_some(metadata);
        }
        // Version change or older than the retention window.
        self.reset_partial(partial_path, metadata_path);
        None
    }

    fn reset_partial(&self, partial_path: &Path, metadata_path: &Path) {
        let _ = std::fs::remove_file(partial_path);
        let _ = std::fs::remove_file(metadata_path);
    }

    fn emit_retry(
        &self,
        spec: &DownloadSpec,
        attempt: u32,
        reason: &str,
        downloaded: u64,
        delay: Duration,
    ) {
        self.observer.on_download_event(&DownloadEvent::Retry {
            url: spec.url.clone(),
            attempt,
            reason: reason.to_string(),
            downloaded,
            delay,
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    Truncate,
    Append,
    /// 416 + complete partial: no writes.
    None,
}

fn decide_mode(
    status: u16,
    existing: u64,
    response: &DownloadResponseHead,
) -> (OpenMode, u64, Option<u64>) {
    if status == 416 {
        return (OpenMode::None, existing, response.content_range_total);
    }
    if status == 206 {
        let total = response.content_range_total.or_else(|| {
            response
                .content_length
                .map(|length| existing.saturating_add(length))
        });
        return (OpenMode::Append, existing, total);
    }
    // 200: the server ignored the range (or this was a fresh request).
    let total = response.content_length;
    (OpenMode::Truncate, 0, total)
}

fn open_partial(path: &Path, mode: OpenMode) -> Result<(), DownloadError> {
    if mode == OpenMode::None {
        return Ok(());
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true);
    match mode {
        OpenMode::Truncate => {
            options.truncate(true);
        }
        OpenMode::Append => {
            options.append(true);
        }
        OpenMode::None => {}
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map(|_| ()).map_err(|error| {
        DownloadError::new(
            DownloadErrorKind::Io,
            format!(
                "could not open the partial download {}: {error}",
                path.display()
            ),
        )
    })
}

fn append_partial(path: &Path, chunk: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(chunk)
}

fn write_metadata(path: &Path, metadata: &PartialMetadata) -> Result<(), DownloadError> {
    let text = serde_json::to_vec(metadata)
        .map_err(|error| DownloadError::new(DownloadErrorKind::Io, error.to_string()))?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        DownloadError::new(
            DownloadErrorKind::Io,
            format!("could not write {}: {error}", path.display()),
        )
    })?;
    file.write_all(&text)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            DownloadError::new(
                DownloadErrorKind::Io,
                format!("could not write {}: {error}", path.display()),
            )
        })
}

fn promote_partial(partial: &Path, final_path: &Path) -> Result<(), DownloadError> {
    #[cfg(windows)]
    {
        if final_path.exists() {
            let _ = std::fs::remove_file(final_path);
        }
    }
    std::fs::rename(partial, final_path).map_err(|error| {
        DownloadError::new(
            DownloadErrorKind::Io,
            format!(
                "could not move the verified download into place ({}): {error}",
                final_path.display()
            ),
        )
    })
}

/// Lowercase hex sha256 of a file stream (never loads the whole artifact).
pub fn sha256_file(path: &Path) -> Result<String, DownloadError> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        DownloadError::new(
            DownloadErrorKind::Io,
            format!("could not read {}: {error}", path.display()),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            DownloadError::new(
                DownloadErrorKind::Io,
                format!("could not read {}: {error}", path.display()),
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Best-effort classification of a legacy transport's `String` error (the
/// pre-V18-12 fakes and seams). Production transports return structured
/// errors from `stream`.
pub fn classify_legacy_error(error: &str) -> DownloadError {
    if let Some(rest) = error.split("HTTP ").nth(1)
        && let Ok(status) = rest
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .parse::<u16>()
    {
        return DownloadError::new(DownloadErrorKind::Http(status), error.to_string());
    }
    let lower = error.to_ascii_lowercase();
    let kind = if lower.contains("timeout") || lower.contains("timed out") {
        DownloadErrorKind::Timeout
    } else if lower.contains("reset") || lower.contains("interrupted") || lower.contains("stall") {
        DownloadErrorKind::Reset
    } else {
        DownloadErrorKind::Connect
    };
    DownloadError::new(kind, error.to_string())
}

fn retry_after_for(head: &DownloadResponseHead) -> Option<Duration> {
    if head.status != 429 && head.status != 503 {
        return None;
    }
    head.retry_after
}

/// Remove abandoned partials (older than `max_age`) in a staging directory
/// (R3.10.6). Superseded artifact names are never requested again, so the
/// per-download check alone cannot expire them.
pub fn gc_stale_partials(dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = unix_now();
    let mut seen_stems: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".part.json") else {
            continue;
        };
        seen_stems.push(stem.to_string());
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(metadata) = serde_json::from_str::<PartialMetadata>(&text) else {
            continue;
        };
        if now.saturating_sub(metadata.created_at) <= max_age.as_secs() {
            continue;
        }
        let _ = std::fs::remove_file(dir.join(format!("{stem}.part")));
        let _ = std::fs::remove_file(&path);
    }
    // Orphan `.part` files without metadata cannot resume; remove them once
    // they are older than the retention window.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".part") else {
            continue;
        };
        if seen_stems.iter().any(|seen| seen == stem) {
            continue;
        }
        let old = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age.as_secs() > max_age.as_secs());
        if old {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Loops over mirror URLs. A transport/integrity failure ends that URL;
/// integrity failures abort the whole transfer (never switch mirrors).
pub fn download_from_sources(
    downloader: &Downloader<'_>,
    spec: &DownloadSpec,
    urls: &[String],
) -> Result<DownloadOutcome, DownloadError> {
    let mut failures: Vec<String> = Vec::new();
    for (index, url) in urls.iter().enumerate() {
        let mut candidate = spec.clone();
        candidate.url = url.clone();
        match downloader.download(&candidate) {
            Ok(outcome) => return Ok(outcome),
            Err(error) => {
                if error.kind == DownloadErrorKind::Integrity {
                    return Err(error);
                }
                let kept_bytes =
                    std::fs::metadata(spec.dest_dir.join(format!("{}.part", spec.file_name)))
                        .map(|metadata| metadata.len())
                        .unwrap_or(0);
                if let Some(next) = urls.get(index + 1) {
                    downloader
                        .observer()
                        .on_download_event(&DownloadEvent::Switching {
                            from: url.clone(),
                            to: next.clone(),
                            reason: error.message.clone(),
                            kept_bytes,
                            attempt: (index + 1) as u32,
                        });
                }
                failures.push(format!("{url}: {}", error.message));
            }
        }
    }
    Err(DownloadError::policy(format!(
        "Could not download {} from any source: {}",
        spec.file_name,
        failures.join("; ")
    )))
}

// ---------------------------------------------------------------------------
// Production reqwest transport
// ---------------------------------------------------------------------------

/// Production transport: async reqwest streamed on a scoped worker thread
/// with a current-thread runtime (the package manager is synchronous and
/// may be called while a tokio runtime is active).
pub struct ReqwestDownloadTransport;

impl DownloadTransport for ReqwestDownloadTransport {
    fn stream(
        &self,
        request: &DownloadHttpRequest,
        sink: &mut (dyn FnMut(DownloadStreamEvent<'_>) -> Result<(), DownloadError> + Send),
    ) -> Result<(), DownloadError> {
        std::thread::scope(|scope| {
            let handle = scope.spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        DownloadError::new(DownloadErrorKind::Connect, error.to_string())
                    })?;
                runtime.block_on(async move {
                    let client = reqwest::Client::builder()
                        .connect_timeout(request.connect_timeout)
                        .build()
                        .map_err(|error| {
                            DownloadError::new(DownloadErrorKind::Connect, error.to_string())
                        })?;
                    let mut builder = client
                        .get(&request.url)
                        .header(reqwest::header::USER_AGENT, rpi_user_agent(VERSION));
                    for (name, value) in &request.headers {
                        builder = builder.header(name, value);
                    }
                    let response = tokio::time::timeout(request.stall_timeout, builder.send())
                        .await
                        .map_err(|_| {
                            DownloadError::new(
                                DownloadErrorKind::Timeout,
                                format!(
                                    "the server sent no response headers for {}s",
                                    request.stall_timeout.as_secs()
                                ),
                            )
                        })?
                        .map_err(classify_reqwest)?;
                    let status = response.status().as_u16();
                    let header = |name: reqwest::header::HeaderName| {
                        response
                            .headers()
                            .get(name)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string)
                    };
                    let head = DownloadResponseHead {
                        status,
                        content_length: response.content_length(),
                        content_range_total: parse_content_range_total(
                            response
                                .headers()
                                .get(reqwest::header::CONTENT_RANGE)
                                .and_then(|value| value.to_str().ok()),
                        ),
                        etag: header(reqwest::header::ETAG),
                        last_modified: header(reqwest::header::LAST_MODIFIED),
                        retry_after: response
                            .headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|value| value.to_str().ok())
                            .and_then(|value| value.trim().parse::<u64>().ok())
                            .map(Duration::from_secs),
                    };
                    sink(DownloadStreamEvent::Head(&head))?;
                    // Non-2xx heads are rejected by the sink before this
                    // point (416 is the one exception and its body is
                    // ignored by the engine).
                    let mut response = response;
                    loop {
                        match tokio::time::timeout(request.stall_timeout, response.chunk()).await {
                            Err(_) => {
                                return Err(DownloadError::new(
                                    DownloadErrorKind::Timeout,
                                    format!(
                                        "the download stalled for {}s without receiving data",
                                        request.stall_timeout.as_secs()
                                    ),
                                ));
                            }
                            Ok(Err(error)) => return Err(classify_reqwest(error)),
                            Ok(Ok(None)) => return Ok(()),
                            Ok(Ok(Some(chunk))) => {
                                sink(DownloadStreamEvent::Chunk(&chunk))?;
                            }
                        }
                    }
                })
            });
            match handle.join() {
                Ok(result) => result,
                Err(_) => Err(DownloadError::new(
                    DownloadErrorKind::Connect,
                    "the download worker panicked",
                )),
            }
        })
    }
}

fn classify_reqwest(error: reqwest::Error) -> DownloadError {
    if error.is_timeout() {
        return DownloadError::new(
            DownloadErrorKind::Timeout,
            format!("the connection timed out: {error}"),
        );
    }
    if error.is_connect() {
        return DownloadError::new(
            DownloadErrorKind::Connect,
            format!("could not connect: {error}"),
        );
    }
    if error.is_body() || error.is_decode() {
        return DownloadError::new(
            DownloadErrorKind::Reset,
            format!("the transfer was interrupted: {error}"),
        );
    }
    DownloadError::new(
        DownloadErrorKind::Connect,
        format!("request failed: {error}"),
    )
}

fn parse_content_range_total(value: Option<&str>) -> Option<u64> {
    let value = value?;
    value.rsplit('/').next()?.trim().parse().ok()
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    type RecordedCalls = Vec<(String, Vec<(String, String)>)>;

    #[derive(Debug, Clone)]
    struct FakeResponse {
        status: u16,
        content_length: Option<u64>,
        content_range_total: Option<u64>,
        retry_after: Option<Duration>,
        chunks: Vec<Result<Vec<u8>, DownloadError>>,
    }

    impl FakeResponse {
        fn ok(bytes: &[u8]) -> Self {
            Self {
                status: 200,
                content_length: Some(bytes.len() as u64),
                content_range_total: None,
                retry_after: None,
                chunks: vec![Ok(bytes.to_vec())],
            }
        }
    }

    struct FakeTransport {
        responses: Mutex<VecDeque<FakeResponse>>,
        calls: Mutex<RecordedCalls>,
    }

    impl FakeTransport {
        fn new(responses: Vec<FakeResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> RecordedCalls {
            self.calls.lock().unwrap().clone()
        }
    }

    impl DownloadTransport for FakeTransport {
        fn stream(
            &self,
            request: &DownloadHttpRequest,
            sink: &mut (dyn FnMut(DownloadStreamEvent<'_>) -> Result<(), DownloadError> + Send),
        ) -> Result<(), DownloadError> {
            self.calls
                .lock()
                .unwrap()
                .push((request.url.clone(), request.headers.clone()));
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| DownloadError::policy("no fake response left"))?;
            let head = DownloadResponseHead {
                status: response.status,
                content_length: response.content_length,
                content_range_total: response.content_range_total,
                retry_after: response.retry_after,
                ..Default::default()
            };
            sink(DownloadStreamEvent::Head(&head))?;
            for chunk in response.chunks {
                let chunk: Result<Vec<u8>, DownloadError> = chunk;
                let bytes = chunk?;
                sink(DownloadStreamEvent::Chunk(&bytes))?;
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingObserver {
        events: Mutex<Vec<DownloadEvent>>,
    }

    impl DownloadObserver for RecordingObserver {
        fn on_download_event(&self, event: &DownloadEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    impl RecordingObserver {
        fn retries(&self) -> usize {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, DownloadEvent::Retry { .. }))
                .count()
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rpi-download-test-{}-{}-{}",
                name,
                std::process::id(),
                unix_now()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn spec(dir: &Path, expected: Option<&str>) -> DownloadSpec {
        let mut spec = DownloadSpec::new("https://example.test/a.rpix", dir, "a.rpix");
        spec.version = "1.2.3".to_owned();
        spec.expected_sha256 = expected.map(str::to_string);
        spec
    }

    fn downloader<'a>(
        transport: &'a FakeTransport,
        observer: &'a RecordingObserver,
    ) -> Downloader<'a> {
        Downloader::new(transport, DownloadPolicy::for_tests(), observer).with_sleeper(|_| {})
    }

    #[test]
    fn success_verifies_and_promotes_without_partial_residue() {
        let dir = TempDir::new("success");
        let bytes = b"verified artifact";
        let transport = FakeTransport::new(vec![FakeResponse::ok(bytes)]);
        let observer = RecordingObserver::default();
        let outcome = downloader(&transport, &observer)
            .download(&spec(dir.path(), Some(&hex(bytes))))
            .expect("download");
        assert_eq!(std::fs::read(&outcome.path).unwrap(), bytes);
        assert_eq!(outcome.sha256, hex(bytes));
        assert_eq!(outcome.attempts, 1);
        assert!(!dir.path().join("a.rpix.part").exists());
        assert!(!dir.path().join("a.rpix.part.json").exists());
        assert!(
            observer
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, DownloadEvent::Done { .. }))
        );
    }

    #[test]
    fn retryable_transport_error_is_retried_and_reported() {
        let dir = TempDir::new("retry");
        let bytes = b"retry body";
        let transport = FakeTransport::new(vec![
            FakeResponse {
                status: 200,
                content_length: Some(bytes.len() as u64),
                content_range_total: None,
                retry_after: None,
                chunks: vec![Err(DownloadError::new(
                    DownloadErrorKind::Reset,
                    "connection reset",
                ))],
            },
            FakeResponse::ok(bytes),
        ]);
        let observer = RecordingObserver::default();
        let outcome = downloader(&transport, &observer)
            .download(&spec(dir.path(), Some(&hex(bytes))))
            .expect("second attempt succeeds");
        assert_eq!(outcome.attempts, 2);
        assert_eq!(observer.retries(), 1);
        let calls = transport.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].1.is_empty(), "failed partial is empty: {calls:?}");
    }

    #[test]
    fn retry_exhaustion_returns_policy_and_keeps_partial() {
        let dir = TempDir::new("exhausted");
        let transport = FakeTransport::new(vec![
            FakeResponse {
                status: 200,
                content_length: Some(4),
                content_range_total: None,
                retry_after: None,
                chunks: vec![
                    Ok(b"part".to_vec()),
                    Err(DownloadError::new(
                        DownloadErrorKind::Reset,
                        "connection reset",
                    )),
                ],
            },
            FakeResponse {
                status: 200,
                content_length: Some(4),
                content_range_total: None,
                retry_after: None,
                chunks: vec![Err(DownloadError::new(
                    DownloadErrorKind::Reset,
                    "connection reset again",
                ))],
            },
        ]);
        let observer = RecordingObserver::default();
        let policy = DownloadPolicy {
            max_attempts: 2,
            ..DownloadPolicy::for_tests()
        };
        let downloader = Downloader::new(&transport, policy, &observer).with_sleeper(|_| {});
        let error = downloader
            .download(&spec(dir.path(), None))
            .expect_err("two resets exhaust the policy");
        assert_eq!(error.kind, DownloadErrorKind::Policy);
        assert!(
            error.message.contains("after 2 attempts"),
            "{}",
            error.message
        );
        assert_eq!(transport.calls().len(), 2);
    }

    #[test]
    fn non_retryable_http_status_stops_immediately() {
        let dir = TempDir::new("404");
        let transport = FakeTransport::new(vec![FakeResponse {
            status: 404,
            content_length: Some(0),
            content_range_total: None,
            retry_after: None,
            chunks: Vec::new(),
        }]);
        let observer = RecordingObserver::default();
        let error = downloader(&transport, &observer)
            .download(&spec(dir.path(), None))
            .unwrap_err();
        assert_eq!(error.kind, DownloadErrorKind::Http(404));
        assert_eq!(transport.calls().len(), 1);
    }

    #[test]
    fn integrity_mismatch_discards_partial_and_never_creates_final() {
        let dir = TempDir::new("integrity");
        let bytes = b"tampered";
        let transport = FakeTransport::new(vec![FakeResponse::ok(bytes)]);
        let observer = RecordingObserver::default();
        let error = downloader(&transport, &observer)
            .download(&spec(dir.path(), Some(&"00".repeat(32))))
            .unwrap_err();
        assert_eq!(error.kind, DownloadErrorKind::Integrity);
        assert!(!dir.path().join("a.rpix").exists());
        assert!(!dir.path().join("a.rpix.part").exists());
        assert!(!dir.path().join("a.rpix.part.json").exists());
        assert_eq!(transport.calls().len(), 1);
    }

    #[test]
    fn resume_sends_range_and_appends_a_206_response() {
        let dir = TempDir::new("resume");
        let transport = FakeTransport::new(vec![
            FakeResponse {
                status: 200,
                content_length: Some(11),
                content_range_total: None,
                retry_after: None,
                chunks: vec![
                    Ok(b"hello ".to_vec()),
                    Err(DownloadError::new(
                        DownloadErrorKind::Reset,
                        "connection reset",
                    )),
                ],
            },
            FakeResponse {
                status: 206,
                content_length: Some(5),
                content_range_total: Some(11),
                retry_after: None,
                chunks: vec![Ok(b"world".to_vec())],
            },
        ]);
        let observer = RecordingObserver::default();
        let outcome = downloader(&transport, &observer)
            .download(&spec(dir.path(), Some(&hex(b"hello world"))))
            .expect("resume succeeds");
        assert_eq!(std::fs::read(&outcome.path).unwrap(), b"hello world");
        let calls = transport.calls();
        assert_eq!(calls.len(), 2);
        assert!(
            calls[1]
                .1
                .iter()
                .any(|(name, value)| name == "Range" && value == "bytes=6-"),
            "range header: {calls:?}"
        );
    }

    #[test]
    fn a_200_on_retry_discards_the_partial_and_downloads_from_zero() {
        let dir = TempDir::new("restart");
        let transport = FakeTransport::new(vec![
            FakeResponse {
                status: 200,
                content_length: Some(11),
                content_range_total: None,
                retry_after: None,
                chunks: vec![
                    Ok(b"hello ".to_vec()),
                    Err(DownloadError::new(
                        DownloadErrorKind::Reset,
                        "connection reset",
                    )),
                ],
            },
            FakeResponse::ok(b"hello world"),
        ]);
        let observer = RecordingObserver::default();
        let outcome = downloader(&transport, &observer)
            .download(&spec(dir.path(), Some(&hex(b"hello world"))))
            .expect("restart succeeds");
        assert_eq!(std::fs::read(&outcome.path).unwrap(), b"hello world");
        assert!(
            observer
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, DownloadEvent::Restarting { .. })),
            "a 200 after a partial must announce the restart"
        );
    }

    #[test]
    fn cross_mirror_fallback_resumes_the_same_partial() {
        let dir = TempDir::new("cross-mirror-resume");
        let transport = FakeTransport::new(vec![
            FakeResponse {
                status: 200,
                content_length: Some(11),
                content_range_total: None,
                retry_after: None,
                chunks: vec![
                    Ok(b"hello ".to_vec()),
                    Err(DownloadError::new(
                        DownloadErrorKind::Reset,
                        "connection reset",
                    )),
                ],
            },
            FakeResponse {
                status: 206,
                content_length: Some(5),
                content_range_total: Some(11),
                retry_after: None,
                chunks: vec![Ok(b"world".to_vec())],
            },
        ]);
        let observer = RecordingObserver::default();
        let downloader = downloader(&transport, &observer);
        let bytes = b"hello world";
        let mut spec = spec(dir.path(), Some(&hex(bytes)));
        spec.url = "https://github.test/a.rpix".to_owned();
        let urls = vec![
            "https://github.test/a.rpix".to_owned(),
            "https://mirror.test/a.rpix".to_owned(),
        ];
        let outcome = download_from_sources(&downloader, &spec, &urls).expect("mirror resume");
        assert_eq!(std::fs::read(&outcome.path).unwrap(), bytes);
        let calls = transport.calls();
        assert_eq!(calls.len(), 2);
        assert!(
            calls[1]
                .1
                .iter()
                .any(|(name, value)| name == "Range" && value == "bytes=6-"),
            "cross-mirror range: {calls:?}"
        );
    }

    #[test]
    fn cross_mirror_resume_sends_if_range() {
        let dir = TempDir::new("if-range");
        let bytes = b"hello world";
        std::fs::write(dir.path().join("a.rpix.part"), b"hello ").unwrap();
        let metadata = PartialMetadata {
            url: "https://github.test/a.rpix".to_owned(),
            version: "1.2.3".to_owned(),
            file_name: "a.rpix".to_owned(),
            etag: Some("\"abc\"".to_owned()),
            last_modified: None,
            total: Some(bytes.len() as u64),
            created_at: unix_now(),
        };
        std::fs::write(
            dir.path().join("a.rpix.part.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        let transport = FakeTransport::new(vec![FakeResponse {
            status: 206,
            content_length: Some(5),
            content_range_total: Some(bytes.len() as u64),
            retry_after: None,
            chunks: vec![Ok(b"world".to_vec())],
        }]);
        let observer = RecordingObserver::default();
        let mut spec = spec(dir.path(), Some(&hex(bytes)));
        spec.url = "https://mirror.test/a.rpix".to_owned();
        downloader(&transport, &observer)
            .download(&spec)
            .expect("resume from another mirror");
        let calls = transport.calls();
        assert!(
            calls[0]
                .1
                .iter()
                .any(|(name, value)| name == "If-Range" && value == "\"abc\""),
            "if-range header: {calls:?}"
        );
    }

    #[test]
    fn a_switched_source_that_404s_keeps_the_partial() {
        let dir = TempDir::new("switch-404");
        let transport = FakeTransport::new(vec![
            FakeResponse {
                status: 200,
                content_length: Some(11),
                content_range_total: None,
                retry_after: None,
                chunks: vec![
                    Ok(b"hello ".to_vec()),
                    Err(DownloadError::new(
                        DownloadErrorKind::Reset,
                        "connection reset",
                    )),
                ],
            },
            FakeResponse {
                status: 404,
                content_length: Some(0),
                content_range_total: None,
                retry_after: None,
                chunks: Vec::new(),
            },
        ]);
        let observer = RecordingObserver::default();
        let downloader = downloader(&transport, &observer);
        let mut spec = spec(dir.path(), Some(&hex(b"hello world")));
        spec.url = "https://github.test/a.rpix".to_owned();
        let urls = vec![
            "https://github.test/a.rpix".to_owned(),
            "https://mirror.test/a.rpix".to_owned(),
        ];
        let error = download_from_sources(&downloader, &spec, &urls).unwrap_err();
        assert_eq!(error.kind, DownloadErrorKind::Policy);
        assert_eq!(
            std::fs::read(dir.path().join("a.rpix.part")).unwrap(),
            b"hello "
        );
        assert!(dir.path().join("a.rpix.part.json").exists());
    }

    #[test]
    fn a_416_with_a_complete_partial_promotes_it() {
        let dir = TempDir::new("416");
        let bytes = b"hello world";
        std::fs::write(dir.path().join("a.rpix.part"), bytes).unwrap();
        let metadata = PartialMetadata {
            url: "https://example.test/a.rpix".to_owned(),
            version: "1.2.3".to_owned(),
            file_name: "a.rpix".to_owned(),
            etag: None,
            last_modified: None,
            total: Some(bytes.len() as u64),
            created_at: unix_now(),
        };
        std::fs::write(
            dir.path().join("a.rpix.part.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        let transport = FakeTransport::new(vec![FakeResponse {
            status: 416,
            content_length: Some(0),
            content_range_total: Some(bytes.len() as u64),
            retry_after: None,
            chunks: Vec::new(),
        }]);
        let observer = RecordingObserver::default();
        let outcome = downloader(&transport, &observer)
            .download(&spec(dir.path(), Some(&hex(bytes))))
            .expect("416 complete");
        assert_eq!(std::fs::read(&outcome.path).unwrap(), bytes);
    }

    #[test]
    fn stale_partial_is_discarded_before_downloading() {
        let dir = TempDir::new("stale");
        std::fs::write(dir.path().join("a.rpix.part"), b"old").unwrap();
        let metadata = PartialMetadata {
            url: "https://example.test/a.rpix".to_owned(),
            version: "0.0.1".to_owned(),
            file_name: "a.rpix".to_owned(),
            etag: None,
            last_modified: None,
            total: None,
            created_at: unix_now(),
        };
        std::fs::write(
            dir.path().join("a.rpix.part.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        let bytes = b"fresh";
        let transport = FakeTransport::new(vec![FakeResponse::ok(bytes)]);
        let observer = RecordingObserver::default();
        let outcome = downloader(&transport, &observer)
            .download(&spec(dir.path(), Some(&hex(bytes))))
            .expect("fresh download");
        assert_eq!(std::fs::read(&outcome.path).unwrap(), bytes);
        let calls = transport.calls();
        assert!(
            calls[0].1.is_empty(),
            "stale partial must not be resumed: {calls:?}"
        );
    }

    #[test]
    fn download_from_sources_falls_back_to_the_mirror() {
        let dir = TempDir::new("fallback");
        let bytes = b"mirror body";
        let transport = FakeTransport::new(vec![
            FakeResponse {
                status: 404,
                content_length: Some(0),
                content_range_total: None,
                retry_after: None,
                chunks: Vec::new(),
            },
            FakeResponse::ok(bytes),
        ]);
        let observer = RecordingObserver::default();
        let downloader = downloader(&transport, &observer);
        let mut spec = spec(dir.path(), Some(&hex(bytes)));
        spec.url = "https://github.test/a.rpix".to_owned();
        let urls = vec![
            "https://github.test/a.rpix".to_owned(),
            "https://mirror.test/a.rpix".to_owned(),
        ];
        let outcome = download_from_sources(&downloader, &spec, &urls).expect("mirror");
        assert_eq!(outcome.url, "https://mirror.test/a.rpix");
        assert_eq!(std::fs::read(&outcome.path).unwrap(), bytes);
        assert!(
            observer.events.lock().unwrap().iter().any(|event| matches!(
                event,
                DownloadEvent::Switching { from, to, .. }
                    if from.contains("github.test") && to.contains("mirror.test")
            )),
            "fallback must emit a Switching event"
        );
    }

    #[test]
    fn integrity_failure_does_not_switch_to_the_mirror() {
        let dir = TempDir::new("no-mirror");
        let bytes = b"bad";
        let transport = FakeTransport::new(vec![FakeResponse::ok(bytes), FakeResponse::ok(bytes)]);
        let observer = RecordingObserver::default();
        let downloader = downloader(&transport, &observer);
        let mut spec = spec(dir.path(), Some(&"11".repeat(32)));
        spec.url = "https://github.test/a.rpix".to_owned();
        let urls = vec![
            "https://github.test/a.rpix".to_owned(),
            "https://mirror.test/a.rpix".to_owned(),
        ];
        let error = download_from_sources(&downloader, &spec, &urls).unwrap_err();
        assert_eq!(error.kind, DownloadErrorKind::Integrity);
        assert_eq!(transport.calls().len(), 1);
    }

    #[test]
    fn retry_after_overrides_the_exponential_delay() {
        let dir = TempDir::new("retry-after");
        let bytes = b"after wait";
        let transport = FakeTransport::new(vec![
            FakeResponse {
                status: 429,
                content_length: Some(0),
                content_range_total: None,
                retry_after: Some(Duration::from_secs(2)),
                chunks: Vec::new(),
            },
            FakeResponse::ok(bytes),
        ]);
        let observer = RecordingObserver::default();
        let slept = Mutex::new(Vec::new());
        let downloader = Downloader::new(&transport, DownloadPolicy::for_tests(), &observer)
            .with_sleeper(|delay| slept.lock().unwrap().push(delay));
        let outcome = downloader
            .download(&spec(dir.path(), Some(&hex(bytes))))
            .expect("429 then success");
        assert_eq!(outcome.attempts, 2);
        assert_eq!(*slept.lock().unwrap(), vec![Duration::from_secs(2)]);
    }

    #[test]
    fn a_concurrent_download_lock_blocks_a_second_writer() {
        let dir = TempDir::new("lock");
        let lock = dir.path().join("a.rpix.part.lock");
        std::fs::write(&lock, b"123").unwrap();
        let transport = FakeTransport::new(vec![FakeResponse::ok(b"body")]);
        let observer = RecordingObserver::default();
        let error = downloader(&transport, &observer)
            .download(&spec(dir.path(), None))
            .unwrap_err();
        assert_eq!(error.kind, DownloadErrorKind::Policy);
        assert!(
            error.message.contains("already running"),
            "{}",
            error.message
        );
        assert_eq!(transport.calls().len(), 0);

        // An abandoned lock is replaced.
        let modified = std::time::SystemTime::now() - Duration::from_secs(7200);
        let file = std::fs::File::options().write(true).open(&lock).unwrap();
        file.set_modified(modified).unwrap();
        let transport = FakeTransport::new(vec![FakeResponse::ok(b"body")]);
        let observer = RecordingObserver::default();
        let outcome = downloader(&transport, &observer)
            .download(&spec(dir.path(), None))
            .expect("stale lock replaced");
        assert_eq!(std::fs::read(outcome.path).unwrap(), b"body");
        assert!(!lock.exists());
    }

    #[test]
    fn stale_gc_removes_abandoned_version_partials() {
        let dir = TempDir::new("gc");
        let stale = PartialMetadata {
            url: "https://example.test/old.rpix".to_owned(),
            version: "0.0.1".to_owned(),
            file_name: "old.rpix".to_owned(),
            etag: None,
            last_modified: None,
            total: None,
            created_at: unix_now().saturating_sub(DEFAULT_PARTIAL_MAX_AGE.as_secs() + 1),
        };
        std::fs::write(dir.path().join("old.rpix.part"), b"old").unwrap();
        std::fs::write(
            dir.path().join("old.rpix.part.json"),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        gc_stale_partials(dir.path(), DEFAULT_PARTIAL_MAX_AGE);
        assert!(!dir.path().join("old.rpix.part").exists());
        assert!(!dir.path().join("old.rpix.part.json").exists());

        // Orphan `.part` without metadata is also expired by mtime.
        let orphan = dir.path().join("orphan.rpix.part");
        std::fs::write(&orphan, b"orphan").unwrap();
        let old = std::time::SystemTime::now() - DEFAULT_PARTIAL_MAX_AGE - Duration::from_secs(1);
        std::fs::File::options()
            .write(true)
            .open(&orphan)
            .unwrap()
            .set_modified(old)
            .unwrap();
        gc_stale_partials(dir.path(), DEFAULT_PARTIAL_MAX_AGE);
        assert!(!orphan.exists());
    }

    #[test]
    fn backoff_is_exponential_capped_and_jitterable() {
        let policy = DownloadPolicy {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
            jitter: false,
            ..DownloadPolicy::for_tests()
        };
        assert_eq!(policy.delay_for(1), Duration::from_secs(1));
        assert_eq!(policy.delay_for(2), Duration::from_secs(2));
        assert_eq!(policy.delay_for(3), Duration::from_secs(4));
        assert_eq!(policy.delay_for(4), Duration::from_secs(8));
        assert_eq!(policy.delay_for(5), Duration::from_secs(8));
        let jittered = DownloadPolicy {
            jitter: true,
            ..policy
        };
        let delay = jittered.delay_for(1).as_millis();
        assert!((800..=1200).contains(&delay), "jittered: {delay}");
    }

    #[test]
    fn legacy_error_classification_maps_http_and_transport_failures() {
        assert_eq!(
            classify_legacy_error("download failed: HTTP 404").kind,
            DownloadErrorKind::Http(404)
        );
        assert_eq!(
            classify_legacy_error("connection reset by peer").kind,
            DownloadErrorKind::Reset
        );
        assert_eq!(
            classify_legacy_error("operation timed out").kind,
            DownloadErrorKind::Timeout
        );
        assert!(DownloadErrorKind::Http(429).is_retryable());
        assert!(DownloadErrorKind::Http(503).is_retryable());
        assert!(!DownloadErrorKind::Http(403).is_retryable());
        assert!(!DownloadErrorKind::Integrity.is_retryable());
    }
}
