//! Usage formatting (TE44 FR-A; TE53 FR-E/FR-G).
//!
//! Two renderers share one typed model:
//!
//! - [`footer_text`] implements the frozen typed footer grammar of 03 §5.7.1
//!   (`windows[]` → `ledgers[]` → `balance[]`, joined with ` / `). It only
//!   reads typed data; an envelope without any typed section keeps the
//!   legacy `displayText` verbatim (third-party scripts are never parsed).
//!   The footer never emits newlines: the host wraps status rows to the
//!   terminal width.
//! - [`report_text`] renders the multi-line provider block used by `/usage`:
//!   a provider title, the fixed `Window / Used / Left / Reset` table,
//!   ledger/balance sections, and error/status/stale/cache markers. The old
//!   TE50 compact `displayText` first line is intentionally gone.
//!
//! Numbers: percentages keep at most one decimal (`.0` removed), money is
//! always two decimals, and counts use the full decimal value with
//! insignificant trailing zeros removed. `statusLine.numberFormat` is not
//! read and there is no `usage.numberFormat`.

use chrono::DateTime;
use rpi_ext_host::usage::{
    LocalTokenUsage, UsageBalanceEntry, UsageEnvelope, UsageLedger, UsageReport, UsageReportSource,
    UsageWindow, UsageWindowStatus,
};

use crate::config::{PercentageMode, UsageConfig};

/// The footer text for one envelope.
///
/// `Some` means a non-empty line; `None` means the caller must clear the
/// status entry (all typed windows unknown and no ledger/balance, or an empty
/// legacy `displayText`).
pub fn footer_text(envelope: &UsageEnvelope, config: &UsageConfig, now_ms: i64) -> Option<String> {
    if has_typed_sections(envelope) {
        let mut segments = Vec::new();
        for window in envelope.windows.as_deref().unwrap_or_default() {
            if let Some(segment) = window_segment(window, config, now_ms) {
                segments.push(segment);
            }
        }
        for ledger in envelope.ledgers.as_deref().unwrap_or_default() {
            if let Some(segment) = ledger_segment(ledger) {
                segments.push(segment);
            }
        }
        for balance in envelope.balance.as_deref().unwrap_or_default() {
            if let Some(segment) = balance_segment(balance) {
                segments.push(segment);
            }
        }
        return (!segments.is_empty()).then(|| segments.join(" / "));
    }
    let text = envelope.display_text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Whether the envelope carries at least one non-empty typed section.
pub fn has_typed_sections(envelope: &UsageEnvelope) -> bool {
    envelope
        .windows
        .as_deref()
        .is_some_and(|windows| !windows.is_empty())
        || envelope
            .ledgers
            .as_deref()
            .is_some_and(|ledgers| !ledgers.is_empty())
        || envelope
            .balance
            .as_deref()
            .is_some_and(|balance| !balance.is_empty())
}

/// The earliest future typed reset in epoch milliseconds, if any. The footer
/// worker uses it to schedule its 60-second local redraw (no request).
pub fn next_reset_ms(envelope: &UsageEnvelope, now_ms: i64) -> Option<i64> {
    envelope
        .windows
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|window| window.status != UsageWindowStatus::Unknown)
        .filter_map(|window| window.reset_at.as_deref())
        .filter_map(reset_epoch_ms)
        .filter(|reset| *reset > now_ms)
        .min()
}

/// The multi-line report for one provider (TE53 FR-E).
pub fn report_text(
    report: &UsageReport,
    current: bool,
    config: &UsageConfig,
    now_ms: i64,
) -> String {
    let mut lines = vec![format!(
        "{}{}",
        sanitize_text(&report.provider),
        if current { " (current)" } else { "" }
    )];
    if report.error.is_none() && report.source == Some(UsageReportSource::Cache) {
        lines.push("Source: cache (no request)".to_owned());
    }
    if let Some(error) = &report.error {
        lines.push(format!("Status: {}", error.kind.as_str()));
        lines.push(format!("Action: {}", error.action));
    }
    if let Some(envelope) = &report.envelope {
        if report.error.is_some() {
            lines.push(String::new());
            lines.push("Stale data (cached):".to_owned());
        }
        // A successful envelope with no typed and no legacy detail keeps
        // only its title: it is not a `no-data` failure. The legacy
        // `displayText` stays a footer-only compatibility surface.
        lines.extend(envelope_lines(envelope, config, now_ms));
    } else if report.error.is_none() {
        lines.push("Status: no-data".to_owned());
        lines.push(format!(
            "Action: {}",
            rpi_ext_host::usage::UsageErrorKind::NoData.action()
        ));
    }
    lines.join("\n")
}

