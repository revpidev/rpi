//! Incremental per-message token counter (V19-07 FR-E / R5.3.3–R5.3.6).
//!
//! The official count of a tier (`Tokenizer::count`) is: added tokens on the
//! RAW text, then every gap NFC-normalized (Qwen / MiniMax), split by each
//! pretokenizer regex in sequence (HF `Split { behavior: Isolated }`), then
//! BPE per final piece. BPE merges, added tokens, regex pieces and NFC
//! composition all cross delta boundaries, so counting each delta on its own
//! and summing is wrong, and recounting the whole message per refresh is the
//! cost this module removes.
//!
//! The counter keeps, per tier, only the OPEN tail of every layer:
//!
//! - **raw added-token prefix**: the raw suffix that could still become an
//!   added token (every proper prefix of the vocabulary is the oracle),
//!   withheld from the plain path until more bytes decide it; added-token
//!   matching runs on the raw bytes BEFORE normalization/splitting, like the
//!   official pass;
//! - **NFC open sequence**: from the last normalization boundary (a starter
//!   that is NFC and cannot compose with what precedes it — covering
//!   combining marks and Hangul L/V/T jamo) to the end; only stable chunks
//!   are normalized, never per-delta NFC results concatenated;
//! - **split chain**: for each regex layer the open piece and the counts of
//!   the closed pieces inside it. A regex match that ended before the old
//!   end cannot change with more text (the bundled patterns look at most one
//!   character ahead), so only the last piece of each layer is open and the
//!   closed pieces are counted exactly once (reused by byte range); when the
//!   open piece is re-cut, the layers below are rebuilt for the new piece;
//! - **leaf**: the open final piece is BPE'd lazily for the provisional value
//!   (cached by length) and counted exactly once when it closes.
//!
//! `finalize` flushes the open tails; the official full pass over the
//! complete text stays the value of record and the refresh loop compares both
//! (a disagreement disables the incremental path for that message).
//!
//! Safety: an open piece or NFC sequence above [`MAX_OPEN_BYTES`] (64 KiB,
//! R5.3.5) sends the message back to the refreshMs full recount
//! ([`CountFallback`]); a pattern with an anchor/look-around other than the
//! single-character `(?!\S)` marks the family incremental-unsafe up front.
//! Both are diagnostics, never wrong numbers.

use std::sync::Arc;

use fancy_regex::Regex as FancyRegex;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::{canonical_combining_class, compose};

use crate::tokens::{self, NormalizerKind, TokenBreakdown, TokenSource, Tokenizer, split_ranges};

/// Internal protection constant (R5.3.5): any open piece or NFC composition
/// sequence larger than this falls back to the full recount. Not a setting.
pub const MAX_OPEN_BYTES: usize = 64 * 1024;

/// `text`, `thinking`, `toolcall`.
pub const TIERS: usize = 3;

/// Why a message left the incremental path (diagnostic + benchmark counter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountFallback {
    /// An open piece or NFC composition sequence exceeded [`MAX_OPEN_BYTES`].
    OpenTooLarge,
}

/// Raw Han/other character buckets of one tier (estimate mode and the
/// heuristic fallback of `auto`): the two counts are accumulated per delta
/// and each tier's `ceil` is taken exactly once, at display time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CharBuckets {
    pub han: u64,
    pub other: u64,
}

impl CharBuckets {
    pub fn push_str(&mut self, text: &str) {
        for ch in text.chars() {
            if tokens::is_han(ch) {
                self.han += 1;
            } else {
                self.other += 1;
            }
        }
    }

    pub fn tokens(&self) -> u64 {
        tokens::heuristic_from_buckets(self.han, self.other)
    }
}

