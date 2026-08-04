//! `AgentSessionRuntime` — owns the current [`AgentSession`] plus its
//! cwd-bound services, and performs session replacement
//! (new / fork / switch / import) with teardown + rebind.
//!
//! Port of `packages/coding-agent/src/core/agent-session-runtime.ts` @ pi
//! 0.82.1 (2efa728).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use thiserror::Error;

use crate::core::agent_session::AgentSession;
use crate::core::agent_session_services::{AgentSessionRuntimeDiagnostic, AgentSessionServices};
use crate::core::extensions::{SessionShutdownReason, SessionStartEvent, SessionStartReason};
use crate::core::session_cwd::assert_session_cwd_exists;
use crate::core::session_manager::{NewSessionOptions, SessionManager};
use crate::error::PirError;
use crate::tools::path_utils::resolve_path;

/// `CreateAgentSessionRuntimeResult` (agent-session-runtime.ts:23-26).
pub struct CreateAgentSessionRuntimeResult {
    pub session: AgentSession,
    pub services: AgentSessionServices,
    pub diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
    pub model_fallback_message: Option<String>,
}

/// Options handed to the runtime factory
/// (`CreateAgentSessionRuntimeFactory` input, agent-session-runtime.ts:35-42).
pub struct CreateRuntimeOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub session_manager: Arc<Mutex<SessionManager>>,
    pub session_start_event: Option<SessionStartEvent>,
}

/// `CreateAgentSessionRuntimeFactory` (agent-session-runtime.ts:35-42).
pub type CreateAgentSessionRuntimeFactory = Arc<
    dyn Fn(
            CreateRuntimeOptions,
        ) -> BoxFuture<'static, Result<CreateAgentSessionRuntimeResult, PirError>>
        + Send
        + Sync,
>;

/// Thrown when import references a JSONL file path that does not exist
/// (`SessionImportFileNotFoundError`, agent-session-runtime.ts:46-54).
#[derive(Debug, Error)]
#[error("File not found: {}", .0.display())]
pub struct SessionImportFileNotFoundError(pub PathBuf);

/// `ReplacedSessionContext` (extensions/types.ts) — the session control
/// surface handed to `withSession` callbacks after a replacement.
pub struct ReplacedSessionContext {
    session: AgentSession,
}

impl ReplacedSessionContext {
    pub fn session(&self) -> &AgentSession {
        &self.session
    }

    pub async fn send_message(
        &self,
        custom_type: &str,
        content: Option<pir_ai::types::UserContent>,
        display: bool,
        details: Option<serde_json::Value>,
        trigger_turn: bool,
        deliver_as: Option<crate::core::agent_session::CustomDeliverAs>,
    ) -> Result<(), PirError> {
        self.session
            .send_custom_message(
                custom_type,
                content,
                display,
                details,
                trigger_turn,
                deliver_as,
            )
            .await
    }

    pub async fn send_user_message(
        &self,
        text: &str,
        images: Option<Vec<pir_ai::types::ImageContent>>,
        deliver_as: Option<crate::core::extensions::StreamingBehavior>,
    ) -> Result<(), PirError> {
        self.session
            .send_user_message(text, images, deliver_as)
            .await
    }
}

/// Callbacks wired by the hosting mode (RPC/interactive).
pub type RebindSessionCallback = Box<dyn Fn(AgentSession) -> BoxFuture<'static, ()> + Send + Sync>;
pub type WithSessionCallback<'a> =
    Option<&'a (dyn Fn(ReplacedSessionContext) -> BoxFuture<'a, ()> + Send + Sync)>;

/// `AgentSessionRuntime` (agent-session-runtime.ts:74-403).
pub struct AgentSessionRuntime {
    session: AgentSession,
    services: AgentSessionServices,
    create_runtime: CreateAgentSessionRuntimeFactory,
    diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
    model_fallback_message: Option<String>,
    rebind_session: Option<RebindSessionCallback>,
}

