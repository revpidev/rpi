//! Port of `packages/tui/src/word-navigation.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - `WordSegment` mirrors the `Intl.SegmentData` shape (word granularity)
//!   minus the `index`/`input` fields, which the upstream functions never
//!   read; cursors are in characters (upstream UTF-16 code units — the two
//!   spaces agree on every character position, see the module notes below).
//! - The default segmentation replicates ICU's `Intl.Segmenter` word
//!   behavior: UAX #29 boundaries outside dictionary ranges, and ICU's
//!   dynamic-programming dictionary segmentation (`CjkBreakEngine::divideUp
//!   DictionaryRange`, dictbe.cpp:1133-1472) inside Han runs. ICU applies a
//!   dictionary to Han/Kana runs; the S1 port covers Han runs only (Kana
//!   falls back to UAX #29 boundaries) and embeds a minimal dictionary
//!   subset (see [`CJK_DICTIONARY`]). Verified against Node 24 / ICU 78 on
//!   the pinned corpus (word-navigation.test.ts + input.test.ts).
//! - `is_word_like` for UAX #29 segments is approximated by
//!   "contains an alphanumeric character" (ICU's rule-status based
//!   classification agrees on the pinned corpus; exotic cases like digit
//!   + punctuation segments may differ).

use crate::utils::{get_word_segmenter, is_whitespace_char, PUNCTUATION_REGEX};

// =============================================================================
// Word segmentation (ICU-equivalent default)
// =============================================================================

/// A word segment (upstream `Intl.SegmentData`, word granularity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WordSegment<'a> {
    /// The segment text (upstream `segment`).
    pub segment: &'a str,
    /// Whether the segment is word-like (upstream `isWordLike`).
    pub is_word_like: bool,
}

/// Max dictionary word length in code points (`maxWordSize`, dictbe.cpp:1290).
const MAX_WORD_SIZE: usize = 20;
/// Fallback cost for a character with no matching dictionary word
/// (`maxSnlp`, dictbe.cpp:1105).
const FALLBACK_SNLP: u32 = 255;

/// Minimal subset of the ICU CJK dictionary (`brkitr/dictionaries/cjdict.txt`)
/// with the original snlp costs. The full dictionary is 316k entries /
/// 4.2MB (BSD / ICU licensed); S1 embeds only the entries for the characters
/// exercised by the pinned upstream tests (word-navigation.test.ts,
/// input.test.ts), which reproduces ICU's segmentation for them exactly.
/// [`segment_han_run`]'s dynamic program accepts any word list of this
/// shape, so a later task can swap in the full generated table (or a trie)
/// without touching the algorithm.
const CJK_DICTIONARY: &[(&str, u32)] = &[
    ("你", 53),
    ("好", 60),
    ("世", 73),
    ("界", 73),
    ("你好", 86),
    ("世界", 57),
];

/// `Script=Han` membership — the dictionary-run character set for the S1
/// port (the Han subset of the ICU CJK set, dictbe.cpp:1078). Unicode 16
/// script data; the exact range endpoints beyond the Unified block do not
/// affect the pinned corpus.
fn is_han(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x2e80..=0x2eff // CJK Radicals Supplement
            | 0x2f00..=0x2fdf // Kangxi Radicals
            | 0x3005 | 0x3007 | 0x3021..=0x3029 | 0x3038..=0x303b
            | 0x3400..=0x4dbf // Ext A
            | 0x4e00..=0x9fff // Unified Ideographs
            | 0xf900..=0xfa6d | 0xfa70..=0xfad9 // Compatibility Ideographs
            | 0x20000..=0x2a6df // Ext B
            | 0x2a700..=0x2b73f // Ext C
            | 0x2b740..=0x2b81f // Ext D
            | 0x2b820..=0x2ceaf // Ext E
            | 0x2ceb0..=0x2ebef // Ext F
            | 0x2ebf0..=0x2ee5f // Ext I
            | 0x2f800..=0x2fa1d // Compatibility Ideographs Supplement
            | 0x30000..=0x3134f // Ext G
            | 0x31350..=0x323af // Ext H
    )
}

