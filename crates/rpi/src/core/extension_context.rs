//! Session/runtime-backed context actions (T15 W5) — the rpi-side
//! implementations of `ExtensionContextActions` (types.ts:1612-1628) and
//! `ExtensionCommandContextActions` (types.ts:1630-1654), bound through
//! `runner.bindCore` / `bindCommandContext` equivalents
//! (agent-session.ts:2405-2431).

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use rpi_ext_host::api::{
    CommandContextActions, CompactOptions, ContextActions, ContextUsage, NavigateTreeOptions,
    ReplacedSessionContext, WithSessionFn,
};
use serde_json::Value;

use crate::core::agent_session::{AgentSession, WeakAgentSession};
use crate::core::agent_session_runtime::AgentSessionRuntime;
use crate::core::extension_host_adapter::host_of_runner;

/// `ExtensionContextActions` backed by the bound session (weak — the
/// session owns the host through the runner slot).
pub struct SessionContextActions {
    session: WeakAgentSession,
}

impl SessionContextActions {
    pub fn new(session: &AgentSession) -> Self {
        SessionContextActions {
            session: session.downgrade(),
        }
    }

    fn session(&self) -> Option<AgentSession> {
        self.session.upgrade()
    }
}

#[async_trait]
impl ContextActions for SessionContextActions {
    fn get_model(&self) -> Option<Value> {
        self.session()
            .and_then(|session| session.model())
            .and_then(|model| serde_json::to_value(model).ok())
    }

    fn is_idle(&self) -> bool {
        self.session().is_none_or(|session| session.is_idle())
    }

    fn is_project_trusted(&self) -> bool {
        self.session().is_none_or(|session| {
            session.settings_manager(|settings| settings.is_project_trusted())
        })
    }

    fn get_signal(&self) -> Option<tokio_util::sync::CancellationToken> {
        self.session().and_then(|session| session.agent().signal())
    }

    /// `abort` (agent-session.ts:2413-2418: the mode's abort handler wins,
    /// else `agent.abort()`).
    fn abort(&self) {
        if let Some(session) = self.session() {
            tokio::spawn(async move {
                session.abort().await;
            });
        }
    }

    fn has_pending_messages(&self) -> bool {
        self.session()
            .is_some_and(|session| session.pending_message_count() > 0)
    }

    /// `shutdown` (agent-session.ts:2421-2422): the mode-provided handler
    /// bound via `ExtensionBindings::shutdown`.
    fn shutdown(&self) {
        if let Some(session) = self.session() {
            session.extension_shutdown();
        }
    }

    fn get_context_usage(&self) -> Option<ContextUsage> {
        self.session()
            .and_then(|session| session.get_context_usage())
            .map(|usage| ContextUsage {
                tokens: usage.tokens,
                context_window: usage.context_window,
                percent: usage.percent,
            })
    }

    /// `compact` — fire-and-forget with the callback pair
    /// (agent-session.ts:2423-2433).
    fn compact(&self, options: CompactOptions) {
        let Some(session) = self.session() else {
            return;
        };
        tokio::spawn(async move {
            match session
                .compact(options.custom_instructions.as_deref())
                .await
            {
                Ok(result) => {
                    if let Some(on_complete) = options.on_complete {
                        match serde_json::to_value(&result) {
                            Ok(value) => on_complete(value),
                            Err(error) => {
                                tracing::warn!("compact onComplete payload: {error}")
                            }
                        }
                    }
                }
                Err(error) => {
                    if let Some(on_error) = options.on_error {
                        on_error(error.to_string());
                    }
                }
            }
        });
    }

    fn get_system_prompt(&self) -> String {
        self.session()
            .map(|session| session.system_prompt())
            .unwrap_or_default()
    }

    fn get_system_prompt_options(&self) -> Value {
        self.session()
            .map(|session| session.system_prompt_options_json())
            .unwrap_or_default()
    }

    // -- v0.11 additions (types.ts @ 4181f66) -------------------------------

