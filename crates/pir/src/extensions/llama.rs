//! Port of `packages/coding-agent/src/extensions/llama/index.ts`
//! @ pi 0.82.1 (2efa728) — the `/llama` command orchestration (load /
//! unload / download flows) plus the `LlamaUi` interface and
//! `runWithProgress` from `ui.ts` (the TUI implementation of [`LlamaUi`]
//! lives in `crates/pir/src/modes/interactive/components/llama_view.rs`).
//!
//! Intentional differences:
//! - The upstream extension context (`ctx.modelRegistry`, `ctx.ui.notify`,
//!   `ctx.mode`) becomes the [`LlamaHost`] trait; the TUI pieces become
//!   [`LlamaUi`]. Both are injectable so the flow is testable headless.
//! - `AbortSignal` becomes [`CancellationToken`]; the `runWithProgress`
//!   settled-future polling becomes a `tokio::select!` loop with a progress
//!   repaint channel (upstream calls `updateProgress` synchronously from the
//!   progress callback; here repaints are forwarded through the loop because
//!   the callbacks run on watcher tasks).

pub mod client;
pub mod huggingface;
pub mod provider;

// ---------------------------------------------------------------------------
// Built-in extension registration (T15 W7, closes D-047)
// ---------------------------------------------------------------------------

/// The built-in hidden extension (extensions/index.ts `builtInExtensions`):
/// registers the llama.cpp provider (native overload, runner.ts:394-400)
/// and the `/llama` command through the real extension host.
pub fn inline_extension() -> pir_ext_host::loader::InlineExtension {
    pir_ext_host::loader::InlineExtension::Named {
        name: "llama.cpp".to_owned(),
        hidden: true,
        factory: Arc::new(|api| {
            Box::pin(async move {
                api.register_native_provider(shared_llama_provider().provider())
                    .await
                    .map_err(|e| e.to_string())?;
                api.register_command(
                    "llama",
                    Some("Manage llama.cpp router models".to_owned()),
                    Arc::new(|_args, ctx| Box::pin(llama_command(ctx))),
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
        }),
    }
}

/// `/llama` handler (index.ts:170-180): the manager UI requires the
/// interactive TUI; it mounts through the interactive bridge's native
/// escape hatch (L0-only downcast).
async fn llama_command(ctx: pir_ext_host::api::ExtensionCommandContext) -> Result<(), String> {
    let is_tui = matches!(ctx.mode(), Ok(pir_ext_host::types::ExtensionMode::Tui));
    let ui = ctx.ui().map_err(|e| e.to_string())?;
    if !is_tui {
        ui.notify(
            "The /llama manager requires the interactive TUI",
            pir_ext_host::api::NotifyType::Error,
        );
        return Ok(());
    }
    let bridge = ui
        .as_any()
        .and_then(|any| {
            any.downcast_ref::<crate::modes::interactive::interactive_mode::ui_bridge::InteractiveUiBridge>()
        })
        .and_then(|bridge| bridge.interactive_ui());
    let Some(ui) = bridge else {
        return Err("/llama requires the interactive UI bridge".to_owned());
    };
    crate::modes::interactive::interactive_mode::InteractiveUi::handle_llama_command(&ui);
    Ok(())
}

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pir_ai::auth::{AuthResult, ModelsError};
use tokio_util::sync::CancellationToken;

pub use client::{
    format_bytes, llama_inference_url, normalize_llama_server_url, LlamaClient, LlamaError,
    LlamaModelInfo, LlamaProgress, LlamaProgressCallback,
};
pub use huggingface::{
    find_hugging_face_token, HuggingFaceClient, HuggingFaceModel, DEFAULT_HUGGING_FACE_URL,
};
pub use provider::{
    create_llama_provider, shared_llama_provider, LlamaProviderController, LLAMA_PROVIDER_ID,
};

/// `LlamaManagerAction` (ui.ts:25).
#[derive(Debug, Clone, PartialEq)]
pub enum LlamaManagerAction {
    Model(Box<LlamaModelInfo>),
    Download,
    Close,
}

/// Return of the connection-error dialog (ui.ts `connectionError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionErrorChoice {
    Retry,
    Close,
}

/// Notify levels (`ctx.ui.notify(message, level)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyLevel {
    Info,
    Warning,
    Error,
}

/// `ProgressState` (ui.ts:27-30): the progress view model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgressState {
    pub title: String,
    pub model: String,
    pub message: String,
    pub ratio: Option<f64>,
    pub detail: Option<String>,
}