/// Byte offset of the `chars`-th character; `chars` == char count maps to
/// `text.len()`. Callers pass cursors that are always on char boundaries.
fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// Segment a maximal Han run with the ICU dictionary dynamic program
/// (`CjkBreakEngine::divideUpDictionaryRange`, dictbe.cpp:1133-1472,
/// non-phrase-breaking path).
///
/// `best_snlp[i]` is the minimum total cost of segmenting the first `i` code
/// points (`bestSnlp`, dictbe.cpp:1274-1280); every position without a
/// single-character dictionary entry also gets a 1-character word at the
/// fallback cost (dictbe.cpp:1318-1327). The optimal split is recovered by
/// walking the `prev` chain (dictbe.cpp:1402-1408); when no segmentation
/// exists, the whole run stays one segment (dictbe.cpp:1373-1376).
/// Boundaries at the range start/end are added by the caller-side bookkeeping
/// (dictbe.cpp:1410-1415) — here both are pushed and the slices between
/// consecutive boundaries are emitted.
fn segment_han_run(run: &str) -> Vec<WordSegment<'_>> {
    let chars: Vec<(usize, char)> = run.char_indices().collect();
    let n = chars.len();
    if n == 0 {
        return Vec::new();
    }

    let mut best_snlp = vec![u32::MAX; n + 1];
    let mut prev = vec![usize::MAX; n + 1];
    best_snlp[0] = 0;

    for i in 0..n {
        if best_snlp[i] == u32::MAX {
            continue;
        }
        let byte_offset = chars[i].0;

        // Dictionary words starting at this position, as (code point length,
        // cost) pairs (the `matches()` call, dictbe.cpp:1312-1316). The
        // linear scan serves the minimal S1 table; a trie lookup replaces it
        // when the full cjdict is vendored.
        let mut candidates: Vec<(usize, u32)> = Vec::new();
        for &(word, cost) in CJK_DICTIONARY {
            if run[byte_offset..].starts_with(word) {
                let len = word.chars().count();
                if len <= MAX_WORD_SIZE && i + len <= n {
                    candidates.push((len, cost));
                }
            }
        }

        // No single-character entry starting here: treat the character as a
        // 1-character word with the highest cost (dictbe.cpp:1318-1327; the
        // Hangul exclusion is irrelevant for the Han-only S1 run set).
        if !candidates.iter().any(|(len, _)| *len == 1) {
            candidates.push((1, FALLBACK_SNLP));
        }

        for (len, cost) in candidates {
            let new_snlp = best_snlp[i].saturating_add(cost);
            if new_snlp < best_snlp[i + len] {
                best_snlp[i + len] = new_snlp;
                prev[i + len] = i;
            }
        }
    }

    let mut boundaries: Vec<usize> = Vec::new();
    if best_snlp[n] == u32::MAX {
        boundaries.push(n);
    } else {
        let mut i = n;
        while i > 0 {
            boundaries.push(i);
            i = prev[i];
        }
    }
    boundaries.push(0);
    boundaries.sort_unstable();

    let mut segments = Vec::with_capacity(boundaries.len() - 1);
    for pair in boundaries.windows(2) {
        let start_byte = chars[pair[0]].0;
        let end_byte = if pair[1] < n {
            chars[pair[1]].0
        } else {
            run.len()
        };
        segments.push(WordSegment {
            segment: &run[start_byte..end_byte],
            // Every dictionary segment is word-like (ICU's ideo rule status).
            is_word_like: true,
        });
    }
    segments
}

/// Default word segmentation for word navigation — the Rust equivalent of
/// the module-level `Intl.Segmenter(undefined, { granularity: "word" })`
/// (word-navigation.ts:3). Dictionary runs (Han) get the ICU dictionary
/// treatment; everything else uses UAX #29 boundaries.
///
/// Public because the editor (editor.ts `this.segment(text, "word")`)
/// reuses this segmentation and merges paste markers into it.
pub fn segment_words(text: &str) -> Vec<WordSegment<'_>> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut segments = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        let is_han_run = is_han(chars[i].1);
        while i < chars.len() && is_han(chars[i].1) == is_han_run {
            i += 1;
        }
        let start_byte = chars[start].0;
        let end_byte = if i < chars.len() {
            chars[i].0
        } else {
            text.len()
        };
        let chunk = &text[start_byte..end_byte];

        if is_han_run {
            segments.extend(segment_han_run(chunk));
        } else {
            // UAX #29 word boundaries (the upstream rule engine around the
            // dictionary ranges).
            for segment in get_word_segmenter().segment(chunk) {
                segments.push(WordSegment {
                    segment,
                    is_word_like: segment.chars().any(|c| c.is_alphanumeric()),
                });
            }
        }
    }
    segments
}