/// Setup callback for `new_session` (agent-session-runtime.ts:225).
pub type NewSessionSetup<'a> = Option<&'a (dyn Fn(&Mutex<SessionManager>) + Send + Sync)>;

/// `fork` / `clone` position (agent-session-runtime.ts:260).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
    Before,
    At,
}

pub struct ForkResult {
    pub cancelled: bool,
    pub selected_text: Option<String>,
}

impl AgentSessionRuntime {
    pub fn new(
        session: AgentSession,
        services: AgentSessionServices,
        create_runtime: CreateAgentSessionRuntimeFactory,
        diagnostics: Vec<AgentSessionRuntimeDiagnostic>,
        model_fallback_message: Option<String>,
    ) -> Self {
        AgentSessionRuntime {
            session,
            services,
            create_runtime,
            diagnostics,
            model_fallback_message,
            rebind_session: None,
        }
    }

    pub fn services(&self) -> &AgentSessionServices {
        &self.services
    }

    pub fn session(&self) -> &AgentSession {
        &self.session
    }

    pub fn cwd(&self) -> &Path {
        &self.services.cwd
    }

    pub fn diagnostics(&self) -> &[AgentSessionRuntimeDiagnostic] {
        &self.diagnostics
    }

    pub fn model_fallback_message(&self) -> Option<&str> {
        self.model_fallback_message.as_deref()
    }

    /// `setRebindSession` (agent-session-runtime.ts:117-119).
    pub fn set_rebind_session(&mut self, rebind: Option<RebindSessionCallback>) {
        self.rebind_session = rebind;
    }

    async fn emit_before_switch(&self, reason: &str, _target_session_file: Option<&str>) -> bool {
        let runner = self.session.extension_runner();
        if !runner.has_handlers("session_before_switch") {
            return false;
        }
        let _ = reason;
        runner
            .emit_cancelable("session_before_switch")
            .await
            .map(|result| result.cancel)
            .unwrap_or(false)
    }

    async fn emit_before_fork(&self, _entry_id: &str, _position: ForkPosition) -> bool {
        let runner = self.session.extension_runner();
        if !runner.has_handlers("session_before_fork") {
            return false;
        }
        runner
            .emit_cancelable("session_before_fork")
            .await
            .map(|result| result.cancel)
            .unwrap_or(false)
    }

    /// `teardownCurrent` (agent-session-runtime.ts:167-175).
    async fn teardown_current(&self, reason: SessionShutdownReason, _target: Option<String>) {
        let _ = reason;
        let _ = &_target;
        self.session
            .extension_runner()
            .emit("session_shutdown")
            .await;
        self.session.dispose();
    }

    fn apply(&mut self, result: CreateAgentSessionRuntimeResult) {
        self.session = result.session;
        self.services = result.services;
        self.diagnostics = result.diagnostics;
        self.model_fallback_message = result.model_fallback_message;
    }

    /// `finishSessionReplacement` (agent-session-runtime.ts:184-191).
    async fn finish_session_replacement(&self, with_session: WithSessionCallback<'_>) {
        if let Some(rebind) = &self.rebind_session {
            rebind(self.session.clone()).await;
        }
        if let Some(with_session) = with_session {
            with_session(ReplacedSessionContext {
                session: self.session.clone(),
            })
            .await;
        }
    }

    /// `switchSession` (agent-session-runtime.ts:193-221).
    pub async fn switch_session(
        &mut self,
        session_path: &str,
        cwd_override: Option<&str>,
        with_session: WithSessionCallback<'_>,
    ) -> Result<bool, PirError> {
        if self.emit_before_switch("resume", Some(session_path)).await {
            return Ok(true);
        }

        let previous_session_file = self.session.session_file();
        let session_manager =
            SessionManager::open(Path::new(session_path), None, cwd_override.map(Path::new))?;
        assert_session_cwd_exists(&session_manager, self.cwd())
            .map_err(|error| PirError::Session(error.to_string()))?;
        let target_file = session_manager
            .get_session_file()
            .map(|p| p.to_string_lossy().into_owned());
        let new_cwd = session_manager.get_cwd().to_path_buf();
        let session_manager = Arc::new(Mutex::new(session_manager));

        self.teardown_current(SessionShutdownReason::Resume, target_file)
            .await;
        self.apply(
            (self.create_runtime)(CreateRuntimeOptions {
                cwd: new_cwd,
                agent_dir: self.services.agent_dir.clone(),
                session_manager,
                session_start_event: Some(SessionStartEvent {
                    reason: SessionStartReason::Resume,
                    previous_session_file: previous_session_file
                        .map(|p| p.to_string_lossy().into_owned()),
                }),
            })
            .await?,
        );
        self.finish_session_replacement(with_session).await;
        Ok(false)
    }

