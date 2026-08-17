//! Diff rendering — port of
//! `packages/coding-agent/src/modes/interactive/components/diff.ts` @ pi
//! 0.82.1 (2efa728).
//!
//! ## Word-diff engine selection
//!
//! Upstream imports the npm `diff` package (`diff.ts:1`, vendored at
//! `external/pi/node_modules/diff`, jsdiff v8.x) and calls `diffWords`
//! (diff.ts:27). The workspace does not depend on `similar`, and its word
//! splitter also does not reproduce jsdiff's semantics (trimmed-equality
//! token comparison, whitespace stitching, and the
//! `dedupeWhitespaceInChangeObjects` pass). Instead, the jsdiff `diffWords`
//! pipeline is ported verbatim — tokenizer (`word.js`), Myers diff with
//! diagonal pruning (`base.js`), and the whitespace-dedup post-process — so
//! the rendered intra-line highlighting is byte-identical to upstream. Unit
//! tests pin this against reference vectors generated with the real vendored
//! jsdiff.
//!
//! Intentional differences:
//! - Options are fixed to upstream's call site (`diff.ts:27`): no
//!   `ignoreCase`, no `intlSegmenter`, no `maxEditLength`/`timeout`.
//! - JS string indexing is UTF-16; the port indexes by `char`. They agree
//!   for BMP text (the only input here is diff line content).
//! - The theme is passed explicitly instead of read from the global `theme`
//!   getter (theme.ts:799-816).
//!
//! Ported source lines are anchored to
//! `external/pi/node_modules/diff/libcjs/diff/{word,base}.js` and
//! `external/pi/node_modules/diff/libcjs/util/string.js`.

use crate::core::themes::Theme;

// ===========================================================================
// jsdiff word-diff engine (libcjs/diff/word.js, libcjs/diff/base.js)
// ===========================================================================

/// JS `\s` — WhiteSpace + LineTerminator (ES2024).
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// The `extendedWordChars` class (word.js:29-31): what `diffWords` counts as
/// a word character.
fn is_word_char(c: char) -> bool {
    matches!(
        c,
        'a'..='z'
            | 'A'..='Z'
            | '0'..='9'
            | '_'
            | '\u{AD}'
            | '\u{C0}'..='\u{D6}'
            | '\u{D8}'..='\u{F6}'
            | '\u{F8}'..='\u{2C6}'
            | '\u{2C8}'..='\u{2D7}'
            | '\u{2DE}'..='\u{2FF}'
            | '\u{1E00}'..='\u{1EFF}'
    )
}

/// `leadingWs` (string.js:130-140): the `^\s*` match.
fn leading_ws(text: &str) -> &str {
    let end = text
        .char_indices()
        .take_while(|&(_, c)| is_js_whitespace(c))
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    &text[..end]
}

/// `trailingWs` (string.js:104-128).
fn trailing_ws(text: &str) -> &str {
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        if !is_js_whitespace(c) {
            break;
        }
        start = i;
    }
    &text[start..]
}

/// `leadingAndTrailingWs` (string.js:142-160, non-segmenter path). Owned
/// strings so callers can hold the whitespace while mutating the source.
fn leading_and_trailing_ws(text: &str) -> (String, String) {
    (leading_ws(text).to_string(), trailing_ws(text).to_string())
}

/// JS `String.trim()` — trim ECMA-262 whitespace (used by
/// `WordDiff.equals`, word.js:42-47).
fn js_trim(text: &str) -> &str {
    let start = text
        .char_indices()
        .find(|&(_, c)| !is_js_whitespace(c))
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let end = text[start..]
        .char_indices()
        .rev()
        .find(|&(_, c)| !is_js_whitespace(c))
        .map(|(i, c)| start + i + c.len_utf8())
        .unwrap_or(start);
    &text[start..end]
}

/// `longestCommonPrefix` (string.js:9-15).
fn longest_common_prefix(a: &str, b: &str) -> String {
    let mut end = 0;
    let mut a_chars = a.chars();
    let mut b_chars = b.chars();
    loop {
        match (a_chars.next(), b_chars.next()) {
            (Some(x), Some(y)) if x == y => end += x.len_utf8(),
            _ => break,
        }
    }
    a[..end].to_string()
}

/// `longestCommonSuffix` (string.js:17-29).
fn longest_common_suffix(a: &str, b: &str) -> String {
    if a.is_empty() || b.is_empty() || a.chars().next_back() != b.chars().next_back() {
        return String::new();
    }
    let mut count = 0;
    let mut a_chars = a.chars();
    let mut b_chars = b.chars();
    loop {
        match (a_chars.next_back(), b_chars.next_back()) {
            (Some(x), Some(y)) if x == y => count += 1,
            _ => break,
        }
    }
    let char_count = a.chars().count();
    a.chars().skip(char_count - count).collect()
}