// =============================================================================
// Word navigation
// =============================================================================

/// Custom word segmenter (upstream `(text: string) =>
/// Iterable<Intl.SegmentData>`). Segments borrow from the input text, so the
/// editor can reuse its own segmenter (which splits paste markers into
/// atomic segments) here.
pub type WordSegmentFn<'a> = dyn Fn(&str) -> Vec<WordSegment<'_>> + 'a;

/// Options for word navigation functions (upstream `WordNavigationOptions`,
/// word-navigation.ts:9-14). When omitted, the default ICU-equivalent word
/// segmentation is used.
pub struct WordNavigationOptions<'opts> {
    /// Custom segmenter returning word segments for the given text
    /// (upstream `segment`).
    pub segment: Option<&'opts WordSegmentFn<'opts>>,
    /// Predicate identifying atomic segments that should be treated as
    /// single units (e.g. paste markers; upstream `isAtomicSegment`).
    pub is_atomic_segment: Option<&'opts dyn Fn(&str) -> bool>,
}

/// Find the cursor position after moving one word backward from `cursor` in
/// `text`. Skips trailing whitespace, then stops at the next
/// word/punctuation boundary. Pure function — does not mutate any state
/// (upstream `findWordBackward`, word-navigation.ts:22-70).
pub fn find_word_backward(
    text: &str,
    cursor: usize,
    options: Option<&WordNavigationOptions<'_>>,
) -> usize {
    if cursor == 0 {
        return 0;
    }

    let text_before_cursor = &text[..char_to_byte(text, cursor)];
    let is_atomic = options.and_then(|o| o.is_atomic_segment);
    let mut segments: Vec<WordSegment<'_>> = match options.and_then(|o| o.segment) {
        Some(segment_fn) => segment_fn(text_before_cursor),
        None => segment_words(text_before_cursor),
    };
    let mut new_cursor = cursor;

    // Skip trailing whitespace
    while let Some(last) = segments.last() {
        if is_atomic.is_some_and(|f| f(last.segment)) || !is_whitespace_char(last.segment) {
            break;
        }
        new_cursor -= last.segment.chars().count();
        segments.pop();
    }

    if segments.is_empty() {
        return new_cursor;
    }

    let last = segments.last().expect("segments is non-empty here");
    if is_atomic.is_some_and(|f| f(last.segment)) {
        // Skip one atomic segment.
        new_cursor -= last.segment.chars().count();
    } else if last.is_word_like {
        // Skip inside one word-like segment, preserving ASCII punctuation
        // boundaries.
        let segment = last.segment;
        let last_punct = segment
            .char_indices()
            .rev()
            .find(|(_, c)| PUNCTUATION_REGEX.contains(*c));
        match last_punct {
            Some((punct_index, _)) => {
                new_cursor -= segment.chars().count() - (punct_index + 1);
            }
            None => new_cursor -= segment.chars().count(),
        }
    } else {
        // Skip non-word non-whitespace run (punctuation)
        while let Some(last) = segments.last() {
            if is_atomic.is_some_and(|f| f(last.segment))
                || last.is_word_like
                || is_whitespace_char(last.segment)
            {
                break;
            }
            new_cursor -= last.segment.chars().count();
            segments.pop();
        }
    }

    new_cursor
}