/// Analysis of a pretokenizer pattern for windowed (open-tail) scanning.
///
/// Two facts must hold for the incremental split to equal the full scan:
///
/// 1. **Nothing looks BACKWARDS**: no anchor (`^`, `$`, `\b`, `\B`) and no
///    look-behind — a window that starts at a piece boundary of the previous
///    layer sees exactly what the full scan saw from that boundary on.
/// 2. **The forward peek past a candidate match end is bounded**: a greedy
///    loop stops at the first non-matching char (one char), an optional
///    trailing group such as `(?i:'s|'t|'re|'ve|'m|'ll|'d)?` may need up to
///    its longest alternative (three chars) before it knows it cannot match,
///    and `(?!\S)` peeks one char. A regex piece whose end lies within that
///    many bytes of the text end is therefore still OPEN: `"don'"` splits as
///    `["don", "'"]` but `"don't"` as `["don't"]`.
///
/// [`tail_guard_bytes`] returns the bound in bytes (4 per char, the widest
/// UTF-8 encoding) or `None` when the pattern uses something this analysis
/// does not understand — the family then always takes the full-recount path.
pub fn tail_guard_bytes(pattern: &str) -> Option<usize> {
    let chars: Vec<char> = pattern.chars().collect();
    // Forward peek in chars: one for the loop stop / lookahead, plus the
    // content length of every quantified group (each iteration boundary can
    // need the whole group again before failing).
    let mut peek_chars = 1usize;
    let mut index = 0usize;
    let mut group_depth = 0usize;
    // Open groups: (content length of the current alternative, longest one).
    let mut groups: Vec<(usize, usize)> = Vec::new();
    while index < chars.len() {
        let ch = chars[index];
        match ch {
            '\\' => {
                // Outside a class `\b`/`\B`/`\A`/`\z`/`\Z` are anchors.
                if matches!(chars.get(index + 1), Some('b' | 'B' | 'A' | 'z' | 'Z')) {
                    return None;
                }
                // `\p{…}`/`\P{…}`/`\x{…}`/`\u{…}`: the braces belong to the
                // escape, not to a quantifier.
                index += 2;
                if chars.get(index) == Some(&'{')
                    && let Some(close) = chars[index..].iter().position(|&c| c == '}')
                {
                    index += close + 1;
                }
                if let Some(top) = groups.last_mut() {
                    top.0 += 1;
                }
            }
            '[' => {
                // Character class (nested classes / `&&` intersections
                // nest `[`/`]`).
                let mut depth = 1usize;
                index += 1;
                while index < chars.len() && depth > 0 {
                    match chars[index] {
                        '\\' => {
                            index += 2;
                            if chars.get(index) == Some(&'{')
                                && let Some(close) = chars[index..].iter().position(|&c| c == '}')
                            {
                                index += close + 1;
                            }
                        }
                        '[' => {
                            depth += 1;
                            index += 1;
                        }
                        ']' => {
                            depth -= 1;
                            index += 1;
                        }
                        _ => index += 1,
                    }
                }
                if depth != 0 {
                    return None;
                }
                if let Some(top) = groups.last_mut() {
                    top.0 += 1;
                }
                if index < chars.len() && matches!(chars[index], '?' | '*' | '+' | '{') {
                    let (quant, next) = quantifier(&chars, index)?;
                    peek_chars += quant;
                    index = next;
                }
            }
            '(' => {
                if chars.get(index + 1) == Some(&'?') {
                    // `(?!\S)` is the only look-around we accept.
                    if chars[index..].starts_with(&['(', '?', '!', '\\', 'S', ')']) {
                        peek_chars += 1;
                        index += 6;
                        if let Some(top) = groups.last_mut() {
                            top.0 += 1;
                        }
                        continue;
                    }
                    // `(?i:` inline flag group: transparent.
                    if chars.get(index + 2) == Some(&'i') && chars.get(index + 3) == Some(&':') {
                        index += 4;
                        groups.push((0, 0));
                        group_depth += 1;
                        continue;
                    }
                    return None;
                }
                groups.push((0, 0));
                group_depth += 1;
                index += 1;
            }
            ')' => {
                let (len, max_alt) = groups.pop()?;
                group_depth -= 1;
                let group_len = len.max(max_alt);
                if index + 1 < chars.len() && matches!(chars[index + 1], '?' | '*' | '+' | '{') {
                    let (quant, next) = quantifier(&chars, index + 1)?;
                    peek_chars += quant.saturating_mul(group_len.max(1));
                    index = next;
                } else {
                    index += 1;
                }
                if let Some(top) = groups.last_mut() {
                    top.0 += group_len;
                }
            }
            '|' => {
                if let Some(top) = groups.last_mut() {
                    top.1 = top.1.max(top.0);
                    top.0 = 0;
                }
                index += 1;
            }
            '^' | '$' => return None,
            '?' | '*' | '+' | '{' => {
                // Quantifier on a single literal char.
                let (quant, next) = quantifier(&chars, index)?;
                peek_chars += quant;
                index = next;
            }
            _ => {
                if let Some(top) = groups.last_mut() {
                    top.0 += 1;
                }
                index += 1;
            }
        }
    }
    if group_depth != 0 {
        return None;
    }
    Some(peek_chars.saturating_mul(4))
}

/// A quantifier's forward peek in chars: `?` → 1 (the optional try), `*`/`+`
/// → 1 (the loop stop), `{m,n}` → `n − m` extra tries. Unbounded or exotic
/// forms → `None`.
fn quantifier(chars: &[char], index: usize) -> Option<(usize, usize)> {
    match chars[index] {
        '?' | '*' | '+' => Some((1, index + 1)),
        '{' => {
            let end = chars[index..].iter().position(|&c| c == '}')? + index;
            let body: String = chars[index + 1..end].iter().collect();
            let mut parts = body.split(',');
            let low: usize = parts.next()?.trim().parse().ok()?;
            let high = match parts.next() {
                Some("") | None => low,
                Some(high) => high.trim().parse().ok()?,
            };
            if high > 64 || parts.next().is_some() {
                return None;
            }
            Some((1.max(high.saturating_sub(low)), end + 1))
        }
        _ => None,
    }
}

/// Whether the pattern may be scanned from a window that starts at a piece
/// boundary of the previous layer (see [`tail_guard_bytes`]).
pub fn pattern_is_incremental_safe(pattern: &str) -> bool {
    tail_guard_bytes(pattern).is_some()
}

/// One piece of a level's open region: ABSOLUTE byte range in the gap text
/// (never invalidated by trimming) and its full recursive count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Piece {
    start: usize,
    end: usize,
    /// `None` while the piece is the last one of an inner layer (the layer
    /// below accounts for it); materialized as soon as it is needed.
    count: Option<u64>,
}

/// One split layer's open-tail state.
#[derive(Debug, Default, Clone)]
struct LevelState {
    /// The open region: a suffix of the parent layer's last piece (level 1:
    /// a suffix of the gap text) that starts at a piece boundary of this
    /// layer's regex. Only the pieces still within the tail guard of the end
    /// (plus the last piece) live here; everything before was committed.
    text: String,
    /// Absolute offset of `text[0]` in the gap text.
    base: usize,
    /// Sum of the full counts of every piece that left the region (final).
    committed: u64,
    /// The pieces currently inside the region, with their counts (stable
    /// ones are reused by range on the next pass; the last piece's count is
    /// only materialized at the leaf).
    pieces: Vec<Piece>,
    /// Absolute start of the parent's last piece this state was built from
    /// (levels ≥ 2): the same start on the next pass means the parent's last
    /// piece only grew — the region can be extended in place.
    parent_last_start: Option<usize>,
    /// Length of the parent's last piece at the last pass (levels ≥ 2).
    parent_last_len: usize,
}

/// The layered open-tail state over one tier's normalized stable text.
struct SplitChain {
    regexes: Vec<Arc<FancyRegex>>,
    engine: Arc<Tokenizer>,
    /// Bytes at the end of the text within which a regex piece is not final
    /// yet ([`tail_guard_bytes`] of the pattern, per level).
    guards: Vec<usize>,
    levels: Vec<LevelState>,
}

impl SplitChain {
    fn new(regexes: Vec<Arc<FancyRegex>>, engine: Arc<Tokenizer>) -> Self {
        let n = regexes.len();
        let guards = regexes
            .iter()
            .map(|regex| tail_guard_bytes(regex.as_str()).unwrap_or(usize::MAX))
            .collect();
        Self {
            regexes,
            engine,
            guards,
            levels: vec![LevelState::default(); n],
        }
    }

