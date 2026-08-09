//! List available models with optional fuzzy search.
//!
//! Port of `packages/coding-agent/src/cli/list-models.ts` @ pi 0.82.1
//! (2efa728).

use rpi_tui::fuzzy::fuzzy_filter;

use crate::core::auth_guidance::format_no_models_available_message;
use crate::core::model_runtime::ModelRuntime;

/// `formatTokenCount` (list-models.ts:14-24): 200000 → "200K", 1000000 → "1M".
pub fn format_token_count(count: u32) -> String {
    let count = u64::from(count);
    // JS `toFixed(1)` rounds exact halves up; Rust's `{:.1}` is
    // round-half-to-even. Pre-round half-away-from-zero to match (counts are
    // integers, so ties like 1.25M are exactly representable).
    if count >= 1_000_000 {
        let millions = count as f64 / 1_000_000.0;
        if count % 1_000_000 == 0 {
            return format!("{}M", count / 1_000_000);
        }
        return format!("{:.1}M", (millions * 10.0).round() / 10.0);
    }
    if count >= 1_000 {
        if count % 1_000 == 0 {
            return format!("{}K", count / 1_000);
        }
        return format!("{:.1}K", (count as f64 / 100.0).round() / 10.0);
    }
    count.to_string()
}

/// `listModels` (list-models.ts:29-111). Returns the rendered table text;
/// the caller prints it (upstream writes to stdout; the load error warning
/// goes to stderr and is returned separately).
pub fn list_models_text(models: Vec<rpi_ai::types::Model>, search_pattern: Option<&str>) -> String {
    if models.is_empty() {
        return format!("{}\n", format_no_models_available_message());
    }

    // Apply fuzzy filter if search pattern provided.
    let mut filtered = match search_pattern {
        Some(pattern) => fuzzy_filter(models, pattern, |m| format!("{} {}", m.provider, m.id)),
        None => models,
    };

    if filtered.is_empty() {
        return format!(
            "No models matching \"{}\"\n",
            search_pattern.unwrap_or_default()
        );
    }

    // Sort by provider, then by model id.
    filtered.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.id.cmp(&b.id)));

    let headers = [
        "provider", "model", "context", "max-out", "thinking", "images",
    ];
    let rows: Vec<[String; 6]> = filtered
        .iter()
        .map(|m| {
            [
                m.provider.clone(),
                m.id.clone(),
                format_token_count(m.context_window),
                format_token_count(m.max_tokens),
                if m.reasoning { "yes" } else { "no" }.to_owned(),
                if m.input.contains(&rpi_ai::types::InputModality::Image) {
                    "yes"
                } else {
                    "no"
                }
                .to_owned(),
            ]
        })
        .collect();

    let mut widths = [0usize; 6];
    for (i, header) in headers.iter().enumerate() {
        widths[i] = header.len();
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = String::new();
    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{h:<width$}", width = widths[i]))
        .collect();
    out.push_str(&header_line.join("  "));
    out.push('\n');
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{cell:<width$}", width = widths[i]))
            .collect();
        out.push_str(&line.join("  "));
        out.push('\n');
    }
    out
}

/// Full `--list-models` flow (list-models.ts:29-111): resolves the available
/// models from the runtime, returns `(stderr_warnings, stdout_text)`.
pub async fn list_models(
    model_runtime: &ModelRuntime,
    search_pattern: Option<&str>,
) -> (Option<String>, String) {
    let warning = model_runtime
        .get_error()
        .map(|error| format!("Warning: errors loading models.json:\n{error}"));
    let models = model_runtime.get_available(None).await.unwrap_or_default();
    (warning, list_models_text(models, search_pattern))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_token_count() {
        assert_eq!(format_token_count(200000), "200K");
        assert_eq!(format_token_count(1000000), "1M");
        assert_eq!(format_token_count(1500000), "1.5M");
        assert_eq!(format_token_count(1500), "1.5K");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(0), "0");
        // JS `toFixed(1)` rounds exact halves up (1.25 → "1.3").
        assert_eq!(format_token_count(1250), "1.3K");
        assert_eq!(format_token_count(1250000), "1.3M");
    }
}