/// The Hugging Face search callback handed to the UI search component
/// (ui.ts `LlamaUi.searchModels(search)`).
pub type HuggingFaceSearchFn = Arc<
    dyn Fn(
            String,
            CancellationToken,
        ) -> BoxFuture<'static, Result<Vec<HuggingFaceModel>, LlamaError>>
        + Send
        + Sync,
>;

/// `LlamaUi` (ui.ts:77-88). Default `confirm`/`connection_error` build on
/// `select`, exactly like the upstream `LlamaView` methods.
#[async_trait::async_trait]
pub trait LlamaUi: Send {
    /// `showModels(serverUrl, models)` — the model list with the
    /// "Download model…" entry; resolves with the chosen action.
    async fn show_models(
        &mut self,
        server_url: &str,
        models: Vec<LlamaModelInfo>,
    ) -> LlamaManagerAction;

    /// `select(title, options)` — `None` on cancel.
    async fn select(&mut self, title: &str, options: Vec<String>) -> Option<String>;

    /// `confirm(title, message)` (ui.ts:381-383): a Yes/No select.
    async fn confirm(&mut self, title: &str, message: &str) -> bool {
        self.select(
            &format!("{title}\n{message}"),
            vec!["Yes".to_owned(), "No".to_owned()],
        )
        .await
        .as_deref()
            == Some("Yes")
    }

    /// `connectionError(serverUrl, message)` (ui.ts:385-388).
    async fn connection_error(&mut self, server_url: &str, message: &str) -> ConnectionErrorChoice {
        match self
            .select(
                &format!("llama.cpp unavailable\n{server_url}\n\n{message}"),
                vec!["Retry".to_owned(), "Close".to_owned()],
            )
            .await
            .as_deref()
        {
            Some("Retry") => ConnectionErrorChoice::Retry,
            _ => ConnectionErrorChoice::Close,
        }
    }

    /// `searchModels(search)` — the Hugging Face search component; resolves
    /// with the chosen `owner/repository[:quant]` or `None`.
    async fn search_models(&mut self, search: HuggingFaceSearchFn) -> Option<String>;

    /// `showStatus(title, message)` — a transient status view.
    fn show_status(&mut self, title: &str, message: &str);

    /// `progress(state)` (ui.ts:419-428): show the progress view and resolve
    /// when the user presses the stop key. Resolving does NOT cancel the
    /// operation — [`run_with_progress`] asks for confirmation first.
    async fn progress(&mut self, state: &ProgressState);

    /// `updateProgress(state)` (ui.ts:430-455): repaint the progress view.
    fn update_progress(&mut self, state: &ProgressState);
}

/// The extension-context slice the `/llama` flow needs (upstream
/// `ExtensionCommandContext`: `mode`, `ui.notify`, `modelRegistry`).
#[async_trait::async_trait]
pub trait LlamaHost: Send + Sync {
    /// `ctx.mode === "tui"` (index.ts:177).
    fn mode_is_tui(&self) -> bool;

    /// `ctx.ui.notify(message, level)`.
    fn notify(&self, message: &str, level: NotifyLevel);

    /// `ctx.modelRegistry.getProviderAuth(LLAMA_PROVIDER_ID)`.
    async fn provider_auth(&self) -> Result<Option<AuthResult>, ModelsError>;

    /// `ctx.modelRegistry.refresh()`.
    async fn refresh_models(&self);
}

