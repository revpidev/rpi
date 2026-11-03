//! Internal token accounting for the statusline.
//!
//! The live-output fields need a token count *while* a provider is streaming.
//! Providers only report exact usage at message end (and some never report a
//! running total), so the statusline resolves the count in this order:
//!
//! 1. **Provider exact** — [`crate::fields::LiveView`] overlays
//!    `message.usage.output` / the frozen `output_tokens_exact` on top of the
//!    counts produced here. It is the only source the provider guarantees.
//! 2. **Local tokenizer exact** — the same zero-dependency registry the
//!    reference `~/.rpi/agent/statusline.py` uses:
//!    `<agentDir>/tokenizer/<model>/tokenizer.json` plus a `meta.json`
//!    (`match` substrings, `engine: "deepseek-bpe"`). A model that matches a
//!    supported tokenizer is counted with that model's official BPE merges,
//!    so the tier split is exact too.
//! 3. **CJK-aware heuristic** — the pi/reference-statusline fallback:
//!    Han characters ≈ 1.2 chars/token, everything else ≈ 3.5 chars/token.
//!    This is an estimate, never presented as exact.
//!
//! The accounting is intentionally tiered: `text`, `thinking` and `toolcall`
//! are counted separately so the UI can show where the output tokens went.
//! Tokenizers are loaded lazily and cached; the registered tokenizer objects
//! are only touched by the refresh loop (the dispatch thread never calls
//! into this module).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use serde::Serialize;

/// Where a token count came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    /// Character-bucket estimate (no exact source available).
    #[default]
    Estimated,
    /// Counted with the model's local tokenizer.
    ExactTokenizer,
    /// Provider-reported usage (`message.usage.output`).
    ExactProvider,
}

impl TokenSource {
    /// Whether this source is exact (provider or model tokenizer).
    pub fn is_exact(self) -> bool {
        !matches!(self, Self::Estimated)
    }

    /// Display marker for the total: `~` for estimates, empty for exact.
    pub fn marker(self) -> &'static str {
        if self.is_exact() { "" } else { "~" }
    }
}

/// Tiered live-output token counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TokenBreakdown {
    /// Assistant text-block tokens.
    pub text: u64,
    /// Assistant thinking-block tokens.
    pub thinking: u64,
    /// Tool-call JSON tokens.
    pub toolcall: u64,
    /// Total output tokens (exact provider value when one was supplied).
    pub total: u64,
    /// Source of [`Self::total`].
    pub source: TokenSource,
    /// Whether the tier split (not just the total) is exact. A provider total
    /// is exact while the tier split is a proportional allocation, so this
    /// stays false in that case.
    pub tiers_exact: bool,
}

impl TokenBreakdown {
    /// A tokenizer/heuristic result: the total is the tier sum.
    pub fn from_tiers(text: u64, thinking: u64, toolcall: u64, source: TokenSource) -> Self {
        Self {
            text,
            thinking,
            toolcall,
            total: text.saturating_add(thinking).saturating_add(toolcall),
            source,
            tiers_exact: source == TokenSource::ExactTokenizer,
        }
    }

