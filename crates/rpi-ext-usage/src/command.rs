//! `/usage` command handling (TE44 FR-A; TE53 FR-D/FR-E/FR-F).
//!
//! The command is a deterministic single-scan parser over the frozen
//! grammar (03 §5.4):
//!
//! ```text
//! /usage
//! /usage <provider-or-alias>
//! /usage all
//! /usage list
//! /usage help
//! /usage [--cached] [--json] [provider-or-alias|all]
//! /usage [--cached] [--json] --provider <registry-key>
//! ```
//!
//! `--provider` consumes the next token verbatim (it may be `--json`, a
//! subcommand name or an alias); direct-position tokens expand the four
//! command aliases. Anything outside the grammar prints help without running
//! a script.
//!
//! Every command validates the `ui.emitStructured` channel first. Reports go
//! through `usageText`/`usageReport` (never `ui.notify` directly and never a
//! JSON string inside a notification). `CommandOutcome.footer` lets the
//! caller publish the *same* fetch result — a command never triggers a
//! trailing refresh/second request.

use rpi_ext_host::usage::{
    LocalTokenUsage, UsageError, UsageErrorKind, UsageFetchResult, UsageReport,
};

use crate::footer::FooterAction;
use crate::format;
use crate::host::{self, Output};
use crate::providers;
use crate::{HostCall, config};

/// Registered command name.
pub const COMMAND_NAME: &str = "usage";

/// `registerCommand` payload.
pub fn command_definition() -> serde_json::Value {
    serde_json::json!({
        "name": COMMAND_NAME,
        "description": "Query typed provider usage: /usage [--cached] [--json] [provider|all], /usage list, /usage help",
    })
}

/// A resolved `/usage` target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The current model provider.
    Current,
    /// Every registered quota provider, failures isolated.
    All,
    /// One explicit registry key (already alias-expanded for direct
    /// positions; `--provider` keys never expand).
    Provider(String),
}

/// The deterministic parse result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedCommand {
    /// `/usage help` or an illegal form.
    Help,
    /// `/usage list`.
    List,
    /// A query/modifiers in the frozen grammar.
    Query {
        /// Resolved target.
        target: Target,
        /// `--cached` (cache only, zero scripts/network).
        cached: bool,
        /// `--json` (structured report payload).
        json: bool,
    },
}

/// One command invocation's output plus the footer action it justifies.
#[derive(Debug)]
pub struct CommandOutcome {
    /// Report to emit through `ui.emitStructured`.
    pub output: Output,
    /// Footer action for this exact result (no implicit refresh).
    pub footer: FooterAction,
}

/// Single-scan parser for the frozen command grammar.
pub fn parse(args: &str) -> ParsedCommand {
    let mut tokens = args.split_whitespace();
    let mut cached = false;
    let mut json = false;
    let mut provider: Option<String> = None;
    let mut positional: Option<String> = None;
    let mut illegal = false;
    while let Some(token) = tokens.next() {
        match token {
            "--cached" => {
                if cached {
                    illegal = true;
                }
                cached = true;
            }
            "--json" => {
                if json {
                    illegal = true;
                }
                json = true;
            }
            "--provider" => {
                if provider.is_some() {
                    illegal = true;
                }
                match tokens.next() {
                    Some(key) => provider = Some(key.to_owned()),
                    None => illegal = true,
                }
            }
            _ if positional.is_none() => positional = Some(token.to_owned()),
            _ => illegal = true,
        }
    }
    if illegal || (provider.is_some() && positional.is_some()) {
        return ParsedCommand::Help;
    }
    if let Some(provider) = provider {
        return ParsedCommand::Query {
            target: Target::Provider(provider),
            cached,
            json,
        };
    }
    match positional.as_deref() {
        None => ParsedCommand::Query {
            target: Target::Current,
            cached,
            json,
        },
        Some("help") if !cached && !json => ParsedCommand::Help,
        Some("list") if !cached && !json => ParsedCommand::List,
        Some("all") => ParsedCommand::Query {
            target: Target::All,
            cached,
            json,
        },
        // `help`/`list` with modifiers are outside the frozen grammar; the
        // registry-key escape hatch is `--provider help` / `--provider list`.
        Some("help") | Some("list") => ParsedCommand::Help,
        Some(token) => ParsedCommand::Query {
            target: Target::Provider(providers::expand_command_alias(token).to_owned()),
            cached,
            json,
        },
    }
}

