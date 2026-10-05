//! Envelope formatting (TE44 FR-A): the parsed usage envelope (V16-05 §7.2)
//! → the command-face multi-line report, and the footer single line.

use serde_json::Value;

/// The footer line: the envelope `displayText` when it is a non-empty string.
pub fn display_text(envelope: &Value) -> Option<&str> {
    envelope
        .get("displayText")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// Render the command report (displayText + every optional field present).
pub fn format_envelope(envelope: &Value) -> String {
    let provider = envelope
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mut lines: Vec<String> = Vec::new();
    match display_text(envelope) {
        Some(text) => lines.push(text.to_owned()),
        None => lines.push(format!("{provider}: no display text")),
    }
    if let Some(plan) = envelope.get("plan").and_then(Value::as_str)
        && !plan.trim().is_empty()
    {
        lines.push(format!("plan: {}", plan.trim()));
    }
    if let Some(balance) = envelope.get("balance").and_then(Value::as_array) {
        for entry in balance {
            let currency = entry.get("currency").and_then(Value::as_str).unwrap_or("?");
            let total = number(entry.get("total")).unwrap_or_else(|| "?".to_owned());
            let mut detail = Vec::new();
            if let Some(granted) = number(entry.get("granted")) {
                detail.push(format!("granted {granted}"));
            }
            if let Some(topped_up) = number(entry.get("toppedUp")) {
                detail.push(format!("topped up {topped_up}"));
            }
            if detail.is_empty() {
                lines.push(format!("balance: {currency} {total}"));
            } else {
                lines.push(format!(
                    "balance: {currency} {total} ({})",
                    detail.join(", ")
                ));
            }
        }
    }
    if let Some(quota) = envelope.get("quota") {
        let mut parts = Vec::new();
        if let Some(used) = number(quota.get("used")) {
            parts.push(format!("used {used}"));
        }
        if let Some(total) = number(quota.get("total")) {
            parts.push(format!("total {total}"));
        }
        if let Some(remaining) = number(quota.get("remaining")) {
            parts.push(format!("remaining {remaining}"));
        }
        if let Some(unit) = quota
            .get("unit")
            .and_then(Value::as_str)
            .filter(|unit| !unit.is_empty())
        {
            parts.push(unit.to_owned());
        }
        if !parts.is_empty() {
            lines.push(format!("quota: {}", parts.join(" ")));
        }
    } else if let Some(used) = number(envelope.get("used")) {
        lines.push(format!("used: {used}"));
    }
    if let Some(reset) = envelope.get("resetAt").and_then(Value::as_str)
        && !reset.trim().is_empty()
    {
        lines.push(format!("reset: {}", reset.trim()));
    }
    lines.join("\n")
}

/// Format a JSON number (or numeric string) without trailing noise.
fn number(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let parsed = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))?;
    if parsed.fract() == 0.0 && parsed.abs() < 1e15 {
        Some(format!("{}", parsed as i64))
    } else {
        let mut text = format!("{parsed:.4}");
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn full_envelope_renders_every_section() {
        let envelope = json!({
            "schemaVersion": 1,
            "provider": "deepseek",
            "plan": "Coding Plan",
            "balance": [
                {"currency": "CNY", "total": 12.5, "granted": 10.0, "toppedUp": 2.5},
                {"currency": "USD", "total": 3}
            ],
            "quota": {"used": 12.5, "total": 30, "remaining": 17.5, "unit": "%"},
            "used": 12.5,
            "resetAt": "2026-10-08T00:00:00Z",
            "displayText": "deepseek: CNY 12.50"
        });
        assert_eq!(
            format_envelope(&envelope),
            "deepseek: CNY 12.50\n\
             plan: Coding Plan\n\
             balance: CNY 12.5 (granted 10, topped up 2.5)\n\
             balance: USD 3\n\
             quota: used 12.5 total 30 remaining 17.5 %\n\
             reset: 2026-10-08T00:00:00Z"
        );
    }

    #[test]
    fn minimal_envelope_answers_the_display_line() {
        let envelope = json!({"schemaVersion": 1, "provider": "x", "displayText": "x: ok"});
        assert_eq!(format_envelope(&envelope), "x: ok");
        assert_eq!(display_text(&envelope), Some("x: ok"));
        // No displayText -> a provider-labelled placeholder; no panic.
        let bare = json!({"schemaVersion": 1, "provider": "x"});
        assert_eq!(display_text(&bare), None);
        assert_eq!(format_envelope(&bare), "x: no display text");
    }

    #[test]
    fn quota_optional_pieces_and_unitless_used_are_rendered() {
        let envelope = json!({
            "provider": "x",
            "quota": {"used": 1, "unit": "requests"},
            "displayText": "x: 1"
        });
        assert_eq!(format_envelope(&envelope), "x: 1\nquota: used 1 requests");
        let envelope = json!({"provider": "x", "used": 2.5, "displayText": "x: 2.5"});
        assert_eq!(format_envelope(&envelope), "x: 2.5\nused: 2.5");
    }
}
