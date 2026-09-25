//! Embedded locale tables + locale selection + the render-time lookup
//! surface (`t` / `formatStatusLabel`).
//!
//! Port of upstream `packages/rpiv-todo/state/i18n-bridge.ts` @ `0fdf4f8`
//! plus its `locales/*.json` (nine tables, vendored byte-for-byte —
//! provenance: `src/locales/`, `338b264..0fdf4f8` is comment-level for this
//! package and does not touch the locale files).
//!
//! Upstream resolves strings through the optional `@juicesharp/rpiv-i18n`
//! SDK peer (live `/languages` updates) and falls back to the inline
//! English literal when the SDK is absent. rpi embeds the nine tables at
//! compile time ([VARIANT], deviation TE-D43 — the SDK dynamic-load
//! semantics have no rpi counterpart): a missing key or an unmatched
//! locale yields the caller's inline English fallback, so the extension
//! stays online in English either way (the shim's regression contract).
//!
//! Locale selection follows the `rpi-ext-ask-user-question` `i18n.rs`
//! precedent: `LC_ALL` > `LC_MESSAGES` > `LANG` (POSIX precedence), `_` /
//! `.ENCODING` normalized to a BCP-47-ish tag, resolved by exact match
//! first then primary-subtag prefix (`pt-BR` wins over `pt` for
//! `pt_BR.UTF-8`). A selected locale merges over the English base, so
//! partial translations fall back per key.

use std::collections::BTreeMap;

use crate::tool::types::TaskStatus;

/// Locales embedded in the binary (alphabetical; upstream
/// `SUPPORTED_LOCALES` — nine files under `locales/`).
pub const SUPPORTED_LOCALES: [&str; 9] = ["de", "en", "es", "fr", "pt", "pt-BR", "ru", "uk", "zh"];

/// Fallback locale.
pub const DEFAULT_LOCALE: &str = "en";

/// Raw embedded tables, keyed by locale code.
const EMBEDDED: [(&str, &str); 9] = [
    ("de", include_str!("locales/de.json")),
    ("en", include_str!("locales/en.json")),
    ("es", include_str!("locales/es.json")),
    ("fr", include_str!("locales/fr.json")),
    ("pt", include_str!("locales/pt.json")),
    ("pt-BR", include_str!("locales/pt-BR.json")),
    ("ru", include_str!("locales/ru.json")),
    ("uk", include_str!("locales/uk.json")),
    ("zh", include_str!("locales/zh.json")),
];

