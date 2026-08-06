//! Port of the `update` branch of `packages/coding-agent/src/
//! package-manager-cli.ts` (`parsePackageCommand` + `refreshModelCatalogs`)
//! @ pi 0.82.1 (2efa728) — W6-C lands the `update --models` target (remote
//! model catalog refresh); the install/remove/list/config commands and the
//! self/extensions update targets remain T14 placeholders (app.rs dispatch).
//!
//! Intentional differences (D-037):
//! - Only the `update` command is parsed here; the other package commands
//!   still hit the T14 placeholder in `app.rs`.
//! - `--force` is accepted but inert for the models target (the refresh
//!   always runs with `force: true`, upstream `refreshModelCatalogs`).
//! - `--approve`/`--no-approve` are accepted but unused (no project-local
//!   config is written by the models target).

use std::time::Duration;

use pir_ai::models::ModelsRefreshOptions;
use tokio_util::sync::CancellationToken;

use crate::config::APP_NAME;
use crate::core::model_runtime::{
    CreateModelRuntimeOptions, ModelRuntime, ModelsPathInput, DEFAULT_MODEL_REFRESH_TIMEOUT_MS,
};

/// `getPackageCommandUsage("update")` (package-manager-cli.ts:86).
pub const UPDATE_USAGE: &str =
    "pir update [source|self|pi] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]";

/// `UpdateTarget` (package-manager-cli.ts:35).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateTarget {
    All,
    Self_,
    Extensions { source: Option<String> },
    Models,
}

/// Parsed `update` command (the `PackageCommandOptions` subset the update
/// branch produces, package-manager-cli.ts:189-387).
#[derive(Debug, Default)]
pub struct ParsedUpdate {
    pub help: bool,
    pub invalid_option: Option<String>,
    pub invalid_argument: Option<String>,
    pub missing_option_value: Option<String>,
    pub conflicting_options: Option<String>,
    pub target: Option<UpdateTarget>,
}

/// `parsePackageCommand` restricted to `update` (package-manager-cli.ts:
/// 216-371). Non-update package commands are not handled here.
pub fn parse_update_args(args: &[String]) -> ParsedUpdate {
    let rest = &args[1..];
    let mut parsed = ParsedUpdate::default();
    let mut source: Option<String> = None;
    let mut self_flag = false;
    let mut extensions_flag = false;
    let mut models_flag = false;
    let mut all_flag = false;
    let mut extension_flag_source: Option<String> = None;

    let mut index = 0;
    while index < rest.len() {
        let arg = &rest[index];
        if arg == "-h" || arg == "--help" {
            parsed.help = true;
        } else if arg == "-l" || arg == "--local" {
            // Valid for install/remove only (package-manager-cli.ts:223-230).
            parsed.invalid_option.get_or_insert_with(|| arg.clone());
        } else if arg == "--self" {
            self_flag = true;
        } else if arg == "--extensions" {
            extensions_flag = true;
        } else if arg == "--models" {
            models_flag = true;
        } else if arg == "--all" {
            all_flag = true;
        } else if arg == "--approve" || arg == "-a" || arg == "--no-approve" || arg == "-na" {
            // `projectTrustOverride` — unused by the models target.
        } else if arg == "--force" {
            // `force` — the models refresh always forces (see module docs).
        } else if arg == "--extension" {
            let value = rest.get(index + 1);
            match value {
                Some(value) if !value.starts_with('-') => {
                    if extension_flag_source.is_some() {
                        parsed.conflicting_options.get_or_insert_with(|| {
                            "--extension can only be provided once".to_owned()
                        });
                    } else {
                        extension_flag_source = Some(value.clone());
                    }
                    index += 1;
                }
                _ => {
                    parsed
                        .missing_option_value
                        .get_or_insert_with(|| arg.clone());
                }
            }
        } else if arg.starts_with('-') {
            parsed.invalid_option.get_or_insert_with(|| arg.clone());
        } else if source.is_none() {
            source = Some(arg.clone());
        } else {
            parsed.invalid_argument.get_or_insert_with(|| arg.clone());
        }
        index += 1;
    }

    // Update target resolution + conflicts (package-manager-cli.ts:320-370).
    if models_flag {
        if self_flag || extensions_flag || all_flag || extension_flag_source.is_some() {
            parsed.conflicting_options.get_or_insert_with(|| {
                "--models cannot be combined with --self, --extensions, --all, or --extension"
                    .to_owned()
            });
        }
        if source.is_some() {
            parsed.conflicting_options.get_or_insert_with(|| {
                "--models cannot be combined with a positional source".to_owned()
            });
        }
        parsed.target = Some(UpdateTarget::Models);
    } else if let Some(extension_source) = extension_flag_source {
        if self_flag || extensions_flag || all_flag {
            parsed.conflicting_options.get_or_insert_with(|| {
                "--extension cannot be combined with --self, --extensions, or --all".to_owned()
            });
        }
        if source.is_some() {
            parsed.conflicting_options.get_or_insert_with(|| {
                "--extension cannot be combined with a positional source".to_owned()
            });
        }
        parsed.target = Some(UpdateTarget::Extensions {
            source: Some(extension_source),
        });
    } else if let Some(positional) = source {
        if positional == "self" || positional == "pi" {
            parsed.target = Some(if extensions_flag {
                UpdateTarget::All
            } else {
                UpdateTarget::Self_
            });
        } else {
            if extensions_flag || self_flag || all_flag {
                parsed.conflicting_options.get_or_insert_with(|| {
                    "positional update targets cannot be combined with --self, --extensions, or --all"
                        .to_owned()
                });
            }
            parsed.target = Some(UpdateTarget::Extensions {
                source: Some(positional),
            });
        }
    } else if all_flag || (self_flag && extensions_flag) {
        parsed.target = Some(UpdateTarget::All);
    } else if self_flag {
        parsed.target = Some(UpdateTarget::Self_);
    } else if extensions_flag {
        parsed.target = Some(UpdateTarget::Extensions { source: None });
    } else {
        parsed.target = Some(UpdateTarget::Self_);
    }
    parsed
}

