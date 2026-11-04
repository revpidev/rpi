//! Internal token accounting for the statusline.
//!
//! The live-output fields need a token count *while* a provider is streaming.
//! Providers only report exact usage at message end (and some never report a
//! running total), so the statusline resolves the count in this order:
//!
//! 1. **Provider exact** — [`crate::fields::LiveView`] overlays
//!    `message.usage.output` / the frozen `output_tokens_exact` on top of the
//!    counts produced here. It is the only source the provider guarantees.
//! 2. **Bundled tokenizer** — the official tokenizer tables of the supported
//!    open model families (DeepSeek, Kimi, GLM, Qwen, Hunyuan, MiniMax) are
//!    converted to a compact binary by `scripts/gen-statusline-tokenizers.py`
//!    and embedded with `include_bytes!`. The plugin does not read a
//!    user-local tokenizer registry, so counting works on every install.
//! 3. **CJK-aware heuristic** — the pi/reference-statusline fallback:
//!    Han characters ≈ 1.2 chars/token, everything else ≈ 3.5 chars/token.
//!    This is an estimate, never presented as exact.
//!
//! The accounting is intentionally tiered: `text`, `thinking` and `toolcall`
//! are counted separately so the UI can show where the output tokens went.
//! Assets are loaded lazily per model family and cached; the registered
//! tokenizer objects are only touched by the refresh loop (the dispatch
//! thread never calls into this module).

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use fancy_regex::Regex as FancyRegex;
use regex::Regex as AddedTokenRegex;
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

/// Where a token count came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    /// Character-bucket estimate (no exact source available).
    #[default]
    Estimated,
    /// Counted with a bundled model tokenizer.
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
/// `model_id` is the provider model id (`ctx.model.id`). A bundled tokenizer
/// for the model family wins; otherwise the CJK bucket heuristic is used.
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

/// The reference statusline's fallback constants (pi `statusline.ts`).
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

// ---------------------------------------------------------------------------
// Bundled tokenizer assets
// ---------------------------------------------------------------------------

/// One bundled tokenizer family. The asset bytes are generated by
/// `scripts/gen-statusline-tokenizers.py` (see the crate-root `NOTICE.md`
/// for the pinned upstream sources and licenses).
struct BuiltinTokenizer {
    /// Stable family id (the cache key).
    id: &'static str,
    /// Lower-case substrings matched against `ctx.model.id`.
    patterns: &'static [&'static str],
    /// zstd-compressed compact tokenizer table.
    asset: &'static [u8],
}

const BUILTINS: &[BuiltinTokenizer] = &[
    BuiltinTokenizer {
        id: "deepseek",
        patterns: &["deepseek"],
        asset: include_bytes!("../assets/tokenizers/deepseek.tok.zst"),
    },
    BuiltinTokenizer {
        id: "kimi",
        patterns: &["kimi", "moonshot"],
        asset: include_bytes!("../assets/tokenizers/kimi.tok.zst"),
    },
    BuiltinTokenizer {
        id: "glm",
        patterns: &["glm"],
        asset: include_bytes!("../assets/tokenizers/glm.tok.zst"),
    },
    BuiltinTokenizer {
        id: "qwen",
        patterns: &["qwen", "qwq"],
        asset: include_bytes!("../assets/tokenizers/qwen.tok.zst"),
    },
    // `hy3`/`hy4` are the Tencent Hunyuan ids in the current catalog. The
    // pinned table is the Hunyuan-A13B tokenizer, the newest published
    // tokenizer of that family at asset-generation time.
    BuiltinTokenizer {
        id: "hunyuan",
        patterns: &["hy3", "hy4"],
        asset: include_bytes!("../assets/tokenizers/hunyuan.tok.zst"),
    },
    BuiltinTokenizer {
        id: "minimax",
        patterns: &["minimax"],
        asset: include_bytes!("../assets/tokenizers/minimax.tok.zst"),
    },
];

/// The asset format magic (`RPI-TOK1`).
const ASSET_MAGIC: &[u8; 8] = b"RPI-TOK1";