/// The local token-only report (ADR-0039 decision 5): no plan, window,
/// ledger, balance, quota, reset or cost field.
pub fn local_text(tokens: &LocalTokenUsage) -> String {
    [
        "Local session tokens".to_owned(),
        format!("input: {}", tokens.input),
        format!("output: {}", tokens.output),
        format!("cacheRead: {}", tokens.cache_read),
        format!("cacheWrite: {}", tokens.cache_write),
        format!("used: {} (input + cacheWrite + output)", tokens.used),
    ]
    .join("\n")
}

/// One provider envelope's sections (used by the report renderer).
fn envelope_lines(envelope: &UsageEnvelope, config: &UsageConfig, now_ms: i64) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(plan) = envelope.plan.as_deref() {
        let plan = sanitize_text(plan);
        if !plan.is_empty() {
            lines.push(format!("plan: {plan}"));
        }
    }
    let typed_windows = envelope
        .windows
        .as_deref()
        .is_some_and(|windows| !windows.is_empty());
    let typed_ledgers = envelope
        .ledgers
        .as_deref()
        .is_some_and(|ledgers| !ledgers.is_empty());
    if typed_windows
        && let Some(table) = window_table(
            envelope.windows.as_deref().unwrap_or_default(),
            config,
            now_ms,
        )
    {
        lines.extend(table);
    }
    if typed_ledgers {
        for ledger in envelope.ledgers.as_deref().unwrap_or_default() {
            lines.extend(ledger_section(ledger));
        }
    }
    if !typed_windows && !typed_ledgers {
        lines.extend(legacy_sections(envelope));
    }
    if let Some(balance) = envelope.balance.as_deref()
        && !balance.is_empty()
    {
        lines.push("Balance".to_owned());
        for entry in balance {
            lines.push(format!("  {}", balance_line(entry)));
        }
    }
    lines
}

/// Legacy `quota` / `used` / `resetAt` fallback (only when the envelope has
/// no typed windows or ledgers; third-party compatibility).
fn legacy_sections(envelope: &UsageEnvelope) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(quota) = &envelope.quota {
        let mut parts = Vec::new();
        if let Some(used) = quota.used {
            parts.push(format!("used {}", format_count(used)));
        }
        if let Some(total) = quota.total {
            parts.push(format!("total {}", format_count(total)));
        }
        if let Some(remaining) = quota.remaining {
            parts.push(format!("remaining {}", format_count(remaining)));
        }
        if let Some(unit) = quota.unit.as_deref() {
            let unit = sanitize_text(unit);
            if !unit.is_empty() {
                parts.push(unit);
            }
        }
        if !parts.is_empty() {
            lines.push("Quota".to_owned());
            lines.push(format!("  {}", parts.join(" ")));
        }
    } else if let Some(used) = envelope.used {
        lines.push(format!("Used: {}", format_count(used)));
    }
    if let Some(reset) = envelope.reset_at.as_deref() {
        let reset = sanitize_text(reset);
        if !reset.is_empty() {
            lines.push(format!("Reset: {reset}"));
        }
    }
    lines
}

/// The fixed `Window / Used / Left / Reset` table. `None` when every window
/// is `unknown` (the caller then prints no table).
fn window_table(windows: &[UsageWindow], config: &UsageConfig, now_ms: i64) -> Option<Vec<String>> {
    let mut rows: Vec<[String; 4]> = Vec::new();
    for window in windows {
        if window.status == UsageWindowStatus::Unknown {
            continue;
        }
        let label = sanitize_text(&window.label);
        if label.is_empty() {
            continue;
        }
        let (used, left) = window_cells(window);
        let mut used = used.unwrap_or_else(|| "—".to_owned());
        if window.status == UsageWindowStatus::RateLimited {
            used.push_str(" · limited");
        }
        let reset = reset_cell(window.reset_at.as_deref(), config, now_ms);
        rows.push([label, used, left.unwrap_or_else(|| "—".to_owned()), reset]);
    }
    if rows.is_empty() {
        return None;
    }
    let headers = ["Window", "Used", "Left", "Reset"];
    let widths: [usize; 4] = std::array::from_fn(|column| {
        rows.iter()
            .map(|row| display_width(&row[column]))
            .chain(std::iter::once(headers[column].len()))
            .max()
            .unwrap_or(0)
    });
    let render_row = |cells: [&str; 4]| {
        let mut line = String::new();
        for (column, cell) in cells.iter().enumerate() {
            if column > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            if column < 3 {
                for _ in display_width(cell)..widths[column] {
                    line.push(' ');
                }
            }
        }
        line
    };
    let mut lines = vec![render_row(headers)];
    for row in &rows {
        lines.push(render_row([&row[0], &row[1], &row[2], &row[3]]));
    }
    Some(lines)
}

