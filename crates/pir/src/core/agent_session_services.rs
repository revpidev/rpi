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

/// `applyExtensionFlagValues` (agent-session-services.ts:81-127). With zero
/// registered extension flags (no extension host yet) every collected flag
/// is unknown — matching upstream with no extensions loaded.
fn apply_extension_flag_values(
    extension_flag_values: &[(String, UnknownFlagValue)],
) -> Vec<AgentSessionRuntimeDiagnostic> {
    if extension_flag_values.is_empty() {
        return Vec::new();
    }
    let unknown_flags: Vec<&String> = extension_flag_values.iter().map(|(name, _)| name).collect();
    let message = if unknown_flags.len() == 1 {
        format!("Unknown option: --{}", unknown_flags[0])
    } else {
        format!(
            "Unknown options: {}",
            unknown_flags
                .iter()
                .map(|name| format!("--{name}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    vec![AgentSessionRuntimeDiagnostic {
        level: DiagnosticLevel::Error,
        message,
    }]
}

/// `createAgentSessionServices` (agent-session-services.ts:134-191).
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
    // Extension provider registrations drain into the model runtime
    // (agent-session-services.ts:155-179) — no extensions yet, so nothing is
    // pending.
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
    diagnostics.extend(apply_extension_flag_values(&options.extension_flag_values));

    Ok(AgentSessionServices {
        cwd,
        agent_dir,
        model_runtime,
        resource_loader: Arc::new(Mutex::new(loader)),
        diagnostics,
    })
}