/// `replacePrefix` (string.js:31-36) — upstream throws when the prefix does
/// not match; that invariant is guaranteed by the algorithm, so the port
/// falls back to the input unchanged instead of panicking.
fn replace_prefix(string: &str, old_prefix: &str, new_prefix: &str) -> String {
    if let Some(rest) = string.strip_prefix(old_prefix) {
        format!("{new_prefix}{rest}")
    } else {
        string.to_string()
    }
}

/// `replaceSuffix` (string.js:38-49) — `!oldSuffix` short-circuits to
/// `string + newSuffix` upstream.
fn replace_suffix(string: &str, old_suffix: &str, new_suffix: &str) -> String {
    if old_suffix.is_empty() {
        return format!("{string}{new_suffix}");
    }
    if string.ends_with(old_suffix) {
        let cut = string.len() - old_suffix.len();
        format!("{}{}", &string[..cut], new_suffix)
    } else {
        string.to_string()
    }
}

/// `removePrefix` (string.js:51-53).
fn remove_prefix(string: &str, prefix: &str) -> String {
    replace_prefix(string, prefix, "")
}

/// `removeSuffix` (string.js:55-57).
fn remove_suffix(string: &str, suffix: &str) -> String {
    replace_suffix(string, suffix, "")
}

/// `overlapCount` (string.js:76-118): longest overlap of the suffix of `a`
/// with the prefix of `b` (KMP-based; ported verbatim, char-indexed).
fn overlap_count(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    if b_chars.is_empty() {
        return 0;
    }
    let start_a = a_chars.len().saturating_sub(b_chars.len());
    let end_b = b_chars.len().min(a_chars.len());
    if end_b == 0 {
        return 0;
    }
    let mut map = vec![0usize; end_b];
    let mut k = 0usize;
    for j in 1..end_b {
        map[j] = if b_chars[j] == b_chars[k] { map[k] } else { k };
        while k > 0 && b_chars[j] != b_chars[k] {
            k = map[k];
        }
        if b_chars[j] == b_chars[k] {
            k += 1;
        }
    }
    k = 0;
    for &a_char in a_chars.iter().skip(start_a) {
        while k > 0 && a_char != b_chars[k] {
            k = map[k];
        }
        if a_char == b_chars[k] {
            k += 1;
        }
    }
    k
}

/// `maximumOverlap` (string.js:59-60): `string2[0..overlapCount(...)]`.
fn maximum_overlap(a: &str, b: &str) -> String {
    let count = overlap_count(a, b);
    b.chars().take(count).collect()
}

/// One change object from `diffWords` (`{ value, added?, removed? }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WordChange {
    pub value: String,
    pub added: bool,
    pub removed: bool,
}

/// `WordDiff.tokenize` (word.js:34-81): regex-split into word runs,
/// whitespace runs, and single non-word chars, then stitch whitespace onto
/// adjacent tokens. The port scans the char classes directly instead of
/// running the regex (same token stream).
fn tokenize(value: &str) -> Vec<String> {
    // `parts = value.match(tokenizeIncludingWhitespace) || []` (word.js:51-55).
    let mut parts: Vec<String> = Vec::new();
    let mut iter = value.char_indices().peekable();
    while let Some(&(start, c)) = iter.peek() {
        if is_word_char(c) {
            let mut end = start;
            while let Some(&(i, ch)) = iter.peek() {
                if !is_word_char(ch) {
                    break;
                }
                end = i + ch.len_utf8();
                iter.next();
            }
            parts.push(value[start..end].to_string());
        } else if is_js_whitespace(c) {
            let mut end = start;
            while let Some(&(i, ch)) = iter.peek() {
                if !is_js_whitespace(ch) {
                    break;
                }
                end = i + ch.len_utf8();
                iter.next();
            }
            parts.push(value[start..end].to_string());
        } else {
            parts.push(c.to_string());
            iter.next();
        }
    }

    // Stitching (word.js:58-79).
    let mut tokens: Vec<String> = Vec::new();
    let mut prev_part: Option<String> = None;
    for part in &parts {
        if part.chars().any(is_js_whitespace) {
            // Whitespace part: append to the previous token (or its own
            // token when it is the first part).
            if prev_part.is_none() {
                tokens.push(part.clone());
            } else {
                let last = tokens.pop().expect("tokens non-empty after a part");
                tokens.push(last + part);
            }
        } else if let Some(prev) = &prev_part {
            if prev.chars().any(is_js_whitespace) {
                // Non-whitespace part after a whitespace part: prepend the
                // whitespace, unless it was already stitched to the previous
                // token (word.js:63-71).
                if tokens.last() == Some(prev) {
                    let last = tokens.pop().expect("tokens non-empty after a part");
                    tokens.push(last + part);
                } else {
                    tokens.push(format!("{prev}{part}"));
                }
            } else {
                tokens.push(part.clone());
            }
        } else {
            tokens.push(part.clone());
        }
        prev_part = Some(part.clone());
    }
    tokens
}

