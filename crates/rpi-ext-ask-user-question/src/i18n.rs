//! Embedded locale tables + locale selection.
//!
//! R-Q5.12 / R-Q7.3: nine locales (en/zh/de/es/fr/pt/pt-BR/ru/uk) are embedded
//! with `include_str!` (no runtime file dependency). The tables are vendored
//! byte-for-byte from upstream `packages/rpiv-ask-user-question/locales/` @
//! `338b264c` (provenance: `locales/README.md`); the parity harness re-checks
//! the vendored bytes against the pinned submodule.
//!
//! Locale selection is the rpi adaptation of the upstream `rpiv-i18n` SDK
//! default (the SDK itself is a [DEFER] non-goal, requirements §10): the SDK
//! reads a `--locale` flag / `~/.config/rpiv-i18n/locale.json` / `LANG`/`LC_ALL`
//! and only does exact-code lookup. rpi has none of those surfaces, so
//! `i18n.rs` selects from the process environment in the order
//! `LC_ALL` > `LC_MESSAGES` > `LANG` (POSIX precedence), maps `_`/`.ENCODING`
//! to a BCP-47 tag, and resolves it against [`SUPPORTED_LOCALES`] by exact tag
//! first, then primary-subtag prefix (`pt-BR` therefore wins over `pt` for
//! `pt_BR.UTF-8`; `pt_PT.UTF-8` falls to `pt`; `zh_CN.UTF-8` falls to `zh`).
//! `C`/`POSIX`/unset/unmatched all fall back to `en`. A selected locale is
//! merged over the English base, so partial translations fall back per key
//! (upstream `pickStringsForLocale`).
//!
//! `t(key, fallback)` mirrors the upstream bridge: a missing/empty key returns
//! the caller's inline English literal.

use std::collections::BTreeMap;

use crate::state::row_intent::{meta, RowKind};

/// Locales embedded in the binary (alphabetical, upstream `SUPPORTED_LOCALES`).
pub const SUPPORTED_LOCALES: [&str; 9] = ["de", "en", "es", "fr", "pt", "pt-BR", "ru", "uk", "zh"];

/// Fallback locale (`DEFAULT_FALLBACK_LOCALE`).
pub const DEFAULT_LOCALE: &str = "en";

/// Raw embedded tables, keyed by locale code.
const EMBEDDED: [(&str, &str); 9] = [
    ("de", include_str!("../locales/de.json")),
    ("en", include_str!("../locales/en.json")),
    ("es", include_str!("../locales/es.json")),
    ("fr", include_str!("../locales/fr.json")),
    ("pt", include_str!("../locales/pt.json")),
    ("pt-BR", include_str!("../locales/pt-BR.json")),
    ("ru", include_str!("../locales/ru.json")),
    ("uk", include_str!("../locales/uk.json")),
    ("zh", include_str!("../locales/zh.json")),
];

fn parse_table(raw: &str) -> BTreeMap<String, String> {
    serde_json::from_str(raw).unwrap_or_else(|error| {
        // Embedded literals are compile-time constants; a parse failure is a
        // build-time contract violation, surfaced once at first use.
        tracing::error!(error = %error, "rpiv-ask-user-question: embedded locale table is invalid JSON");
        BTreeMap::new()
    })
}

/// One locale's table (English base merged with the locale overlay).
#[derive(Clone, Debug)]
pub struct I18n {
    locale: String,
    strings: BTreeMap<String, String>,
}

impl I18n {
    /// Build the table for `locale` (unmatched codes resolve to `en`).
    pub fn for_locale(locale: &str) -> Self {
        let resolved = match_locale(locale).unwrap_or(DEFAULT_LOCALE);
        let mut strings = parse_table(
            EMBEDDED
                .iter()
                .find(|(code, _)| *code == DEFAULT_LOCALE)
                .map(|(_, raw)| *raw)
                .unwrap_or("{}"),
        );
        if resolved != DEFAULT_LOCALE {
            if let Some((_, raw)) = EMBEDDED.iter().find(|(code, _)| *code == resolved) {
                strings.extend(parse_table(raw));
            }
        }
        Self {
            locale: resolved.to_owned(),
            strings,
        }
    }