/// `printPackageCommandHelp("update")` (package-manager-cli.ts:150-173),
/// plain text (headless stdout; upstream chalk bold is TTY-only).
pub fn update_help() -> String {
    format!(
        r#"Usage:
  {UPDATE_USAGE}

Update pi, installed packages, or model catalogs.

Options:
  --self                  Update pi only (default when no target is given)
  --extensions            Update installed packages only
  --models                Refresh model catalogs only
  --all                   Update pi and installed packages
  --extension <source>    Update one package only
  -a, --approve           Trust project-local files for this command
  -na, --no-approve       Ignore project-local files for this command
  --force                 Reinstall pi even if the current version is latest

Short forms:
  {APP_NAME} update                Update pi only
  {APP_NAME} update --all          Update pi and all extensions
  {APP_NAME} update --models       Refresh model catalogs only
  {APP_NAME} update <source>       Update one package
  {APP_NAME} update pi             Update pi only (self works as alias to pi)
"#
    )
}

/// `refreshModelCatalogs` (package-manager-cli.ts:397-421): create the
/// runtime offline (cache restore only), then force-refresh model catalogs
/// over the network with a 15s timeout.
async fn refresh_model_catalogs() -> Result<(), String> {
    let agent_dir = crate::config::get_agent_dir();
    let runtime = ModelRuntime::create(CreateModelRuntimeOptions {
        auth_path: Some(agent_dir.join("auth.json")),
        models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
        allow_model_network: false,
        ..Default::default()
    })
    .await;
    let token = CancellationToken::new();
    let abort = token.clone();
    let timeout = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(DEFAULT_MODEL_REFRESH_TIMEOUT_MS)).await;
        abort.cancel();
    });
    let result = runtime
        .refresh(Some(ModelsRefreshOptions {
            allow_network: Some(true),
            force: Some(true),
            signal: Some(token),
        }))
        .await;
    timeout.abort();
    if result.aborted {
        return Err("Model catalog refresh timed out.".to_owned());
    }
    if !result.errors.is_empty() {
        let details: Vec<String> = result
            .errors
            .iter()
            .map(|(provider, message)| format!("{provider}: {message}"))
            .collect();
        return Err(format!(
            "Could not refresh model catalogs: {}",
            details.join("; ")
        ));
    }
    Ok(())
}

