//! Tests for `crates/pir/src/tools/edit_diff.rs` — pure algorithm tests.
//!
//! Port of the algorithm-level test cases from `tools.test.ts` (edit fuzzy,
//! CRLF, multi-edit) and `edit-diff.ts` edge cases.

use pir::tools::edit_diff::*;

// ---------------------------------------------------------------------------
// Line-ending helpers
// ---------------------------------------------------------------------------

#[test]
fn test_detect_line_ending_crlf() {
    assert_eq!(detect_line_ending("a\r\nb\r\n"), "\r\n");
}

#[test]
fn test_detect_line_ending_lf() {
    assert_eq!(detect_line_ending("a\nb\n"), "\n");
}

#[test]
fn test_detect_line_ending_mixed_crlf_first() {
    // CRLF appears before the first lone LF → CRLF
    assert_eq!(detect_line_ending("a\r\nb\nc"), "\r\n");
}

#[test]
fn test_detect_line_ending_mixed_lf_first() {
    // LF appears before CRLF → LF
    assert_eq!(detect_line_ending("a\nb\r\nc"), "\n");
}

#[test]
fn test_detect_line_ending_no_newline() {
    assert_eq!(detect_line_ending("no newline"), "\n");
}

#[test]
fn test_detect_line_ending_only_cr() {
    // Bare \r (not part of \r\n) → normalize will handle, detect returns \n
    assert_eq!(detect_line_ending("a\rb\r"), "\n");
}

#[test]
fn test_normalize_to_lf() {
    assert_eq!(normalize_to_lf("a\r\nb\rc\nd"), "a\nb\nc\nd");
    assert_eq!(normalize_to_lf("a\r\nb"), "a\nb");
    assert_eq!(normalize_to_lf(""), "");
}

#[test]
fn test_restore_line_endings_crlf() {
    assert_eq!(restore_line_endings("a\nb\n", "\r\n"), "a\r\nb\r\n");
}

#[test]
fn test_restore_line_endings_lf() {
    assert_eq!(restore_line_endings("a\nb\n", "\n"), "a\nb\n");
}

// ---------------------------------------------------------------------------
// BOM
// ---------------------------------------------------------------------------

#[test]
fn test_strip_bom_present() {
    let (bom, text) = strip_bom("\u{FEFF}hello");
    assert_eq!(bom, "\u{FEFF}");
    assert_eq!(text, "hello");
}

#[test]
fn test_strip_bom_absent() {
    let (bom, text) = strip_bom("hello");
    assert_eq!(bom, "");
    assert_eq!(text, "hello");
}

// ---------------------------------------------------------------------------
// Fuzzy normalisation
// ---------------------------------------------------------------------------

#[test]
fn test_normalize_fuzzy_strip_trailing_whitespace() {
    assert_eq!(
        normalize_for_fuzzy_match("line one   \nline two  "),
        "line one\nline two"
    );
}

#[test]
fn test_normalize_fuzzy_smart_single_quotes() {
    assert_eq!(
        normalize_for_fuzzy_match("\u{2018}hi\u{2019}\u{201A}\u{201B}"),
        "'hi'''"
    );
}

#[test]
fn test_normalize_fuzzy_smart_double_quotes() {
    assert_eq!(
        normalize_for_fuzzy_match("\u{201C}hi\u{201D}\u{201E}\u{201F}"),
        "\"hi\"\"\""
    );
}

#[test]
fn test_normalize_fuzzy_dashes() {
    for dash in [
        '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}',
    ] {
        let input: String = [dash].iter().collect();
        assert_eq!(normalize_for_fuzzy_match(&input), "-");
    }
}

#[test]
fn test_normalize_fuzzy_nbsp() {
    assert_eq!(
        normalize_for_fuzzy_match("hello\u{00A0}world"),
        "hello world"
    );
}

#[test]
fn test_normalize_fuzzy_special_spaces() {
    for sp in [
        '\u{2002}', '\u{2005}', '\u{200A}', '\u{202F}', '\u{205F}', '\u{3000}',
    ] {
        let input: String = ["a", &sp.to_string(), "b"].concat();
        assert_eq!(normalize_for_fuzzy_match(&input), "a b");
    }
}

#[test]
fn test_normalize_fuzzy_nfkc_fullwidth() {
    // Fullwidth → halfwidth via NFKC
    assert_eq!(normalize_for_fuzzy_match("ＡＢＣ１２３"), "ABC123");
}

#[test]
fn test_normalize_fuzzy_nfkc_fullwidth_punctuation() {
    assert_eq!(normalize_for_fuzzy_match("你好，世界"), "你好,世界");
}