/// Longest matching family pattern wins; ties keep the builtin order.
fn builtin_for_model(model_id: &str) -> Option<&'static BuiltinTokenizer> {
    let lowered = model_id.to_ascii_lowercase();
    let mut best: Option<(usize, &'static BuiltinTokenizer)> = None;
    for builtin in BUILTINS {
        for pattern in builtin.patterns {
            if !lowered.contains(pattern) {
                continue;
            }
            if best.is_none_or(|(length, _)| pattern.len() > length) {
                best = Some((pattern.len(), builtin));
            }
        }
    }
    best.map(|(_, builtin)| builtin)
}

/// Parsed tokenizer cache, keyed by family id. `None` means the asset failed
/// to load (logged once); the heuristic stays in charge for that family.
///
/// Only the active family is kept parsed: switching models across families
/// drops the previous table (a table can retain tens of MB of merge/vocab
/// structures), and switching back reloads it on the next count.
type TokenizerCache = Mutex<HashMap<&'static str, Option<Arc<Tokenizer>>>>;

fn tokenizer_cache() -> &'static TokenizerCache {
    static CACHE: OnceLock<TokenizerCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tokenizer_for_model(model_id: Option<&str>) -> Option<Arc<Tokenizer>> {
    let model_id = model_id.filter(|id| !id.is_empty())?;
    let builtin = builtin_for_model(model_id)?;
    {
        let cache = tokenizer_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = cache.get(builtin.id) {
            return entry.clone();
        }
    }
    let loaded = match parse_asset(builtin.asset) {
        Ok(tokenizer) => Some(Arc::new(tokenizer)),
        Err(error) => {
            tracing::warn!(
                family = builtin.id,
                %error,
                "rpi-statusline: bundled tokenizer asset unusable; using estimate"
            );
            None
        }
    };
    let mut cache = tokenizer_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    cache.retain(|id, _| *id == builtin.id);
    cache.insert(builtin.id, loaded.clone());
    loaded
}

// ---------------------------------------------------------------------------
// Compact asset parsing
// ---------------------------------------------------------------------------

/// One parsed tokenizer: the pretokenizer chain, the added-token matcher, the
/// normalizer and the BPE engine.
struct Tokenizer {
    normalizer: NormalizerKind,
    pretokenizer: Vec<FancyRegex>,
    added: Option<AddedTokenMatcher>,
    base_ids: [u32; 256],
    engine: Engine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NormalizerKind {
    None,
    Nfc,
}

enum Engine {
    /// HF `tokenizers` BPE table: pair ranks plus the merged token id.
    Hf(HfBpe),
    /// tiktoken `mergeable_ranks`: rank of the concatenated token bytes.
    Tiktoken(TiktokenBpe),
}

struct HfBpe {
    /// Keyed by `(left_id << 32) | right_id`; the value is the merge rank.
    pairs: HashMap<u64, u32>,
    /// Merge rank → the resulting token id.
    results: Vec<u32>,
    /// Whole-token membership for `ignore_merges` models (GLM/Hunyuan).
    vocab: Option<HashSet<Box<str>>>,
}

struct TiktokenBpe {
    /// Rank → token bytes (the tiktoken vocabulary order).
    tokens: Vec<Box<[u8]>>,
    /// Token bytes → rank, for the concatenated-pair lookup.
    ranks: HashMap<Box<[u8]>, u32>,
}

fn pair_key(left: u32, right: u32) -> u64 {
    (u64::from(left) << 32) | u64::from(right)
}

/// A bounds-checked little-endian reader over the uncompressed asset.
struct AssetReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> AssetReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| "asset offset overflow".to_owned())?;
        let slice = self
            .data
            .get(self.position..end)
            .ok_or_else(|| format!("asset truncated at {} (need {length} bytes)", self.position))?;
        self.position = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    fn string(&mut self) -> Result<&'a str, String> {
        let bytes = self.bytes()?;
        std::str::from_utf8(bytes).map_err(|error| format!("asset string is not UTF-8: {error}"))
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.position)
    }
}