/// `isConnectionError` (index.ts:11-15). The reqwest transport
/// classification ([`LlamaError::connection`]) covers what upstream matches
/// in undici's message text; the substring check is kept for parity with
/// error messages raised elsewhere.
pub fn is_connection_error(error: &LlamaError) -> bool {
    if error.connection {
        return true;
    }
    let message = error.message.to_lowercase();
    message.contains("fetch failed") || message.contains("timeout") || message.contains("network")
}

/// `connectionErrorMessage` (index.ts:17-20).
pub fn connection_error_message(error: &LlamaError) -> String {
    if is_connection_error(error) {
        "Could not connect to the server.".to_owned()
    } else {
        error.message.clone()
    }
}

/// `parseHuggingFaceModel` (index.ts:22-27): split `owner/repository[:quant]`
/// at the first `:` after the first `/`.
pub fn parse_hugging_face_model(value: &str) -> (String, Option<String>) {
    let search_from = value.find('/').map(|index| index + 1).unwrap_or(0);
    let colon = value[search_from..]
        .find(':')
        .map(|index| search_from + index);
    match colon {
        Some(colon) => (
            value[..colon].to_owned(),
            Some(value[colon + 1..].to_owned()),
        ),
        None => (value.to_owned(), None),
    }
}

/// `configuredClient` (index.ts:29-40): resolve the router URL/api key from
/// the stored credential/env; warn and return `None` when unconfigured.
pub async fn configured_client(host: &dyn LlamaHost) -> Result<Option<LlamaClient>, LlamaError> {
    let result = host
        .provider_auth()
        .await
        .map_err(|error| LlamaError::new(error.message))?;
    let Some(result) = result else {
        host.notify(
            &format!("Configure llama.cpp with /login {LLAMA_PROVIDER_ID}"),
            NotifyLevel::Warning,
        );
        return Ok(None);
    };
    let configured_url = result
        .env
        .as_ref()
        .and_then(|env| env.get("LLAMA_BASE_URL"))
        .filter(|value| !value.is_empty())
        .cloned()
        .or(result.auth.base_url)
        .unwrap_or_default();
    let server_url = normalize_llama_server_url(&configured_url)?;
    Ok(Some(LlamaClient::new(
        &server_url,
        result.auth.api_key.as_deref(),
    )?))
}

/// `syncCatalog` (index.ts:46-55): publish loaded models into the provider
/// and refresh the model registry.
async fn sync_catalog(
    host: &dyn LlamaHost,
    controller: &LlamaProviderController,
    client: &LlamaClient,
    catalog: Option<Vec<LlamaModelInfo>>,
) -> Result<Vec<LlamaModelInfo>, LlamaError> {
    let current = match catalog {
        Some(catalog) => catalog,
        None => client.list(false, None).await?,
    };
    controller.set_catalog(&current, client.server_url())?;
    host.refresh_models().await;
    Ok(current)
}

/// Outcome of [`run_with_progress`] (`{ cancelled, value }`, ui.ts:505).
pub enum RunOutcome<T> {
    Cancelled,
    Completed(T),
}