/// `WordDiff.join` (word.js:84-94): strip leading whitespace from every
/// token except the first.
fn word_join(tokens: &[String]) -> String {
    let mut out = String::new();
    for (i, token) in tokens.iter().enumerate() {
        if i == 0 {
            out.push_str(token);
        } else {
            out.push_str(&token[leading_ws(token).len()..]);
        }
    }
    out
}

/// A node in the Myers best-path linked list (base.js:157-172).
#[derive(Clone)]
struct Component {
    count: usize,
    added: bool,
    removed: bool,
    previous: Option<Box<Component>>,
}

/// `bestPath` entry (base.js:74): `{ oldPos, lastComponent }`.
#[derive(Clone)]
struct Path {
    old_pos: i32,
    last_component: Option<Box<Component>>,
}

/// `addToPath` (base.js:157-172): extend a path; merge with the previous
/// component when it has the same kind.
fn add_to_path(path: &Path, added: bool, removed: bool, old_pos_inc: i32) -> Path {
    let last = &path.last_component;
    if let Some(last) = last {
        if last.added == added && last.removed == removed {
            return Path {
                old_pos: path.old_pos + old_pos_inc,
                last_component: Some(Box::new(Component {
                    count: last.count + 1,
                    added,
                    removed,
                    previous: last.previous.clone(),
                })),
            };
        }
    }
    Path {
        old_pos: path.old_pos + old_pos_inc,
        last_component: Some(Box::new(Component {
            count: 1,
            added,
            removed,
            previous: last.clone(),
        })),
    }
}

/// `extractCommon` (base.js:174-190).
fn extract_common(
    base_path: &mut Path,
    new_tokens: &[String],
    old_tokens: &[String],
    diagonal_path: i32,
) -> i32 {
    let new_len = new_tokens.len() as i32;
    let old_len = old_tokens.len() as i32;
    let mut old_pos = base_path.old_pos;
    let mut new_pos = old_pos - diagonal_path;
    let mut common_count = 0;
    while new_pos + 1 < new_len
        && old_pos + 1 < old_len
        && js_trim(&old_tokens[(old_pos + 1) as usize])
            == js_trim(&new_tokens[(new_pos + 1) as usize])
    {
        new_pos += 1;
        old_pos += 1;
        common_count += 1;
    }
    if common_count > 0 {
        base_path.last_component = Some(Box::new(Component {
            count: common_count,
            added: false,
            removed: false,
            previous: base_path.last_component.take(),
        }));
    }
    base_path.old_pos = old_pos;
    new_pos
}

/// `buildValues` (base.js:216-262) with `WordDiff.join` for the value
/// reconstruction.
fn build_values(
    last_component: Option<Box<Component>>,
    new_tokens: &[String],
    old_tokens: &[String],
) -> Vec<WordChange> {
    // Linked list -> Vec in order (base.js:218-229).
    let mut components: Vec<Box<Component>> = Vec::new();
    let mut next = last_component;
    while let Some(mut component) = next {
        let previous = component.previous.take();
        components.push(component);
        next = previous;
    }
    components.reverse();

    let mut changes: Vec<WordChange> = Vec::new();
    let mut new_pos = 0usize;
    let mut old_pos = 0usize;
    for component in components {
        let count = component.count;
        if !component.removed {
            let value = word_join(&new_tokens[new_pos..new_pos + count]);
            new_pos += count;
            if !component.added {
                old_pos += count;
            }
            changes.push(WordChange {
                value,
                added: component.added,
                removed: false,
            });
        } else {
            let value = word_join(&old_tokens[old_pos..old_pos + count]);
            old_pos += count;
            changes.push(WordChange {
                value,
                added: false,
                removed: true,
            });
        }
    }
    changes
}

