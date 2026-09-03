//! Defaults and limits mirroring upstream `packages/core/src/constants.ts` @
//! b0111612 plus the pipeline limits inlined in `extract.ts`.

/// `DEFAULT_BROWSER` (constants.ts:3).
pub const DEFAULT_BROWSER: &str = "chrome_145";

/// `DEFAULT_OS` (constants.ts:4).
pub const DEFAULT_OS: &str = "windows";

/// `DEFAULT_MAX_CHARS` (constants.ts:5).
pub const DEFAULT_MAX_CHARS: u64 = 50_000;

/// `DEFAULT_TIMEOUT_MS` (constants.ts:6).
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// `DEFAULT_BATCH_CONCURRENCY` (constants.ts:7).
pub const DEFAULT_BATCH_CONCURRENCY: u64 = 8;

/// `DEFAULT_INCLUDE_REPLIES` (constants.ts:8) — "extractors". The plain-bool
/// spelling documents the value; consumed via `IncludeReplies::Extractors`.
#[allow(dead_code)]
pub const DEFAULT_INCLUDE_REPLIES_EXTRACTORS: bool = true;

/// `DEFAULT_ACCEPT_HEADER` (constants.ts:9-10): markdown/html/text requests.
pub const DEFAULT_ACCEPT_HEADER: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

/// `DEFAULT_RAW_ACCEPT_HEADER` (constants.ts:11-12).
pub const DEFAULT_RAW_ACCEPT_HEADER: &str = "text/html,application/xhtml+xml,application/json,application/xml;q=0.9,text/markdown;q=0.8,text/plain;q=0.8,*/*;q=0.7";

/// `DEFAULT_JSON_ACCEPT_HEADER` (constants.ts:13-14).
pub const DEFAULT_JSON_ACCEPT_HEADER: &str =
    "application/json,text/json,application/ld+json;q=0.9,text/plain;q=0.8,*/*;q=0.7";

/// `DEFAULT_ACCEPT_LANGUAGE_HEADER` (constants.ts:15).
pub const DEFAULT_ACCEPT_LANGUAGE_HEADER: &str = "en-US,en;q=0.9";

/// `HTML_CONTENT_TYPES` (extract.ts:64-69): content-type sniff list that
/// routes a response into the HTML extraction branch (substring match).
pub const HTML_CONTENT_TYPES: [&str; 4] = [
    "text/html",
    "application/xhtml+xml",
    "text/plain",
    "text/markdown",
];

/// `MAX_CLIENT_SIDE_REDIRECTS` (extract.ts:71) — meta refresh budget
/// (FR-P1-2 recursion wiring).
pub const MAX_CLIENT_SIDE_REDIRECTS: u32 = 5;

/// `MAX_ALTERNATE_LINK_FALLBACKS` (extract.ts:72) — FR-P1-3 recursion budget.
pub const MAX_ALTERNATE_LINK_FALLBACKS: u32 = 3;

/// `MIN_EXTRACTED_WORDS_BEFORE_ALTERNATE_FALLBACK` (extract.ts:73) — the
/// thin-content threshold.
pub const MIN_EXTRACTED_WORDS_BEFORE_ALTERNATE_FALLBACK: u64 = 30;

/// Meta refresh scan window (extract.ts:979 `body.slice(0, 4096)`).
pub const META_REFRESH_SCAN_WINDOW: usize = 4096;

/// Default temp download subdirectory. Upstream: `join(tmpdir(),
/// "smart-fetch-pi")` (settings.ts:176); rpi derives the name from the product
/// identifier per ADR-0001 — declared [VARIANT] in requirements §3.
pub const DEFAULT_TEMP_DIR_NAME: &str = "smart-fetch-rpi";

// ---------------------------------------------------------------------------
// V13-04 progress throttling (audit #4: every-chunk progress push across an
// FFI boundary). Upstream emits per event with a process-internal callback
// (extract.ts:1218 / tool.ts:246), so it needs no throttle; rpi's
// `UpdateSink::push` pays serialization + synchronous FFI + host dispatch
// per frame — see deviation TE-D37.
// ---------------------------------------------------------------------------

/// Minimum interval between PUSHED progress frames (FR-A R1) — 100ms, the
/// same order as the spinner clock (index.ts `SPINNER_INTERVAL`).
#[allow(dead_code)]
pub const PROGRESS_MIN_INTERVAL_MS: std::time::Duration = std::time::Duration::from_millis(100);

/// Minimum NEW bytes between PUSHED progress frames (FR-A R1): a fast wire
/// with big chunks must not exceed the time gate alone, a slow wire with
/// tiny chunks must not starve.
#[allow(dead_code)]
pub const PROGRESS_MIN_DELTA_BYTES: u64 = 64 * 1024;

/// Batch snapshot dirty-check granularity (FR-B R4): progress rounded to 1%
/// — tiny fractional jitter is not a visible change (the rendered bars are
/// percentage-scaled).
#[allow(dead_code)]
pub const PROGRESS_GRANULARITY: f64 = 0.01;