/// `runWithProgress` (ui.ts:504-542): run the operation with a progress
/// view; a stop request asks for confirmation, then calls `cancel` and
/// aborts the operation token.
pub async fn run_with_progress<T, R, RFut, C, CFut>(
    ui: &mut dyn LlamaUi,
    title: &str,
    model: &str,
    cancel_title: &str,
    run: R,
    cancel: C,
) -> Result<RunOutcome<T>, LlamaError>
where
    T: Send + 'static,
    R: FnOnce(CancellationToken, LlamaProgressCallback) -> RFut,
    RFut: std::future::Future<Output = Result<T, LlamaError>> + Send + 'static,
    C: FnOnce() -> CFut,
    CFut: std::future::Future<Output = Result<(), LlamaError>> + Send,
{
    let token = CancellationToken::new();
    let state: Arc<Mutex<ProgressState>> = Arc::new(Mutex::new(ProgressState {
        title: title.to_owned(),
        model: model.to_owned(),
        message: "Starting…".to_owned(),
        ratio: None,
        detail: None,
    }));
    // Progress callbacks run on watcher tasks; repaints are forwarded to the
    // UI through this channel (see module docs).
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let update: LlamaProgressCallback = {
        let state = Arc::clone(&state);
        Arc::new(move |progress: LlamaProgress| {
            {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.message = progress.message;
                if progress.ratio.is_some() {
                    state.ratio = progress.ratio;
                }
                if progress.detail.is_some() {
                    state.detail = progress.detail;
                }
            }
            let _ = progress_tx.send(());
        })
    };
    let mut settled = tokio::spawn(run(token.clone(), update));
    let current_state =
        |state: &Arc<Mutex<ProgressState>>| state.lock().unwrap_or_else(|e| e.into_inner()).clone();

    loop {
        let snapshot = current_state(&state);
        tokio::select! {
            result = &mut settled => {
                let value = result.map_err(|error| LlamaError::new(format!("task join: {error}")))??;
                return Ok(RunOutcome::Completed(value));
            }
            Some(()) = progress_rx.recv() => {
                ui.update_progress(&snapshot);
            }
            _ = ui.progress(&snapshot) => {
                let snapshot = current_state(&state);
                let stop = ui.confirm(cancel_title, &snapshot.model).await;
                if !stop || settled.is_finished() {
                    continue;
                }
                // `try { await options.cancel() } finally { controller.abort() }`
                let cancel_result = cancel().await;
                token.cancel();
                let settled_result = settled
                    .await
                    .map_err(|error| LlamaError::new(format!("task join: {error}")));
                cancel_result?;
                let _ = settled_result;
                return Ok(RunOutcome::Cancelled);
            }
        }
    }
}

/// `loadModel` (index.ts:57-114).
async fn load_model(
    host: &dyn LlamaHost,
    controller: &LlamaProviderController,
    ui: &mut dyn LlamaUi,
    client: &LlamaClient,
    catalog: &[LlamaModelInfo],
    target: &LlamaModelInfo,
) -> Result<(), LlamaError> {
    let loaded: Vec<LlamaModelInfo> = catalog
        .iter()
        .filter(|model| model.id != target.id && model.is_loaded())
        .cloned()
        .collect();
    let mut replace = false;
    if !loaded.is_empty() {
        let choice = ui
            .select(
                &format!(
                    "{} model{} loaded",
                    loaded.len(),
                    if loaded.len() == 1 { " is" } else { "s are" }
                ),
                vec![
                    "Unload all and load".to_owned(),
                    "Keep loaded and load".to_owned(),
                    "Cancel".to_owned(),
                ],
            )
            .await;
        let Some(choice) = choice else {
            return Ok(());
        };
        if choice == "Cancel" {
            return Ok(());
        }
        replace = choice == "Unload all and load";
    }

    // `restoreLoaded` (index.ts:76-80): never leave previously loaded models
    // unloaded after a cancelled/failed replacement load.
    let restore_loaded = || async {
        host.notify("Restoring previously loaded models", NotifyLevel::Info);
        for model in &loaded {
            client
                .load_and_wait(&model.id, Arc::new(|_| {}), None)
                .await?;
        }
        sync_catalog(host, controller, client, None).await?;
        Ok::<(), LlamaError>(())
    };
    if replace {
        for model in &loaded {
            client.unload_and_wait(&model.id, None).await?;
        }
    }

    let run_client = client.clone();
    let run_model = target.id.clone();
    let cancel_client = client.clone();
    let cancel_model = target.id.clone();
    let result = run_with_progress(
        ui,
        "Loading model",
        &target.id,
        "Stop loading?",
        move |signal, update| {
            let client = run_client;
            async move {
                client
                    .load_and_wait(&run_model, update, Some(&signal))
                    .await
            }
        },
        move || {
            let client = cancel_client;
            async move { client.unload(&cancel_model, None).await }
        },
    )
    .await;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            if replace {
                // Preserve the original load error (index.ts:105-112).
                let _ = restore_loaded().await;
            }
            return Err(error);
        }
    };
    match outcome {
        RunOutcome::Cancelled => {
            if replace {
                restore_loaded().await?;
            }
            Ok(())
        }
        RunOutcome::Completed(_) => {
            let refreshed = sync_catalog(host, controller, client, None).await?;
            let loaded_model = refreshed.iter().find(|model| model.id == target.id);
            let message = if loaded_model
                .is_some_and(|model| model.status.value == client::LlamaModelStatusValue::LOADED)
            {
                format!("Loaded {}", target.id)
            } else {
                format!("Load started for {}", target.id)
            };
            host.notify(&message, NotifyLevel::Info);
            Ok(())
        }
    }
}