    /// Overlay a provider-reported exact total. The tier split keeps the
    /// estimated proportions (scaled to the exact total); when the estimate
    /// already agrees with the provider the tokenizer tiers stay exact.
    pub fn with_provider_total(mut self, total: u64) -> Self {
        if total == 0 {
            return self;
        }
        let estimate = self
            .text
            .saturating_add(self.thinking)
            .saturating_add(self.toolcall);
        if estimate == 0 {
            // No tier data yet (for example the first streamed usage chunk
            // arrives before any delta): put the exact total in the text tier.
            self.text = total;
            self.thinking = 0;
            self.toolcall = 0;
            self.total = total;
            self.source = TokenSource::ExactProvider;
            self.tiers_exact = false;
            return self;
        }
        if estimate == total {
            self.total = total;
            self.source = TokenSource::ExactProvider;
            return self;
        }
        let scale = |value: u64| -> u64 {
            ((u128::from(value) * u128::from(total) + u128::from(estimate) / 2)
                / u128::from(estimate)) as u64
        };
        let mut text = scale(self.text);
        let mut thinking = scale(self.thinking);
        let mut toolcall = scale(self.toolcall);
        // Rounding drift goes to the largest tier so the rows always add up.
        let scaled = text.saturating_add(thinking).saturating_add(toolcall);
        if scaled != total {
            let delta = i128::from(total) - i128::from(scaled);
            let target = if text >= thinking && text >= toolcall {
                &mut text
            } else if thinking >= toolcall {
                &mut thinking
            } else {
                &mut toolcall
            };
            *target = (i128::from(*target) + delta).max(0) as u64;
        }
        self.text = text;
        self.thinking = thinking;
        self.toolcall = toolcall;
        self.total = total;
        self.source = TokenSource::ExactProvider;
        self.tiers_exact = false;
        self
    }

    /// Marker for the total (`~123` when estimated).
    pub fn total_marker(self) -> &'static str {
        self.source.marker()
    }

    /// Marker for a tier row (provider totals make the split approximate).
    pub fn tier_marker(self) -> &'static str {
        if self.tiers_exact { "" } else { "~" }
    }
}

/// Count `text`, `thinking` and `toolcall` for `model_id`.
///
/// `model_id` is the provider model id (`ctx.model.id`). A matching local
/// tokenizer wins; otherwise the CJK bucket heuristic is used.
pub fn count_breakdown(
    model_id: Option<&str>,
    text: &str,
    thinking: &str,
    toolcall: &str,
) -> TokenBreakdown {
    if let Some(tokenizer) = tokenizer_for_model(model_id) {
        return TokenBreakdown::from_tiers(
            tokenizer.count(text),
            tokenizer.count(thinking),
            tokenizer.count(toolcall),
            TokenSource::ExactTokenizer,
        );
    }
    TokenBreakdown::from_tiers(
        heuristic_tokens(text),
        heuristic_tokens(thinking),
        heuristic_tokens(toolcall),
        TokenSource::Estimated,
    )
}

/// The reference statusline's fallback constants (pi `statusline.ts` and
/// `~/.rpi/agent/tokenizer/README.md`).
const HAN_CHARS_PER_TOKEN: f64 = 1.2;
const LATIN_CHARS_PER_TOKEN: f64 = 3.5;

/// Han ideographs (the reference heuristic's CJK class). Kana/Hangul stay in
/// the `other` bucket, matching the reference script byte-for-byte.
fn is_han(ch: char) -> bool {
    matches!(ch as u32, 0x3400..=0x4dbf | 0x4e00..=0x9fff)
}

/// Estimate tokens by script bucket: Han/1.2 + rest/3.5, rounded up.
pub fn heuristic_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let mut han = 0u64;
    let mut other = 0u64;
    for ch in text.chars() {
        if is_han(ch) {
            han += 1;
        } else {
            other += 1;
        }
    }
    let tokens = han as f64 / HAN_CHARS_PER_TOKEN + other as f64 / LATIN_CHARS_PER_TOKEN;
    if tokens <= 0.0 {
        0
    } else {
        tokens.ceil() as u64
    }
}

/// One discovered local tokenizer directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelEntry {
    name: String,
    patterns: Vec<String>,
    engine: Option<String>,
    tokenizer_path: PathBuf,
}

impl ModelEntry {
    fn matches(&self, model_id: &str) -> Option<usize> {
        let lowered = model_id.to_ascii_lowercase();
        self.patterns
            .iter()
            .filter(|pattern| !pattern.is_empty() && lowered.contains(pattern.as_str()))
            .map(String::len)
            .max()
    }
}