fn parse_asset(compressed: &[u8]) -> Result<Tokenizer, String> {
    let raw = zstd::stream::decode_all(std::io::Cursor::new(compressed))
        .map_err(|error| format!("zstd decode failed: {error}"))?;
    let mut reader = AssetReader::new(&raw);
    let magic = reader.take(ASSET_MAGIC.len())?;
    if magic != ASSET_MAGIC {
        return Err("bad asset magic (expected RPI-TOK1)".to_owned());
    }
    let engine_kind = reader.u8()?;
    let flags = reader.u8()?;
    let normalizer = match reader.u8()? {
        0 => NormalizerKind::None,
        1 => NormalizerKind::Nfc,
        other => return Err(format!("unknown normalizer id {other}")),
    };
    let _reserved = reader.u8()?;
    if flags & !1 != 0 {
        return Err(format!("unknown asset flags 0x{flags:02x}"));
    }

    let pattern_count = reader.u16()? as usize;
    let mut pretokenizer = Vec::with_capacity(pattern_count);
    for _ in 0..pattern_count {
        let behavior = reader.u8()?;
        let invert = reader.u8()?;
        let _reserved = reader.u16()?;
        let pattern = reader.string()?.to_owned();
        // The generator canonicalizes the tiktoken `(Removed, invert)`
        // convention to `(Isolated, false)`; anything else is unsupported.
        if behavior != 0 || invert != 0 {
            return Err(format!(
                "unsupported split behavior {behavior} invert={invert}"
            ));
        }
        let regex = FancyRegex::new(&pattern)
            .map_err(|error| format!("invalid pretokenizer regex: {error}"))?;
        pretokenizer.push(regex);
    }

    let added_count = reader.u32()? as usize;
    let mut added = Vec::with_capacity(added_count);
    for _ in 0..added_count {
        added.push(reader.string()?.to_owned());
    }
    let added = AddedTokenMatcher::new(&added);

    let mut engine = match engine_kind {
        0 => {
            let rank_count = reader.u32()? as usize;
            let mut pairs = HashMap::with_capacity(rank_count);
            let mut results = Vec::with_capacity(rank_count);
            for rank in 0..rank_count {
                let left = reader.u32()?;
                let right = reader.u32()?;
                let result = reader.u32()?;
                if pairs.insert(pair_key(left, right), rank as u32).is_some() {
                    return Err(format!("duplicate merge pair at rank {rank}"));
                }
                results.push(result);
            }
            Engine::Hf(HfBpe {
                pairs,
                results,
                vocab: None,
            })
        }
        1 => {
            let token_count = reader.u32()? as usize;
            let mut tokens: Vec<Box<[u8]>> = Vec::with_capacity(token_count);
            let mut ranks = HashMap::with_capacity(token_count);
            for rank in 0..token_count {
                let token = reader.bytes()?.to_vec().into_boxed_slice();
                if ranks.insert(token.clone(), rank as u32).is_some() {
                    return Err(format!("duplicate tiktoken token at rank {rank}"));
                }
                tokens.push(token);
            }
            Engine::Tiktoken(TiktokenBpe { tokens, ranks })
        }
        other => return Err(format!("unknown engine id {other}")),
    };

    let mut base_ids = [0u32; 256];
    for slot in &mut base_ids {
        *slot = reader.u32()?;
    }
    match &engine {
        Engine::Tiktoken(tiktoken) => {
            if base_ids
                .iter()
                .any(|id| *id as usize >= tiktoken.tokens.len())
            {
                return Err("tiktoken base id outside the vocabulary".to_owned());
            }
        }
        Engine::Hf(_) => {}
    }

    if flags & 1 != 0 {
        let vocab_count = reader.u32()? as usize;
        let mut vocab = HashSet::with_capacity(vocab_count);
        for _ in 0..vocab_count {
            vocab.insert(reader.string()?.to_owned().into_boxed_str());
        }
        match &mut engine {
            Engine::Hf(hf) => {
                if hf.results.iter().any(|id| *id as usize >= vocab_count) {
                    return Err("merge result id outside the vocabulary".to_owned());
                }
                hf.vocab = Some(vocab);
            }
            Engine::Tiktoken(_) => {
                return Err("ignore_merges is only valid for the HF BPE engine".to_owned());
            }
        }
    }
    if reader.remaining() != 0 {
        return Err(format!("{} trailing asset bytes", reader.remaining()));
    }
    Ok(Tokenizer {
        normalizer,
        pretokenizer,
        added,
        base_ids,
        engine,
    })
}