// ---------------------------------------------------------------------------
// apply_edits_to_normalized_content — exact match cases
// ---------------------------------------------------------------------------

fn make_edit(old: &str, new: &str, idx: usize) -> EditReplacement {
    EditReplacement {
        old_text: old.to_string(),
        new_text: new.to_string(),
        edit_index: idx,
    }
}

#[test]
fn test_apply_single_exact_replacement() {
    let content = "Hello, world!";
    let edits = vec![make_edit("world", "testing", 0)];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert_eq!(result.new_content, "Hello, testing!");
    assert_eq!(result.base_content, "Hello, world!");
}

#[test]
fn test_apply_not_found_error_single() {
    let content = "Hello, world!";
    let edits = vec![make_edit("nonexistent", "x", 0)];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert!(err.contains("Could not find the exact text in file.txt"));
    assert!(err.contains("The old text must match exactly including all whitespace and newlines."));
}

#[test]
fn test_apply_not_found_error_multi() {
    // First edit matches, second doesn't.
    let content = "Hello world";
    let edits = vec![
        make_edit("Hello", "Hi", 0),
        make_edit("nonexistent", "x", 1),
    ];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert!(err.contains("Could not find edits[1] in file.txt"));
    assert!(err.contains("The oldText must match exactly"));
}

#[test]
fn test_apply_duplicate_error_single() {
    let content = "foo foo foo";
    let edits = vec![make_edit("foo", "bar", 0)];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert!(err.contains("Found 3 occurrences of the text in file.txt"));
}

#[test]
fn test_apply_duplicate_error_multi() {
    let content = "foo foo";
    let edits = vec![make_edit("foo", "bar", 0), make_edit("foo", "baz", 1)];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert!(err.contains("occurrences"));
}

#[test]
fn test_apply_empty_old_text_single() {
    let content = "hello";
    let edits = vec![make_edit("", "x", 0)];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert_eq!(err, "oldText must not be empty in file.txt.");
}

#[test]
fn test_apply_empty_old_text_multi() {
    let content = "hello";
    let edits = vec![make_edit("hello", "hi", 0), make_edit("", "x", 1)];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert_eq!(err, "edits[1].oldText must not be empty in file.txt.");
}

#[test]
fn test_apply_no_change_error_single() {
    // If newText equals oldText after normalisation → no change.
    let content = "hello world";
    let edits = vec![make_edit("hello", "hello", 0)];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert!(err.contains("No changes made to file.txt."));
    assert!(err.contains("The replacement produced identical content."));
}

#[test]
fn test_apply_no_change_error_multi() {
    // Both edits are no-ops (oldText == newText).
    let content = "hello world";
    let edits = vec![
        make_edit("hello", "hello", 0),
        make_edit("world", "world", 1),
    ];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert!(err.contains("No changes made to file.txt."));
    assert!(err.contains("The replacements produced identical content."));
}

#[test]
fn test_apply_overlap_error() {
    let content = "one\ntwo\nthree\n";
    let edits = vec![
        make_edit("one\ntwo\n", "ONE\nTWO\n", 0),
        make_edit("two\nthree\n", "TWO\nTHREE\n", 1),
    ];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert!(err.contains("overlap in file.txt"));
    assert!(err.contains("edits[0]"));
    assert!(err.contains("edits[1]"));
}

#[test]
fn test_apply_multiple_disjoint_edits() {
    let content = "alpha\nbeta\ngamma\ndelta\n";
    let edits = vec![
        make_edit("alpha\n", "ALPHA\n", 0),
        make_edit("gamma\n", "GAMMA\n", 1),
    ];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert_eq!(result.new_content, "ALPHA\nbeta\nGAMMA\ndelta\n");
}

#[test]
fn test_apply_edits_match_original_not_incremental() {
    let content = "foo\nbar\nbaz\n";
    let edits = vec![
        make_edit("foo\n", "foo bar\n", 0),
        make_edit("bar\n", "BAR\n", 1),
    ];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert_eq!(result.new_content, "foo bar\nBAR\nbaz\n");
}

// ---------------------------------------------------------------------------
// Fuzzy match cases
// ---------------------------------------------------------------------------

#[test]
fn test_apply_fuzzy_trailing_whitespace_stripped() {
    let content = "line one   \nline two  \nline three\n";
    let edits = vec![make_edit("line one\nline two\n", "replaced\n", 0)];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert_eq!(result.new_content, "replaced\nline three\n");
}