/// `unloadModel` (index.ts:116-126). The explicit `confirm` is the
/// never-silently-unload guarantee (docs/llama-cpp.md).
async fn unload_model(
    host: &dyn LlamaHost,
    controller: &LlamaProviderController,
    ui: &mut dyn LlamaUi,
    client: &LlamaClient,
    model: &LlamaModelInfo,
) -> Result<(), LlamaError> {
    if !ui.confirm("Unload model?", &model.id).await {
        return Ok(());
    }
    client.unload_and_wait(&model.id, None).await?;
    sync_catalog(host, controller, client, None).await?;
    host.notify(&format!("Unloaded {}", model.id), NotifyLevel::Info);
    Ok(())
}

/// `downloadModel` (index.ts:128-172).
async fn download_model(
    host: &dyn LlamaHost,
    controller: &LlamaProviderController,
    ui: &mut dyn LlamaUi,
    client: &LlamaClient,
    hugging_face: &HuggingFaceClient,
) -> Result<(), LlamaError> {
    let search: HuggingFaceSearchFn = {
        let hugging_face = hugging_face.clone();
        Arc::new(move |query: String, signal: CancellationToken| {
            let hugging_face = hugging_face.clone();
            Box::pin(async move { hugging_face.search(&query, Some(&signal)).await })
        })
    };
    let Some(selected) = ui.search_models(search).await else {
        return Ok(());
    };
    let (repository, parsed_quantization) = parse_hugging_face_model(&selected);
    ui.show_status("Loading model details", &repository);
    let details = hugging_face.details(&repository, None).await?;
    if details.gated.is_gated() {
        let approval = if details.gated.is_manual() {
            "Manual approval is required"
        } else {
            "Accept the access terms"
        };
        let choice = ui
            .select(
                &format!(
                    "Hugging Face access required\n{}\n\n{approval} at:\nhttps://huggingface.co/{}\n\nThe llama.cpp server needs HF_TOKEN with access.",
                    details.id, details.id
                ),
                vec!["Continue".to_owned(), "Back".to_owned()],
            )
            .await;
        if choice.as_deref() != Some("Continue") {
            return Ok(());
        }
    }
    let mut quantization = parsed_quantization;
    if quantization.is_none() && !details.quantizations.is_empty() {
        let options: Vec<String> = details
            .quantizations
            .iter()
            .map(|entry| {
                let mut detail_parts: Vec<String> = Vec::new();
                if let Some(size) = entry.size {
                    detail_parts.push(format_bytes(size as f64));
                }
                if entry.name == "Q4_K_M" {
                    detail_parts.push("recommended".to_owned());
                }
                if detail_parts.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{} · {}", entry.name, detail_parts.join(" · "))
                }
            })
            .collect();
        let Some(choice) = ui
            .select(
                &format!("Select quantization\n{}", details.id),
                options.clone(),
            )
            .await
        else {
            return Ok(());
        };
        let Some(index) = options.iter().position(|option| *option == choice) else {
            return Ok(());
        };
        let Some(name) = details
            .quantizations
            .get(index)
            .map(|entry| entry.name.clone())
        else {
            return Ok(());
        };
        quantization = Some(name);
    }
    let model = match &quantization {
        Some(quantization) => format!("{}:{quantization}", details.id),
        None => details.id.clone(),
    };
    let run_client = client.clone();
    let run_model = model.clone();
    let cancel_client = client.clone();
    let cancel_model = model.clone();
    let outcome = run_with_progress(
        ui,
        "Downloading model",
        &model,
        "Stop download?",
        move |signal, update| {
            let client = run_client;
            async move {
                client
                    .download_and_wait(&run_model, update, Some(&signal))
                    .await
            }
        },
        move || {
            let client = cancel_client;
            async move { client.unload(&cancel_model, None).await }
        },
    )
    .await?;
    let RunOutcome::Completed(catalog) = outcome else {
        return Ok(());
    };
    sync_catalog(host, controller, client, Some(catalog)).await?;
    host.notify(&format!("Downloaded {model}"), NotifyLevel::Info);
    Ok(())
}

