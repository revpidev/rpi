//! Cwd-bound runtime services for one effective session cwd.
//!
//! Port of `packages/coding-agent/src/core/agent-session-services.ts` @ pi
//! 0.82.1 (2efa728).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::cli::args::UnknownFlagValue;
use crate::cli::diagnostics::DiagnosticLevel;
use crate::config::get_agent_dir;
use crate::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime, ModelsPathInput};
use crate::core::resource_loader::{DefaultResourceLoader, DefaultResourceLoaderOptions};
use crate::core::settings_manager::SettingsManager;
use crate::error::PirError;
use crate::tools::path_utils::resolve_path;

/// `AgentSessionRuntimeDiagnostic` (agent-session-services.ts:25-28).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionRuntimeDiagnostic {
    /// `"info" | "warning" | "error"`.
    pub level: DiagnosticLevel,
    pub message: String,
}

/// Customizer hook for loader options (CLI resource flags).
pub type ResourceLoaderOptionsCustomizer =
    Box<dyn FnOnce(&mut DefaultResourceLoaderOptions) + Send>;

/// `AgentSessionServices` (agent-session-services.ts:72-79). The settings
/// manager is owned by the resource loader (T09 ownership model); access it
/// through the loader's `settings_manager()`/`settings_manager_mut()`.
#[derive(Clone)]
pub struct AgentSessionServices {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub model_runtime: Arc<ModelRuntime>,
    pub resource_loader: Arc<Mutex<DefaultResourceLoader>>,
    pub diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
}

/// `CreateAgentSessionServicesOptions` (agent-session-services.ts:37-45).
pub struct CreateAgentSessionServicesOptions {
    pub cwd: PathBuf,
    pub agent_dir: Option<PathBuf>,
    pub settings_manager: Option<SettingsManager>,
    pub model_runtime: Option<Arc<ModelRuntime>>,
    pub extension_flag_values: Vec<(String, UnknownFlagValue)>,
    /// Extra loader options (CLI resource flags). `cwd` / `agent_dir` /
    /// `settings_manager` are always overridden by the service inputs, like
    /// upstream (agent-session-services.ts:146-151).
    pub resource_loader_options: Option<ResourceLoaderOptionsCustomizer>,
}

/// `applyExtensionFlagValues` (agent-session-services.ts:81-127) — called by
/// the caller AFTER the extension host's final load, so only the loaded
/// extensions' registered flags decide "unknown". Boolean flags set `true`
/// regardless of the given value; string flags require a value.
pub fn apply_extension_flag_values(
    host: &pir_ext_host::host::NativeExtensionHost,
    extension_flag_values: &[(String, UnknownFlagValue)],
) -> Vec<AgentSessionRuntimeDiagnostic> {
    let mut diagnostics = Vec::new();
    if extension_flag_values.is_empty() {
        return diagnostics;
    }
    let registered_flags = host.get_flags();
    let mut unknown: Vec<&String> = Vec::new();
    for (name, value) in extension_flag_values {
        match registered_flags.get(name) {
            None => unknown.push(name),
            Some(flag) => match (flag.flag_type, value) {
                (pir_ext_host::types::FlagType::Boolean, _) => {
                    host.runtime()
                        .set_flag_value(name, pir_ext_host::types::FlagValue::Boolean(true));
                }
                (pir_ext_host::types::FlagType::String, UnknownFlagValue::String(value)) => {
                    host.runtime().set_flag_value(
                        name,
                        pir_ext_host::types::FlagValue::String(value.clone()),
                    );
                }
                (pir_ext_host::types::FlagType::String, UnknownFlagValue::Boolean(_)) => {
                    diagnostics.push(AgentSessionRuntimeDiagnostic {
                        level: DiagnosticLevel::Error,
                        message: format!("Extension flag \"--{name}\" requires a value"),
                    });
                }
            },
        }
    }
    if !unknown.is_empty() {
        let message = if unknown.len() == 1 {
            format!("Unknown option: --{}", unknown[0])
        } else {
            format!(
                "Unknown options: {}",
                unknown
                    .iter()
                    .map(|name| format!("--{name}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        diagnostics.push(AgentSessionRuntimeDiagnostic {
            level: DiagnosticLevel::Error,
            message,
        });
    }
    diagnostics
}

/// `createAgentSessionServices` (agent-session-services.ts:134-191).
///
/// T15 W3: `extension_flag_values` are validated and applied by the caller
/// AFTER the extension host's final load (`applyExtensionFlagValues`,
/// agent-session-services.ts:81-127, wired in app.rs): only the loaded
/// extensions' registered flags decide "unknown", so services creation no
/// longer reports them.
pub async fn create_agent_session_services(
    options: CreateAgentSessionServicesOptions,
) -> Result<AgentSessionServices, PirError> {
    let cwd = resolve_path(
        &options.cwd.to_string_lossy(),
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    );
    let agent_dir = options
        .agent_dir
        .map(|dir| {
            resolve_path(
                &dir.to_string_lossy(),
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            )
        })
        .unwrap_or_else(get_agent_dir);

    let model_runtime = match options.model_runtime {
        Some(runtime) => runtime,
        None => {
            ModelRuntime::create(CreateModelRuntimeOptions {
                credentials: None,
                auth_path: Some(agent_dir.join("auth.json")),
                models_path: ModelsPathInput::Path(agent_dir.join("models.json")),
                ..Default::default()
            })
            .await
        }
    };

    let mut loader_options = DefaultResourceLoaderOptions::new(cwd.clone(), agent_dir.clone());
    loader_options.settings_manager = options.settings_manager;
    if let Some(customize) = options.resource_loader_options {
        customize(&mut loader_options);
    }
    // Service inputs always win (agent-session-services.ts:146-151).
    loader_options.cwd = cwd.clone();
    loader_options.agent_dir = agent_dir.clone();
    let mut loader = DefaultResourceLoader::new(loader_options);
    loader.reload();

    let mut diagnostics = Vec::new();
    // Built-in (hidden) extension provider registrations drain into the
    // model runtime (agent-session-services.ts:166-178
    // `pendingNativeProviderRegistrations`): the llama.cpp extension is the
    // only built-in (extensions/index.ts `builtInExtensions`); until the T15
    // extension host lands, this is the registration seam (D-047).
    if let Err(message) = model_runtime
        .register_native_provider(crate::extensions::llama::shared_llama_provider().provider())
        .await
    {
        diagnostics.push(AgentSessionRuntimeDiagnostic {
            level: DiagnosticLevel::Error,
            message: format!("Extension \"<inline:llama.cpp>\" error: {message}"),
        });
    }
    // Offline refresh (agent-session-services.ts:180
    // `refresh({ allowNetwork: false })`): session startup must not block on
    // dynamic catalog network fetches.
    model_runtime
        .refresh(Some(pir_ai::models::ModelsRefreshOptions {
            allow_network: Some(false),
            force: None,
            signal: None,
        }))
        .await;
    // `options.extension_flag_values`: applied post-load by the caller
    // (see header); intentionally not judged here.
    let _ = &options.extension_flag_values;

    Ok(AgentSessionServices {
        cwd,
        agent_dir,
        model_runtime,
        resource_loader: Arc::new(Mutex::new(loader)),
        diagnostics,
    })
}