    /// Select the table from the process environment (`LC_ALL` > `LC_MESSAGES`
    /// > `LANG`).
    pub fn detect() -> Self {
        Self::for_locale(&detect_locale_from_env(|key| std::env::var(key).ok()))
    }

    /// The resolved locale code.
    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// Render-time lookup; missing/empty values return `fallback`
    /// (upstream `tr`).
    pub fn t<'a>(&'a self, key: &str, fallback: &'a str) -> &'a str {
        self.strings
            .get(key)
            .filter(|value| !value.is_empty())
            .map(String::as_str)
            .unwrap_or(fallback)
    }

    /// Locale-aware sentinel label with the canonical English label as
    /// fallback (upstream `displayLabel`).
    pub fn display_label(&self, kind: RowKind) -> String {
        let fallback = meta(kind).label;
        self.t(&format!("sentinel.{}", kind.as_str()), fallback)
            .to_owned()
    }

    /// All embedded tables (test/parity seam).
    pub fn all_tables() -> Vec<(&'static str, BTreeMap<String, String>)> {
        EMBEDDED
            .iter()
            .map(|(code, raw)| (*code, parse_table(raw)))
            .collect()
    }
}

/// Normalize a POSIX locale value (`es_ES.UTF-8`) to a BCP-47-ish tag
/// (`es-ES`). `C`, `POSIX` and empty values yield `None`.
pub fn parse_locale_env(value: &str) -> Option<String> {
    let without_encoding = value.split('.').next().unwrap_or("");
    let tag = without_encoding.trim().replace('_', "-");
    if tag.is_empty() || tag.eq_ignore_ascii_case("C") || tag.eq_ignore_ascii_case("POSIX") {
        return None;
    }
    Some(tag)
}

/// Resolve a BCP-47 tag against [`SUPPORTED_LOCALES`]: exact (case-insensitive)
/// match first, then primary-subtag match against the base locale.
pub fn match_locale(tag: &str) -> Option<&'static str> {
    let lowered = tag.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        return None;
    }
    if let Some(exact) = SUPPORTED_LOCALES
        .iter()
        .find(|code| code.eq_ignore_ascii_case(&lowered))
    {
        return Some(*exact);
    }
    let primary = lowered.split('-').next().unwrap_or("");
    SUPPORTED_LOCALES
        .iter()
        .find(|code| code.eq_ignore_ascii_case(primary))
        .copied()
}