/// The `/llama` command handler (index.ts:174-220). `showLlamaUi`'s custom
/// component mounting is the caller's job (the TUI wiring mounts the view,
/// then invokes this); escaping errors are reported by the caller, mirroring
/// `showLlamaUi`'s error boundary (ui.ts:480-492).
pub async fn run_llama_manager(
    host: &dyn LlamaHost,
    controller: &LlamaProviderController,
    ui: &mut dyn LlamaUi,
    hugging_face: HuggingFaceClient,
) -> Result<(), LlamaError> {
    if !host.mode_is_tui() {
        host.notify(
            "/llama is available in interactive mode",
            NotifyLevel::Warning,
        );
        return Ok(());
    }
    let Some(client) = configured_client(host).await? else {
        return Ok(());
    };

    // `readCatalog` (index.ts:184-194): retry/close on connection failure.
    async fn read_catalog(
        host: &dyn LlamaHost,
        controller: &LlamaProviderController,
        ui: &mut dyn LlamaUi,
        client: &LlamaClient,
    ) -> Result<Option<Vec<LlamaModelInfo>>, LlamaError> {
        loop {
            match sync_catalog(host, controller, client, None).await {
                Ok(catalog) => return Ok(Some(catalog)),
                Err(error) => {
                    if ui
                        .connection_error(client.server_url(), &connection_error_message(&error))
                        .await
                        == ConnectionErrorChoice::Close
                    {
                        return Ok(None);
                    }
                }
            }
        }
    }

    let Some(mut catalog) = read_catalog(host, controller, ui, &client).await? else {
        return Ok(());
    };
    loop {
        let action = ui.show_models(client.server_url(), catalog.clone()).await;
        if matches!(action, LlamaManagerAction::Close) {
            return Ok(());
        }
        let result = match &action {
            LlamaManagerAction::Download => {
                download_model(host, controller, ui, &client, &hugging_face).await
            }
            LlamaManagerAction::Model(model) if model.is_loaded() => {
                unload_model(host, controller, ui, &client, model.as_ref()).await
            }
            LlamaManagerAction::Model(model)
                if model.status.value == client::LlamaModelStatusValue::UNLOADED =>
            {
                load_model(host, controller, ui, &client, &catalog, model.as_ref()).await
            }
            LlamaManagerAction::Model(model) => {
                host.notify(
                    &format!("{} is {}", model.id, model.status.value),
                    NotifyLevel::Warning,
                );
                Ok(())
            }
            LlamaManagerAction::Close => Ok(()),
        };
        let action_error = result.err();
        let Some(refreshed) = read_catalog(host, controller, ui, &client).await? else {
            return Ok(());
        };
        catalog = refreshed;
        if let Some(error) = action_error {
            if !is_connection_error(&error) {
                host.notify(&error.message, NotifyLevel::Error);
            }
        }
    }
}