    fn reset(&mut self) {
        self.levels.clear();
        self.levels = vec![LevelState::default(); self.regexes.len()];
    }

    /// The full recursive count of a COMPLETE level-`level` piece (its
    /// sub-pieces come from `regexes[level]` downwards; the leaf is BPE).
    fn full_count(&self, level: usize, piece: &str) -> u64 {
        if piece.is_empty() {
            return 0;
        }
        if level >= self.regexes.len() {
            return self.engine.count_piece(piece);
        }
        split_ranges(piece, &self.regexes[level])
            .into_iter()
            .map(|(start, end)| self.full_count(level + 1, &piece[start..end]))
            .fold(0u64, u64::saturating_add)
    }

    /// Append normalized stable text to the gap.
    fn append(&mut self, text: &str) -> Result<(), CountFallback> {
        if text.is_empty() {
            return Ok(());
        }
        self.levels[0].text.push_str(text);
        self.process(1)
    }

    /// Recompute level `level` from its region (already extended or rebuilt
    /// by the parent): split, commit the stable prefix pieces, keep the open
    /// ones with cached counts, and hand the last piece down.
    fn process(&mut self, level: usize) -> Result<(), CountFallback> {
        let n = self.regexes.len();
        let idx = level - 1;
        let state = &mut self.levels[idx];
        if state.text.len() > MAX_OPEN_BYTES {
            return Err(CountFallback::OpenTooLarge);
        }
        let guard = self.guards[idx];
        let base = state.base;
        let text = std::mem::take(&mut state.text);
        let old_pieces = std::mem::take(&mut state.pieces);
        let ranges = split_ranges(&text, &self.regexes[idx]);
        let last = ranges.len() - 1;
        let stable_limit = text.len().saturating_sub(guard);
        let mut committed_add = 0u64;
        let mut trim_to = 0usize;
        let mut pieces: Vec<Piece> = Vec::with_capacity(ranges.len());
        for (index, &(start, end)) in ranges.iter().enumerate() {
            let (absolute_start, absolute_end) = (base + start, base + end);
            let cached = old_pieces
                .iter()
                .find(|piece| piece.start == absolute_start && piece.end == absolute_end)
                .and_then(|piece| piece.count);
            let is_last = index == last;
            // Inner layers leave the last piece to the layer below (its count
            // is only materialized once it stops being last); the leaf
            // materializes every piece.
            let needed = !(is_last && level < n);
            let count = match cached {
                Some(count) => Some(count),
                None if needed => Some(self.piece_count(level, &text[start..end])),
                None => None,
            };
            if !is_last && end <= stable_limit {
                // Stable: it left the open tail for good.
                committed_add = committed_add.saturating_add(count.unwrap_or(0));
                trim_to = end;
                continue;
            }
            pieces.push(Piece {
                start: absolute_start,
                end: absolute_end,
                count,
            });
        }
        let state = &mut self.levels[idx];
        state.committed = state.committed.saturating_add(committed_add);
        state.base = base + trim_to;
        state.text = text[trim_to..].to_owned();
        state.pieces = pieces;
        if level == n {
            return Ok(());
        }
        // Hand the last piece to the layer below.
        let (last_start, last_end) = ranges[last];
        let absolute_start = base + last_start;
        let child = &mut self.levels[idx + 1];
        match child.parent_last_start {
            Some(start) if start == absolute_start => {
                // The parent's last piece only grew: extend the region in place.
                let added = (last_end - last_start) - child.parent_last_len;
                child.text.push_str(&text[last_end - added..last_end]);
                child.parent_last_len = last_end - last_start;
            }
            _ => {
                // Re-cut (merged, shrunk or first seen): rebuild from the
                // parent's last piece.
                child.text = text[last_start..last_end].to_owned();
                child.base = absolute_start;
                child.committed = 0;
                child.pieces.clear();
                child.parent_last_start = Some(absolute_start);
                child.parent_last_len = last_end - last_start;
            }
        }
        self.process(level + 1)
    }

    fn piece_count(&self, level: usize, piece: &str) -> u64 {
        if level >= self.regexes.len() {
            self.engine.count_piece(piece)
        } else {
            self.full_count(level, piece)
        }
    }

    /// Close everything: the open pieces are final now.
    fn close(&mut self) -> u64 {
        let total = self.provisional();
        self.reset();
        total
    }

    /// Committed pieces + the open region's provisional count down every
    /// layer (the leaf BPE's each of its pieces).
    fn provisional(&self) -> u64 {
        fn level(chain: &SplitChain, idx: usize) -> u64 {
            let state = &chain.levels[idx];
            let mut total = state.committed;
            let last = state.pieces.len();
            for (index, piece) in state.pieces.iter().enumerate() {
                if index + 1 == last && idx + 1 < chain.levels.len() {
                    // The last piece is accounted for by the layer below.
                    break;
                }
                total = total.saturating_add(piece.count.unwrap_or(0));
            }
            if idx + 1 < chain.levels.len() {
                total.saturating_add(level(chain, idx + 1))
            } else {
                total
            }
        }
        level(self, 0)
    }
}

/// The plain (non-added-token) path of one tier: NFC open sequence + the
/// split chain over the normalized stable text.
struct PlainGap {
    normalizer: NormalizerKind,
    /// Raw bytes from the last normalization boundary to the end (NFC
    /// families); always empty for the others.
    nfc_open: String,
    chain: SplitChain,
}

impl PlainGap {
    fn new(
        normalizer: NormalizerKind,
        regexes: Vec<Arc<FancyRegex>>,
        engine: Arc<Tokenizer>,
    ) -> Self {
        Self {
            normalizer,
            nfc_open: String::new(),
            chain: SplitChain::new(regexes, engine),
        }
    }