/// Select a locale code from `LC_ALL` > `LC_MESSAGES` > `LANG` (the lookup
/// function is injected for testability). The first variable that parses AND
/// matches a supported locale wins; otherwise `en`.
pub fn detect_locale_from_env(mut get: impl FnMut(&str) -> Option<String>) -> String {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        let Some(value) = get(key) else { continue };
        let Some(tag) = parse_locale_env(&value) else {
            continue;
        };
        if let Some(locale) = match_locale(&tag) {
            return locale.to_owned();
        }
    }
    DEFAULT_LOCALE.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl FnMut(&str) -> Option<String> + use<> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn i18n_locale_matrix_matches_task_spec() {
        // POSIX precedence LC_ALL > LC_MESSAGES > LANG.
        assert_eq!(
            detect_locale_from_env(env(&[("LC_ALL", "fr_FR.UTF-8"), ("LANG", "de_DE.UTF-8"),])),
            "fr"
        );
        assert_eq!(
            detect_locale_from_env(env(&[
                ("LC_MESSAGES", "ru_RU.UTF-8"),
                ("LANG", "de_DE.UTF-8"),
            ])),
            "ru"
        );
        assert_eq!(
            detect_locale_from_env(env(&[("LANG", "uk_UA.UTF-8")])),
            "uk"
        );
        // pt-BR exact tag wins over the pt prefix.
        assert_eq!(
            detect_locale_from_env(env(&[("LANG", "pt_BR.UTF-8")])),
            "pt-BR"
        );
        // pt-PT falls back to the pt prefix.
        assert_eq!(
            detect_locale_from_env(env(&[("LANG", "pt_PT.UTF-8")])),
            "pt"
        );
        // zh_CN -> zh prefix.
        assert_eq!(
            detect_locale_from_env(env(&[("LANG", "zh_CN.UTF-8")])),
            "zh"
        );
        // C/POSIX are skipped; the next variable is consulted.
        assert_eq!(
            detect_locale_from_env(env(&[("LC_ALL", "C"), ("LANG", "es_ES.UTF-8")])),
            "es"
        );
        // Unsupported tags fall back to en.
        assert_eq!(
            detect_locale_from_env(env(&[("LANG", "ja_JP.UTF-8")])),
            "en"
        );
        // No env at all -> en.
        assert_eq!(detect_locale_from_env(env(&[])), "en");
    }

    #[test]
    fn i18n_parse_and_match_helpers() {
        assert_eq!(parse_locale_env("es_ES.UTF-8"), Some("es-ES".to_owned()));
        assert_eq!(parse_locale_env("pt-BR"), Some("pt-BR".to_owned()));
        assert_eq!(parse_locale_env("C"), None);
        assert_eq!(parse_locale_env("POSIX"), None);
        assert_eq!(parse_locale_env("  "), None);
        assert_eq!(match_locale("pt-br"), Some("pt-BR"));
        assert_eq!(match_locale("PT"), Some("pt"));
        assert_eq!(match_locale("en-US"), Some("en"));
        assert_eq!(match_locale("xx"), None);
    }

    #[test]
    fn i18n_tables_cover_english_and_merge_overlays() {
        let en = I18n::for_locale("en");
        assert_eq!(en.locale(), "en");
        assert_eq!(en.t("sentinel.other", "fb"), "Type something.");
        assert_eq!(en.t("sentinel.next", "fb"), "Next");
        assert_eq!(en.t("missing.key", "inline fallback"), "inline fallback");

        let zh = I18n::for_locale("zh");
        assert_eq!(zh.locale(), "zh");
        // Overlay key translated; a key absent from zh falls back to English.
        assert_eq!(zh.t("sentinel.next", "fb"), "下一个");
        assert_eq!(zh.t("hint.clear", "fb"), "Ctrl+U 清空");
        // `review.global_hint` is present in en but absent from zh -> English base.
        assert_eq!(
            zh.t("review.global_hint", "fb"),
            en.t("review.global_hint", "fb")
        );

        // Unknown locale resolves to en.
        assert_eq!(I18n::for_locale("ja").locale(), "en");
    }

    #[test]
    fn i18n_display_label_uses_locale_and_english_fallback() {
        let zh = I18n::for_locale("zh");
        assert_eq!(zh.display_label(RowKind::Other), "输入内容");
        assert_eq!(zh.display_label(RowKind::Next), "下一个");
        let en = I18n::for_locale("en");
        assert_eq!(en.display_label(RowKind::Option), "");
        assert_eq!(en.display_label(RowKind::Other), "Type something.");
    }

    #[test]
    fn i18n_all_locales_are_subsets_of_english_and_keep_templates() {
        let tables = I18n::all_tables();
        assert_eq!(tables.len(), 9);
        let english = tables
            .iter()
            .find(|(code, _)| *code == "en")
            .map(|(_, table)| table.clone())
            .expect("en table");
        for key in ["hint.collapse", "hint.expand_line"] {
            assert!(
                english
                    .get(key)
                    .is_some_and(|value| value.contains("{key}")),
                "en.{key} must carry the {{key}} placeholder"
            );
        }
        for (code, table) in tables {
            for (key, value) in &table {
                // `_meta.*` entries are translator notes, not UI strings.
                if key.starts_with('_') {
                    continue;
                }
                assert!(
                    english.contains_key(key),
                    "{code}.{key} has no English base key"
                );
                if key == "hint.collapse" || key == "hint.expand_line" {
                    assert!(value.contains("{key}"), "{code}.{key} lost the template");
                }
            }
        }
    }
}