/// `handlePackageCommand` for `pir update` (package-manager-cli.ts:702-735).
/// Returns the process exit code. `args[0]` must be `"update"`.
pub async fn run_update(args: &[String]) -> i32 {
    let parsed = parse_update_args(args);
    if parsed.help {
        print!("{}", update_help());
        return 0;
    }
    if let Some(option) = &parsed.invalid_option {
        eprintln!("Unknown option {option} for \"update\".");
        eprintln!("Use \"{APP_NAME} --help\" or \"{UPDATE_USAGE}\".");
        return 1;
    }
    if let Some(option) = &parsed.missing_option_value {
        eprintln!("Missing value for {option}.");
        eprintln!("Usage: {UPDATE_USAGE}");
        return 1;
    }
    if let Some(argument) = &parsed.invalid_argument {
        eprintln!("Unexpected argument {argument}.");
        eprintln!("Usage: {UPDATE_USAGE}");
        return 1;
    }
    if let Some(conflict) = &parsed.conflicting_options {
        eprintln!("{conflict}");
        eprintln!("Usage: {UPDATE_USAGE}");
        return 1;
    }
    match parsed.target {
        Some(UpdateTarget::Models) => match refresh_model_catalogs().await {
            Ok(()) => {
                println!("Model catalogs refreshed");
                0
            }
            Err(message) => {
                eprintln!("Error: {message}");
                1
            }
        },
        // Self/extensions/all targets land with package management (T14).
        _ => {
            eprintln!("Error: pir update is not available yet (T14)");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of the update-command intent of
    //! `packages/coding-agent/test/package-manager.test.ts` arg validation.

    use super::*;

    fn parse(input: &[&str]) -> ParsedUpdate {
        parse_update_args(&input.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn target(input: &[&str]) -> Option<UpdateTarget> {
        parse(input).target
    }

    #[test]
    fn update_models_target_parses() {
        assert_eq!(target(&["update", "--models"]), Some(UpdateTarget::Models));
    }

    #[test]
    fn update_models_rejects_combined_targets() {
        let parsed = parse(&["update", "--models", "--all"]);
        assert_eq!(parsed.target, Some(UpdateTarget::Models));
        assert_eq!(
            parsed.conflicting_options.as_deref(),
            Some("--models cannot be combined with --self, --extensions, --all, or --extension")
        );
        let parsed = parse(&["update", "--models", "my-package"]);
        assert_eq!(
            parsed.conflicting_options.as_deref(),
            Some("--models cannot be combined with a positional source")
        );
        let parsed = parse(&["update", "--models", "--extension", "foo"]);
        assert_eq!(
            parsed.conflicting_options.as_deref(),
            Some("--models cannot be combined with --self, --extensions, --all, or --extension")
        );
    }

    #[test]
    fn update_defaults_to_self() {
        assert_eq!(target(&["update"]), Some(UpdateTarget::Self_));
        assert_eq!(target(&["update", "pi"]), Some(UpdateTarget::Self_));
        assert_eq!(target(&["update", "self"]), Some(UpdateTarget::Self_));
    }

    #[test]
    fn update_flag_combinations() {
        assert_eq!(
            target(&["update", "--self", "--extensions"]),
            Some(UpdateTarget::All)
        );
        assert_eq!(target(&["update", "--all"]), Some(UpdateTarget::All));
        assert_eq!(
            target(&["update", "--extensions"]),
            Some(UpdateTarget::Extensions { source: None })
        );
        assert_eq!(
            target(&["update", "--extension", "npm:@foo/bar"]),
            Some(UpdateTarget::Extensions {
                source: Some("npm:@foo/bar".to_owned())
            })
        );
        assert_eq!(
            target(&["update", "my-package"]),
            Some(UpdateTarget::Extensions {
                source: Some("my-package".to_owned())
            })
        );
    }

    #[test]
    fn update_reports_invalid_options_and_arguments() {
        // `-l` is install/remove-only (package-manager-cli.ts:223-230).
        let parsed = parse(&["update", "-l"]);
        assert_eq!(parsed.invalid_option.as_deref(), Some("-l"));
        // Unknown flag.
        let parsed = parse(&["update", "--bogus"]);
        assert_eq!(parsed.invalid_option.as_deref(), Some("--bogus"));
        // Second positional is an invalid argument.
        let parsed = parse(&["update", "a", "b"]);
        assert_eq!(parsed.invalid_argument.as_deref(), Some("b"));
        // --extension without a value.
        let parsed = parse(&["update", "--extension"]);
        assert_eq!(parsed.missing_option_value.as_deref(), Some("--extension"));
        let parsed = parse(&["update", "--extension", "--models"]);
        assert_eq!(parsed.missing_option_value.as_deref(), Some("--extension"));
    }

    #[test]
    fn update_accepts_help_and_trust_flags() {
        assert!(parse(&["update", "--help"]).help);
        assert!(parse(&["update", "-h"]).help);
        // --approve/--no-approve/--force are accepted on update.
        let parsed = parse(&["update", "--models", "--approve", "--force"]);
        assert_eq!(parsed.target, Some(UpdateTarget::Models));
        assert!(parsed.conflicting_options.is_none());
    }

    #[test]
    fn update_help_text_mentions_models() {
        let help = update_help();
        assert!(help.contains("--models                Refresh model catalogs only"));
        assert!(help.contains("pir update --models       Refresh model catalogs only"));
    }
}