    /// `newSession` (agent-session-runtime.ts:223-257).
    pub async fn new_session(
        &mut self,
        parent_session: Option<&str>,
        setup: NewSessionSetup<'_>,
        with_session: WithSessionCallback<'_>,
    ) -> Result<bool, PirError> {
        if self.emit_before_switch("new", None).await {
            return Ok(true);
        }

        let previous_session_file = self.session.session_file();
        let (session_dir, is_persisted) = {
            let manager = self.session.session_manager();
            let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            (
                manager.get_session_dir().to_path_buf(),
                manager.is_persisted(),
            )
        };
        let mut session_manager = if is_persisted {
            SessionManager::create(self.cwd(), Some(&session_dir), NewSessionOptions::default())?
        } else {
            SessionManager::in_memory(Some(self.cwd()), NewSessionOptions::default())?
        };
        if let Some(parent_session) = parent_session {
            session_manager.new_session(NewSessionOptions {
                id: None,
                parent_session: Some(parent_session.to_owned()),
            })?;
        }
        let target_file = session_manager
            .get_session_file()
            .map(|p| p.to_string_lossy().into_owned());
        let session_manager = Arc::new(Mutex::new(session_manager));

        self.teardown_current(SessionShutdownReason::New, target_file)
            .await;
        self.apply(
            (self.create_runtime)(CreateRuntimeOptions {
                cwd: self.services.cwd.clone(),
                agent_dir: self.services.agent_dir.clone(),
                session_manager: session_manager.clone(),
                session_start_event: Some(SessionStartEvent {
                    reason: SessionStartReason::New,
                    previous_session_file: previous_session_file
                        .map(|p| p.to_string_lossy().into_owned()),
                }),
            })
            .await?,
        );
        if let Some(setup) = setup {
            setup(&session_manager);
            let messages = session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .build_session_context()
                .messages;
            self.session.agent().set_messages(messages);
        }
        self.finish_session_replacement(with_session).await;
        Ok(false)
    }