/// `(Used, Left)` cells for one window. `Used` is `None` only when the
/// window has no displayable representation (invalid for an ok window; the
/// host rejects it before it reaches the plugin, but formatters stay total).
fn window_cells(window: &UsageWindow) -> (Option<String>, Option<String>) {
    if window.unlimited == Some(true) {
        return (Some("unlimited".to_owned()), None);
    }
    if window.used_percent.is_some() || window.remaining_percent.is_some() {
        let used = window
            .used_percent
            .or_else(|| window.remaining_percent.map(|remaining| 100.0 - remaining))
            .unwrap_or(0.0);
        let left = window
            .remaining_percent
            .or_else(|| window.used_percent.map(|used| 100.0 - used))
            .unwrap_or(0.0);
        return (
            Some(format!("{}%", format_percent(used))),
            Some(format!("{}%", format_percent(left))),
        );
    }
    if let Some(unit) = window.unit.as_deref() {
        let unit = sanitize_text(unit);
        let used = window.used;
        let total = window.total;
        let remaining = window.remaining;
        if used.is_none() && total.is_none() && remaining.is_none() {
            return (None, None);
        }
        let used_cell = match (used, total) {
            (Some(used), Some(total)) => Some(format!(
                "{}/{} {unit}",
                format_count(used),
                format_count(total)
            )),
            (Some(used), None) => Some(format!("{} {unit}", format_count(used))),
            (None, Some(total)) => Some(format!("{} {unit}", format_count(total))),
            (None, None) => None,
        };
        let left_cell = remaining.map(|remaining| format!("{} {unit}", format_count(remaining)));
        return (used_cell, left_cell);
    }
    (None, None)
}

/// `reset` cell for the report table (never filtered by
/// `usage.resetCountdown`: the detailed report always shows the reset).
fn reset_cell(reset_at: Option<&str>, _config: &UsageConfig, _now_ms: i64) -> String {
    reset_at
        .map(sanitize_text)
        .filter(|reset| !reset.is_empty())
        .unwrap_or_else(|| "—".to_owned())
}

fn ledger_section(ledger: &UsageLedger) -> Vec<String> {
    let label = sanitize_text(&ledger.label);
    let mut lines = vec![label];
    let unit = sanitize_text(&ledger.unit);
    let value = |amount: f64| {
        if ledger.kind.is_monetary() {
            format_money(amount)
        } else {
            format_count(amount)
        }
    };
    if let Some(used) = ledger.used {
        lines.push(format!("  used: {} {unit}", value(used)));
    }
    if let Some(total) = ledger.total {
        lines.push(format!("  total: {} {unit}", value(total)));
    }
    if let Some(remaining) = ledger.remaining {
        lines.push(format!("  left: {} {unit}", value(remaining)));
    }
    lines
}

fn balance_line(entry: &UsageBalanceEntry) -> String {
    let currency = sanitize_text(&entry.currency);
    let Some(total) = entry.total else {
        return currency;
    };
    let mut line = format!("{currency} {}", format_money(total));
    let mut detail = Vec::new();
    if let Some(granted) = entry.granted {
        detail.push(format!("granted {}", format_money(granted)));
    }
    if let Some(topped_up) = entry.topped_up {
        detail.push(format!("topped up {}", format_money(topped_up)));
    }
    if !detail.is_empty() {
        line.push_str(&format!(" ({})", detail.join(", ")));
    }
    line
}

