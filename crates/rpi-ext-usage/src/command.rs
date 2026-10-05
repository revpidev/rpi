//! `/usage` command handling (TE44 FR-A; plugin 01 §2).
//!
//! Four paths: no argument (the current model provider), an explicit
//! provider key, `all`, and anything else (usage line). Reports go through
//! `ui.notify` (multi-line text; RPC receives it as an event, print/no-UI
//! hosts degrade to a no-op without an error). After a command the footer is
//! refreshed from the framework cache, so `/usage` doubles as a manual
//! footer refresh.

use std::time::Instant;

use serde_json::{Value, json};

use crate::footer::{self, FooterState};
use crate::format::format_envelope;
use crate::{HostCall, config, host, providers};

/// Registered command name.
pub const COMMAND_NAME: &str = "usage";

/// `registerCommand` payload.
pub fn command_definition() -> Value {
    json!({
        "name": COMMAND_NAME,
        "description": "Query the current provider's balance or plan quota (/usage, /usage <provider>, /usage all)",
    })
}

/// Run one `/usage` invocation synchronously (command dispatch is enqueued
/// on the refresh worker; this also serves the no-worker test fallback).
pub fn handle_now(
    host: &dyn HostCall,
    config: &config::UsageConfig,
    state: &mut FooterState,
    args: &str,
    now: Instant,
) {
    if !config.enabled {
        host::notify(
            host,
            "rpi-usage is disabled (settings usage.enabled=false).",
        );
        return;
    }
    let known = host::usage_list_providers(host);
    let trimmed = args.trim();
    let message = if trimmed.is_empty() {
        current_report(host, &known)
    } else if trimmed == "all" {
        all_report(host, &known)
    } else if trimmed.split_whitespace().count() == 1 {
        explicit_report(host, &known, trimmed)
    } else {
        usage_line()
    };
    host::notify(host, &message);
    footer::refresh(host, config, state, false, false, now);
}

/// `/usage` — the current model provider.
fn current_report(host: &dyn HostCall, known: &[String]) -> String {
    let model = host::current_model(host);
    let model_provider = model
        .as_ref()
        .and_then(host::model_provider)
        .unwrap_or_default();
    let Some(provider) = providers::alias_for(model_provider) else {
        return guidance(model_provider, known);
    };
    if !known.iter().any(|entry| entry == provider) {
        return guidance(model_provider, known);
    }
    report_for(host, provider)
}

/// `/usage <provider>` — an explicit registry key.
fn explicit_report(host: &dyn HostCall, known: &[String], provider: &str) -> String {
    if !known.iter().any(|entry| entry == provider) {
        return guidance(provider, known);
    }
    report_for(host, provider)
}

/// `/usage all` — every reachable provider, failures isolated.
fn all_report(host: &dyn HostCall, known: &[String]) -> String {
    if known.is_empty() {
        return guidance("(none)", known);
    }
    let mut sections: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for provider in known {
        match host::usage_fetch(host, provider, true) {
            Some(envelope) => sections.push(format_envelope(&envelope)),
            None => failures.push(provider.clone()),
        }
    }
    if !failures.is_empty() {
        sections.push(format!(
            "no data: {} (script failed or no API key)",
            failures.join(", ")
        ));
    }
    sections.join("\n\n")
}

/// One provider's formatted report.
fn report_for(host: &dyn HostCall, provider: &str) -> String {
    match host::usage_fetch(host, provider, true) {
        Some(envelope) => format_envelope(&envelope),
        None => format!(
            "rpi-usage: no data for `{provider}`.\n\
             The script failed or no API key is configured; log in with /login \
             or set the provider's environment variable."
        ),
    }
}

/// No matching script: configuration guidance (R-US-1.4).
fn guidance(model_provider: &str, known: &[String]) -> String {
    let provider = if model_provider.trim().is_empty() {
        "(unknown)"
    } else {
        model_provider
    };
    let dir = config::agent_dir().join("usage-providers");
    let known = if known.is_empty() {
        "(none)".to_owned()
    } else {
        known.join(", ")
    };
    format!(
        "rpi-usage: no script matches `{provider}`.\n\
         Add {}/<provider>.py or map it in settings usage.providers, then set \
         its API key via /login or the provider's environment variable.\n\
         Known providers: {known}",
        dir.display()
    )
}