#[test]
fn test_apply_fuzzy_fullwidth_punctuation() {
    let content = "你好，世界\n你好（世界）\n";
    let edits = vec![make_edit(
        "你好,世界\n你好(世界)\n",
        "你好，pi\n你好(pi)\n",
        0,
    )];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert_eq!(result.new_content, "你好，pi\n你好(pi)\n");
}

#[test]
fn test_apply_fuzzy_nfkc_compatibility() {
    let content = "ＡＢＣ１２３\ncafe\u{0301}\n";
    let edits = vec![make_edit("ABC123\ncafé\n", "XYZ789\ncoffee\n", 0)];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert_eq!(result.new_content, "XYZ789\ncoffee\n");
}

#[test]
fn test_apply_fuzzy_smart_single_quotes() {
    let content = "console.log(\u{2018}hello\u{2019});\n";
    let edits = vec![make_edit(
        "console.log('hello');",
        "console.log('world');",
        0,
    )];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert!(result.new_content.contains("world"));
}

#[test]
fn test_apply_fuzzy_smart_double_quotes() {
    let content = "const msg = \u{201C}Hello World\u{201D};\n";
    let edits = vec![make_edit(
        "const msg = \"Hello World\";",
        "const msg = \"Goodbye\";",
        0,
    )];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert!(result.new_content.contains("Goodbye"));
}

#[test]
fn test_apply_fuzzy_unicode_dashes() {
    let content = "range: 1\u{2013}5\nbreak\u{2014}here\n";
    let edits = vec![make_edit(
        "range: 1-5\nbreak-here",
        "range: 10-50\nbreak--here",
        0,
    )];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert!(result.new_content.contains("10-50"));
}

#[test]
fn test_apply_fuzzy_nbsp() {
    let content = "hello\u{00A0}world\n";
    let edits = vec![make_edit("hello world", "hello universe", 0)];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert!(result.new_content.contains("universe"));
}

#[test]
fn test_apply_exact_preferred_over_fuzzy() {
    let content = "const x = 'exact';\nconst y = 'other';\n";
    let edits = vec![make_edit("const x = 'exact';", "const x = 'changed';", 0)];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert_eq!(
        result.new_content,
        "const x = 'changed';\nconst y = 'other';\n"
    );
}

#[test]
fn test_apply_fuzzy_still_not_found() {
    let content = "completely different content\n";
    let edits = vec![make_edit("this does not exist", "replacement", 0)];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert!(err.contains("Could not find the exact text"));
}

#[test]
fn test_apply_fuzzy_duplicate_detection() {
    // Two lines that are identical after trailing whitespace stripping.
    let content = "hello world   \nhello world\n";
    let edits = vec![make_edit("hello world", "replaced", 0)];
    let err = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap_err();
    assert!(err.contains("Found 2 occurrences"));
}

#[test]
fn test_apply_fuzzy_multi_edit_mode() {
    let content = "console.log(\u{2018}hello\u{2019});\nhello\u{00A0}world\n";
    let edits = vec![
        make_edit("console.log('hello');\n", "console.log('world');\n", 0),
        make_edit("hello world\n", "hello universe\n", 1),
    ];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    assert_eq!(
        result.new_content,
        "console.log('world');\nhello universe\n"
    );
}

#[test]
fn test_apply_fuzzy_preserve_correct_occurrence() {
    // originalContent = "replace me   \nafter   \n"
    let content = "replace me   \nafter   \n";
    let edits = vec![make_edit("replace me\n", "after\n", 0)];
    let result = apply_edits_to_normalized_content(content, &edits, "file.txt").unwrap();
    // Expected: "after\nafter   \n"
    assert_eq!(result.new_content, "after\nafter   \n");
}

#[test]
fn test_apply_fuzzy_preserve_untouched_lines_multi() {
    let original = [
        "keep before  ",
        "first target  ",
        "first after",
        "keep middle   ",
        "second target  ",
        "second after",
        "keep after  ",
        "",
    ]
    .join("\n");
    let edits = vec![
        make_edit("first target\nfirst after", "FIRST\nFIRST2", 0),
        make_edit("second target\nsecond after", "SECOND\nSECOND2", 1),
    ];
    let result = apply_edits_to_normalized_content(&original, &edits, "file.txt").unwrap();
    let expected = [
        "keep before  ",
        "FIRST",
        "FIRST2",
        "keep middle   ",
        "SECOND",
        "SECOND2",
        "keep after  ",
        "",
    ]
    .join("\n");
    assert_eq!(result.new_content, expected);
}