// ---------------------------------------------------------------------------
// Counting
// ---------------------------------------------------------------------------

impl Tokenizer {
    /// Count the tokens in `text`: added tokens as one token each, every gap
    /// through the normalizer → pretokenizer → BPE chain.
    fn count(&self, text: &str) -> u64 {
        if text.is_empty() {
            return 0;
        }
        let mut total = 0u64;
        let mut position = 0usize;
        if let Some(added) = &self.added {
            for found in added.find_iter(text) {
                if found.start() > position {
                    total = total.saturating_add(self.count_plain(&text[position..found.start()]));
                }
                total = total.saturating_add(1);
                position = found.end();
            }
        }
        if position < text.len() {
            total = total.saturating_add(self.count_plain(&text[position..]));
        }
        total
    }

    fn count_plain(&self, text: &str) -> u64 {
        if text.is_empty() {
            return 0;
        }
        let normalized: Cow<'_, str> = match self.normalizer {
            NormalizerKind::None => Cow::Borrowed(text),
            NormalizerKind::Nfc => Cow::Owned(text.nfc().collect()),
        };
        let mut pieces = vec![normalized.into_owned()];
        for pattern in &self.pretokenizer {
            pieces = split_isolated(pieces, pattern);
        }
        pieces
            .iter()
            .map(|piece| self.count_piece(piece))
            .fold(0u64, u64::saturating_add)
    }

    fn count_piece(&self, piece: &str) -> u64 {
        if piece.is_empty() {
            return 0;
        }
        match &self.engine {
            Engine::Hf(engine) => engine.count(piece, &self.base_ids),
            Engine::Tiktoken(engine) => engine.count(piece, &self.base_ids),
        }
    }
}

impl HfBpe {
    fn count(&self, piece: &str, base_ids: &[u32; 256]) -> u64 {
        if let Some(vocab) = &self.vocab
            && vocab.contains(byte_encode(piece).as_str())
        {
            // `ignore_merges`: a pretoken that is already a whole vocabulary
            // entry is emitted as one token without merging.
            return 1;
        }
        let mut symbols: Vec<u32> = piece
            .as_bytes()
            .iter()
            .map(|byte| base_ids[*byte as usize])
            .collect();
        bpe_count(
            &mut symbols,
            |left, right| self.pairs.get(&pair_key(left, right)).copied(),
            |rank| self.results[rank as usize],
        )
    }
}

impl TiktokenBpe {
    fn count(&self, piece: &str, base_ids: &[u32; 256]) -> u64 {
        let mut symbols: Vec<u32> = piece
            .as_bytes()
            .iter()
            .map(|byte| base_ids[*byte as usize])
            .collect();
        let mut scratch = Vec::new();
        bpe_count(
            &mut symbols,
            |left, right| {
                scratch.clear();
                scratch.extend_from_slice(&self.tokens[left as usize]);
                scratch.extend_from_slice(&self.tokens[right as usize]);
                self.ranks.get(scratch.as_slice()).copied()
            },
            |rank| rank,
        )
    }
}

/// `(rank, left, right, left_version, right_version)` merge candidate.
type MergeCandidate = (u32, usize, usize, u64, u64);
/// Min-heap by merge rank.
type MergeHeap = BinaryHeap<Reverse<MergeCandidate>>;