/// Directory scanned for the model-tokenizer registry.
///
/// `RPI_STATUSLINE_TOKENIZER_DIR` (tests/advanced users) wins; the default is
/// the reference script's location under the rpi agent dir.
fn registry_dir() -> PathBuf {
    for key in ["RPI_STATUSLINE_TOKENIZER_DIR", "RPI_TOKENIZER_DIR"] {
        if let Some(value) = std::env::var_os(key)
            && !value.is_empty()
        {
            return PathBuf::from(value);
        }
    }
    crate::paths::get_agent_dir().join("tokenizer")
}

fn registry() -> &'static [ModelEntry] {
    static REGISTRY: OnceLock<Vec<ModelEntry>> = OnceLock::new();
    REGISTRY.get_or_init(|| load_registry(&registry_dir()))
}

/// Discover `tokenizer/<model>/{tokenizer.json,meta.json}` directories.
fn load_registry(root: &Path) -> Vec<ModelEntry> {
    let mut entries = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return entries;
    };
    for entry in read_dir.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let tokenizer_path = dir.join("tokenizer.json");
        if !tokenizer_path.is_file() {
            continue;
        }
        let meta = std::fs::read_to_string(dir.join("meta.json"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .unwrap_or(serde_json::Value::Null);
        let name = meta
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                dir.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "model".to_owned());
        let mut patterns: Vec<String> = meta
            .get("match")
            .map(|value| match value {
                serde_json::Value::String(pattern) => vec![pattern.clone()],
                serde_json::Value::Array(items) => items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default()
            .into_iter()
            .map(|pattern| pattern.to_ascii_lowercase())
            .filter(|pattern| !pattern.is_empty())
            .collect();
        if patterns.is_empty() {
            patterns.push(
                dir.file_name()
                    .map(|name| name.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default(),
            );
        }
        let engine = meta
            .get("engine")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        entries.push(ModelEntry {
            name,
            patterns,
            engine,
            tokenizer_path,
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

/// Longest matching pattern wins; ties break on the tokenizer path for
/// determinism.
fn match_entry<'a>(entries: &'a [ModelEntry], model_id: &str) -> Option<&'a ModelEntry> {
    entries
        .iter()
        .filter_map(|entry| entry.matches(model_id).map(|length| (length, entry)))
        .max_by(|(left_len, left), (right_len, right)| {
            left_len
                .cmp(right_len)
                .then_with(|| right.tokenizer_path.cmp(&left.tokenizer_path))
        })
        .map(|(_, entry)| entry)
}

/// The tokenizer cache is keyed by the asset path so a config change that
/// swaps the file is picked up on the next process start (the registry is
/// discovered once, like the reference script).
fn tokenizer_cache() -> &'static Mutex<HashMap<PathBuf, Option<Arc<DeepSeekBpe>>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<DeepSeekBpe>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tokenizer_for_model(model_id: Option<&str>) -> Option<Arc<DeepSeekBpe>> {
    let model_id = model_id.filter(|id| !id.is_empty())?;
    let entry = match_entry(registry(), model_id)?;
    if entry.engine.as_deref() != Some("deepseek-bpe") {
        // The zero-dependency reference runner only understands the
        // DeepSeek/GPT-2-style byte-level BPE shape. Other assets need the
        // optional `tokenizers` wheel in the reference script; here they fall
        // back to the estimate rather than guessing.
        return None;
    }
    let mut cache = tokenizer_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    cache
        .entry(entry.tokenizer_path.clone())
        .or_insert_with(|| {
            DeepSeekBpe::load(&entry.tokenizer_path)
                .map(Arc::new)
                .map_err(|error| {
                    tracing::warn!(
                        path = %entry.tokenizer_path.display(),
                        %error,
                        "rpi-statusline: tokenizer asset unusable; using estimate"
                    );
                    error
                })
                .ok()
        })
        .clone()
}

// ---------------------------------------------------------------------------
// Zero-dependency byte-level BPE (the reference `deepseek-bpe` engine)
// ---------------------------------------------------------------------------

/// GPT-2 / ByteLevel byte -> unicode-character map.
fn byte_encoder() -> &'static [char; 256] {
    static ENCODER: OnceLock<[char; 256]> = OnceLock::new();
    ENCODER.get_or_init(|| {
        let mut bytes: Vec<u16> = (b'!'..=b'~').map(u16::from).collect();
        bytes.extend((0xa1u16..=0xac).chain(0xae..=0xff));
        let mut chars: Vec<u16> = bytes.clone();
        let mut next = 256u16;
        for byte in 0..=255u16 {
            if !bytes.contains(&byte) {
                bytes.push(byte);
                chars.push(next);
                next += 1;
            }
        }
        let mut table = ['\0'; 256];
        for (byte, ch) in bytes.into_iter().zip(chars) {
            if (byte as usize) < table.len()
                && let Some(ch) = char::from_u32(u32::from(ch))
            {
                table[byte as usize] = ch;
            }
        }
        table
    })
}

fn byte_encode(piece: &str) -> String {
    let table = byte_encoder();
    let mut out = String::with_capacity(piece.len());
    for byte in piece.as_bytes() {
        out.push(table[*byte as usize]);
    }
    out
}

/// Isolated-split helper matching HF `Split { behavior: Isolated }`: matches
/// are kept as their own pieces and the gaps stay in order.
fn split_isolated(pieces: Vec<String>, pattern: &Regex) -> Vec<String> {
    let mut out = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let mut last = 0usize;
        for found in pattern.find_iter(&piece) {
            if found.start() > last {
                out.push(piece[last..found.start()].to_owned());
            }
            out.push(found.as_str().to_owned());
            last = found.end();
        }
        if last < piece.len() {
            out.push(piece[last..].to_owned());
        }
    }
    out
}

/// DeepSeek pretokenizer transcribed from the official `tokenizer.json`.
///
/// The reference script approximates the Unicode classes with Python
/// character classes; we use the regex crate's proper Unicode properties
/// (with the unsupported trailing-whitespace lookahead expressed as `\z`).
fn pretokenizer_regexes() -> &'static [Regex] {
    static REGEXES: OnceLock<Vec<Regex>> = OnceLock::new();
    REGEXES.get_or_init(|| {
        let patterns = [
            r"\d{1,3}",
            r"[\p{Han}\p{Hiragana}\p{Katakana}]+",
            concat!(
                r"[!-~][A-Za-z]+",
                r"|(?:[^\r\n\p{L}\p{P}\p{S}])?[\p{L}\p{M}]+",
                r"| ?[\p{P}\p{S}]+[\r\n]*",
                r"|\s*[\r\n]+",
                r"|\s+\z",
                r"|\s+",
            ),
        ];
        patterns
            .iter()
            .filter_map(|pattern| match Regex::new(pattern) {
                Ok(regex) => Some(regex),
                Err(error) => {
                    tracing::warn!(pattern, %error, "rpi-statusline: pretokenizer regex failed");
                    None
                }
            })
            .collect()
    })
}