// ---------------------------------------------------------------------------
// Diff generation
// ---------------------------------------------------------------------------

#[test]
fn test_generate_diff_simple() {
    let old = "Hello, world!";
    let new = "Hello, testing!";
    let result = generate_diff_string(old, new, 4);
    assert!(result.diff.contains("-1 Hello, world!"));
    assert!(result.diff.contains("+1 Hello, testing!"));
    assert_eq!(result.first_changed_line, Some(1));
}

#[test]
fn test_generate_diff_multi_edit_collapse() {
    let lines: Vec<String> = (0..600).map(|i| format!("line {:03}", i + 1)).collect();
    let content = format!("{}\n", lines.join("\n"));
    let new_content = content
        .replace("line 100\n", "LINE 100\n")
        .replace("line 300\n", "LINE 300\n")
        .replace("line 500\n", "LINE 500\n");

    let result = generate_diff_string(&content, &new_content, 4);
    assert!(result.diff.contains("LINE 100"));
    assert!(result.diff.contains("LINE 300"));
    assert!(result.diff.contains("LINE 500"));
    assert!(result.diff.contains("..."));
    assert!(!result.diff.contains("line 250"));
    // The diff should be much shorter than 50 lines.
    assert!(result.diff.split('\n').count() < 50);
}

#[test]
fn test_generate_diff_first_changed_line() {
    let old = "a\nb\nc\nd\ne\n";
    let new = "a\nB\nc\nD\ne\n";
    let result = generate_diff_string(old, new, 4);
    assert_eq!(result.first_changed_line, Some(2));
}

#[test]
fn test_generate_diff_no_changes() {
    let content = "a\nb\nc\n";
    let result = generate_diff_string(content, content, 4);
    assert!(result.diff.is_empty());
    assert_eq!(result.first_changed_line, None);
}

// ---------------------------------------------------------------------------
// Unified patch
// ---------------------------------------------------------------------------

#[test]
fn test_generate_unified_patch_basic() {
    let old = "Hello, world!";
    let new = "Hello, testing!";
    let patch = generate_unified_patch("file.txt", old, new, 4);
    assert!(patch.contains("--- file.txt"));
    assert!(patch.contains("+++ file.txt"));
    assert!(patch.contains("@@"));
    assert!(patch.contains("-Hello, world!"));
    assert!(patch.contains("+Hello, testing!"));
}

#[test]
fn test_generate_unified_patch_no_newline_at_end() {
    let old = "Hello, world!";
    let new = "Hello, world!\nmore";
    let patch = generate_unified_patch("file.txt", old, new, 4);
    assert!(patch.contains("\\ No newline at end of file"));
}

#[test]
fn test_generate_unified_patch_multiline() {
    let old = "alpha\nbeta\ngamma\ndelta\n";
    let new = "ALPHA\nbeta\nGAMMA\ndelta\n";
    let patch = generate_unified_patch("file.txt", old, new, 4);
    // Should have two hunks (ALPHA and GAMMA are far enough apart)
    assert!(patch.contains("@@"));
    assert!(patch.contains("-alpha"));
    assert!(patch.contains("+ALPHA"));
    assert!(patch.contains("-gamma"));
    assert!(patch.contains("+GAMMA"));
}

// ---------------------------------------------------------------------------
// compute_edits_diff
// ---------------------------------------------------------------------------

#[test]
fn test_compute_edits_diff_missing_file() {
    let cwd = std::env::temp_dir();
    let missing = cwd.join("definitely-not-here-12345.txt");
    let result = compute_edits_diff(
        &missing.to_string_lossy(),
        &[make_edit("hello", "world", 0)],
        &cwd,
    );
    assert!(result.error.is_some());
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .contains("Could not edit file:"));
    assert!(result.error.as_ref().unwrap().contains("ENOENT"));
}

#[test]
fn test_compute_edits_diff_success() {
    let tmp_dir = std::env::temp_dir().join(format!("pir-edit-diff-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp_dir);
    let file = tmp_dir.join("compute.txt");
    std::fs::write(&file, "Hello, world!").unwrap();
    let result = compute_edits_diff(
        &file.to_string_lossy(),
        &[make_edit("world", "testing", 0)],
        &tmp_dir,
    );
    assert!(result.error.is_none());
    assert!(result.diff.is_some());
    assert!(result.patch.is_some());
    assert!(result.first_changed_line.is_some());
    assert!(result.diff.as_ref().unwrap().contains("testing"));
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