/// Count the symbols left after BPE merging. `rank_of` returns the rank of an
/// adjacent pair (the heap picks the lowest); `merged_id` maps that rank to
/// the resulting symbol id.
fn bpe_count(
    symbols: &mut [u32],
    mut rank_of: impl FnMut(u32, u32) -> Option<u32>,
    merged_id: impl Fn(u32) -> u32,
) -> u64 {
    let length = symbols.len();
    if length <= 1 {
        return length as u64;
    }
    let mut previous: Vec<isize> = (0..length as isize).map(|index| index - 1).collect();
    let mut next: Vec<isize> = (1..=length as isize).collect();
    next[length - 1] = -1;
    let mut alive = vec![true; length];
    let mut version = vec![0u64; length];
    let mut heap = MergeHeap::new();

    for index in 0..length.saturating_sub(1) {
        push_merge(&mut heap, symbols, &next, &version, index, &mut rank_of);
    }

    let mut remaining = length;
    while let Some(Reverse((rank, left, right, left_version, right_version))) = heap.pop() {
        if !alive[left]
            || !alive[right]
            || next[left] != right as isize
            || version[left] != left_version
            || version[right] != right_version
        {
            continue;
        }
        symbols[left] = merged_id(rank);
        alive[right] = false;
        next[left] = next[right];
        if next[left] != -1 {
            previous[next[left] as usize] = left as isize;
        }
        version[left] += 1;
        remaining -= 1;
        push_merge(&mut heap, symbols, &next, &version, left, &mut rank_of);
        let before = previous[left];
        if before != -1 {
            push_merge(
                &mut heap,
                symbols,
                &next,
                &version,
                before as usize,
                &mut rank_of,
            );
        }
    }
    remaining as u64
}

fn push_merge(
    heap: &mut MergeHeap,
    symbols: &[u32],
    next: &[isize],
    version: &[u64],
    index: usize,
    rank_of: &mut impl FnMut(u32, u32) -> Option<u32>,
) {
    let right = next[index];
    if right == -1 {
        return;
    }
    let right = right as usize;
    if let Some(rank) = rank_of(symbols[index], symbols[right]) {
        heap.push(Reverse((
            rank,
            index,
            right,
            version[index],
            version[right],
        )));
    }
}

// ---------------------------------------------------------------------------
// Pretokenizer + added tokens
// ---------------------------------------------------------------------------

/// HF `Split { behavior: Isolated }`: matches are their own pieces and the
/// gaps between them stay in order.
fn split_isolated(pieces: Vec<String>, pattern: &FancyRegex) -> Vec<String> {
    let mut out = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let mut last = 0usize;
        for found in pattern.find_iter(&piece) {
            let Ok(found) = found else {
                tracing::warn!("rpi-statusline: pretokenizer regex match failed");
                continue;
            };
            let (start, end) = (found.start(), found.end());
            if start == end {
                continue;
            }
            if start > last {
                out.push(piece[last..start].to_owned());
            }
            out.push(piece[start..end].to_owned());
            last = end;
        }
        if last < piece.len() {
            out.push(piece[last..].to_owned());
        }
    }
    out
}

/// Added-token matcher (`normalized: false`): every token is extracted from
/// the raw text before normalization/pretokenization and counts as one token.
struct AddedTokenMatcher {
    regex: AddedTokenRegex,
}

impl AddedTokenMatcher {
    fn new(tokens: &[String]) -> Option<Self> {
        let mut tokens: Vec<&str> = tokens
            .iter()
            .map(String::as_str)
            .filter(|token| !token.is_empty())
            .collect();
        // Longest first, so a token that contains another one wins; equal
        // lengths keep their sorted order for determinism.
        tokens.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        tokens.dedup();
        if tokens.is_empty() {
            return None;
        }
        let pattern = tokens
            .iter()
            .map(|token| regex::escape(token))
            .collect::<Vec<_>>()
            .join("|");
        AddedTokenRegex::new(&format!("(?:{pattern})"))
            .ok()
            .map(|regex| Self { regex })
    }

    fn find_iter<'a>(&'a self, text: &'a str) -> impl Iterator<Item = regex::Match<'a>> {
        self.regex.find_iter(text)
    }
}