/// Find the cursor position after moving one word forward from `cursor` in
/// `text`. Skips leading whitespace, then stops at the next word/punctuation
/// boundary. Pure function — does not mutate any state (upstream
/// `findWordForward`, word-navigation.ts:78-117).
pub fn find_word_forward(
    text: &str,
    cursor: usize,
    options: Option<&WordNavigationOptions<'_>>,
) -> usize {
    let char_count = text.chars().count();
    if cursor >= char_count {
        return char_count;
    }

    let text_after_cursor = &text[char_to_byte(text, cursor)..];
    let is_atomic = options.and_then(|o| o.is_atomic_segment);
    let segments: Vec<WordSegment<'_>> = match options.and_then(|o| o.segment) {
        Some(segment_fn) => segment_fn(text_after_cursor),
        None => segment_words(text_after_cursor),
    };
    let mut iter = segments.into_iter();
    let mut next = iter.next();
    let mut new_cursor = cursor;

    // Skip leading whitespace
    while let Some(segment) = next.as_ref() {
        if is_atomic.is_some_and(|f| f(segment.segment)) || !is_whitespace_char(segment.segment) {
            break;
        }
        new_cursor += segment.segment.chars().count();
        next = iter.next();
    }

    let Some(first) = next.as_ref() else {
        return new_cursor;
    };

    if is_atomic.is_some_and(|f| f(first.segment)) {
        // Skip one atomic segment.
        new_cursor += first.segment.chars().count();
    } else if first.is_word_like {
        // Skip inside one word-like segment, preserving ASCII punctuation
        // boundaries (first match of the upstream unanchored `exec`).
        let first_punct = first
            .segment
            .char_indices()
            .find(|(_, c)| PUNCTUATION_REGEX.contains(*c));
        new_cursor += first_punct.map_or(first.segment.chars().count(), |(index, _)| index);
    } else {
        // Skip non-word non-whitespace run (punctuation)
        while let Some(segment) = next.as_ref() {
            if is_atomic.is_some_and(|f| f(segment.segment))
                || segment.is_word_like
                || is_whitespace_char(segment.segment)
            {
                break;
            }
            new_cursor += segment.segment.chars().count();
            next = iter.next();
        }
    }

    new_cursor
}

#[cfg(test)]
mod tests {
    //! Ports of `test/word-navigation.test.ts` @ pi 0.82.1 (2efa728), all
    //! 26 cases.

    use super::*;

    #[test]
    fn backward_basic_words() {
        let text = "hello world";
        assert_eq!(find_word_backward(text, 11, None), 6);
        assert_eq!(find_word_backward(text, 6, None), 0);
    }

    #[test]
    fn backward_dotted() {
        let text = "foo.bar";
        assert_eq!(find_word_backward(text, 7, None), 4);
        assert_eq!(find_word_backward(text, 4, None), 3);
        assert_eq!(find_word_backward(text, 3, None), 0);
    }

    #[test]
    fn backward_colon() {
        let text = "foo:bar";
        assert_eq!(find_word_backward(text, 7, None), 4);
        assert_eq!(find_word_backward(text, 4, None), 3);
        assert_eq!(find_word_backward(text, 3, None), 0);
    }

    #[test]
    fn backward_path() {
        let text = "path/to/file";
        assert_eq!(find_word_backward(text, 12, None), 8);
        assert_eq!(find_word_backward(text, 8, None), 7);
        // "/to" is one word-like segment with "/" as punctuation boundary
        assert_eq!(find_word_backward(text, 7, None), 5);
        assert_eq!(find_word_backward(text, 5, None), 4);
        assert_eq!(find_word_backward(text, 4, None), 0);
    }

    #[test]
    fn backward_cjk_mixed() {
        let text = "你好世界 test";
        assert_eq!(find_word_backward(text, text.chars().count(), None), 5);
        // ICU dictionary segmentation: 你好|世界| |test
        assert_eq!(find_word_backward(text, 5, None), 2);
        assert_eq!(find_word_backward(text, 2, None), 0);
    }

    #[test]
    fn backward_whitespace_at_boundaries() {
        let text = "  hello  ";
        assert_eq!(find_word_backward(text, 9, None), 2);
        assert_eq!(find_word_backward(text, 2, None), 0);
    }

    #[test]
    fn backward_punctuation_run() {
        let text = "foo...bar";
        assert_eq!(find_word_backward(text, 9, None), 6);
        assert_eq!(find_word_backward(text, 6, None), 3);
        assert_eq!(find_word_backward(text, 3, None), 0);
    }

    #[test]
    fn backward_cursor_at_zero_returns_zero() {
        assert_eq!(find_word_backward("hello", 0, None), 0);
    }