/// One footer window segment (03 §5.7.1).
fn window_segment(window: &UsageWindow, config: &UsageConfig, now_ms: i64) -> Option<String> {
    if window.status == UsageWindowStatus::Unknown {
        return None;
    }
    let label = sanitize_text(&window.label);
    if label.is_empty() {
        return None;
    }
    let value = if window.unlimited == Some(true) {
        "unlimited".to_owned()
    } else if window.used_percent.is_some() || window.remaining_percent.is_some() {
        match config.percentage {
            PercentageMode::Used => {
                let used = window
                    .used_percent
                    .or_else(|| window.remaining_percent.map(|remaining| 100.0 - remaining))?;
                format!("{}%", format_percent(used))
            }
            PercentageMode::Remaining => {
                let remaining = window
                    .remaining_percent
                    .or_else(|| window.used_percent.map(|used| 100.0 - used))?;
                format!("{}% left", format_percent(remaining))
            }
        }
    } else {
        let unit = sanitize_text(window.unit.as_deref()?);
        match (window.used, window.total, window.remaining) {
            (Some(used), Some(total), _) => {
                format!("{}/{} {unit}", format_count(used), format_count(total))
            }
            (Some(used), None, _) => format!("{} {unit} used", format_count(used)),
            (None, None, Some(remaining)) => {
                format!("{} {unit} left", format_count(remaining))
            }
            (None, Some(total), _) => format!("{} {unit}", format_count(total)),
            (None, None, None) => return None,
        }
    };
    let mut segment = format!("{label} {value}");
    if window.status == UsageWindowStatus::RateLimited {
        segment.push_str(" · limited");
    }
    if let Some(reset) = reset_suffix(window.reset_at.as_deref(), config, now_ms) {
        segment.push(' ');
        segment.push_str(&reset);
    }
    Some(segment)
}

/// One footer ledger segment. Monetary ledgers use the currency prefix;
/// counted ledgers keep their unit. Kinds/currencies are never summed.
fn ledger_segment(ledger: &UsageLedger) -> Option<String> {
    let label = sanitize_text(&ledger.label);
    if label.is_empty() {
        return None;
    }
    if ledger.kind.is_monetary() {
        let currency = sanitize_text(ledger.currency.as_deref()?);
        let value = match (ledger.used, ledger.total, ledger.remaining) {
            (Some(used), Some(total), _) => {
                format!("{currency} {}/{}", format_money(used), format_money(total))
            }
            (Some(used), None, _) => format!("{currency} {} used", format_money(used)),
            (None, _, Some(remaining)) => {
                format!("{currency} {} left", format_money(remaining))
            }
            (None, Some(total), None) => format!("{currency} {}", format_money(total)),
            (None, None, None) => return None,
        };
        return Some(format!("{label} {value}"));
    }
    let unit = sanitize_text(&ledger.unit);
    let value = match (ledger.used, ledger.total, ledger.remaining) {
        (Some(used), Some(total), _) => {
            format!("{}/{} {unit}", format_count(used), format_count(total))
        }
        (Some(used), None, _) => format!("{} {unit} used", format_count(used)),
        (None, _, Some(remaining)) => format!("{} {unit} left", format_count(remaining)),
        (None, Some(total), None) => format!("{} {unit}", format_count(total)),
        (None, None, None) => return None,
    };
    Some(format!("{label} {value}"))
}

fn balance_segment(entry: &UsageBalanceEntry) -> Option<String> {
    let currency = sanitize_text(&entry.currency);
    let total = entry.total?;
    Some(format!("Balance {currency} {}", format_money(total)))
}

/// 03 §5.7.1 reset suffix: `(~48m)`, `(~2h5m)`, `(~3d22h)`, `<1m`, or a
/// static `(reset RFC3339)` when `usage.resetCountdown=false`. Past or
/// invalid timestamps produce no suffix.
fn reset_suffix(reset_at: Option<&str>, config: &UsageConfig, now_ms: i64) -> Option<String> {
    let reset_at = reset_at?;
    if !config.reset_countdown {
        let reset = sanitize_text(reset_at);
        return (!reset.is_empty()).then(|| format!("(reset {reset})"));
    }
    let reset = reset_epoch_ms(reset_at)?;
    countdown(reset - now_ms).map(|duration| format!("(~{duration})"))
}

