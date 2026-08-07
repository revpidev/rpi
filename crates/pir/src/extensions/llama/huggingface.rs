//! Port of `packages/coding-agent/src/extensions/llama/huggingface.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! `HuggingFaceClient` (search/details), `findHuggingFaceToken` and the
//! quantization extraction (`QUANTIZATION_PATTERN`, shard suffix, mmproj
//! exclusion). `AbortSignal` becomes `tokio_util::sync::CancellationToken`;
//! the 15s request timeout stays on the HTTP request.
//!
//! Intentional differences:
//! - `findHuggingFaceToken` takes an injected env map and home directory
//!   instead of `process.env`/`os.homedir()` (tests use a temporary HOME;
//!   helpers [`process_env`]/[`default_home_dir`] provide the real values).
//! - `file.size` is read as an integer (`as_i64`); a non-integral JSON
//!   number counts as a missing size where upstream's `typeof === "number"`
//!   would add it (sizes are byte counts in practice).
//! - `quantizations` sort uses plain string ordering instead of
//!   `localeCompare` (quantization names are ASCII).
//! - The token never appears in errors or logs: it is only set as a request
//!   header, and client structs do not derive `Debug`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use reqwest::header::AUTHORIZATION;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::client::LlamaError;

/// `DEFAULT_HUGGING_FACE_URL` (huggingface.ts:5).
pub const DEFAULT_HUGGING_FACE_URL: &str = "https://huggingface.co";

/// `QUANTIZATION_PATTERN` (huggingface.ts:6-7), case-insensitive.
static QUANTIZATION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[-_.])((?:UD-)?(?:IQ\d(?:_[A-Z0-9]+)+|Q\d(?:_[A-Z0-9]+)+|BF16|F16|F32|MXFP\d(?:_[A-Z0-9]+)*))$",
    )
    .expect("QUANTIZATION_PATTERN is verified valid at development time")
});

/// `SHARD_SUFFIX_PATTERN` (huggingface.ts:8).
static SHARD_SUFFIX_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-\d{5}-of-\d{5}$")
        .expect("SHARD_SUFFIX_PATTERN is verified valid at development time")
});

/// Rate-limit delay header (`retry-after` or `ratelimit` `t=…`).
static RATE_LIMIT_T_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|;)t=(\d+)")
        .expect("RATE_LIMIT_T_PATTERN is verified valid at development time")
});

/// Per-request timeout (upstream `AbortSignal.timeout(15_000)`).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// `HuggingFaceModel` (huggingface.ts:10-13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuggingFaceModel {
    pub id: String,
    pub downloads: i64,
}

/// `HuggingFaceQuantization` (huggingface.ts:15-18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuggingFaceQuantization {
    pub name: String,
    pub size: Option<i64>,
}

/// `HuggingFaceModelDetails.gated` — `false | "auto" | "manual"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatedAccess {
    NotGated,
    Auto,
    Manual,
}

impl GatedAccess {
    /// Truthiness of the upstream value: `"auto"`/`"manual"` are gated.
    pub fn is_gated(self) -> bool {
        !matches!(self, GatedAccess::NotGated)
    }

    pub fn is_manual(self) -> bool {
        matches!(self, GatedAccess::Manual)
    }
}

/// `HuggingFaceModelDetails` (huggingface.ts:20-24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuggingFaceModelDetails {
    pub id: String,
    pub gated: GatedAccess,
    pub quantizations: Vec<HuggingFaceQuantization>,
}

/// `payloadError(payload, fallback)` (huggingface.ts:26-30).
fn payload_error(payload: &Value, fallback: &str) -> String {
    payload
        .get("error")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| fallback.to_owned())
}

/// `parseRateLimitDelay(value)` (huggingface.ts:32-35).
fn parse_rate_limit_delay(value: Option<&str>) -> Option<u64> {
    value
        .and_then(|value| RATE_LIMIT_T_PATTERN.captures(value))
        .and_then(|captures| captures.get(1))
        .and_then(|delay| delay.as_str().parse().ok())
}

/// `readToken(path)` (huggingface.ts:37-44).
async fn read_token(path: &Path) -> Option<String> {
    let Ok(token) = tokio::fs::read_to_string(path).await else {
        return None;
    };
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_owned())
    }
}

/// Process environment as a map (the `NodeJS.ProcessEnv` stand-in).
pub fn process_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