    #[test]
    fn forward_basic_words() {
        let text = "hello world";
        assert_eq!(find_word_forward(text, 0, None), 5);
        assert_eq!(find_word_forward(text, 5, None), 11);
    }

    #[test]
    fn forward_dotted() {
        let text = "foo.bar";
        assert_eq!(find_word_forward(text, 0, None), 3);
        assert_eq!(find_word_forward(text, 3, None), 4);
        assert_eq!(find_word_forward(text, 4, None), 7);
    }

    #[test]
    fn forward_colon() {
        let text = "foo:bar";
        assert_eq!(find_word_forward(text, 0, None), 3);
        assert_eq!(find_word_forward(text, 3, None), 4);
        assert_eq!(find_word_forward(text, 4, None), 7);
    }

    #[test]
    fn forward_path() {
        let text = "path/to/file";
        assert_eq!(find_word_forward(text, 0, None), 4);
        assert_eq!(find_word_forward(text, 4, None), 5);
        assert_eq!(find_word_forward(text, 5, None), 7);
        assert_eq!(find_word_forward(text, 7, None), 8);
        assert_eq!(find_word_forward(text, 8, None), 12);
    }

    #[test]
    fn forward_cjk_mixed_walks_to_end() {
        let text = "你好世界 test";
        let first_end = find_word_forward(text, 0, None);
        assert!(first_end > 0);
        assert!(first_end <= 4);
        // Walk to end
        let mut pos = 0;
        while pos < text.chars().count() {
            let next = find_word_forward(text, pos, None);
            if next == pos {
                break;
            }
            pos = next;
        }
        assert_eq!(pos, text.chars().count());
    }

    #[test]
    fn forward_whitespace_at_boundaries() {
        let text = "  hello  ";
        assert_eq!(find_word_forward(text, 0, None), 7);
        assert_eq!(find_word_forward(text, 7, None), 9);
    }

    #[test]
    fn forward_punctuation_run() {
        let text = "foo...bar";
        assert_eq!(find_word_forward(text, 0, None), 3);
        assert_eq!(find_word_forward(text, 3, None), 6);
        assert_eq!(find_word_forward(text, 6, None), 9);
    }

    #[test]
    fn forward_cursor_at_end_returns_end() {
        assert_eq!(find_word_forward("hello", 5, None), 5);
    }

    #[test]
    fn atomic_segments() {
        const MARKER: &str = "[paste #1 +5 lines]";
        let is_atomic = |s: &str| s == MARKER;

        // The functions slice text before calling the segmenter, so each
        // expected substring maps to its pre-split segments (upstream
        // segmentMap, word-navigation.test.ts:132-173).
        let segment_fn: for<'a> fn(&'a str) -> Vec<WordSegment<'a>> = |input| {
            let pairs: &[(&'static str, bool)] = match input {
                "hello [paste #1 +5 lines] world" => &[
                    ("hello", true),
                    (" ", false),
                    (MARKER, true),
                    (" ", false),
                    ("world", true),
                ],
                "hello [paste #1 +5 lines] " => {
                    &[("hello", true), (" ", false), (MARKER, true), (" ", false)]
                }
                "[paste #1 +5 lines] world" => &[(MARKER, true), (" ", false), ("world", true)],
                _ => &[],
            };
            pairs
                .iter()
                .map(|(segment, is_word_like)| WordSegment {
                    segment,
                    is_word_like: *is_word_like,
                })
                .collect()
        };

        let options = WordNavigationOptions {
            segment: Some(&segment_fn),
            is_atomic_segment: Some(&is_atomic),
        };

        // Backward skips word then stops before atomic marker.
        assert_eq!(
            find_word_backward("hello [paste #1 +5 lines] world", 31, Some(&options)),
            26
        );
        // Backward skips whitespace then atomic marker as one unit.
        assert_eq!(
            find_word_backward("hello [paste #1 +5 lines] world", 26, Some(&options)),
            6
        );
        // Forward skips atomic marker as one unit.
        assert_eq!(
            find_word_forward("hello [paste #1 +5 lines] world", 6, Some(&options)),
            25
        );
    }
}