    fn push(&mut self, raw: &str) -> Result<(), CountFallback> {
        if raw.is_empty() {
            return Ok(());
        }
        match self.normalizer {
            NormalizerKind::None => self.chain.append(raw),
            NormalizerKind::Nfc => {
                self.nfc_open.push_str(raw);
                if self.nfc_open.len() > MAX_OPEN_BYTES {
                    return Err(CountFallback::OpenTooLarge);
                }
                let boundary = last_normalization_boundary(&self.nfc_open);
                if boundary > 0 {
                    let stable = self.nfc_open[..boundary].to_owned();
                    self.nfc_open.drain(..boundary);
                    let normalized: String = stable.nfc().collect();
                    self.chain.append(&normalized)?;
                }
                Ok(())
            }
        }
    }

    /// The gap ends: flush the NFC open sequence and close the chain.
    fn close(&mut self) -> Result<u64, CountFallback> {
        if self.normalizer == NormalizerKind::Nfc && !self.nfc_open.is_empty() {
            let open = std::mem::take(&mut self.nfc_open);
            let normalized: String = open.nfc().collect();
            self.chain.append(&normalized)?;
        }
        Ok(self.chain.close())
    }

    fn provisional(&mut self) -> u64 {
        self.chain.provisional()
    }
}

/// The raw added-token layer of one tier.
struct RawAdded {
    max_added: usize,
    prefixes: Arc<crate::compact::BytePoolIndex>,
    /// Unresolved raw suffix: a possible partial added token, plus at most
    /// the complete match that contains it when the cut lands inside one.
    open: String,
}

/// Per-tier state.
struct TierCounter {
    raw: Option<RawAdded>,
    gap: PlainGap,
    /// Tokens of everything before the open gap (closed gaps + added
    /// tokens).
    finalized: u64,
    fallback: Option<CountFallback>,
}

/// The streaming counter for ONE assistant message (one policy, one model).
pub struct IncrementalCounter {
    tokenizer: Arc<Tokenizer>,
    tiers: Vec<TierCounter>,
    buckets: Vec<CharBuckets>,
    fallback: Option<CountFallback>,
    /// Whether the family's patterns allow windowed scanning; `false` means
    /// every message falls back to the full recount up front.
    safe: bool,
}

impl std::fmt::Debug for IncrementalCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncrementalCounter")
            .field("fallback", &self.fallback)
            .field("safe", &self.safe)
            .field(
                "finalized",
                &self
                    .tiers
                    .iter()
                    .map(|tier| tier.finalized)
                    .collect::<Vec<_>>(),
            )
            .field("buckets", &self.buckets)
            .finish()
    }
}

impl IncrementalCounter {
    pub(crate) fn new(tokenizer: Arc<Tokenizer>) -> Self {
        let safe = tokenizer
            .pretokenizer()
            .iter()
            .all(|regex| pattern_is_incremental_safe(regex.as_str()));
        let tiers = (0..TIERS)
            .map(|_| TierCounter {
                raw: tokenizer.added().map(|_| RawAdded {
                    max_added: tokenizer.max_added_token_bytes(),
                    prefixes: Arc::clone(
                        tokenizer.added_prefixes().expect("added tokens ⇒ prefixes"),
                    ),
                    open: String::new(),
                }),
                gap: PlainGap::new(
                    tokenizer.normalizer(),
                    tokenizer.pretokenizer().to_vec(),
                    Arc::clone(&tokenizer),
                ),
                finalized: 0,
                fallback: None,
            })
            .collect();
        Self {
            tokenizer,
            tiers,
            buckets: vec![CharBuckets::default(); TIERS],
            fallback: None,
            safe,
        }
    }

    /// Feed one delta of `tier` (0 text, 1 thinking, 2 toolcall). O(open
    /// tail + new bytes). After a fallback the delta only feeds the cheap
    /// heuristic buckets.
    pub fn push(&mut self, tier: usize, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.buckets[tier].push_str(delta);
        if self.fallback.is_some() {
            return;
        }
        if !self.safe {
            self.fallback = Some(CountFallback::OpenTooLarge);
            return;
        }
        if let Err(fallback) = self.push_tier(tier, delta) {
            self.fallback = Some(fallback);
            self.tiers[tier].fallback = Some(fallback);
        }
    }

    fn push_tier(&mut self, tier: usize, delta: &str) -> Result<(), CountFallback> {
        let Some(raw) = self.tiers[tier].raw.as_mut() else {
            return self.tiers[tier].gap.push(delta);
        };
        // One scan over (unresolved raw suffix + delta): every position before
        // the previous cut was proven unable to start an added token.
        let mut whole = std::mem::take(&mut raw.open);
        whole.push_str(delta);
        let end = whole.len();
        let matcher = self
            .tokenizer
            .added()
            .expect("raw layer exists only with added tokens");
        let matches: Vec<(usize, usize)> = matcher
            .find_iter(&whole)
            .map(|found| (found.start(), found.end()))
            .collect();
        // The withheld suffix starts at the earliest position from which the
        // rest could still become (part of) an added token: a last match that
        // ends at the text end and is itself a PROPER prefix of a longer token
        // may still grow, so it is withheld whole; otherwise the trailing gap
        // is scanned from the last match end for its earliest proper-prefix
        // position. Never inside a complete match.
        let last_match_end = matches.last().map(|(_, end)| *end).unwrap_or(0);
        let mut cut = end;
        if let Some(&(start, match_end)) = matches.last()
            && match_end == end
            && raw.prefixes.contains(&whole.as_bytes()[start..])
        {
            cut = start;
        } else {
            let mut position = last_match_end.max(end.saturating_sub(raw.max_added));
            while position < end {
                if whole.is_char_boundary(position)
                    && raw.prefixes.contains(&whole.as_bytes()[position..])
                {
                    cut = position;
                    break;
                }
                position += 1;
            }
        }
        if std::env::var("RPI_V1907_DEBUG_RAW").is_ok() {
            eprintln!(
                "raw whole={whole:?} matches={matches:?} cut={cut} max_added={}",
                raw.max_added
            );
        }
        let mut cursor = 0usize;
        for &(start, end) in &matches {
            if end > cut {
                break;
            }
            let tier_state = &mut self.tiers[tier];
            if start > cursor {
                tier_state.gap.push(&whole[cursor..start])?;
            }
            let closed = tier_state.gap.close()?;
            tier_state.finalized = tier_state
                .finalized
                .saturating_add(closed)
                .saturating_add(1);
            cursor = end;
        }
        if cursor < cut {
            self.tiers[tier].gap.push(&whole[cursor..cut])?;
        }
        self.tiers[tier].raw.as_mut().expect("raw layer").open = whole[cut..].to_owned();
        Ok(())
    }