/// The user's home directory (`os.homedir()`; on Unix `$HOME`).
pub fn default_home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `findHuggingFaceToken(env, home)` (huggingface.ts:46-61): `HF_TOKEN`
/// first, then `$HF_TOKEN_PATH`, `$HF_HOME/token`, `$XDG_CACHE_HOME/
/// huggingface/token`, `~/.cache/huggingface/token` (deduplicated, first
/// non-empty trimmed content wins). The token is returned, never logged.
pub async fn find_hugging_face_token(env: &HashMap<String, String>, home: &Path) -> Option<String> {
    let from_environment = env
        .get("HF_TOKEN")
        .map(|token| token.trim())
        .filter(|token| !token.is_empty());
    if let Some(token) = from_environment {
        return Some(token.to_owned());
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(path) = env.get("HF_TOKEN_PATH") {
        paths.push(PathBuf::from(path));
    }
    if let Some(hf_home) = env.get("HF_HOME") {
        paths.push(PathBuf::from(hf_home).join("token"));
    }
    if let Some(xdg_cache) = env.get("XDG_CACHE_HOME") {
        paths.push(PathBuf::from(xdg_cache).join("huggingface").join("token"));
    }
    paths.push(home.join(".cache").join("huggingface").join("token"));
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        if !seen.insert(path.clone()) {
            continue;
        }
        if let Some(token) = read_token(&path).await {
            return Some(token);
        }
    }
    None
}

/// `HuggingFaceClient` (huggingface.ts:63-157).
#[derive(Clone)]
pub struct HuggingFaceClient {
    token: Option<String>,
    base_url: String,
    http: reqwest::Client,
}

impl HuggingFaceClient {
    /// Constructor with an injectable base URL (tests point it at a
    /// loopback server).
    pub fn new(token: Option<String>, base_url: impl Into<String>) -> Self {
        HuggingFaceClient {
            token,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
        }
    }