/// Myers diff over token lists (base.js:64-155), sync path with no
/// `maxEditLength`/`timeout` (the call site passes no options).
fn myers_diff(old_tokens: &[String], new_tokens: &[String]) -> Vec<WordChange> {
    use std::collections::HashMap;

    let old_len = old_tokens.len() as i32;
    let new_len = new_tokens.len() as i32;
    let max_edit_length = old_len + new_len;

    // `bestPath = [{ oldPos: -1, lastComponent: undefined }]` (base.js:73).
    let mut best_path: HashMap<i32, Path> = HashMap::new();
    let mut start_path = Path {
        old_pos: -1,
        last_component: None,
    };
    let mut new_pos = extract_common(&mut start_path, new_tokens, old_tokens, 0);
    if start_path.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
        // Identity per the equality and tokenizer (base.js:80-84): a single
        // keep component spanning everything.
        return build_values(start_path.last_component, new_tokens, old_tokens);
    }
    best_path.insert(0, start_path);

    let mut min_diagonal_to_consider = i32::MIN;
    let mut max_diagonal_to_consider = i32::MAX;
    let mut edit_length = 1;
    loop {
        if edit_length > max_edit_length {
            // Unreachable for finite inputs without a maxEditLength;
            // upstream returns undefined here (base.js:140-144).
            return Vec::new();
        }
        let mut done: Option<Vec<WordChange>> = None;
        let lo = min_diagonal_to_consider.max(-edit_length);
        let hi = max_diagonal_to_consider.min(edit_length);
        let mut diagonal_path = lo;
        while diagonal_path <= hi {
            let remove_path = best_path.get(&(diagonal_path - 1)).cloned();
            let add_path = best_path.get(&(diagonal_path + 1)).cloned();
            if remove_path.is_some() {
                best_path.remove(&(diagonal_path - 1));
            }
            let can_add = add_path.as_ref().is_some_and(|p| {
                let add_path_new_pos = p.old_pos - diagonal_path;
                0 <= add_path_new_pos && add_path_new_pos < new_len
            });
            let can_remove = remove_path
                .as_ref()
                .is_some_and(|p| p.old_pos + 1 < old_len);
            if !can_add && !can_remove {
                // If this path is a terminal then prune (base.js:96-101).
                best_path.remove(&diagonal_path);
                diagonal_path += 2;
                continue;
            }
            // Select the prior path whose old-position is farthest from the
            // origin (base.js:104-115).
            let base_path = if !can_remove
                || (can_add
                    && remove_path.as_ref().expect("can_remove").old_pos
                        < add_path.as_ref().expect("can_add").old_pos)
            {
                add_to_path(add_path.as_ref().expect("can_add"), true, false, 0)
            } else {
                add_to_path(remove_path.as_ref().expect("can_remove"), false, true, 1)
            };
            let mut base_path = base_path;
            new_pos = extract_common(&mut base_path, new_tokens, old_tokens, diagonal_path);
            if base_path.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                done = Some(build_values(
                    base_path.last_component,
                    new_tokens,
                    old_tokens,
                ));
                break;
            } else {
                if base_path.old_pos + 1 >= old_len {
                    max_diagonal_to_consider = max_diagonal_to_consider.min(diagonal_path - 1);
                }
                if new_pos + 1 >= new_len {
                    min_diagonal_to_consider = min_diagonal_to_consider.max(diagonal_path + 1);
                }
                best_path.insert(diagonal_path, base_path);
            }
            diagonal_path += 2;
        }
        if let Some(done) = done {
            return done;
        }
        edit_length += 1;
    }
}

/// `WordDiff.postProcess` (word.js:96-112): track the change objects between
/// keeps and dedupe their whitespace.
fn post_process(changes: &mut [WordChange]) {
    let flags: Vec<(bool, bool)> = changes.iter().map(|c| (c.added, c.removed)).collect();
    let mut last_keep: Option<usize> = None;
    let mut insertion: Option<usize> = None;
    let mut deletion: Option<usize> = None;
    for (i, &(added, removed)) in flags.iter().enumerate() {
        if added {
            insertion = Some(i);
        } else if removed {
            deletion = Some(i);
        } else {
            if insertion.is_some() || deletion.is_some() {
                dedupe_whitespace_in_change_objects(
                    changes,
                    last_keep,
                    deletion,
                    insertion,
                    Some(i),
                );
            }
            last_keep = Some(i);
            insertion = None;
            deletion = None;
        }
    }
    if insertion.is_some() || deletion.is_some() {
        dedupe_whitespace_in_change_objects(changes, last_keep, deletion, insertion, None);
    }
}