    /// The provisional per-tier count so far (stable pieces + open tail).
    /// `None` for a tier on the full-recount path.
    pub fn provisional(&mut self) -> Option<TokenBreakdown> {
        if self.fallback.is_some() {
            return None;
        }
        let mut tiers = [0u64; TIERS];
        for (index, tier) in self.tiers.iter_mut().enumerate() {
            let gap = tier.gap.provisional();
            tiers[index] = tier.finalized.saturating_add(gap);
        }
        Some(TokenBreakdown::from_tiers(
            tiers[0],
            tiers[1],
            tiers[2],
            TokenSource::ExactTokenizer,
        ))
    }

    /// The raw character buckets (estimate mode / heuristic fallback).
    pub fn buckets(&self) -> &[CharBuckets] {
        &self.buckets
    }

    /// The heuristic breakdown of everything fed so far.
    pub fn heuristic(&self) -> TokenBreakdown {
        TokenBreakdown::from_tiers(
            self.buckets[0].tokens(),
            self.buckets[1].tokens(),
            self.buckets[2].tokens(),
            TokenSource::Estimated,
        )
    }

    /// Why this message left the incremental path, if it did.
    pub fn fallback(&self) -> Option<CountFallback> {
        self.fallback
    }

    /// Close the open tails and return the per-tier counts (equal to the
    /// official full pass when the incremental path was used).
    pub fn finalize(&mut self) -> Option<TokenBreakdown> {
        if self.fallback.is_some() {
            return None;
        }
        let mut tiers = [0u64; TIERS];
        for (index, tier) in self.tiers.iter_mut().enumerate() {
            // An unresolved raw suffix is plain text once no more bytes
            // follow: flush it through the gap before closing.
            let unresolved = match tier.raw.as_mut() {
                Some(raw) => std::mem::take(&mut raw.open),
                None => String::new(),
            };
            if !unresolved.is_empty()
                && let Err(fallback) = tier.gap.push(&unresolved)
            {
                self.fallback = Some(fallback);
                return None;
            }
            match tier.gap.close() {
                Ok(closed) => {
                    // Any unresolved raw suffix is plain text once no more
                    // bytes follow: flush it through the gap too.
                    tiers[index] = tier.finalized.saturating_add(closed);
                }
                Err(fallback) => {
                    self.fallback = Some(fallback);
                    return None;
                }
            }
        }
        Some(TokenBreakdown::from_tiers(
            tiers[0],
            tiers[1],
            tiers[2],
            TokenSource::ExactTokenizer,
        ))
    }
}

/// The largest `b` (char boundary, `1..=len`) such that normalizing
/// `text[..b]` and `text[b..]` separately equals normalizing the whole —
/// a normalization boundary: the char at `b` is a starter (combining class
/// 0), is itself NFC, and cannot compose with the char before it (covers
/// Hangul L+V / LV+T and every starter+starter composition). `0` when the
/// whole open sequence is still one composition unit.
pub fn last_normalization_boundary(text: &str) -> usize {
    let mut boundary = 0;
    let mut previous: Option<char> = None;
    for (index, ch) in text.char_indices() {
        if index == 0 {
            previous = Some(ch);
            continue;
        }
        if is_normalization_boundary(previous.expect("set after the first char"), ch) {
            boundary = index;
        }
        previous = Some(ch);
    }
    boundary
}