/// Wrong argument count.
fn usage_line() -> String {
    "usage: /usage (current provider) · /usage <provider> · /usage all".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_host::UsageFakeHost;

    fn envelope(provider: &str, text: &str) -> Value {
        json!({"schemaVersion": 1, "provider": provider, "displayText": text})
    }

    fn handle(host: &UsageFakeHost, args: &str) -> (String, FooterState) {
        let mut state = FooterState::default();
        handle_now(
            host,
            &config::UsageConfig::default(),
            &mut state,
            args,
            Instant::now(),
        );
        let message = host.notifications().last().cloned().unwrap_or_default();
        (message, state)
    }

    #[test]
    fn no_argument_uses_the_model_alias_and_forces_a_fetch() {
        let host = UsageFakeHost::new();
        host.set_model(json!({"id": "glm-5.3", "provider": "zai-coding-cn"}));
        host.set_providers(vec!["glm-coding-plan".to_owned()]);
        host.set_envelope(
            "glm-coding-plan",
            json!({
                "schemaVersion": 1,
                "provider": "glm-coding-plan",
                "plan": "max",
                "displayText": "glm-coding-plan: 5h 1% used",
                "quota": {"used": 1, "total": 100, "unit": "%"}
            }),
        );
        let (message, state) = handle(&host, "");
        assert!(message.contains("glm-coding-plan: 5h 1% used"), "{message}");
        assert!(message.contains("plan: max"), "{message}");
        assert!(message.contains("quota: used 1 total 100 %"), "{message}");
        assert_eq!(
            host.fetch_calls().first(),
            Some(&("glm-coding-plan".to_owned(), true)),
            "the command fetch forces past the cache"
        );
        assert_eq!(state.provider.as_deref(), Some("glm-coding-plan"));
        // The trailing footer refresh re-reads the (fresh) cache.
        assert_eq!(
            host.statuses().last(),
            Some(&(
                "rpi-usage".to_owned(),
                Some("glm-coding-plan: 5h 1% used".to_owned())
            ))
        );
    }

    #[test]
    fn unknown_current_provider_points_at_configuration() {
        let host = UsageFakeHost::new();
        host.set_model(json!({"id": "local", "provider": "dgx-spark"}));
        host.set_providers(vec!["deepseek".to_owned()]);
        let (message, _state) = handle(&host, "");
        assert!(
            message.contains("no script matches `dgx-spark`"),
            "{message}"
        );
        assert!(message.contains("usage-providers"), "{message}");
        assert!(message.contains("Known providers: deepseek"), "{message}");
        assert!(
            host.fetch_calls().is_empty(),
            "no fetch for an unmapped provider"
        );
    }

    #[test]
    fn explicit_provider_and_unknown_provider_paths() {
        let host = UsageFakeHost::new();
        host.set_providers(vec!["deepseek".to_owned()]);
        host.set_envelope("deepseek", envelope("deepseek", "deepseek: CNY 1"));
        let (message, _) = handle(&host, "deepseek");
        assert_eq!(message, "deepseek: CNY 1");
        let (message, _) = handle(&host, "nope");
        assert!(message.contains("no script matches `nope`"), "{message}");
        let (message, _) = handle(&host, "a b");
        assert!(message.starts_with("usage: /usage"), "{message}");
    }

    #[test]
    fn all_queries_every_provider_and_isolates_failures() {
        let host = UsageFakeHost::new();
        host.set_providers(vec!["deepseek".to_owned(), "kimi-code".to_owned()]);
        host.set_envelope("deepseek", envelope("deepseek", "deepseek: CNY 1"));
        host.fail_fetch("kimi-code");
        let (message, _) = handle(&host, "all");
        assert!(message.contains("deepseek: CNY 1"), "{message}");
        assert!(
            message.contains("no data: kimi-code (script failed or no API key)"),
            "{message}"
        );
    }

    #[test]
    fn failure_report_and_disabled_switch() {
        let host = UsageFakeHost::new();
        host.set_providers(vec!["deepseek".to_owned()]);
        host.fail_fetch("deepseek");
        let (message, _) = handle(&host, "deepseek");
        assert!(message.contains("no data for `deepseek`"), "{message}");
        // Disabled: no fetch, no host surface beyond the notification.
        let disabled = config::UsageConfig {
            enabled: false,
            ..config::UsageConfig::default()
        };
        let host = UsageFakeHost::new();
        host.set_providers(vec!["deepseek".to_owned()]);
        let mut state = FooterState::default();
        handle_now(&host, &disabled, &mut state, "", Instant::now());
        assert!(
            host.notifications()
                .last()
                .is_some_and(|message| message.contains("disabled")),
            "{:?}",
            host.notifications()
        );
        assert!(host.fetch_calls().is_empty());
    }
}