/// `dedupeWhitespaceInChangeObjects` (word.js:131-254): redistribute
/// whitespace between keep/delete/insert objects so the rendered diff has
/// no duplicated spaces. Ported verbatim (non-segmenter path).
#[allow(clippy::too_many_arguments)]
fn dedupe_whitespace_in_change_objects(
    changes: &mut [WordChange],
    start_keep: Option<usize>,
    deletion: Option<usize>,
    insertion: Option<usize>,
    end_keep: Option<usize>,
) {
    if let (Some(d), Some(n)) = (deletion, insertion) {
        // Both a deletion and an insertion (word.js:159-199). All
        // whitespace reads happen on owned clones so the borrows do not
        // outlive the mutations.
        let (old_ws_prefix, old_ws_suffix) = leading_and_trailing_ws(&changes[d].value.clone());
        let (new_ws_prefix, new_ws_suffix) = leading_and_trailing_ws(&changes[n].value.clone());
        if let Some(s) = start_keep {
            let common = longest_common_prefix(&old_ws_prefix, &new_ws_prefix);
            let keep = changes[s].value.clone();
            changes[s].value = replace_suffix(&keep, &new_ws_prefix, &common);
            let del = changes[d].value.clone();
            changes[d].value = remove_prefix(&del, &common);
            let ins = changes[n].value.clone();
            changes[n].value = remove_prefix(&ins, &common);
        }
        if let Some(e) = end_keep {
            let common = longest_common_suffix(&old_ws_suffix, &new_ws_suffix);
            let keep = changes[e].value.clone();
            changes[e].value = replace_prefix(&keep, &new_ws_suffix, &common);
            let del = changes[d].value.clone();
            changes[d].value = remove_suffix(&del, &common);
            let ins = changes[n].value.clone();
            changes[n].value = remove_suffix(&ins, &common);
        }
    } else if let Some(n) = insertion {
        // The whitespace reflects the new text; just dedupe leading
        // whitespace against the keeps (word.js:201-211).
        if let Some(_s) = start_keep {
            let ws = leading_ws(&changes[n].value).len();
            changes[n].value = changes[n].value[ws..].to_string();
        }
        if let Some(e) = end_keep {
            let ws = leading_ws(&changes[e].value).len();
            changes[e].value = changes[e].value[ws..].to_string();
        }
    } else if let (Some(s), Some(e)) = (start_keep, end_keep) {
        // Deletion only, between two keeps (word.js:213-239).
        let Some(d) = deletion else { return };
        let new_ws_full = leading_ws(&changes[e].value).to_string();
        let (del_ws_start, del_ws_end) = leading_and_trailing_ws(&changes[d].value.clone());
        let new_ws_start = longest_common_prefix(&new_ws_full, &del_ws_start);
        let del = changes[d].value.clone();
        changes[d].value = remove_prefix(&del, &new_ws_start);
        let new_ws_remaining = remove_prefix(&new_ws_full, &new_ws_start);
        let new_ws_end = longest_common_suffix(&new_ws_remaining, &del_ws_end);
        let del = changes[d].value.clone();
        changes[d].value = remove_suffix(&del, &new_ws_end);
        let keep = changes[e].value.clone();
        changes[e].value = replace_prefix(&keep, &new_ws_full, &new_ws_end);
        // Anything left of the new whitespace that wasn't given to endKeep
        // goes to startKeep (word.js:237-239).
        let start_suffix = new_ws_full[..new_ws_full.len() - new_ws_end.len()].to_string();
        let keep = changes[s].value.clone();
        changes[s].value = replace_suffix(&keep, &new_ws_full, &start_suffix);
    } else if let Some(e) = end_keep {
        // Deletion at the start of the text (word.js:241-249).
        let Some(d) = deletion else { return };
        let end_keep_ws_prefix = leading_ws(&changes[e].value);
        let deletion_ws_suffix = trailing_ws(&changes[d].value.clone()).to_string();
        let overlap = maximum_overlap(&deletion_ws_suffix, end_keep_ws_prefix);
        let del = changes[d].value.clone();
        changes[d].value = remove_suffix(&del, &overlap);
    } else if let Some(s) = start_keep {
        // Deletion at the end of the text (word.js:251-260).
        let Some(d) = deletion else { return };
        let start_keep_ws_suffix = trailing_ws(&changes[s].value);
        let deletion_ws_prefix = leading_ws(&changes[d].value.clone()).to_string();
        let overlap = maximum_overlap(start_keep_ws_suffix, &deletion_ws_prefix);
        let del = changes[d].value.clone();
        changes[d].value = remove_prefix(&del, &overlap);
    }
}

/// `diffWords(oldStr, newStr)` (word.js:114-124): tokenize, diff, postProcess.
pub(crate) fn diff_words(old_text: &str, new_text: &str) -> Vec<WordChange> {
    let old_tokens: Vec<String> = tokenize(old_text)
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect();
    let new_tokens: Vec<String> = tokenize(new_text)
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect();
    let mut changes = myers_diff(&old_tokens, &new_tokens);
    post_process(&mut changes);
    changes
}

// ===========================================================================
// renderDiff (diff.ts)
// ===========================================================================

/// Parsed diff line (diff.ts:6-12): `^([+-\s])(\s*\d*)\s(.*)$` with JS
/// `\s`/`\d` semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedDiffLine {
    prefix: char,
    line_num: String,
    content: String,
}

/// `parseDiffLine` (diff.ts:8-12).
///
/// The regex group `(\s*\d*)` is greedy with backtracking: the separator
/// `\s` lands on the last whitespace position that can follow a
/// `\s*\d*`-shaped prefix. The port derives that position directly: try
/// right after the leading whitespace+digit runs; otherwise fall back to the
/// last whitespace char of the leading run (the digit run can never hold the
/// separator).
fn parse_diff_line(line: &str) -> Option<ParsedDiffLine> {
    let first = line.chars().next()?;
    if first != '+' && first != '-' && !is_js_whitespace(first) {
        return None;
    }
    let rest = &line[first.len_utf8()..];

    // Leading `\s*` run, then `\d*` run of `rest`.
    let ws_end = rest
        .char_indices()
        .take_while(|&(_, c)| is_js_whitespace(c))
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    let digits_end = rest[ws_end..]
        .char_indices()
        .take_while(|&(_, c)| c.is_ascii_digit())
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .map(|d| ws_end + d)
        .unwrap_or(ws_end);

    // First attempt: separator right after the greedy `\s*\d*` prefix.
    let separator = if digits_end < rest.len()
        && rest[digits_end..]
            .chars()
            .next()
            .is_some_and(is_js_whitespace)
    {
        digits_end
    } else if ws_end > 0 {
        // Backtracking: `\s*` shrinks by one so the separator becomes the
        // last whitespace char of the leading run.
        ws_end - 1
    } else {
        return None;
    };

    Some(ParsedDiffLine {
        prefix: first,
        line_num: rest[..separator].to_string(),
        content: rest[separator + 1..].to_string(),
    })
}