/// Run one `/usage` invocation. `None` means the current mode has no output
/// channel (`unsupported-output`), so nothing was fetched or printed.
pub fn handle_now(
    host: &dyn HostCall,
    config: &config::UsageConfig,
    args: &str,
    now_ms: i64,
) -> Option<CommandOutcome> {
    if let Err(error) = host::validate_structured(host) {
        tracing::debug!(%error, "rpi-usage: structured output channel unavailable");
        return None;
    }
    let parsed = parse(args);
    if !config.enabled {
        return Some(if parsed == ParsedCommand::Help {
            text_outcome(help_text(), FooterAction::None)
        } else {
            text_outcome(
                "rpi-usage is disabled (settings usage.enabled=false).".to_owned(),
                FooterAction::Clear,
            )
        });
    }
    match parsed {
        ParsedCommand::Help => Some(text_outcome(help_text(), FooterAction::None)),
        ParsedCommand::List => Some(text_outcome(list_text(host), FooterAction::None)),
        ParsedCommand::Query {
            target,
            cached,
            json,
        } => Some(run_query(host, config, target, cached, json, now_ms)),
    }
}

fn run_query(
    host: &dyn HostCall,
    config: &config::UsageConfig,
    target: Target,
    cached: bool,
    json: bool,
    now_ms: i64,
) -> CommandOutcome {
    let known = host::usage_list_providers(host);
    let model_provider = host::current_model(host)
        .as_ref()
        .and_then(host::model_provider)
        .unwrap_or_default()
        .to_owned();
    let current_provider = providers::current_provider(&model_provider, &known);

    match target {
        Target::Current => {
            if let Some(provider) = current_provider {
                return query_one(host, config, &provider, cached, json, true, now_ms);
            }
            if cached {
                // `--cached` never substitutes local session tokens for a
                // quota cache (ADR-0039 decision 5).
                let provider = display_provider(&model_provider);
                let report = UsageReport::from_fetch(
                    &provider,
                    &UsageFetchResult::failure(UsageError::new(UsageErrorKind::CacheMiss)),
                    true,
                );
                return report_outcome(&report, json, config, now_ms);
            }
            local_report(host, &model_provider, json, config)
        }
        Target::Provider(provider) => {
            if !known.iter().any(|entry| entry == &provider) {
                let report = UsageReport::from_fetch(
                    &provider,
                    &UsageFetchResult::failure(UsageError::new(UsageErrorKind::NoData)),
                    false,
                );
                return report_outcome(&report, json, config, now_ms);
            }
            let current = current_provider.as_deref() == Some(provider.as_str());
            query_one(host, config, &provider, cached, json, current, now_ms)
        }
        Target::All => {
            if known.is_empty() {
                return if json {
                    CommandOutcome {
                        output: Output::Json(serde_json::Value::Array(Vec::new())),
                        footer: FooterAction::None,
                    }
                } else {
                    text_outcome(
                        "No usage providers are registered.".to_owned(),
                        FooterAction::None,
                    )
                };
            }
            let mut reports = Vec::with_capacity(known.len());
            for provider in &known {
                let result = fetch(host, provider, cached);
                let current = current_provider.as_deref() == Some(provider.as_str());
                reports.push(UsageReport::from_fetch(provider, &result, current));
            }
            if json {
                let data =
                    serde_json::Value::Array(reports.iter().map(UsageReport::to_json).collect());
                CommandOutcome {
                    output: Output::Json(data),
                    footer: FooterAction::None,
                }
            } else {
                let text = reports
                    .iter()
                    .map(|report| {
                        format::report_text(report, report.current.unwrap_or(false), config, now_ms)
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                text_outcome(text, FooterAction::None)
            }
        }
    }
}

fn query_one(
    host: &dyn HostCall,
    config: &config::UsageConfig,
    provider: &str,
    cached: bool,
    json: bool,
    current: bool,
    now_ms: i64,
) -> CommandOutcome {
    let result = fetch(host, provider, cached);
    let report = UsageReport::from_fetch(provider, &result, current);
    let mut outcome = report_outcome(&report, json, config, now_ms);
    if !cached {
        // The same result drives the footer: a success or stale fallback is
        // published; a failure without a same-identity stale envelope clears.
        outcome.footer = if result.envelope.is_some() {
            FooterAction::Publish(Box::new(result))
        } else {
            FooterAction::Clear
        };
    }
    outcome
}

fn fetch(host: &dyn HostCall, provider: &str, cached: bool) -> UsageFetchResult {
    if cached {
        host::usage_cached(host, provider)
    } else {
        host::usage_fetch_detailed(host, provider, true)
    }
}

/// Local token-only fallback (ADR-0039 decision 5): current provider has no
/// registered quota provider, so read `ctx.sessionStats` and show no plan,
/// window, ledger, balance, quota, reset or cost field. No script or network
/// runs, and the footer is untouched.
fn local_report(
    host: &dyn HostCall,
    model_provider: &str,
    json: bool,
    config: &config::UsageConfig,
) -> CommandOutcome {
    let provider = display_provider(model_provider);
    let Some(stats) = host::session_stats(host) else {
        let report = UsageReport::from_fetch(
            &provider,
            &UsageFetchResult::failure(UsageError::new(UsageErrorKind::NoData)),
            true,
        );
        return report_outcome(&report, json, config, 0);
    };
    let tokens = LocalTokenUsage::from_session_stats(&stats);
    let report = UsageReport::local(&provider, tokens);
    if json {
        CommandOutcome {
            output: Output::Json(report.to_json()),
            footer: FooterAction::None,
        }
    } else {
        text_outcome(format::local_text(&tokens), FooterAction::None)
    }
}

fn display_provider(model_provider: &str) -> String {
    let provider = model_provider.trim();
    if provider.is_empty() {
        "unknown".to_owned()
    } else {
        provider.to_owned()
    }
}

fn report_outcome(
    report: &UsageReport,
    json: bool,
    config: &config::UsageConfig,
    now_ms: i64,
) -> CommandOutcome {
    let current = report.current.unwrap_or(false);
    if json {
        CommandOutcome {
            output: Output::Json(report.to_json()),
            footer: FooterAction::None,
        }
    } else {
        text_outcome(
            format::report_text(report, current, config, now_ms),
            FooterAction::None,
        )
    }
}

fn text_outcome(text: String, footer: FooterAction) -> CommandOutcome {
    CommandOutcome {
        output: Output::Text(text),
        footer,
    }
}

/// `/usage help` text (syntax, aliases, cache, JSON rules).
pub fn help_text() -> String {
    [
        "usage: /usage [--cached] [--json] [provider|all]".to_owned(),
        "       /usage [--cached] [--json] --provider <registry-key>".to_owned(),
        "       /usage list".to_owned(),
        "       /usage help".to_owned(),
        String::new(),
        "Aliases: claude->anthropic, gpt/codex->openai-codex, grok->xai".to_owned(),
        "--cached: read the current account's cache only (no script, no request)".to_owned(),
        "--json: emit the structured usage report instead of text".to_owned(),
    ]
    .join("\n")
}

/// `/usage list`: current, registered and aliases (never subscription state).
fn list_text(host: &dyn HostCall) -> String {
    let known = host::usage_list_providers(host);
    let model_provider = host::current_model(host)
        .as_ref()
        .and_then(host::model_provider)
        .unwrap_or_default()
        .to_owned();
    let current = providers::current_provider(&model_provider, &known)
        .or_else(|| {
            // Show the model provider when it has no registered script so
            // the user can see why `/usage` falls back to local tokens.
            (!model_provider.trim().is_empty()).then(|| model_provider.clone())
        })
        .unwrap_or_else(|| "(none)".to_owned());
    let registered = if known.is_empty() {
        "(none)".to_owned()
    } else {
        known.join(", ")
    };
    let aliases = providers::COMMAND_ALIASES
        .iter()
        .map(|(alias, provider)| format!("{alias}->{provider}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Current: {current}\nRegistered: {registered}\nAliases: {aliases}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footer::{FooterState, apply};
    use crate::test_host::UsageFakeHost;
    use serde_json::json;

    fn query(target: Target, cached: bool, json: bool) -> ParsedCommand {
        ParsedCommand::Query {
            target,
            cached,
            json,
        }
    }

    #[test]
    fn parser_accepts_the_frozen_grammar() {
        assert_eq!(parse(""), query(Target::Current, false, false));
        assert_eq!(
            parse(" gpt "),
            query(Target::Provider("openai-codex".to_owned()), false, false)
        );
        assert_eq!(parse("--json --cached all"), query(Target::All, true, true));
        assert_eq!(
            parse("--cached deepseek --json"),
            query(Target::Provider("deepseek".to_owned()), true, true)
        );
        assert_eq!(
            parse("--provider xai"),
            query(Target::Provider("xai".to_owned()), false, false)
        );
        // The next token after --provider is raw, even a flag/subcommand.
        assert_eq!(
            parse("--cached --provider --json"),
            query(Target::Provider("--json".to_owned()), true, false)
        );
        assert_eq!(
            parse("--provider list"),
            query(Target::Provider("list".to_owned()), false, false)
        );
        assert_eq!(parse("list"), ParsedCommand::List);
        assert_eq!(parse("help"), ParsedCommand::Help);
        assert_eq!(parse(""), query(Target::Current, false, false));
        for illegal in [
            "--cached --cached",
            "--json --json",
            "--provider",
            "--provider a b",
            "a b",
            "list --json",
            "help --cached",
        ] {
            assert_eq!(parse(illegal), ParsedCommand::Help, "{illegal}");
        }
    }

    fn host_with(provider: &str) -> UsageFakeHost {
        let host = UsageFakeHost::new();
        host.set_model(json!({"id": "m", "provider": provider}));
        host
    }

    fn run(host: &UsageFakeHost, args: &str) -> CommandOutcome {
        handle_now(host, &config::UsageConfig::default(), args, 0).expect("output channel")
    }

    fn text(outcome: &CommandOutcome) -> String {
        match &outcome.output {
            Output::Text(text) => text.clone(),
            Output::Json(value) => panic!("expected text, got {value}"),
        }
    }

    fn json(outcome: &CommandOutcome) -> serde_json::Value {
        match &outcome.output {
            Output::Json(value) => value.clone(),
            Output::Text(text) => panic!("expected json, got {text}"),
        }
    }

    fn window(id: &str, label: &str, percent: f64, reset: &str) -> serde_json::Value {
        json!({
            "id": id,
            "label": label,
            "status": "ok",
            "usedPercent": percent,
            "remainingPercent": 100.0 - percent,
            "resetAt": reset,
        })
    }

    #[test]
    fn current_force_query_emits_a_report_and_publishes_the_same_result() {
        let host = host_with("zai");
        host.set_providers(vec!["glm-coding-plan".to_owned()]);
        host.set_envelope(
            "glm-coding-plan",
            json!({
                "schemaVersion": 1,
                "provider": "glm-coding-plan",
                "plan": "max",
                "displayText": "5h 1%",
                "windows": [window("5h", "5h", 1.0, "2027-01-15T08:48:00Z")]
            }),
        );
        let outcome = run(&host, "");
        assert_eq!(
            host.fetch_calls(),
            vec![("glm-coding-plan".to_owned(), true)],
            "one forced fetch"
        );
        assert!(text(&outcome).starts_with("glm-coding-plan (current)\nplan: max\n"));
        assert!(text(&outcome).contains("Window"));
        assert!(matches!(&outcome.footer, FooterAction::Publish(result)
            if result.envelope.as_ref().is_some_and(|envelope| envelope.provider == "glm-coding-plan")));
        // Applying the outcome writes the footer without another fetch.
        let mut state = FooterState::default();
        apply(
            &host,
            &config::UsageConfig::default(),
            &mut state,
            outcome.footer,
            1_800_000_000_000,
        );
        assert_eq!(
            host.statuses().last(),
            Some(&("rpi-usage".to_owned(), Some("5h 1% (~48m)".to_owned())))
        );
        assert_eq!(host.fetch_count("glm-coding-plan"), 1, "no second request");
    }

    #[test]
    fn json_query_reports_the_typed_object_and_all_reports_an_array() {
        let host = host_with("deepseek");
        host.set_providers(vec!["deepseek".to_owned(), "xai".to_owned()]);
        host.set_envelope(
            "deepseek",
            json!({
                "schemaVersion": 1,
                "provider": "deepseek",
                "displayText": "CNY 1.00",
                "balance": [{"currency": "CNY", "total": 1.0}]
            }),
        );
        host.set_envelope(
            "xai",
            json!({
                "schemaVersion": 1,
                "provider": "xai",
                "displayText": "7d 42.5%",
                "windows": [window("included_allowance", "7d", 42.5, "2027-01-19T06:00:00Z")]
            }),
        );
        let outcome = run(&host, "--json");
        let report = json(&outcome);
        assert_eq!(report["schemaVersion"], json!(1));
        assert_eq!(report["provider"], json!("deepseek"));
        assert_eq!(report["stale"], json!(false));
        assert_eq!(report["current"], json!(true));
        assert_eq!(report["envelope"]["balance"][0]["currency"], json!("CNY"));
        assert_eq!(host.fetch_calls(), vec![("deepseek".to_owned(), true)]);
        // `all` force queries each registered provider and keeps an array.
        let outcome = run(&host, "all --json");
        let reports = json(&outcome);
        let array = reports.as_array().expect("array");
        assert_eq!(array.len(), 2);
        assert_eq!(array[0]["provider"], json!("deepseek"));
        assert_eq!(array[1]["provider"], json!("xai"));
        assert_eq!(
            host.fetch_calls(),
            vec![
                ("deepseek".to_owned(), true),
                ("deepseek".to_owned(), true),
                ("xai".to_owned(), true),
            ]
        );
        assert!(matches!(outcome.footer, FooterAction::None));
    }

    #[test]
    fn cached_help_list_and_illegal_paths_run_no_scripts() {
        let host = host_with("deepseek");
        host.set_providers(vec!["deepseek".to_owned()]);
        host.set_envelope(
            "deepseek",
            json!({"schemaVersion": 1, "provider": "deepseek", "displayText": "cached"}),
        );
        let outcome = run(&host, "--cached");
        assert!(text(&outcome).contains("Source: cache (no request)"));
        assert!(matches!(outcome.footer, FooterAction::None));
        assert!(
            !host.called("ctx.usage.fetchDetailed"),
            "cached uses ctx.usage.cached"
        );
        assert!(host.called("ctx.usage.cached"));

        for (args, needle) in [
            ("help", "usage: /usage"),
            ("list", "Registered: deepseek"),
            ("--cached --cached", "usage: /usage"),
            ("a b", "usage: /usage"),
        ] {
            let outcome = run(&host, args);
            assert!(
                text(&outcome).contains(needle),
                "{args}: {}",
                text(&outcome)
            );
            assert!(matches!(outcome.footer, FooterAction::None), "{args}");
        }
        assert_eq!(
            host.fetch_calls(),
            Vec::<(String, bool)>::new(),
            "no forced fetch"
        );
    }

    #[test]
    fn all_json_without_providers_is_an_empty_array() {
        let host = host_with("deepseek");
        let outcome = run(&host, "all --json");
        assert_eq!(json(&outcome), serde_json::Value::Array(Vec::new()));
        assert!(matches!(outcome.footer, FooterAction::None));
        let outcome = run(&host, "all");
        assert_eq!(text(&outcome), "No usage providers are registered.");
    }

    #[test]
    fn explicit_unknown_provider_is_no_data_and_never_local() {
        let host = host_with("dgx-spark");
        host.set_providers(vec!["deepseek".to_owned()]);
        host.set_session_stats(json!({
            "sessionId": "s", "startedAt": "2020-01-01T00:00:00Z", "entryCount": 1,
            "tokens": {"input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4},
            "cost": 0.0
        }));
        let outcome = run(&host, "nope");
        assert!(text(&outcome).contains("Status: no-data"));
        assert!(!text(&outcome).contains("Local session tokens"));
        assert!(!host.called("ctx.sessionStats"));
        assert!(matches!(outcome.footer, FooterAction::None));
        // Alias expansion reaches the registered provider.
        host.set_envelope(
            "deepseek",
            json!({"schemaVersion": 1, "provider": "deepseek", "displayText": "CNY 1"}),
        );
        let outcome = run(&host, "--provider deepseek");
        assert!(text(&outcome).starts_with("deepseek"));
        assert_eq!(host.fetch_calls(), vec![("deepseek".to_owned(), true)]);
    }

    #[test]
    fn current_provider_without_a_quota_script_falls_back_to_local_tokens() {
        let host = host_with("openrouter");
        host.set_providers(vec!["deepseek".to_owned()]);
        host.set_session_stats(json!({
            "sessionId": "s", "startedAt": "2020-01-01T00:00:00Z", "entryCount": 1,
            "tokens": {"input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40},
            "cost": 0.0123
        }));
        let outcome = run(&host, "");
        assert_eq!(
            text(&outcome),
            "Local session tokens\ninput: 10\noutput: 20\ncacheRead: 30\ncacheWrite: 40\n\
             used: 70 (input + cacheWrite + output)"
        );
        assert!(matches!(outcome.footer, FooterAction::None));
        assert!(
            !host.called("ctx.usage.fetchDetailed"),
            "zero scripts/network"
        );
        assert!(!host.called("ctx.usage.cached"));
        // JSON keeps schemaVersion/provider/stale + source/localTokens, no
        // envelope/error/quota fields.
        let report = json(&run(&host, "--json"));
        assert_eq!(report["schemaVersion"], json!(1));
        assert_eq!(report["provider"], json!("openrouter"));
        assert_eq!(report["stale"], json!(false));
        assert_eq!(report["source"], json!("local"));
        assert_eq!(report["localTokens"]["used"], json!(70));
        for absent in [
            "envelope", "error", "plan", "windows", "ledgers", "balance", "quota",
        ] {
            assert!(report.get(absent).is_none(), "{absent}");
        }
        assert!(!host.called("ctx.usage.fetchDetailed"));
        assert!(!host.called("ctx.usage.cached"));
        // Cached never uses the local fallback.
        // With no registered provider there is nothing to read from cache;
        // the synthesized cache-miss must not query a model-provider id.
        let cached = run(&host, "--cached");
        assert!(text(&cached).contains("Status: cache-miss"));
        assert!(!host.called("ctx.usage.cached"));
        assert!(!host.called("ctx.usage.fetchDetailed"));
    }

    #[test]
    fn supported_provider_errors_are_not_masked_by_local_tokens() {
        let host = host_with("deepseek");
        host.set_providers(vec!["deepseek".to_owned()]);
        host.fail_fetch("deepseek");
        host.set_failure(
            "deepseek",
            json!({"stale": false, "error": {"kind": "transport", "action": "Check the network and provider status, then retry."}}),
        );
        host.set_session_stats(json!({
            "sessionId": "s", "startedAt": "2020-01-01T00:00:00Z", "entryCount": 1,
            "tokens": {"input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4},
            "cost": 0.0
        }));
        let outcome = run(&host, "");
        assert!(text(&outcome).contains("Status: transport"));
        assert!(!text(&outcome).contains("Local session tokens"));
        assert!(!host.called("ctx.sessionStats"));
        assert!(matches!(outcome.footer, FooterAction::Clear));
    }

    #[test]
    fn disabled_and_unsupported_output_paths_fetch_nothing() {
        let host = host_with("deepseek");
        host.set_providers(vec!["deepseek".to_owned()]);
        let disabled = config::UsageConfig {
            enabled: false,
            ..config::UsageConfig::default()
        };
        let outcome = handle_now(&host, &disabled, "", 0).expect("channel");
        assert!(text(&outcome).contains("disabled"));
        assert!(matches!(outcome.footer, FooterAction::Clear));
        let help = handle_now(&host, &disabled, "help", 0).expect("channel");
        assert!(text(&help).contains("usage: /usage"));
        assert!(matches!(help.footer, FooterAction::None));
        assert!(!host.called("ctx.usage.fetchDetailed"));
        // NullUi: unsupported-output before anything else.
        host.set_structured_unsupported();
        assert!(handle_now(&host, &config::UsageConfig::default(), "", 0).is_none());
        assert!(!host.called("ctx.usage.fetchDetailed"));
    }
}