/// `~48m` / `~2h5m` / `~3d22h` / `~<1m` (at most two units).
fn countdown(delta_ms: i64) -> Option<String> {
    if delta_ms <= 0 {
        return None;
    }
    if delta_ms < 60_000 {
        return Some("<1m".to_owned());
    }
    let minutes = delta_ms / 60_000;
    if minutes < 60 {
        return Some(format!("{minutes}m"));
    }
    let hours = delta_ms / 3_600_000;
    let leftover_minutes = (delta_ms % 3_600_000) / 60_000;
    if hours < 24 {
        return Some(if leftover_minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{leftover_minutes}m")
        });
    }
    let days = delta_ms / 86_400_000;
    let leftover_hours = (delta_ms % 86_400_000) / 3_600_000;
    Some(if leftover_hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d{leftover_hours}h")
    })
}

/// Parse one RFC3339 reset timestamp to epoch milliseconds.
pub fn reset_epoch_ms(reset_at: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(reset_at)
        .ok()
        .map(|datetime| datetime.timestamp_millis())
}

/// Percent with at most one decimal; `.0` is removed.
pub fn format_percent(value: f64) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if rounded.fract().abs() < f64::EPSILON {
        format!("{}", rounded as i64)
    } else {
        format!("{rounded:.1}")
    }
}