/// `replaceTabs` (diff.ts:17-19).
fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

/// `renderIntraLineDiff` (diff.ts:26-66): word-level diff, with inverse
/// video on changed tokens. Leading whitespace of the first removed/added
/// part is kept plain so indentation is not highlighted.
fn render_intra_line_diff(old_content: &str, new_content: &str) -> (String, String) {
    let word_diff = diff_words(old_content, new_content);

    let mut removed_line = String::new();
    let mut added_line = String::new();
    let mut is_first_removed = true;
    let mut is_first_added = true;

    for part in word_diff {
        if part.removed {
            let mut value = part.value.clone();
            if is_first_removed {
                let leading_ws = leading_ws(&value).to_string();
                value = value[leading_ws.len()..].to_string();
                removed_line.push_str(&leading_ws);
                is_first_removed = false;
            }
            if !value.is_empty() {
                removed_line.push_str(&Theme::inverse(&value));
            }
        } else if part.added {
            let mut value = part.value.clone();
            if is_first_added {
                let leading_ws = leading_ws(&value).to_string();
                value = value[leading_ws.len()..].to_string();
                added_line.push_str(&leading_ws);
                is_first_added = false;
            }
            if !value.is_empty() {
                added_line.push_str(&Theme::inverse(&value));
            }
        } else {
            removed_line.push_str(&part.value);
            added_line.push_str(&part.value);
        }
    }

    (removed_line, added_line)
}

/// `RenderDiffOptions` (diff.ts:68-71): `filePath` is unused, kept for API
/// compatibility.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderDiffOptions {
    pub file_path: Option<String>,
}