    /// `fork` (agent-session-runtime.ts:259-349).
    pub async fn fork(
        &mut self,
        entry_id: &str,
        position: ForkPosition,
        with_session: WithSessionCallback<'_>,
    ) -> Result<ForkResult, PirError> {
        if self.emit_before_fork(entry_id, position).await {
            return Ok(ForkResult {
                cancelled: true,
                selected_text: None,
            });
        }

        let selected_entry = {
            let manager = self.session.session_manager();
            let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager.get_entry(entry_id)
        };
        let Some(selected_entry) = selected_entry else {
            return Err(PirError::Session("Invalid entry ID for forking".to_owned()));
        };

        let mut selected_text: Option<String> = None;
        let target_leaf_id: Option<String> = match position {
            ForkPosition::At => Some(selected_entry.id().to_owned()),
            ForkPosition::Before => {
                let known = selected_entry.known();
                let is_user_message = matches!(
                    known,
                    Some(pir_agent::session::SessionEntry::Message(entry))
                        if matches!(entry.message, pir_agent::AgentMessage::User(_))
                );
                if !is_user_message {
                    return Err(PirError::Session("Invalid entry ID for forking".to_owned()));
                }
                if let Some(pir_agent::session::SessionEntry::Message(entry)) = known {
                    if let pir_agent::AgentMessage::User(user) = &entry.message {
                        selected_text =
                            Some(pir_ai::utils::text::content_text_user(&user.content, ""));
                    }
                }
                selected_entry.parent_id().map(str::to_owned)
            }
        };

        let previous_session_file = self.session.session_file();
        let is_persisted = {
            let manager = self.session.session_manager();
            let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager.is_persisted()
        };

        if is_persisted {
            let current_session_file = self.session.session_file().ok_or_else(|| {
                PirError::Session("Persisted session is missing a session file".to_owned())
            })?;
            let session_dir = {
                let manager = self.session.session_manager();
                let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
                manager.get_session_dir().to_path_buf()
            };

            let Some(target_leaf_id) = target_leaf_id else {
                // No target leaf: new empty session parented at the current
                // file (agent-session-runtime.ts:293-307).
                let mut session_manager = SessionManager::create(
                    self.cwd(),
                    Some(&session_dir),
                    NewSessionOptions::default(),
                )?;
                session_manager.new_session(NewSessionOptions {
                    id: None,
                    parent_session: Some(current_session_file.to_string_lossy().into_owned()),
                })?;
                let target_file = session_manager
                    .get_session_file()
                    .map(|p| p.to_string_lossy().into_owned());
                self.teardown_current(SessionShutdownReason::Fork, target_file)
                    .await;
                self.apply(
                    (self.create_runtime)(CreateRuntimeOptions {
                        cwd: self.services.cwd.clone(),
                        agent_dir: self.services.agent_dir.clone(),
                        session_manager: Arc::new(Mutex::new(session_manager)),
                        session_start_event: Some(SessionStartEvent {
                            reason: SessionStartReason::Fork,
                            previous_session_file: previous_session_file
                                .map(|p| p.to_string_lossy().into_owned()),
                        }),
                    })
                    .await?,
                );
                self.finish_session_replacement(with_session).await;
                return Ok(ForkResult {
                    cancelled: false,
                    selected_text,
                });
            };

            if !current_session_file.exists() {
                return Err(PirError::Session(
                    "This session has not been saved yet. Wait for the first assistant response before cloning or forking it."
                        .to_owned(),
                ));
            }
            let mut session_manager =
                SessionManager::open(&current_session_file, Some(&session_dir), None)?;
            let forked_path = session_manager.create_branched_session(&target_leaf_id)?;
            let Some(_forked_path) = forked_path else {
                return Err(PirError::Session(
                    "Failed to create forked session".to_owned(),
                ));
            };
            let new_cwd = session_manager.get_cwd().to_path_buf();
            let target_file = session_manager
                .get_session_file()
                .map(|p| p.to_string_lossy().into_owned());
            self.teardown_current(SessionShutdownReason::Fork, target_file)
                .await;
            self.apply(
                (self.create_runtime)(CreateRuntimeOptions {
                    cwd: new_cwd,
                    agent_dir: self.services.agent_dir.clone(),
                    session_manager: Arc::new(Mutex::new(session_manager)),
                    session_start_event: Some(SessionStartEvent {
                        reason: SessionStartReason::Fork,
                        previous_session_file: previous_session_file
                            .clone()
                            .map(|p| p.to_string_lossy().into_owned()),
                    }),
                })
                .await?,
            );
            self.finish_session_replacement(with_session).await;
            return Ok(ForkResult {
                cancelled: false,
                selected_text,
            });
        }

        // In-memory session (agent-session-runtime.ts:332-348).
        let session_manager = self.session.session_manager();
        {
            let mut manager = session_manager.lock().unwrap_or_else(|e| e.into_inner());
            match &target_leaf_id {
                None => {
                    manager.new_session(NewSessionOptions {
                        id: None,
                        parent_session: previous_session_file
                            .clone()
                            .map(|p| p.to_string_lossy().into_owned()),
                    })?;
                }
                Some(target_leaf_id) => {
                    manager.create_branched_session(target_leaf_id)?;
                }
            }
        }
        let target_file = {
            let manager = session_manager.lock().unwrap_or_else(|e| e.into_inner());
            manager
                .get_session_file()
                .map(|p| p.to_string_lossy().into_owned())
        };
        self.teardown_current(SessionShutdownReason::Fork, target_file)
            .await;
        self.apply(
            (self.create_runtime)(CreateRuntimeOptions {
                cwd: self.services.cwd.clone(),
                agent_dir: self.services.agent_dir.clone(),
                session_manager,
                session_start_event: Some(SessionStartEvent {
                    reason: SessionStartReason::Fork,
                    previous_session_file: previous_session_file
                        .clone()
                        .map(|p| p.to_string_lossy().into_owned()),
                }),
            })
            .await?,
        );
        self.finish_session_replacement(with_session).await;
        Ok(ForkResult {
            cancelled: false,
            selected_text,
        })
    }