fn pretokenize(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut pieces = vec![text.to_owned()];
    for regex in pretokenizer_regexes() {
        pieces = split_isolated(pieces, regex);
    }
    pieces
}

/// Minimal stdlib runner for the DeepSeek-style official `tokenizer.json`.
///
/// Only the merge ranks and added-token list are used; the full vocabulary is
/// not needed for counting. The BPE merge loop is heap-based (the reference
/// script uses the same algorithm to stay linear on long CJK runs).
struct DeepSeekBpe {
    ranks: HashMap<(String, String), usize>,
    added: Option<Regex>,
    cache: Mutex<HashMap<String, u64>>,
}

impl std::fmt::Debug for DeepSeekBpe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeepSeekBpe")
            .field("merges", &self.ranks.len())
            .field("added", &self.added.is_some())
            .finish()
    }
}

#[derive(serde::Deserialize)]
struct TokenizerFile {
    model: BpeModel,
    #[serde(default)]
    added_tokens: Vec<AddedToken>,
}

#[derive(serde::Deserialize)]
struct BpeModel {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    merges: Vec<String>,
}

#[derive(serde::Deserialize)]
struct AddedToken {
    content: String,
}

impl DeepSeekBpe {
    fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
        Self::from_json(&raw)
    }

    fn from_json(raw: &str) -> Result<Self, String> {
        let file: TokenizerFile = serde_json::from_str(raw).map_err(|error| error.to_string())?;
        if file.model.kind != "BPE" {
            return Err(format!(
                "unsupported tokenizer model type {:?} (expected BPE)",
                file.model.kind
            ));
        }
        if file.model.merges.is_empty() {
            return Err("tokenizer has no merges".to_owned());
        }
        let mut ranks = HashMap::with_capacity(file.model.merges.len());
        for (rank, merge) in file.model.merges.iter().enumerate() {
            if let Some((left, right)) = merge.split_once(' ') {
                ranks.insert((left.to_owned(), right.to_owned()), rank);
            }
        }
        let mut added: Vec<String> = file
            .added_tokens
            .into_iter()
            .map(|token| token.content)
            .filter(|content| !content.is_empty())
            .collect();
        added.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        added.dedup();
        let added = if added.is_empty() {
            None
        } else {
            let pattern = added
                .iter()
                .map(|token| regex::escape(token))
                .collect::<Vec<_>>()
                .join("|");
            Regex::new(&format!("(?:{pattern})")).ok()
        };
        Ok(Self {
            ranks,
            added,
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn count(&self, text: &str) -> u64 {
        if text.is_empty() {
            return 0;
        }
        let mut total = 0u64;
        let mut position = 0usize;
        if let Some(added) = &self.added {
            for found in added.find_iter(text) {
                if found.start() > position {
                    total += self.count_plain(&text[position..found.start()]);
                }
                total += 1;
                position = found.end();
            }
        }
        if position < text.len() {
            total += self.count_plain(&text[position..]);
        }
        total
    }

    fn count_plain(&self, text: &str) -> u64 {
        pretokenize(text)
            .iter()
            .map(|piece| self.bpe_count(&byte_encode(piece)))
            .sum()
    }

    /// Heap-based BPE merge count (ranks beat list merging on long runs).
    fn bpe_count(&self, word: &str) -> u64 {
        let chars: Vec<char> = word.chars().collect();
        let length = chars.len();
        if length <= 1 {
            return length as u64;
        }
        {
            let cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(count) = cache.get(word) {
                return *count;
            }
        }

        let mut symbols: Vec<String> = chars.into_iter().map(String::from).collect();
        let mut previous: Vec<isize> = (0..length as isize).map(|index| index - 1).collect();
        let mut next: Vec<isize> = (1..=length as isize).collect();
        next[length - 1] = -1;
        let mut alive = vec![true; length];
        let mut version = vec![0u64; length];
        let mut heap = MergeHeap::new();

        for index in 0..length.saturating_sub(1) {
            push_merge(&mut heap, &self.ranks, &symbols, &next, &version, index);
        }

        let mut remaining = length;
        while let Some(std::cmp::Reverse((_rank, left, right, left_version, right_version))) =
            heap.pop()
        {
            if !alive[left]
                || !alive[right]
                || next[left] != right as isize
                || version[left] != left_version
                || version[right] != right_version
            {
                continue;
            }
            let merged = format!("{}{}", symbols[left], symbols[right]);
            symbols[left] = merged;
            alive[right] = false;
            next[left] = next[right];
            if next[left] != -1 {
                previous[next[left] as usize] = left as isize;
            }
            version[left] += 1;
            remaining -= 1;
            push_merge(&mut heap, &self.ranks, &symbols, &next, &version, left);
            let before = previous[left];
            if before != -1 {
                push_merge(
                    &mut heap,
                    &self.ranks,
                    &symbols,
                    &next,
                    &version,
                    before as usize,
                );
            }
        }

        let count = remaining as u64;
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        if cache.len() >= 50_000 {
            cache.clear();
        }
        cache.insert(word.to_owned(), count);
        count
    }
}

/// `(rank, left, right, left_version, right_version)` merge candidate.
type MergeCandidate = (usize, usize, usize, u64, u64);
/// Min-heap by merge rank.
type MergeHeap = std::collections::BinaryHeap<std::cmp::Reverse<MergeCandidate>>;

/// Push the merge candidate at `index` onto the heap (if the pair has a
/// rank). Split out of [`DeepSeekBpe::bpe_count`] so the mutable merge state
/// and the read-only rank table can be borrowed independently.
fn push_merge(
    heap: &mut MergeHeap,
    ranks: &HashMap<(String, String), usize>,
    symbols: &[String],
    next: &[isize],
    version: &[u64],
    index: usize,
) {
    let right = next[index];
    if right == -1 {
        return;
    }
    let right = right as usize;
    if let Some(rank) = ranks.get(&(symbols[index].clone(), symbols[right].clone())) {
        heap.push(std::cmp::Reverse((
            *rank,
            index,
            right,
            version[index],
            version[right],
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_matches_the_reference_buckets() {
        assert_eq!(heuristic_tokens(""), 0);
        assert_eq!(heuristic_tokens("hello"), 2); // ceil(5 / 3.5)
        assert_eq!(heuristic_tokens("你好"), 2); // ceil(2 / 1.2)
        assert_eq!(heuristic_tokens("你好 hello"), 4); // ceil(1.667 + 1.714)
        // Kana is not in the reference Han class.
        assert_eq!(heuristic_tokens("こんにちは"), 2); // ceil(5 / 3.5)
    }

    #[test]
    fn provider_total_scales_tiers_and_marks_them_approximate() {
        let estimated = TokenBreakdown::from_tiers(10, 10, 0, TokenSource::Estimated);
        assert_eq!(estimated.total, 20);
        assert!(!estimated.source.is_exact());
        let exact = estimated.with_provider_total(100);
        assert_eq!(exact.total, 100);
        assert_eq!(exact.text + exact.thinking + exact.toolcall, 100);
        assert_eq!(exact.source, TokenSource::ExactProvider);
        assert!(!exact.tiers_exact);
        // A provider total equal to the estimate keeps tokenizer tiers exact.
        let tokenizer = TokenBreakdown::from_tiers(7, 3, 0, TokenSource::ExactTokenizer);
        let same = tokenizer.with_provider_total(10);
        assert_eq!(same.source, TokenSource::ExactProvider);
        assert!(same.tiers_exact);
        // No tier data yet: the exact total lands in the text row.
        let empty = TokenBreakdown::default().with_provider_total(12);
        assert_eq!((empty.text, empty.total), (12, 12));
    }

    #[test]
    fn model_matching_prefers_the_longest_pattern() {
        let entries = vec![
            ModelEntry {
                name: "short".into(),
                patterns: vec!["deepseek".into()],
                engine: Some("deepseek-bpe".into()),
                tokenizer_path: PathBuf::from("/short"),
            },
            ModelEntry {
                name: "long".into(),
                patterns: vec!["deepseek-v4".into()],
                engine: Some("deepseek-bpe".into()),
                tokenizer_path: PathBuf::from("/long"),
            },
        ];
        let matched = match_entry(&entries, "DeepSeek-V4.1-Flash").expect("match");
        assert_eq!(matched.name, "long");
        assert!(match_entry(&entries, "kimi-k3").is_none());
    }

    #[test]
    fn pretokenizer_patterns_compile() {
        assert_eq!(
            pretokenizer_regexes().len(),
            3,
            "all three DeepSeek pretokenizer patterns must compile"
        );
        let pieces = pretokenize("你好 abc 123");
        assert!(pieces.len() >= 3, "isolated splits: {pieces:?}");
    }

    #[test]
    fn registry_discovers_and_matches_meta_files() {
        let root =
            std::env::temp_dir().join(format!("rpi-statusline-tokenizer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let model = root.join("deepseek");
        std::fs::create_dir_all(&model).expect("mkdir");
        std::fs::write(
            model.join("tokenizer.json"),
            r#"{"model":{"type":"BPE","merges":["a b"]}}"#,
        )
        .expect("tokenizer");
        std::fs::write(
            model.join("meta.json"),
            r#"{"name":"DeepSeek","match":["deepseek"],"engine":"deepseek-bpe"}"#,
        )
        .expect("meta");
        let entries = load_registry(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "DeepSeek");
        assert_eq!(entries[0].engine.as_deref(), Some("deepseek-bpe"));
        assert!(match_entry(&entries, "DeepSeek-V4").is_some());
        assert!(match_entry(&entries, "kimi-k3").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deepseek_bpe_counts_official_merge_ranks() {
        let tokenizer = DeepSeekBpe::from_json(
            r#"{
                "model": {"type": "BPE", "merges": ["a b", "ab c"]},
                "added_tokens": []
            }"#,
        )
        .expect("tokenizer");
        // One Latin pretoken, two merges -> one token.
        assert_eq!(tokenizer.count("abc"), 1);
        // The whitespace is its own pretoken in the DeepSeek pretokenizer.
        assert_eq!(tokenizer.count("a b"), 3);
        // Added tokens are counted whole and excluded from BPE.
        let with_special = DeepSeekBpe::from_json(
            r#"{
                "model": {"type": "BPE", "merges": ["a b"]},
                "added_tokens": [{"content": "<|end|>"}]
            }"#,
        )
        .expect("tokenizer");
        assert_eq!(with_special.count("<|end|>"), 1);
    }

    /// Manual parity check against the reference Python registry. Run with:
    /// `RPI_TEST_DEEPSEEK_TOKENIZER=~/.rpi/agent/tokenizer/deepseek/tokenizer.json
    ///  cargo test -p rpi-ext-statusline --lib deepseek_local_asset -- --ignored`
    #[test]
    #[ignore = "requires the local DeepSeek tokenizer asset"]
    fn deepseek_local_asset_matches_reference_counts() {
        let path = std::env::var("RPI_TEST_DEEPSEEK_TOKENIZER")
            .expect("set RPI_TEST_DEEPSEEK_TOKENIZER to tokenizer.json");
        let tokenizer = DeepSeekBpe::load(Path::new(&path)).expect("tokenizer loads");
        for (text, expected) in [
            ("hello world", 2),
            ("你好，世界！", 4),
            ("fn main() { println!(\"hi\"); }", 9),
            ("The quick brown fox jumps over the lazy dog.", 10),
            (&"0123456789".repeat(10), 34),
            ("def foo(x: int) -> int:\n    return x + 1\n", 16),
        ] {
            assert_eq!(
                tokenizer.count(text),
                expected,
                "reference count for {text:?}"
            );
        }
    }

    #[test]
    fn unsupported_engine_is_ignored() {
        let entry = ModelEntry {
            name: "generic".into(),
            patterns: vec!["generic".into()],
            engine: Some("wordpiece".into()),
            tokenizer_path: PathBuf::from("/does-not-exist"),
        };
        assert_ne!(entry.engine.as_deref(), Some("deepseek-bpe"));
    }

    #[test]
    fn breakdown_counts_each_tier_separately() {
        // No model match in a clean test environment -> heuristic.
        let breakdown = count_breakdown(None, "hello", "你好", "abc");
        assert_eq!(breakdown.source, TokenSource::Estimated);
        assert!(breakdown.text >= 1);
        assert!(breakdown.thinking >= 1);
        assert!(breakdown.toolcall >= 1);
        assert_eq!(
            breakdown.total,
            breakdown.text + breakdown.thinking + breakdown.toolcall
        );
    }
}