/// `renderDiff` (diff.ts:79-147): render a diff string with colored lines
/// and intra-line change highlighting.
pub fn render_diff(diff_text: &str, theme: &Theme, _options: RenderDiffOptions) -> String {
    let lines: Vec<&str> = diff_text.split('\n').collect();
    let mut result: Vec<String> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let Some(parsed) = parse_diff_line(line) else {
            result.push(theme.fg("toolDiffContext", line));
            i += 1;
            continue;
        };

        if parsed.prefix == '-' {
            // Collect consecutive removed lines (diff.ts:94-102).
            let mut removed_lines: Vec<ParsedDiffLine> = Vec::new();
            while i < lines.len() {
                let Some(p) = parse_diff_line(lines[i]) else {
                    break;
                };
                if p.prefix != '-' {
                    break;
                }
                removed_lines.push(p);
                i += 1;
            }

            // Collect consecutive added lines (diff.ts:104-111).
            let mut added_lines: Vec<ParsedDiffLine> = Vec::new();
            while i < lines.len() {
                let Some(p) = parse_diff_line(lines[i]) else {
                    break;
                };
                if p.prefix != '+' {
                    break;
                }
                added_lines.push(p);
                i += 1;
            }

            // Single-line modification: intra-line diff (diff.ts:113-125).
            if removed_lines.len() == 1 && added_lines.len() == 1 {
                let removed = &removed_lines[0];
                let added = &added_lines[0];
                let (removed_line, added_line) = render_intra_line_diff(
                    &replace_tabs(&removed.content),
                    &replace_tabs(&added.content),
                );
                result.push(theme.fg(
                    "toolDiffRemoved",
                    &format!("-{} {}", removed.line_num, removed_line),
                ));
                result.push(theme.fg(
                    "toolDiffAdded",
                    &format!("+{} {}", added.line_num, added_line),
                ));
            } else {
                // Show all removed lines first, then all added lines
                // (diff.ts:127-133).
                for removed in &removed_lines {
                    result.push(theme.fg(
                        "toolDiffRemoved",
                        &format!("-{} {}", removed.line_num, replace_tabs(&removed.content)),
                    ));
                }
                for added in &added_lines {
                    result.push(theme.fg(
                        "toolDiffAdded",
                        &format!("+{} {}", added.line_num, replace_tabs(&added.content)),
                    ));
                }
            }
        } else if parsed.prefix == '+' {
            // Standalone added line (diff.ts:135-138).
            result.push(theme.fg(
                "toolDiffAdded",
                &format!("+{} {}", parsed.line_num, replace_tabs(&parsed.content)),
            ));
            i += 1;
        } else {
            // Context line (diff.ts:139-143).
            result.push(theme.fg(
                "toolDiffContext",
                &format!(" {} {}", parsed.line_num, replace_tabs(&parsed.content)),
            ));
            i += 1;
        }
    }

    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference vectors generated with the vendored jsdiff 8.0.4
    /// (`node scripts/gen-diff-vectors.mjs`): `(old, new, parts)` where each
    /// part is `(value, kind)` with kind ∈ keep/removed/added.
    type Vector = (
        &'static str,
        &'static str,
        &'static [(&'static str, &'static str)],
    );
    const VECTORS: &[Vector] = &[
        (
            "const a = 2;",
            "const a = 3;",
            &[
                ("const a = ", "keep"),
                ("2", "removed"),
                ("3", "added"),
                (";", "keep"),
            ],
        ),
        (
            "foo bar",
            "foo baz",
            &[("foo ", "keep"), ("bar", "removed"), ("baz", "added")],
        ),
        (
            "foo bar baz",
            "foo qux baz",
            &[
                ("foo ", "keep"),
                ("bar", "removed"),
                ("qux", "added"),
                (" baz", "keep"),
            ],
        ),
        (
            "foo   bar baz",
            "foo  baz",
            &[("foo  ", "keep"), (" bar ", "removed"), ("baz", "keep")],
        ),
        (
            "hello world",
            "hello there world",
            &[("hello ", "keep"), ("there ", "added"), ("world", "keep")],
        ),
        (
            "x = 1",
            "x = 2",
            &[("x = ", "keep"), ("1", "removed"), ("2", "added")],
        ),
        (
            "a.b.c",
            "a.c",
            &[("a.", "keep"), ("b.", "removed"), ("c", "keep")],
        ),
        (
            "let x = 10;",
            "let x = 100;",
            &[
                ("let x = ", "keep"),
                ("10", "removed"),
                ("100", "added"),
                (";", "keep"),
            ],
        ),
        (
            "  const y = foo(a, b);",
            "  const y = foo(a, c);",
            &[
                ("  const y = foo(a, ", "keep"),
                ("b", "removed"),
                ("c", "added"),
                (");", "keep"),
            ],
        ),
        ("", "new content", &[("new content", "added")]),
        ("old content", "", &[("old content", "removed")]),
        ("same", "same", &[("same", "keep")]),
        (
            "import { a } from \"./m\";",
            "import { a, b } from \"./m\";",
            &[
                ("import { a", "keep"),
                (", b ", "added"),
                ("} from \"./m\";", "keep"),
            ],
        ),
        (
            "fn(a, b)",
            "fn(a, b, c)",
            &[("fn(a, b", "keep"), (", c", "added"), (")", "keep")],
        ),
        (
            "alpha beta gamma",
            "alpha beta gamma delta",
            &[("alpha beta gamma ", "keep"), ("delta", "added")],
        ),
        (
            "\tconst z = 1;",
            "\tconst z = 2;",
            &[
                ("\tconst z = ", "keep"),
                ("1", "removed"),
                ("2", "added"),
                (";", "keep"),
            ],
        ),
        (
            "// comment here",
            "// comment here!",
            &[("// comment here", "keep"), ("!", "added")],
        ),
    ];

    fn change(value: &str, kind: &str) -> WordChange {
        WordChange {
            value: value.to_string(),
            added: kind == "added",
            removed: kind == "removed",
        }
    }

    /// Byte-exact parity with the vendored jsdiff `diffWords`.
    #[test]
    fn diff_words_matches_jsdiff_reference_vectors() {
        for (old, new, parts) in VECTORS {
            let expected: Vec<WordChange> = parts.iter().map(|(v, k)| change(v, k)).collect();
            assert_eq!(
                diff_words(old, new),
                expected,
                "diffWords({old:?}, {new:?}) must match jsdiff 8.0.4"
            );
        }
    }

    #[test]
    fn js_trim_matches_ecma_whitespace() {
        assert_eq!(js_trim("  foo  "), "foo");
        assert_eq!(js_trim("\t\nfoo\r\n"), "foo");
        assert_eq!(js_trim("\u{00A0}foo\u{3000}"), "foo");
        assert_eq!(js_trim("   "), "");
        assert_eq!(js_trim(""), "");
        // Multibyte last non-whitespace char: the end bound must advance by
        // the char's UTF-8 length, not 1, or the slice panics mid-char.
        assert_eq!(js_trim(" 云 "), "云");
        assert_eq!(js_trim("你好 "), "你好");
        assert_eq!(js_trim("xé "), "xé");
    }

    #[test]
    fn leading_trailing_ws() {
        assert_eq!(leading_ws("  abc"), "  ");
        assert_eq!(leading_ws("abc"), "");
        assert_eq!(trailing_ws("abc  "), "  ");
        assert_eq!(trailing_ws("  abc"), "");
        assert_eq!(
            leading_and_trailing_ws("  abc  "),
            ("  ".to_string(), "  ".to_string())
        );
    }

    #[test]
    fn parse_diff_lines() {
        // "+123 content" -> prefix '+', lineNum "123", content "content"
        assert_eq!(
            parse_diff_line("+123 content"),
            Some(ParsedDiffLine {
                prefix: '+',
                line_num: "123".into(),
                content: "content".into()
            })
        );
        // " 123 content" (context line)
        assert_eq!(
            parse_diff_line(" 123 content"),
            Some(ParsedDiffLine {
                prefix: ' ',
                line_num: "123".into(),
                content: "content".into()
            })
        );
        // "     ..." -> prefix ' ', lineNum "   ", content "..."
        assert_eq!(
            parse_diff_line("     ..."),
            Some(ParsedDiffLine {
                prefix: ' ',
                line_num: "   ".into(),
                content: "...".into()
            })
        );
        // "  -42 x": the digit run is empty ('-' is not a digit), the
        // separator falls back to the last whitespace of the leading run.
        assert_eq!(
            parse_diff_line("  -42 x"),
            Some(ParsedDiffLine {
                prefix: ' ',
                line_num: String::new(),
                content: "-42 x".into()
            })
        );
        // "  42 x" -> lineNum " 42" (whitespace + digits in group 2).
        assert_eq!(
            parse_diff_line("  42 x"),
            Some(ParsedDiffLine {
                prefix: ' ',
                line_num: " 42".into(),
                content: "x".into()
            })
        );
        // No leading prefix char -> parse fails -> context line styling.
        assert_eq!(parse_diff_line("plain text"), None);
        assert_eq!(parse_diff_line(""), None);
    }

    #[test]
    fn tokenize_words_whitespace_punctuation() {
        // "foo bar" -> ["foo ", " bar"]: whitespace stitches to the
        // preceding word, then the following word prepends the leftover
        // whitespace part.
        assert_eq!(tokenize("foo bar"), vec!["foo ", " bar"]);
        // "foo  bar" -> ["foo  ", "  bar"] (two-space run).
        assert_eq!(tokenize("foo  bar"), vec!["foo  ", "  bar"]);
        // Whitespace-only text keeps its own token (word.js:62-65).
        assert_eq!(tokenize("  "), vec!["  "]);
        // Punctuation: "a,b" -> ["a", ",", "b"].
        assert_eq!(tokenize("a,b"), vec!["a", ",", "b"]);
        // "a. b" -> ["a", ". ", " b"].
        assert_eq!(tokenize("a. b"), vec!["a", ". ", " b"]);
        // Trailing whitespace attaches to the previous token: "a. ".
        assert_eq!(tokenize("a. "), vec!["a", ". "]);
        // Word chars incl. underscores and digits.
        assert_eq!(tokenize("foo_bar2"), vec!["foo_bar2"]);
    }

    #[test]
    fn render_diff_basic_lines() {
        let theme = crate::core::themes::load_theme("dark", None).expect("builtin theme");
        let input = " 12 const a = 1;\n-13 const a = 2;\n+13 const a = 3;\n 14 const b = 4;";
        let out = render_diff(input, &theme, RenderDiffOptions::default());
        // Line content is preserved; the intra-line diff inverse-highlights
        // only the changed word ("2" vs "3").
        let stripped = out.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");
        assert_eq!(
            stripped,
            " 12 const a = 1;\n-13 const a = 2;\n+13 const a = 3;\n 14 const b = 4;"
        );
        // Removed line: red fg, and the changed word wrapped in inverse.
        let removed = out.lines().nth(1).unwrap();
        assert!(removed.starts_with("\u{1b}[38;"));
        assert!(removed.contains("\u{1b}[7m2\u{1b}[27m"));
        let added = out.lines().nth(2).unwrap();
        assert!(added.contains("\u{1b}[7m3\u{1b}[27m"));
        // Context lines are dim/gray (toolDiffContext) without inverse.
        let context = out.lines().next().unwrap();
        assert!(context.starts_with("\u{1b}[38;"));
        assert!(!context.contains("\u{1b}[7m"));
    }

    #[test]
    fn render_diff_multi_line_hunk_without_intra_diff() {
        let theme = crate::core::themes::load_theme("dark", None).expect("builtin theme");
        // Two removed + two added lines: no intra-line diffing
        // (diff.ts:113-125 requires exactly 1 removed + 1 added). The
        // trailing empty line of the input renders as an empty context line
        // (upstream processes it the same way).
        let input = "-1 a\n-2 b\n+1 c\n+2 d\n";
        let out = render_diff(input, &theme, RenderDiffOptions::default());
        let stripped = out.lines().map(strip_ansi).collect::<Vec<_>>().join("\n");
        assert_eq!(stripped, "-1 a\n-2 b\n+1 c\n+2 d\n");
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