    /// `request(path, signal)` (huggingface.ts:72-98): bearer token header,
    /// 15s timeout, JSON parse failure → `Null` (upstream `undefined`),
    /// 429 → rate-limit error with `retry-after`/`ratelimit` delay, other
    /// non-ok → `payloadError(payload, "Hugging Face returned HTTP
    /// {status}")`.
    async fn request(
        &self,
        path: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<Value, LlamaError> {
        let mut request = self.http.get(format!("{}{}", self.base_url, path));
        if let Some(token) = &self.token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let response =
            super::client::send_with_signal(request.timeout(REQUEST_TIMEOUT), signal).await?;
        let status = response.status();
        // reqwest consumes the response on `json()` — capture the rate-limit
        // headers first (upstream reads them off the Response after parsing).
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|delay| *delay > 0);
        let rate_limit = response
            .headers()
            .get("ratelimit")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let payload = response.json::<Value>().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let fallback = format!("Hugging Face returned HTTP {}", status.as_u16());
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let delay = retry_after.or_else(|| {
                    parse_rate_limit_delay(rate_limit.as_deref()).filter(|delay| *delay > 0)
                });
                return Err(LlamaError::new(match delay {
                    Some(delay) => format!("Hugging Face rate limit reached; retry in {delay}s"),
                    None => "Hugging Face rate limit reached".to_owned(),
                }));
            }
            return Err(LlamaError::new(payload_error(&payload, &fallback)));
        }
        Ok(payload)
    }

    /// `search(query, signal)` (huggingface.ts:100-116).
    pub async fn search(
        &self,
        query: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<Vec<HuggingFaceModel>, LlamaError> {
        let mut url = url::Url::parse(&format!("{}/api/models", self.base_url))
            .map_err(|error| LlamaError::new(error.to_string()))?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("search", query);
            pairs.append_pair("filter", "gguf");
            pairs.append_pair("sort", "downloads");
            pairs.append_pair("direction", "-1");
            pairs.append_pair("limit", "20");
        }
        // `request` prepends the base URL — hand over path + query only.
        let path = format!(
            "{}{}",
            url.path(),
            url.query()
                .map(|query| format!("?{query}"))
                .unwrap_or_default()
        );
        let payload = self.request(&path, signal).await?;
        let Some(results) = payload.as_array() else {
            return Err(LlamaError::new(
                "Hugging Face returned invalid search results",
            ));
        };
        let mut models = Vec::new();
        for value in results {
            if !value.is_object() {
                continue;
            }
            let Some(id) = value.get("id").and_then(Value::as_str) else {
                continue;
            };
            let downloads = value.get("downloads").and_then(Value::as_i64).unwrap_or(0);
            models.push(HuggingFaceModel {
                id: id.to_owned(),
                downloads,
            });
        }
        Ok(models)
    }

    /// `details(id, signal)` (huggingface.ts:118-157).
    pub async fn details(
        &self,
        id: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<HuggingFaceModelDetails, LlamaError> {
        let encoded_id = id
            .split('/')
            .map(crate::core::remote_catalog_provider::encode_uri_component)
            .collect::<Vec<_>>()
            .join("/");
        let payload = self
            .request(&format!("/api/models/{encoded_id}?blobs=true"), signal)
            .await?;
        if !payload.is_object() {
            return Err(LlamaError::new(
                "Hugging Face returned invalid model details",
            ));
        }
        let mut sizes: HashMap<String, (i64, bool)> = HashMap::new();
        if let Some(siblings) = payload.get("siblings").and_then(Value::as_array) {
            for value in siblings {
                if !value.is_object() {
                    continue;
                }
                let Some(rfilename) = value.get("rfilename").and_then(Value::as_str) else {
                    continue;
                };
                if !rfilename.to_lowercase().ends_with(".gguf") {
                    continue;
                }
                let filename = rfilename.rsplit('/').next().unwrap_or("");
                if filename.to_lowercase().starts_with("mmproj") {
                    continue;
                }
                let stem = filename[..filename.len() - ".gguf".len()].to_owned();
                let stem = SHARD_SUFFIX_PATTERN.replace(&stem, "");
                let Some(quantization) = QUANTIZATION_PATTERN
                    .captures(&stem)
                    .and_then(|captures| captures.get(1))
                    .map(|m| m.as_str().to_uppercase())
                else {
                    continue;
                };
                let entry = sizes.entry(quantization).or_insert((0, true));
                match value.get("size").and_then(Value::as_i64) {
                    Some(size) => entry.0 += size,
                    None => entry.1 = false,
                }
            }
        }
        let mut quantizations: Vec<HuggingFaceQuantization> = sizes
            .into_iter()
            .map(|(name, (total, complete))| HuggingFaceQuantization {
                name,
                size: if complete { Some(total) } else { None },
            })
            .collect();
        quantizations.sort_by(|left, right| {
            if left.name == "Q4_K_M" {
                return std::cmp::Ordering::Less;
            }
            if right.name == "Q4_K_M" {
                return std::cmp::Ordering::Greater;
            }
            left.size
                .unwrap_or(i64::MAX)
                .cmp(&right.size.unwrap_or(i64::MAX))
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(HuggingFaceModelDetails {
            id: payload
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| id.to_owned()),
            gated: match payload.get("gated").and_then(Value::as_str) {
                Some("auto") => GatedAccess::Auto,
                Some("manual") => GatedAccess::Manual,
                _ => GatedAccess::NotGated,
            },
            quantizations,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "pir-llama-hf-test-{}-{nanos}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// Upstream: `findHuggingFaceToken({ HF_TOKEN: " hf-secret " })` →
    /// `"hf-secret"` (llama-extension.test.ts "searches Hugging Face…").
    #[tokio::test]
    async fn finds_token_from_environment_first() {
        let home = TempDir::new();
        let token = find_hugging_face_token(&env(&[("HF_TOKEN", " hf-secret ")]), &home.0).await;
        assert_eq!(token.as_deref(), Some("hf-secret"));
    }

    /// The lookup chain (huggingface.ts:46-61): HF_TOKEN_PATH, HF_HOME,
    /// XDG_CACHE_HOME, then `~/.cache/huggingface/token`.
    #[tokio::test]
    async fn finds_token_through_the_path_chain() {
        let home = TempDir::new();

        // HF_TOKEN_PATH wins over the other file locations.
        let token_file = home.0.join("token-file");
        std::fs::write(&token_file, "from-path\n").expect("write");
        let found = find_hugging_face_token(
            &env(&[("HF_TOKEN_PATH", token_file.to_str().expect("utf8"))]),
            &home.0,
        )
        .await;
        assert_eq!(found.as_deref(), Some("from-path"));

        // HF_HOME/token.
        let hf_home = home.0.join("hf-home");
        std::fs::create_dir_all(&hf_home).expect("mkdir");
        std::fs::write(hf_home.join("token"), "from-hf-home").expect("write");
        let found = find_hugging_face_token(
            &env(&[("HF_HOME", hf_home.to_str().expect("utf8"))]),
            &home.0,
        )
        .await;
        assert_eq!(found.as_deref(), Some("from-hf-home"));

        // XDG_CACHE_HOME/huggingface/token.
        let xdg = home.0.join("xdg");
        std::fs::create_dir_all(xdg.join("huggingface")).expect("mkdir");
        std::fs::write(xdg.join("huggingface").join("token"), " from-xdg ").expect("write");
        let found = find_hugging_face_token(
            &env(&[("XDG_CACHE_HOME", xdg.to_str().expect("utf8"))]),
            &home.0,
        )
        .await;
        assert_eq!(found.as_deref(), Some("from-xdg"));

        // Fallback: ~/.cache/huggingface/token.
        let cache = home.0.join(".cache").join("huggingface");
        std::fs::create_dir_all(&cache).expect("mkdir");
        std::fs::write(cache.join("token"), "from-home-cache").expect("write");
        let found = find_hugging_face_token(&env(&[]), &home.0).await;
        assert_eq!(found.as_deref(), Some("from-home-cache"));
    }

    /// Empty/whitespace files and empty HF_TOKEN are skipped
    /// (huggingface.ts:37-48).
    #[tokio::test]
    async fn skips_empty_tokens() {
        let home = TempDir::new();
        let token_file = home.0.join("empty");
        std::fs::write(&token_file, "   \n").expect("write");
        let found = find_hugging_face_token(
            &env(&[
                ("HF_TOKEN", "  "),
                ("HF_TOKEN_PATH", token_file.to_str().expect("utf8")),
            ]),
            &home.0,
        )
        .await;
        assert_eq!(found, None);
    }
}