    /// `getScopedModels` (runner.ts:706-709 @ 4181f66): read-only snapshot
    /// of the resolved model scope. The session holds `scoped_models:
    /// Vec<ScopedModel>` from the model resolver.
    fn get_scoped_models(&self) -> Vec<rpi_ext_host::types::ScopedModel> {
        self.session()
            .map(|session| {
                session
                    .scoped_models()
                    .iter()
                    .map(|sm| rpi_ext_host::types::ScopedModel {
                        model: serde_json::to_value(&sm.model).unwrap_or(Value::Null),
                        thinking_level: sm
                            .thinking_level
                            .as_ref()
                            .and_then(|tl| serde_json::to_value(tl).ok())
                            .and_then(|v| v.as_str().map(str::to_owned)),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `getSystemPromptSource` (resource-loader.ts:327-329, T24).
    fn get_system_prompt_source_path(&self) -> Option<String> {
        self.session().and_then(|session| {
            session
                .resource_loader()
                .lock()
                .ok()?
                .get_system_prompt_source()
                .map(|p| p.to_string_lossy().into_owned())
        })
    }

    /// `getAppendSystemPromptSources` (resource-loader.ts:335-337, T24).
    fn get_append_system_prompt_source_paths(&self) -> Vec<String> {
        self.session()
            .and_then(|session| {
                Some(
                    session
                        .resource_loader()
                        .lock()
                        .ok()?
                        .get_append_system_prompt_sources()
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect(),
                )
            })
            .unwrap_or_default()
    }
}

/// `ExtensionCommandContextActions` backed by the shared runtime handle
/// (rpc mode binds this; the methods take `&mut self` upstream semantics
/// through the mutex).
///
/// The handle is a `Weak`: a strong one cycles runtime → session → runner
/// → host → command actions → runtime and leaks the whole graph (the
/// RPC-mode shutdown path hangs on it).
pub struct RuntimeCommandActions {
    runtime: Weak<tokio::sync::Mutex<AgentSessionRuntime>>,
}

impl RuntimeCommandActions {
    pub fn new(runtime: &Arc<tokio::sync::Mutex<AgentSessionRuntime>>) -> Self {
        RuntimeCommandActions {
            runtime: Arc::downgrade(runtime),
        }
    }

    fn runtime(&self) -> Option<Arc<tokio::sync::Mutex<AgentSessionRuntime>>> {
        self.runtime.upgrade()
    }

    /// Build the extension-facing replaced-session context for the runtime's
    /// current session (agent-session.ts:3304-3312 `createReplacedSessionContext`).
    fn replaced_context(session: &AgentSession) -> Option<ReplacedSessionContext> {
        let host = host_of_runner(&session.extension_runner())?;
        let ctx = host.create_command_context();
        Some(ReplacedSessionContext::new(ctx, host.runtime()))
    }
}

/// Adapt an extension `withSession` closure into the runtime's internal
/// callback shape (a borrow over the call).
fn map_with_session(
    with_session: Option<WithSessionFn>,
) -> Option<
    impl Fn(
            crate::core::agent_session_runtime::ReplacedSessionContext,
        ) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
> {
    with_session.map(|with_session| {
        move |replaced: crate::core::agent_session_runtime::ReplacedSessionContext| {
            let with_session = with_session.clone();
            Box::pin(async move {
                if let Some(ctx) = RuntimeCommandActions::replaced_context(replaced.session()) {
                    with_session(ctx).await;
                }
            }) as futures::future::BoxFuture<'static, ()>
        }
    })
}

#[async_trait]
impl CommandContextActions for RuntimeCommandActions {
    async fn wait_for_idle(&self) {
        let Some(runtime) = self.runtime() else {
            return;
        };
        let session = runtime.lock().await.session().clone();
        session.wait_for_idle().await;
    }

    async fn new_session(
        &self,
        parent_session: Option<String>,
        with_session: Option<WithSessionFn>,
    ) -> bool {
        let Some(runtime) = self.runtime() else {
            return false;
        };
        let mut runtime = runtime.lock().await;
        match runtime
            .new_session(
                parent_session.as_deref(),
                None,
                map_with_session(with_session).map(|f| Box::new(f) as _),
            )
            .await
        {
            Ok(cancelled) => cancelled,
            Err(error) => {
                tracing::warn!("extension newSession failed: {error}");
                false
            }
        }
    }

    async fn fork(
        &self,
        entry_id: &str,
        position: Option<String>,
        with_session: Option<WithSessionFn>,
    ) -> bool {
        let position = match position.as_deref() {
            Some("at") => crate::core::agent_session_runtime::ForkPosition::At,
            _ => crate::core::agent_session_runtime::ForkPosition::Before,
        };
        let Some(runtime) = self.runtime() else {
            return false;
        };
        let mut runtime = runtime.lock().await;
        match runtime
            .fork(
                entry_id,
                position,
                map_with_session(with_session).map(|f| Box::new(f) as _),
            )
            .await
        {
            Ok(result) => result.cancelled,
            Err(error) => {
                tracing::warn!("extension fork failed: {error}");
                false
            }
        }
    }

    async fn navigate_tree(&self, target_id: &str, options: NavigateTreeOptions) -> bool {
        let Some(runtime) = self.runtime() else {
            return false;
        };
        let session = runtime.lock().await.session().clone();
        match session
            .navigate_tree(
                target_id,
                crate::core::agent_session::NavigateTreeOptions {
                    summarize: options.summarize.unwrap_or(false),
                    custom_instructions: options.custom_instructions,
                    replace_instructions: options.replace_instructions.unwrap_or(false),
                    label: options.label,
                },
            )
            .await
        {
            Ok(result) => result.cancelled,
            Err(error) => {
                tracing::warn!("extension navigateTree failed: {error}");
                false
            }
        }
    }

    async fn switch_session(
        &self,
        session_path: &str,
        with_session: Option<WithSessionFn>,
    ) -> bool {
        let Some(runtime) = self.runtime() else {
            return false;
        };
        let mut runtime = runtime.lock().await;
        match runtime
            .switch_session(
                session_path,
                None,
                map_with_session(with_session).map(|f| Box::new(f) as _),
                None,
            )
            .await
        {
            Ok(cancelled) => cancelled,
            Err(error) => {
                tracing::warn!("extension switchSession failed: {error}");
                false
            }
        }
    }

    /// `reload` — the session+host reload flow (agent-session.ts:2600-2628).
    async fn reload(&self) {
        let Some(runtime) = self.runtime() else {
            return;
        };
        let session = runtime.lock().await.session().clone();
        session.reload().await;
    }
}