fn is_normalization_boundary(previous: char, next: char) -> bool {
    if canonical_combining_class(next) != 0 {
        return false;
    }
    if !unicode_normalization::is_nfc(&next.to_string()) {
        return false;
    }
    if canonical_combining_class(previous) != 0 {
        // A starter after a mark is blocked from every composition.
        return true;
    }
    if compose(previous, next).is_some() {
        return false;
    }
    // Conjoining jamo: `L V` normalizes to an LV syllable that then composes
    // with a following `T` (V + T alone does not), so a jamo run — and a jamo
    // after a precomposed syllable — stays one composition unit.
    let is_leading = |c: char| matches!(c, '\u{1100}'..='\u{115F}');
    let is_trailing = |c: char| matches!(c, '\u{11A8}'..='\u{11C2}');
    let is_jamo_or_syllable =
        |c: char| matches!(c, '\u{1100}'..='\u{11FF}' | '\u{AC00}'..='\u{D7A3}');
    if is_leading(next) {
        // A leading jamo never composes backwards: it always starts a new
        // syllable.
        return true;
    }
    !(is_trailing(next) && is_jamo_or_syllable(previous))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{BUILTINS, builtin_for_model, count_breakdown, tokenizer_for_model};

    fn tokenizer(model: &str) -> Arc<Tokenizer> {
        tokenizer_for_model(Some(model)).expect("bundled tokenizer")
    }

    /// Feed `text` in `chunk`-byte pieces (on char boundaries) and compare the
    /// finalized per-tier counts with the official full pass.
    fn check_incremental(model: &str, tokenizer: &Arc<Tokenizer>, texts: [&str; 3], chunk: usize) {
        let mut counter = IncrementalCounter::new(Arc::clone(tokenizer));
        for (tier, text) in texts.iter().enumerate() {
            let mut rest = *text;
            while !rest.is_empty() {
                let mut end = rest.len().min(chunk);
                while !rest.is_char_boundary(end) {
                    end += 1;
                }
                counter.push(tier, &rest[..end]);
                rest = &rest[end..];
            }
        }
        let finalized = counter.finalize().expect("no fallback");
        let official = count_breakdown(Some(model), texts[0], texts[1], texts[2]);
        assert_eq!(
            (finalized.text, finalized.thinking, finalized.toolcall),
            (official.text, official.thinking, official.toolcall),
            "chunk {chunk}: {:?}",
            texts
        );
    }

    const MODELS: [(&str, &str); 6] = [
        ("deepseek", "deepseek-chat"),
        ("kimi", "kimi-k3.1-pro"),
        ("glm", "glm-5.3"),
        ("qwen", "qwen3.5"),
        ("hunyuan", "hy4-preview"),
        ("minimax", "minimax-m3"),
    ];

    fn corpus() -> Vec<String> {
        let mut texts = vec![
            "Hello, world! This is a short sentence.\n".to_owned(),
            "The quick brown fox jumps over the lazy dog 0123456789 times, isn't it? I'd say it's fine... we've been here — they're mine; you'll see.\n\n".to_owned(),
            "don't 's 't 're 've 'm 'll 'd DON'T It's".to_owned(),
            "a  b   c    d\t\te\n\nf\n\n\ng  \n".to_owned(),
            "  leading and trailing   ".to_owned(),
            "中文测试：一二三四五六七八九十，你好世界！这是一个较长的中文句子，包含标点。".to_owned(),
            "カタカナ ひらがな 漢字 mixed with English 123 and 日本語テキスト".to_owned(),
            "한국어 텍스트 테스트 hangul: 가나다라마바사 한글 완성형 syllables".to_owned(),
            "Jamo: \u{1100}\u{1161}\u{11A8} \u{1100}\u{1161} composed: 가".to_owned(),
            "Combining: e\u{0301} a\u{0300}\u{0301} n\u{0303} = é àñ; decomposed and composed mix \u{00E9}\u{0301}".to_owned(),
            "{\"tool\": \"bash\", \"args\": {\"command\": \"ls -la /tmp && echo done\", \"timeout\": 3000}, \"escaped\": \"a\\\"b\\\\c\\nd\"}".to_owned(),
            "```rust\nfn main() {\n    println!(\"{:?}\", vec![1, 2, 3]);\n}\n```\n".to_owned(),
            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg== trailing".to_owned(),
            "<|im_end|><|im_start|>user\nhello<|im_end|>\n<|im_start|>assistant\nhi there<|endoftext|>".to_owned(),
            "<|im_start|>user\n hel <|im_end|> lo <|endoftext|> x <|im_start|><|im_start|>".to_owned(),
            "[BOS]kimi[EOS]<|im_end|><|im_user|>hi<|im_assistant|>yo<BOS><|reserved_token_5|>".to_owned(),
            "<fim_prefix>def f(<fim_middle>: int) -> str:<fim_suffix>\n    return str(x)<fim_pad> ]!p~[".to_owned(),
            "<|endoftext|>[MASK][gMASK]<sop><eop>|endoftext|<|end".to_owned(),
            "mixed 中文 and English with 中文 punctuation，and numbers 1234567890 and 987 and 1".to_owned(),
            "Café naïve résumé Zürich Ångström Øresund şekilli ışık günümüz".to_owned(),
            "ᄀᄁᄂᄃᄅᄆᄇᄈᄉᄊᄋᄌᄍᄎᄏᄐᄑᄒ ᅡᅢᅣᅤᅥᅦᅧᅨᅩᅪᅫᅬᅭᅮᅯᅰᅱᅲ ᆨᆩᆪᆫᆬᆭᆮᆯᆰᆱᆲᆳᆴᆵᆶᆷᆸᆹᆺᆻᆼᆽᆾᆿᇀᇁᇂ".to_owned(),
            "ẛᵜṰṹΩ㎧ ﬁ ﬂ ½ ¼ ① ㍿ 🇺🇸 👨‍👩‍👧‍👦 😀 emoji 👍🏽 ✨🚀".to_owned(),
            "Devanagari: नमस्ते दुनिया क़िताब हिन्दी; Bengali: নমস্কার কিতাব; Tamil: வணக்கம்; Thai: สวัสดีครับ".to_owned(),
            "....\n\n\n\n   \n\t\t \n end".to_owned(),
            "".to_owned(),
        ];
        // Long whitespace and base64/JSON-string stress (open-tail bounds).
        texts.push("x".repeat(70_000));
        texts.push(format!("\"{}\"", "a".repeat(70_000)));
        texts.push(" ".repeat(70_000));
        texts
    }

    #[test]
    fn every_family_matches_the_official_count_at_every_chunk_size() {
        let corpus = corpus();
        for (family, model) in MODELS {
            let tokenizer = tokenizer(model);
            assert_eq!(
                builtin_for_model(model).map(|builtin| builtin.id),
                Some(family)
            );
            // The oversized texts have their own fallback test; the sweep
            // stays on texts the incremental path must handle exactly.
            let short: Vec<&str> = corpus
                .iter()
                .filter(|text| text.len() < 2_000)
                .map(String::as_str)
                .collect();
            let mut cases: Vec<[&str; 3]> = Vec::new();
            for (index, text) in short.iter().enumerate() {
                cases.push([text, "", ""]);
                cases.push(["", text, ""]);
                cases.push(["", "", text]);
                let next = short.get(index + 1).copied().unwrap_or("");
                cases.push([text, next, "write(file, \"{}\")"]);
            }
            for chunk in [1usize, 3, 7, 64] {
                for texts in &cases {
                    check_incremental(model, &tokenizer, *texts, chunk);
                }
            }
        }
    }

    /// A realistic long answer (≈2 KiB, mixed prose/code/CJK/whitespace)
    /// fed one character at a time still finalizes exactly, for every family.
    #[test]
    fn long_mixed_answer_one_char_at_a_time_matches_the_official_count() {
        let paragraph = "The assistant explains: 这是一个较长的回答 with **Markdown**, `code`, \
\"json\": {\"a\": [1, 2, 3]}\n\n- list item 1\n- list item 2\n\n\tindented code(); // comment\n";
        let text: String = paragraph.repeat(8);
        for (family, model) in MODELS {
            let tokenizer = tokenizer(model);
            let official = tokenizer.count(&text);
            assert!(official > 0);
            assert_eq!(
                builtin_for_model(model).map(|builtin| builtin.id),
                Some(family)
            );
            check_incremental(
                model,
                &tokenizer,
                [&text, "思考中… thinking", "{\"cmd\":\"ls\"}"],
                1,
            );
            check_incremental(
                model,
                &tokenizer,
                [&text, "思考中… thinking", "{\"cmd\":\"ls\"}"],
                13,
            );
        }
    }

    /// A long mixed answer streamed in `chunk`-byte deltas (≈ 40 KiB).
    fn answer() -> String {
        let paragraph = "The assistant explains: 这是一个较长的回答 with **Markdown**, `code`, \
    \"json\": {\"a\": [1, 2, 3]}\n\n- list item 1\n- list item 2\n\n\tindented code(); // comment\n\
    你好世界，这是第二段。Numbers 1234567890 and 987 and 1. Contractions: don't, it's, we've, they'll.\n";
        paragraph.repeat(80)
    }

    fn chunks(text: &str, chunk: usize) -> Vec<&str> {
        let mut out = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let mut end = rest.len().min(chunk);
            while !rest.is_char_boundary(end) {
                end += 1;
            }
            out.push(&rest[..end]);
            rest = &rest[end..];
        }
        out
    }

    struct Report {
        label: String,
        bpe_calls: u64,
        ticks: u64,
        cpu_ms: u128,
        total: u64,
        source: TokenSource,
        fallbacks: u64,
        recomputes: u64,
    }

    /// V19-07 FR-G R3 counting benchmark (ADR-0049 decision 10): the same
    /// streamed input under the three calculation paths of the statusline —
    /// v0.1.8 `auto` (official full pass per refresh tick), candidate `auto`
    /// (incremental) and `estimate` (buckets, no tokenizer) — reporting BPE
    /// piece evaluations, open-tail recomputes, fallbacks, finalize parity
    /// and CPU time. `cargo test -p rpi-ext-statusline --release --lib
    /// v19_counting_bench -- --ignored --nocapture`.
    #[test]
    #[ignore = "V19-07 counting benchmark; run explicitly with --ignored --nocapture"]
    fn v19_counting_bench_auto_official_vs_incremental_vs_estimate() {
        let text = answer();
        let models = ["glm-5.3", "kimi-k3.1-pro", "deepseek-chat"];
        let mut reports: Vec<Report> = Vec::new();
        for model in models {
            let tokenizer = tokenizer_for_model(Some(model)).expect("bundled family");
            let delta_chunk = 24; // ≈ 6 tokens per provider delta
            let deltas = chunks(&text, delta_chunk);
            let tick_every = 4; // refreshMs tick every 4 deltas

            // 1. v0.1.8 auto: the official full pass at every refresh tick.
            tokens::reset_bpe_calls();
            let started = std::time::Instant::now();
            let mut ticks = 0u64;
            let mut accumulated = String::new();
            let mut last = TokenBreakdown::default();
            for (index, delta) in deltas.iter().enumerate() {
                accumulated.push_str(delta);
                if index % tick_every == 0 || index + 1 == deltas.len() {
                    last = count_breakdown(Some(model), &accumulated, "", "");
                    ticks += 1;
                }
            }
            let official_calls = tokens::bpe_calls();
            reports.push(Report {
                label: format!("{model} auto/official per tick"),
                bpe_calls: official_calls,
                ticks,
                cpu_ms: started.elapsed().as_millis(),
                total: last.total,
                source: last.source,
                fallbacks: 0,
                recomputes: 0,
            });

            // 2. Candidate auto: the incremental counter, one provisional per tick.
            tokens::reset_bpe_calls();
            let started = std::time::Instant::now();
            let mut counter = IncrementalCounter::new(Arc::clone(&tokenizer));
            let mut ticks = 0u64;
            let mut recomputes = 0u64;
            let mut previous = 0u64;
            for (index, delta) in deltas.iter().enumerate() {
                counter.push(0, delta);
                if index % tick_every == 0 || index + 1 == deltas.len() {
                    let provisional = counter.provisional().expect("no fallback").total;
                    // A provisional below the previous one means a piece was
                    // re-split (the open tail shrank): counted as a recompute.
                    if provisional < previous {
                        recomputes += 1;
                    }
                    previous = provisional;
                    ticks += 1;
                }
            }
            let final_breakdown = counter.finalize().expect("no fallback");
            let incremental_calls = tokens::bpe_calls();
            let official_total = tokenizer.count(&text);
            assert_eq!(
                final_breakdown.total, official_total,
                "{model}: finalize == official"
            );
            reports.push(Report {
                label: format!("{model} auto/incremental"),
                bpe_calls: incremental_calls,
                ticks,
                cpu_ms: started.elapsed().as_millis(),
                total: final_breakdown.total,
                source: final_breakdown.source,
                fallbacks: u64::from(counter.fallback() == Some(CountFallback::OpenTooLarge)),
                recomputes,
            });

            // 3. estimate: buckets only, no tokenizer.
            tokens::reset_bpe_calls();
            let started = std::time::Instant::now();
            let mut buckets = [CharBuckets::default(); TIERS];
            for delta in &deltas {
                buckets[0].push_str(delta);
            }
            let estimate = tokens::heuristic_breakdown(&text, "", "");
            let estimate_calls = tokens::bpe_calls();
            reports.push(Report {
                label: format!("{model} estimate (buckets)"),
                bpe_calls: estimate_calls,
                ticks: deltas.len() as u64,
                cpu_ms: started.elapsed().as_millis(),
                total: estimate.total,
                source: estimate.source,
                fallbacks: 0,
                recomputes: 0,
            });
            assert_eq!(buckets[0].tokens(), estimate.text);
            assert_eq!(estimate.source, TokenSource::Estimated);
            assert_eq!(estimate_calls, 0, "estimate never runs BPE");
        }
        let mut out = String::new();
        out.push_str(&format!(
            "V19-07 counting benchmark: {} bytes streamed in 24-byte deltas, one tick per 4 deltas\n",
            text.len()
        ));
        for report in &reports {
            out.push_str(&format!(
                "{:<42} BPE piece evals {:>10}  ticks {:>5}  CPU {:>5} ms  total {:>6} ({:?})  fallbacks {} open-tail recomputes {}\n",
                report.label, report.bpe_calls, report.ticks, report.cpu_ms, report.total, report.source, report.fallbacks, report.recomputes
            ));
        }
        println!("{out}");
        // The invariants the numbers must show.
        for window in reports.chunks(3) {
            let (official, incremental, estimate) = (&window[0], &window[1], &window[2]);
            assert_eq!(official.total, incremental.total, "same official total");
            assert!(
                incremental.bpe_calls * 10 < official.bpe_calls,
                "{}: incremental {} vs official {} BPE evals",
                incremental.label,
                incremental.bpe_calls,
                official.bpe_calls
            );
            assert_eq!(estimate.bpe_calls, 0);
            assert!(estimate.total > 0 && estimate.source == TokenSource::Estimated);
            assert_eq!(
                incremental.fallbacks, 0,
                "the mixed answer stays on the incremental path"
            );
        }
    }

    #[test]
    fn oversized_open_pieces_fall_back_instead_of_miscounting() {
        let tokenizer = tokenizer("glm-5.3");
        let mut counter = IncrementalCounter::new(Arc::clone(&tokenizer));
        // One 70 KiB word: the open piece exceeds 64 KiB.
        counter.push(0, &"a".repeat(70_000));
        assert_eq!(counter.fallback(), Some(CountFallback::OpenTooLarge));
        assert_eq!(counter.provisional(), None);
        assert_eq!(counter.finalize(), None);
        // The heuristic buckets still work (one ceil over the raw count).
        assert_eq!(counter.heuristic().total, 20_000);
        assert_eq!(
            counter.heuristic().total,
            tokens::heuristic_tokens(&"a".repeat(70_000))
        );
        // Just under the bound: no fallback.
        let mut counter = IncrementalCounter::new(Arc::clone(&tokenizer));
        counter.push(0, &"a".repeat(60_000));
        assert_eq!(counter.fallback(), None);
        let finalized = counter.finalize().expect("no fallback");
        assert_eq!(
            finalized.total,
            tokenizer.count(&"a".repeat(60_000)),
            "a long word still tokenizes exactly"
        );
    }

    #[test]
    fn provisional_never_exceeds_the_final_count() {
        let tokenizer = tokenizer("deepseek-chat");
        let text = "The quick brown fox jumps over the lazy dog. 中文测试 with numbers 12345 and <|begin▁of▁sentence｜> special tokens.";
        let mut counter = IncrementalCounter::new(Arc::clone(&tokenizer));
        let mut rest = text;
        let mut last = 0;
        while !rest.is_empty() {
            let end = rest.len().min(3);
            let end = {
                let mut end = end;
                while !rest.is_char_boundary(end) {
                    end -= 1;
                }
                end.min(rest.len())
            };
            counter.push(0, &rest[..end]);
            rest = &rest[end..];
            last = counter.provisional().expect("no fallback").total;
        }
        let final_total = counter.finalize().expect("no fallback").total;
        assert_eq!(final_total, tokenizer.count(text));
        assert!(last <= final_total, "the provisional never overcounts");
    }

    #[test]
    fn heuristic_buckets_take_one_ceil_per_tier() {
        let mut buckets = [CharBuckets::default(); TIERS];
        for (tier, text) in ["你好", " hello", "世界 world"].into_iter().enumerate() {
            for ch in text.chars() {
                buckets[tier].push_str(&ch.to_string());
            }
        }
        assert_eq!(buckets[0].tokens(), tokens::heuristic_tokens("你好"));
        assert_eq!(buckets[1].tokens(), tokens::heuristic_tokens(" hello"));
        assert_eq!(buckets[2].tokens(), tokens::heuristic_tokens("世界 world"));
        // Not the sum of per-delta ceils.
        assert_eq!(buckets[1].tokens(), 2);
        assert_ne!(
            " hello"
                .chars()
                .map(|c| tokens::heuristic_tokens(&c.to_string()))
                .sum::<u64>(),
            buckets[1].tokens()
        );
    }

    #[test]
    fn normalization_boundaries_cover_marks_and_hangul_jamo() {
        assert_eq!(last_normalization_boundary("abc"), 2);
        assert_eq!(
            last_normalization_boundary("a\u{0301}"),
            0,
            "mark joins the starter"
        );
        assert_eq!(
            last_normalization_boundary("a\u{0301}b"),
            3,
            "starter after a mark"
        );
        assert_eq!(
            last_normalization_boundary("\u{1100}\u{1161}"),
            0,
            "L + V composes"
        );
        assert_eq!(
            last_normalization_boundary("\u{1100}\u{1161}\u{11A8}"),
            0,
            "LV + T composes"
        );
        assert_eq!(
            last_normalization_boundary("\u{1100}\u{1161}\u{11A8}\u{1100}"),
            9,
            "L starts a new syllable"
        );
        assert_eq!(last_normalization_boundary("e\u{0301}\u{0301}"), 0);
        // Decomposed-only characters (not NFC) stay open.
        assert_eq!(
            last_normalization_boundary("a\u{2126}"),
            0,
            "OHM SIGN is not NFC"
        );
        // Composing starters (Bengali E + AA).
        assert_eq!(last_normalization_boundary("\u{09C7}\u{09BE}"), 0);
        assert_eq!(last_normalization_boundary("\u{09BE}\u{09C7}"), 3);
        assert!(BUILTINS.iter().all(|builtin| {
            tokenizer_for_model(Some(match builtin.id {
                "hunyuan" => "hy3-preview",
                other => other,
            }))
            .expect("tokenizer")
            .pretokenizer()
            .iter()
            .all(|regex| pattern_is_incremental_safe(regex.as_str()))
        }));
        assert!(pattern_is_incremental_safe(
            "(?i:'s|'t)|[a-z^$]+|\\s+(?!\\S)|\\s+"
        ));
        assert!(!pattern_is_incremental_safe("^abc"));
        assert!(!pattern_is_incremental_safe("a$b"));
        assert!(!pattern_is_incremental_safe("\\bword"));
        assert!(!pattern_is_incremental_safe("(?<=x)y"));
        assert!(!pattern_is_incremental_safe("x(?=yy)"));
    }
}