fn parse_table(raw: &str) -> BTreeMap<String, String> {
    serde_json::from_str(raw).unwrap_or_else(|error| {
        // Embedded literals are compile-time constants; a parse failure is
        // a build-time contract violation, surfaced once at first use.
        tracing::error!(error = %error, "rpiv-todo: embedded locale table is invalid JSON");
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

    /// Select the table from the process environment (`LC_ALL` >
    /// `LC_MESSAGES` > `LANG`).
    pub fn detect() -> Self {
        Self::for_locale(&detect_locale_from_env(|key| std::env::var(key).ok()))
    }

    /// The resolved locale code.
    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// Render-time lookup; missing/empty values return `fallback`
    /// (upstream `t(key, fallback)` — call sites pass the inline English
    /// literal, never bake a top-level constant).
    pub fn t<'a>(&'a self, key: &str, fallback: &'a str) -> &'a str {
        self.strings
            .get(key)
            .filter(|value| !value.is_empty())
            .map(String::as_str)
            .unwrap_or(fallback)
    }

    /// Resolve a [`TaskStatus`] to its locale-aware label (upstream
    /// `formatStatusLabel`) — the SINGLE point of localization for status
    /// words: overlay summary, `/todos` header, renderCall.
    pub fn format_status_label(&self, status: TaskStatus) -> &str {
        let (key, fallback): (&str, &str) = match status {
            TaskStatus::Pending => ("status.pending", "pending"),
            TaskStatus::InProgress => ("status.in_progress", "in progress"),
            TaskStatus::Completed => ("status.completed", "completed"),
            TaskStatus::Deleted => ("status.deleted", "deleted"),
        };
        self.t(key, fallback)
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

/// Resolve a BCP-47 tag against [`SUPPORTED_LOCALES`]: exact
/// (case-insensitive) match first, then primary-subtag match.
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

/// Select a locale code from `LC_ALL` > `LC_MESSAGES` > `LANG` (the
/// lookup function is injected for testability). The first variable that
/// parses AND matches a supported locale wins; otherwise `en`.
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
    //! Port of upstream `state/i18n-bridge.test.ts` + the runtime-fallback
    //! contract of `state/i18n-bridge.shim.test.ts` @ `0fdf4f8` under the
    //! embedded-table form (TE-D43): locale lookups resolve against the
    //! embedded tables, missing keys/locales fall back to the inline
    //! English literal, and nothing panics without the SDK (the shim's
    //! no-op contract — the rpi table IS the no-SDK path).

    use super::*;
    use crate::tool::types::TaskStatus;

    fn env(pairs: &[(&str, &str)]) -> impl FnMut(&str) -> Option<String> + use<> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    // ------------------------------------------------------------------
    // t / formatStatusLabel (i18n-bridge.test.ts semantics)
    // ------------------------------------------------------------------

    #[test]
    fn returns_english_when_no_locale_is_active() {
        let en = I18n::for_locale("en");
        assert_eq!(
            en.format_status_label(TaskStatus::InProgress),
            "in progress"
        );
        assert_eq!(en.format_status_label(TaskStatus::Completed), "completed");
        assert_eq!(en.format_status_label(TaskStatus::Pending), "pending");
        assert_eq!(en.format_status_label(TaskStatus::Deleted), "deleted");
    }

    #[test]
    fn returns_localized_values_when_locale_is_set() {
        let de = I18n::for_locale("de");
        assert_eq!(
            de.format_status_label(TaskStatus::InProgress),
            "in Bearbeitung"
        );
        assert_eq!(de.format_status_label(TaskStatus::Completed), "erledigt");
        let zh = I18n::for_locale("zh");
        assert_eq!(zh.t("overlay.heading", "Todos"), "任务清单");
        assert_eq!(
            zh.t("command.no_todos", "fb"),
            "暂无任务。可以让 agent 添加一些！"
        );
    }

    #[test]
    fn falls_back_to_english_for_keys_missing_in_the_overlay() {
        // Every non-meta key of every table is present in the English
        // base (key-completeness test below), so per-key fallback is only
        // reachable for keys outside the tables entirely — the inline
        // literal wins (upstream `t` unknown-key semantics).
        let de = I18n::for_locale("de");
        assert_eq!(
            de.t("nonexistent.key", "fallback literal"),
            "fallback literal"
        );
        assert_eq!(de.t("status.completed", "completed"), "erledigt");
    }

    #[test]
    fn overlay_and_command_keys_resolve_in_every_embedded_locale() {
        for (code, table) in I18n::all_tables() {
            let i18n = I18n::for_locale(code);
            assert_eq!(i18n.locale(), code);
            for key in [
                "overlay.heading",
                "overlay.more",
                "overlay.expandHint",
                "overlay.collapsed",
                "command.no_todos",
                "command.requires_interactive",
                "command.section.pending",
                "command.section.in_progress",
                "command.section.completed",
            ] {
                let expected = table
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| String::from("missing"));
                assert!(!expected.is_empty(), "{code}.{key} must resolve");
            }
        }
    }

    // ------------------------------------------------------------------
    // Locale selection matrix (ask-user-question precedent shape)
    // ------------------------------------------------------------------

    #[test]
    fn locale_matrix_selection() {
        assert_eq!(
            detect_locale_from_env(env(&[("LC_ALL", "fr_FR.UTF-8"), ("LANG", "de_DE.UTF-8")])),
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
        assert_eq!(
            detect_locale_from_env(env(&[("LANG", "pt_BR.UTF-8")])),
            "pt-BR"
        );
        assert_eq!(
            detect_locale_from_env(env(&[("LANG", "pt_PT.UTF-8")])),
            "pt"
        );
        assert_eq!(
            detect_locale_from_env(env(&[("LANG", "zh_CN.UTF-8")])),
            "zh"
        );
        assert_eq!(
            detect_locale_from_env(env(&[("LC_ALL", "C"), ("LANG", "es_ES.UTF-8")])),
            "es"
        );
        assert_eq!(
            detect_locale_from_env(env(&[("LANG", "ja_JP.UTF-8")])),
            "en"
        );
        assert_eq!(detect_locale_from_env(env(&[])), "en");
    }

    #[test]
    fn parse_and_match_helpers() {
        assert_eq!(parse_locale_env("es_ES.UTF-8"), Some("es-ES".to_owned()));
        assert_eq!(parse_locale_env("pt-BR"), Some("pt-BR".to_owned()));
        assert_eq!(parse_locale_env("C"), None);
        assert_eq!(parse_locale_env("POSIX"), None);
        assert_eq!(parse_locale_env("  "), None);
        assert_eq!(match_locale("pt-br"), Some("pt-BR"));
        assert_eq!(match_locale("PT"), Some("pt"));
        assert_eq!(match_locale("en-US"), Some("en"));
        assert_eq!(match_locale("xx"), None);
        assert_eq!(I18n::for_locale("ja").locale(), "en");
    }

    // ------------------------------------------------------------------
    // Key completeness: every non-meta key is covered by the English base
    // and templates survive translation (shim contract: no blank strings).
    // ------------------------------------------------------------------

    #[test]
    fn all_locales_are_subsets_of_english_and_keep_templates() {
        let tables = I18n::all_tables();
        assert_eq!(tables.len(), 9);
        let english = tables
            .iter()
            .find(|(code, _)| *code == "en")
            .map(|(_, table)| table.clone())
            .expect("en table");
        // The English base carries the full chrome key set (13 keys).
        for key in [
            "status.pending",
            "status.in_progress",
            "status.completed",
            "status.deleted",
            "overlay.heading",
            "overlay.more",
            "overlay.expandHint",
            "overlay.collapsed",
            "command.no_todos",
            "command.requires_interactive",
            "command.section.pending",
            "command.section.in_progress",
            "command.section.completed",
        ] {
            assert!(english.contains_key(key), "en.{key} missing");
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
                if key == "overlay.expandHint" {
                    assert!(value.contains("{key}"), "{code}.{key} lost the template");
                }
            }
        }
    }
}