/// Full decimal count with insignificant trailing zeros removed.
pub fn format_count(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let mut text = format!("{value:.6}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

/// Money is always two decimals (footer grammar and ledger sections).
pub fn format_money(value: f64) -> String {
    format!("{value:.2}")
}

/// Strip data ANSI sequences, C0/C1 controls, CR/LF/TAB and collapse runs of
/// spaces (the same rule the statusline applies to untrusted field values).
pub fn sanitize_text(text: &str) -> String {
    let without_ansi = strip_csi(text);
    let mut out = String::with_capacity(without_ansi.len());
    let mut previous_space = false;
    for ch in without_ansi.chars() {
        let ch = match ch {
            '\r' | '\n' | '\t' => ' ',
            ch if ch.is_control() => continue,
            ch => ch,
        };
        if ch == ' ' {
            if !previous_space {
                out.push(' ');
            }
            previous_space = true;
        } else {
            previous_space = false;
            out.push(ch);
        }
    }
    out.trim().to_owned()
}

/// Drop every CSI sequence (`ESC [ ... final-byte`), not only SGR.
fn strip_csi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && index + 1 < bytes.len() && bytes[index + 1] == b'[' {
            let mut end = index + 2;
            while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                end += 1;
            }
            if end < bytes.len() {
                index = end + 1;
                continue;
            }
        }
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// Display width used for table padding (one column per Unicode scalar; CJK
/// labels are rare in usage data and never change the content).
fn display_width(text: &str) -> usize {
    text.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ext_host::usage::{
        UsageError, UsageErrorKind, UsageFetchResult, UsageLedgerKind, UsageQuota,
        UsageWindowStatus,
    };

    fn config() -> UsageConfig {
        UsageConfig::default()
    }

    fn window(id: &str, label: &str, status: UsageWindowStatus) -> UsageWindow {
        UsageWindow {
            id: id.to_owned(),
            label: label.to_owned(),
            status,
            used_percent: None,
            remaining_percent: None,
            used: None,
            total: None,
            remaining: None,
            unit: None,
            unlimited: None,
            reset_at: None,
            duration_seconds: None,
        }
    }

    fn envelope() -> UsageEnvelope {
        UsageEnvelope {
            schema_version: 1,
            provider: "x".to_owned(),
            plan: None,
            balance: None,
            windows: None,
            ledgers: None,
            quota: None,
            used: None,
            reset_at: None,
            display_text: "x: ok".to_owned(),
        }
    }

    const NOW_MS: i64 = 1_800_000_000_000;
    const RESET_48M: &str = "2027-01-15T08:48:00Z";
    const RESET_2H5M: &str = "2027-01-15T10:05:00Z";
    const RESET_3D22H: &str = "2027-01-19T06:00:00Z";

    #[test]
    fn percent_windows_render_used_and_remaining_with_reset() {
        let mut environment = envelope();
        environment.windows = Some(vec![
            {
                let mut first = window("5h", "5h", UsageWindowStatus::Ok);
                first.used_percent = Some(39.0);
                first.remaining_percent = Some(61.0);
                first.reset_at = Some(RESET_48M.to_owned());
                first
            },
            {
                let mut second = window("7d", "7d", UsageWindowStatus::Ok);
                second.used_percent = Some(31.25);
                second.remaining_percent = Some(68.75);
                second.reset_at = Some(RESET_3D22H.to_owned());
                second
            },
        ]);
        let used = footer_text(&environment, &config(), NOW_MS).expect("footer");
        assert_eq!(used, "5h 39% (~48m) / 7d 31.3% (~3d22h)");
        let remaining = UsageConfig {
            percentage: PercentageMode::Remaining,
            ..config()
        };
        assert_eq!(
            footer_text(&environment, &remaining, NOW_MS).expect("footer"),
            "5h 61% left (~48m) / 7d 68.8% left (~3d22h)"
        );
    }

    #[test]
    fn counted_unlimited_ledger_and_balance_segments_follow_the_grammar() {
        let mut environment = envelope();
        environment.windows = Some(vec![
            {
                let mut counted = window("5h", "5h", UsageWindowStatus::Ok);
                counted.used = Some(12.0);
                counted.total = Some(30.0);
                counted.remaining = Some(18.0);
                counted.unit = Some("requests".to_owned());
                counted.reset_at = Some(RESET_2H5M.to_owned());
                counted
            },
            {
                let mut only_total = window("d", "d", UsageWindowStatus::Ok);
                only_total.total = Some(30.0);
                only_total.unit = Some("requests".to_owned());
                only_total
            },
            {
                let mut unlimited = window("u", "u", UsageWindowStatus::Ok);
                unlimited.unlimited = Some(true);
                unlimited.reset_at = Some(RESET_3D22H.to_owned());
                unlimited
            },
        ]);
        environment.ledgers = Some(vec![
            UsageLedger {
                id: "on_demand".to_owned(),
                label: "On-demand".to_owned(),
                kind: UsageLedgerKind::OnDemand,
                unit: "USD".to_owned(),
                currency: Some("USD".to_owned()),
                used: Some(3.0),
                total: Some(50.0),
                remaining: Some(47.0),
            },
            UsageLedger {
                id: "prepaid".to_owned(),
                label: "Prepaid".to_owned(),
                kind: UsageLedgerKind::Prepaid,
                unit: "USD".to_owned(),
                currency: Some("USD".to_owned()),
                used: None,
                total: None,
                remaining: Some(12.5),
            },
        ]);
        environment.balance = Some(vec![UsageBalanceEntry {
            currency: "CNY".to_owned(),
            total: Some(12.5),
            granted: Some(0.0),
            topped_up: Some(12.5),
        }]);
        assert_eq!(
            footer_text(&environment, &config(), NOW_MS).expect("footer"),
            "5h 12/30 requests (~2h5m) / d 30 requests / u unlimited (~3d22h) / \
             On-demand USD 3.00/50.00 / Prepaid USD 12.50 left / Balance CNY 12.50"
        );
    }

    #[test]
    fn footer_counted_remaining_only_uses_left() {
        let mut environment = envelope();
        let mut window = window("c", "c", UsageWindowStatus::Ok);
        window.unit = Some("requests".to_owned());
        window.remaining = Some(18.0);
        environment.windows = Some(vec![window]);
        assert_eq!(
            footer_text(&environment, &config(), NOW_MS).as_deref(),
            Some("c 18 requests left")
        );
    }

    #[test]
    fn static_reset_limited_unknown_and_single_sided_counts() {
        let mut environment = envelope();
        environment.windows = Some(vec![
            {
                let mut limited = window("5h", "5h", UsageWindowStatus::RateLimited);
                limited.used_percent = Some(39.0);
                limited.remaining_percent = Some(61.0);
                limited.reset_at = Some(RESET_48M.to_owned());
                limited
            },
            {
                let mut unknown = window("x", "x", UsageWindowStatus::Unknown);
                unknown.used_percent = None;
                unknown
            },
            {
                let mut used_only = window("c", "c", UsageWindowStatus::Ok);
                used_only.used = Some(12.0);
                used_only.unit = Some("requests".to_owned());
                used_only
            },
        ]);
        let static_config = UsageConfig {
            reset_countdown: false,
            ..config()
        };
        assert_eq!(
            footer_text(&environment, &static_config, NOW_MS).expect("footer"),
            "5h 39% · limited (reset 2027-01-15T08:48:00Z) / c 12 requests used"
        );
        let mut all_unknown = envelope();
        let mut unknown = window("x", "x", UsageWindowStatus::Unknown);
        unknown.used_percent = None;
        all_unknown.windows = Some(vec![unknown]);
        assert_eq!(footer_text(&all_unknown, &config(), NOW_MS), None);
    }

    #[test]
    fn legacy_display_text_is_kept_verbatim_without_typed_data() {
        let mut environment = envelope();
        environment.display_text = "5h 39%(~48m) / 7d 31%(~3d22h)".to_owned();
        assert_eq!(
            footer_text(&environment, &config(), NOW_MS).as_deref(),
            Some("5h 39%(~48m) / 7d 31%(~3d22h)")
        );
        environment.display_text = "  ".to_owned();
        assert_eq!(footer_text(&environment, &config(), NOW_MS), None);
    }

    #[test]
    fn report_table_sections_and_cache_stale_errors() {
        let environment = UsageEnvelope {
            schema_version: 1,
            provider: "anthropic".to_owned(),
            plan: Some("Claude Pro".to_owned()),
            balance: Some(vec![UsageBalanceEntry {
                currency: "USD".to_owned(),
                total: Some(0.0),
                granted: None,
                topped_up: None,
            }]),
            windows: Some(vec![
                {
                    let mut first = window("five_hour", "5h", UsageWindowStatus::Ok);
                    first.used_percent = Some(39.0);
                    first.remaining_percent = Some(61.0);
                    first.reset_at = Some(RESET_48M.to_owned());
                    first.duration_seconds = Some(18_000);
                    first
                },
                {
                    let mut weekly = window("seven_day", "7d", UsageWindowStatus::RateLimited);
                    weekly.used_percent = Some(100.0);
                    weekly.remaining_percent = Some(0.0);
                    weekly
                },
            ]),
            ledgers: None,
            quota: None,
            used: None,
            reset_at: None,
            display_text: "legacy line".to_owned(),
        };
        let report =
            UsageReport::from_fetch("anthropic", &UsageFetchResult::network(environment), true);
        let text = report_text(&report, true, &config(), NOW_MS);
        assert!(
            text.starts_with("anthropic (current)\nplan: Claude Pro\n"),
            "{text}"
        );
        assert!(
            text.contains("Window  Used            Left  Reset"),
            "{text}"
        );
        assert!(
            text.contains("5h      39%             61%   2027-01-15T08:48:00Z"),
            "{text}"
        );
        assert!(text.contains("7d      100% · limited  0%    —"), "{text}");
        assert!(text.contains("Balance\n  USD 0.00"), "{text}");
        assert!(
            !text.contains("legacy line"),
            "displayText is not a report line"
        );

        let mut failed = UsageFetchResult::cache_with_error(
            UsageEnvelope {
                display_text: "stale".to_owned(),
                ..envelope()
            },
            UsageError::new(UsageErrorKind::RateLimited),
        );
        failed.stale = true;
        let report = UsageReport::from_fetch("x", &failed, false);
        let text = report_text(&report, false, &config(), NOW_MS);
        assert!(text.contains("Status: rate-limited"), "{text}");
        assert!(text.contains("Stale data (cached):"), "{text}");

        let miss = UsageReport::from_fetch(
            "x",
            &UsageFetchResult::failure(UsageError::new(UsageErrorKind::CacheMiss)),
            false,
        );
        let text = report_text(&miss, false, &config(), NOW_MS);
        assert_eq!(
            text,
            "x\nStatus: cache-miss\nAction: Run /usage without --cached to fetch fresh data."
        );
    }

    #[test]
    fn legacy_display_text_only_success_keeps_the_title_without_a_false_error() {
        let environment = UsageEnvelope {
            display_text: "third-party: 42 left".to_owned(),
            ..envelope()
        };
        let report = UsageReport::from_fetch("x", &UsageFetchResult::network(environment), false);
        assert_eq!(report_text(&report, false, &config(), NOW_MS), "x");
    }

    #[test]
    fn remaining_only_counted_window_keeps_its_row() {
        let mut environment = envelope();
        let mut window = window("c", "c", UsageWindowStatus::Ok);
        window.unit = Some("requests".to_owned());
        window.remaining = Some(18.0);
        environment.windows = Some(vec![window]);
        let report = UsageReport::from_fetch("x", &UsageFetchResult::network(environment), false);
        let text = report_text(&report, false, &config(), NOW_MS);
        assert!(text.contains("c  "), "{text}");
        assert!(text.contains("c       —     18 requests"), "{text}");
    }

    #[test]
    fn cached_source_line_and_local_token_text() {
        let environment = UsageEnvelope {
            display_text: "cached".to_owned(),
            ..envelope()
        };
        let report =
            UsageReport::from_fetch("x", &UsageFetchResult::cache(environment, false), true);
        assert_eq!(
            report_text(&report, true, &config(), NOW_MS),
            "x (current)\nSource: cache (no request)"
        );
        let tokens = LocalTokenUsage {
            input: 10,
            output: 20,
            cache_read: 30,
            cache_write: 40,
            used: 70,
        };
        assert_eq!(
            local_text(&tokens),
            "Local session tokens\ninput: 10\noutput: 20\ncacheRead: 30\ncacheWrite: 40\n\
             used: 70 (input + cacheWrite + output)"
        );
    }

    #[test]
    fn countdown_and_number_helpers() {
        assert_eq!(countdown(30_000).as_deref(), Some("<1m"));
        assert_eq!(countdown(60_000).as_deref(), Some("1m"));
        assert_eq!(countdown(2_880_000).as_deref(), Some("48m"));
        assert_eq!(countdown(7_500_000).as_deref(), Some("2h5m"));
        assert_eq!(countdown(338_400_000).as_deref(), Some("3d22h"));
        assert_eq!(countdown(86_400_000).as_deref(), Some("1d"));
        assert_eq!(countdown(0), None);
        assert_eq!(format_percent(39.0), "39");
        assert_eq!(format_percent(42.45), "42.5");
        assert_eq!(format_count(12.0), "12");
        assert_eq!(format_count(12.5), "12.5");
        assert_eq!(format_money(12.5), "12.50");
        assert_eq!(sanitize_text("a\u{1b}[31mb\n\u{7f}c\t  d "), "ab c d");
    }

    #[test]
    fn next_reset_ms_skips_unknown_and_past_windows() {
        let mut environment = envelope();
        environment.windows = Some(vec![
            {
                let mut past = window("p", "p", UsageWindowStatus::Ok);
                past.reset_at = Some("2020-01-01T00:00:00Z".to_owned());
                past
            },
            {
                let mut unknown = window("x", "x", UsageWindowStatus::Unknown);
                unknown.reset_at = Some(RESET_48M.to_owned());
                unknown
            },
            {
                let mut future = window("f", "f", UsageWindowStatus::Ok);
                future.reset_at = Some(RESET_3D22H.to_owned());
                future
            },
        ]);
        assert_eq!(
            next_reset_ms(&environment, NOW_MS),
            reset_epoch_ms(RESET_3D22H)
        );
        assert_eq!(next_reset_ms(&environment, i64::MAX), None);
    }

    #[test]
    fn legacy_quota_and_reset_render_only_without_typed_windows() {
        let environment = UsageEnvelope {
            plan: Some("max".to_owned()),
            quota: Some(UsageQuota {
                used: Some(12.5),
                total: Some(30.0),
                remaining: Some(17.5),
                unit: Some("%".to_owned()),
            }),
            used: Some(12.5),
            reset_at: Some(RESET_48M.to_owned()),
            ..envelope()
        };
        let lines = envelope_lines(&environment, &config(), NOW_MS);
        assert_eq!(
            lines,
            vec![
                "plan: max".to_owned(),
                "Quota".to_owned(),
                "  used 12.5 total 30 remaining 17.5 %".to_owned(),
                format!("Reset: {RESET_48M}"),
            ]
        );
        let mut with_window = environment.clone();
        let mut typed = window("5h", "5h", UsageWindowStatus::Ok);
        typed.used_percent = Some(39.0);
        typed.remaining_percent = Some(61.0);
        with_window.windows = Some(vec![typed]);
        let lines = envelope_lines(&with_window, &config(), NOW_MS);
        assert!(!lines.iter().any(|line| line == "Quota"), "{lines:?}");
    }

    #[test]
    fn legacy_used_only_and_balance_without_total() {
        let environment = UsageEnvelope {
            used: Some(2.5),
            balance: Some(vec![UsageBalanceEntry {
                currency: "CNY".to_owned(),
                total: None,
                granted: None,
                topped_up: None,
            }]),
            ..envelope()
        };
        let lines = envelope_lines(&environment, &config(), NOW_MS);
        assert_eq!(
            lines,
            vec![
                "Used: 2.5".to_owned(),
                "Balance".to_owned(),
                "  CNY".to_owned()
            ]
        );
    }
}