    /// `importFromJsonl` (agent-session-runtime.ts:358-393).
    pub async fn import_from_jsonl(
        &mut self,
        input_path: &str,
        cwd_override: Option<&str>,
    ) -> Result<bool, PirError> {
        let resolved_path = resolve_path(
            input_path,
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        );
        if !resolved_path.exists() {
            return Err(PirError::Session(
                SessionImportFileNotFoundError(resolved_path).to_string(),
            ));
        }

        let session_dir = {
            let manager = self.session.session_manager();
            let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
            manager.get_session_dir().to_path_buf()
        };
        if !session_dir.exists() {
            std::fs::create_dir_all(&session_dir)?;
        }

        let destination_path = session_dir.join(
            resolved_path
                .file_name()
                .ok_or_else(|| PirError::Session("Invalid session file name".to_owned()))?,
        );
        if self
            .emit_before_switch("resume", Some(&destination_path.to_string_lossy()))
            .await
        {
            return Ok(true);
        }

        let previous_session_file = self.session.session_file();
        if resolved_path != destination_path {
            std::fs::copy(&resolved_path, &destination_path)?;
        }

        let session_manager = SessionManager::open(
            &destination_path,
            Some(&session_dir),
            cwd_override.map(Path::new),
        )?;
        assert_session_cwd_exists(&session_manager, self.cwd())
            .map_err(|error| PirError::Session(error.to_string()))?;
        let new_cwd = session_manager.get_cwd().to_path_buf();
        let target_file = session_manager
            .get_session_file()
            .map(|p| p.to_string_lossy().into_owned());

        self.teardown_current(SessionShutdownReason::Resume, target_file)
            .await;
        self.apply(
            (self.create_runtime)(CreateRuntimeOptions {
                cwd: new_cwd,
                agent_dir: self.services.agent_dir.clone(),
                session_manager: Arc::new(Mutex::new(session_manager)),
                session_start_event: Some(SessionStartEvent {
                    reason: SessionStartReason::Resume,
                    previous_session_file: previous_session_file
                        .map(|p| p.to_string_lossy().into_owned()),
                }),
            })
            .await?,
        );
        self.finish_session_replacement(None).await;
        Ok(false)
    }

    /// `dispose` (agent-session-runtime.ts:395-402).
    pub async fn dispose(&self) {
        self.session
            .extension_runner()
            .emit("session_shutdown")
            .await;
        self.session.dispose();
    }
}

/// `createAgentSessionRuntime` (agent-session-runtime.ts:411-429).
pub async fn create_agent_session_runtime(
    create_runtime: CreateAgentSessionRuntimeFactory,
    options: CreateRuntimeOptions,
) -> Result<AgentSessionRuntime, PirError> {
    {
        let manager = options
            .session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_session_cwd_exists(&manager, &options.cwd)
            .map_err(|error| PirError::Session(error.to_string()))?;
    }
    let result = (create_runtime)(options).await?;
    Ok(AgentSessionRuntime::new(
        result.session,
        result.services,
        create_runtime,
        result.diagnostics,
        result.model_fallback_message,
    ))
}