// ---------------------------------------------------------------------------
// ByteLevel byte → unicode-character map (GPT-2)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

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
    fn builtin_matching_prefers_the_family_keyword() {
        assert_eq!(
            builtin_for_model("deepseek-ai/DeepSeek-V4.1-Flash").map(|builtin| builtin.id),
            Some("deepseek")
        );
        assert_eq!(
            builtin_for_model("kimi-k3.1-pro").map(|builtin| builtin.id),
            Some("kimi")
        );
        assert_eq!(
            builtin_for_model("zai-coding-cn/glm-5.3").map(|builtin| builtin.id),
            Some("glm")
        );
        assert_eq!(
            builtin_for_model("Qwen/Qwen3.5-397B-A17B").map(|builtin| builtin.id),
            Some("qwen")
        );
        assert_eq!(
            builtin_for_model("tencent/hy4-preview").map(|builtin| builtin.id),
            Some("hunyuan")
        );
        assert_eq!(
            builtin_for_model("MiniMax-M3.1-Flash").map(|builtin| builtin.id),
            Some("minimax")
        );
        assert!(builtin_for_model("test-model").is_none());
    }

    #[test]
    fn unknown_models_keep_the_estimate_and_known_ones_use_the_bundled_table() {
        let unknown = count_breakdown(Some("test-model"), "hello", "", "");
        assert_eq!(unknown.source, TokenSource::Estimated);
        assert!(unknown.total_marker() == "~");

        let deepseek = count_breakdown(Some("deepseek-chat"), "hello world", "", "");
        assert_eq!(deepseek.source, TokenSource::ExactTokenizer);
        assert_eq!(deepseek.total, 2);
        assert_eq!(deepseek.total_marker(), "");
    }

    #[derive(Deserialize)]
    struct Vector {
        text: String,
        count: u64,
    }

    /// Replays the counts generated from the official `tokenizers`/`tiktoken`
    /// implementations against the embedded assets. This is the contract the
    /// `scripts/gen-statusline-tokenizers.py` output must keep.
    #[test]
    fn bundled_assets_match_official_counts() {
        let vectors: HashMap<String, Vec<Vector>> = serde_json::from_str(include_str!(
            "../tests/fixtures/statusline_tokenizer_vectors.json"
        ))
        .expect("tokenizer vectors parse");
        assert_eq!(vectors.len(), BUILTINS.len());
        for (family, entries) in vectors {
            // The Hunyuan family matches the catalog's `hy3`/`hy4` ids, not
            // the family name itself.
            let model_id = if family == "hunyuan" {
                "tencent/hy3-preview".to_owned()
            } else {
                format!("{family}-model")
            };
            let tokenizer = tokenizer_for_model(Some(&model_id))
                .unwrap_or_else(|| panic!("{family}: bundled tokenizer did not load"));
            for vector in entries {
                assert_eq!(
                    tokenizer.count(&vector.text),
                    vector.count,
                    "{family}: count for {:?}",
                    vector.text
                );
            }
        }
    }

    #[derive(Deserialize)]
    struct ManifestFamily {
        id: String,
        patterns: Vec<String>,
    }

    #[derive(Deserialize)]
    struct Manifest {
        #[serde(rename = "formatVersion")]
        format_version: u32,
        families: Vec<ManifestFamily>,
    }

    #[test]
    fn bundled_assets_parse_and_match_the_manifest() {
        let manifest: Manifest =
            serde_json::from_str(include_str!("../assets/tokenizers/manifest.json"))
                .expect("tokenizer manifest parses");
        assert_eq!(manifest.format_version, 1);
        assert_eq!(manifest.families.len(), BUILTINS.len());
        for (family, builtin) in manifest.families.iter().zip(BUILTINS) {
            assert_eq!(family.id, builtin.id);
            assert_eq!(
                family
                    .patterns
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                builtin.patterns,
                "{}: manifest patterns drifted from the registry",
                builtin.id
            );
            let tokenizer = parse_asset(builtin.asset)
                .unwrap_or_else(|error| panic!("{}: {error}", builtin.id));
            assert!(
                !tokenizer.pretokenizer.is_empty(),
                "{} needs a pretokenizer",
                builtin.id
            );
        }
    }
}
