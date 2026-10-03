//! V16-04 FR-H (#9631, f5c946480): per-model image resize profiles thread
//! through the prompt path (`before_agent_start`-selected model) and the
//! tool-result path (current model), and a missing profile keeps the
//! historical global defaults (zero-regression red line).
//!
//! Port of `packages/coding-agent/test/suite/agent-session-prompt.test.ts`
//! (`it("uses the model selected by before_agent_start for image
//! normalization")`) and `agent-session-tool-result-images.test.ts`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi::core::agent_session::{AgentSession, PromptOptions};
use rpi_agent::messages::AgentMessage;
use rpi_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use rpi_ai::types::{
    ImageContent, ModelImageInputLimits, ModelImageResizeOptions, ModelInputLimits,
};
use rpi_ext_host::api::ExtensionApi;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_test_support::faux::{
    FauxAiProvider, FauxAssistantOptions, FauxModelDefinition, FauxProvider, FauxProviderOptions,
    FauxResponseStep, faux_assistant_message, faux_tool_call,
};
use serde_json::{Value, json};

/// 40×20 red PNG (the `read`-side fixtures use 1×1/10×10, too small for a
/// meaningful strict profile).
const IMAGE_40X20_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAACgAAAAUCAIAAABwJOjsAAAAJElEQVR4nO3NMQ0AAAwEofdvupVxCwk7uy3RrGKxWCwWi8WJB336HQ594lo5AAAAAElFTkSuQmCC";

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rpi-image-resize-profile-test-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn strict_resize_profile() -> ModelImageResizeOptions {
    ModelImageResizeOptions {
        max_width: Some(8),
        max_height: Some(8),
        max_bytes: None,
        jpeg_quality: None,
    }
}

fn model_definition(id: &str, profile: Option<ModelImageResizeOptions>) -> FauxModelDefinition {
    FauxModelDefinition {
        id: id.to_owned(),
        name: Some(id.to_owned()),
        reasoning: Some(true),
        input: None,
        input_limits: profile.map(|resize| ModelInputLimits {
            max_request_bytes: None,
            images: Some(ModelImageInputLimits {
                resize: Some(resize),
                max_per_message: None,
                max_per_request: None,
            }),
        }),
        cost: None,
        context_window: Some(200_000),
        max_tokens: Some(8192),
    }
}

/// Inline extension installing a `before_agent_start` handler that switches
/// the session to the given model before prompt assembly continues.
async fn host_setting_model(model: rpi_ai::types::Model) -> Arc<NativeExtensionHost> {
    let host = NativeExtensionHost::new("/v16-04-cwd");
    let model_json = serde_json::to_value(&model).expect("model json");
    let factory: ExtensionFactory = Arc::new(move |api: ExtensionApi| {
        let api_for_handler = api.clone();
        let model_json = model_json.clone();
        api.on(
            "before_agent_start",
            Arc::new(move |_payload, _ctx| {
                let api = api_for_handler.clone();
                let model_json = model_json.clone();
                Box::pin(async move {
                    api.set_model(model_json)
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok(Value::Null)
                })
            }),
        )
        .expect("register before_agent_start handler");
        Box::pin(async { Ok(()) })
    });
    let errors = host
        .load_inline(&[InlineExtension::Anonymous(factory)])
        .await;
    assert!(errors.is_empty(), "extension load errors: {errors:?}");
    Arc::new(host)
}

struct ImageTool;

#[async_trait]
impl AgentTool for ImageTool {
    fn name(&self) -> &str {
        "image_tool"
    }
    fn label(&self) -> &str {
        "image_tool"
    }
    fn description(&self) -> &str {
        "returns a fixed 40x20 image"
    }
    fn parameters(&self) -> &Value {
        static PARAMS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        PARAMS.get_or_init(|| json!({"type": "object", "properties": {}}))
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        Ok(AgentToolResult {
            content: vec![rpi_ai::types::ToolResultContent::Image(ImageContent {
                data: IMAGE_40X20_PNG_BASE64.to_owned(),
                mime_type: "image/png".to_owned(),
            })],
            ..Default::default()
        })
    }
}

struct SessionFixture {
    session: AgentSession,
    _tmp: TempDir,
}

async fn session_fixture(
    responses: Vec<FauxResponseStep>,
    host: Option<Arc<NativeExtensionHost>>,
    custom_tools: Vec<Arc<dyn AgentTool>>,
    tools: Option<Vec<String>>,
    initial_model_id: &str,
) -> SessionFixture {
    let tmp = TempDir::new();
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![
            model_definition("loose", None),
            model_definition("strict", Some(strict_resize_profile())),
        ]),
        ..Default::default()
    });
    provider.set_responses(responses);
    let model = provider
        .get_model(Some(initial_model_id))
        .expect("initial faux model");

    let model_runtime = rpi::core::model_runtime::ModelRuntime::create(
        rpi::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: rpi::core::model_runtime::ModelsPathInput::Path(
                agent_dir.join("models.json"),
            ),
            ..Default::default()
        },
    )
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("register faux provider");

    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");

    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session"),
    ));
    let host_for_bind = host.clone();
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        tools,
        custom_tools,
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: host,
        ..Default::default()
    })
    .await
    .expect("create session");
    if let Some(host) = host_for_bind {
        // app.rs wiring: `pi.setModel()`/`setActiveTools()` need the session
        // actions bound to the host after session creation.
        rpi::core::extension_actions::bind_session_actions(&host, &created.session).await;
    }

    SessionFixture {
        session: created.session,
        _tmp: tmp,
    }
}

fn prompt_options_with_image() -> PromptOptions {
    PromptOptions {
        images: Some(vec![ImageContent {
            data: IMAGE_40X20_PNG_BASE64.to_owned(),
            mime_type: "image/png".to_owned(),
        }]),
        ..Default::default()
    }
}

/// Text + image blocks of the first user message.
fn first_user_content(session: &AgentSession) -> (String, Vec<ImageContent>) {
    let messages = session.messages();
    for message in messages {
        if let AgentMessage::User(user) = message {
            let mut text = String::new();
            let mut images = Vec::new();
            if let rpi_ai::types::UserContent::Blocks(blocks) = user.content {
                for block in blocks {
                    match block {
                        rpi_ai::types::UserContentBlock::Text(t) => text.push_str(&t.text),
                        rpi_ai::types::UserContentBlock::Image(image) => images.push(image),
                    }
                }
            }
            return (text, images);
        }
    }
    panic!("no user message in transcript");
}

// ---------------------------------------------------------------------------
// FR-H entry (1): prompt images / before_agent_start-selected model
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_images_follow_the_before_agent_start_selected_model() {
    // The strict model is selected from inside the hook, so normalization
    // must run after the hook (agent-session.ts:2024-2029 @ f5c946480).
    let strict = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![model_definition(
            "strict",
            Some(strict_resize_profile()),
        )]),
        ..Default::default()
    })
    .get_model(Some("strict"))
    .expect("strict model");
    let host = host_setting_model(strict).await;

    let fixture = session_fixture(
        vec![faux_assistant_message("done", FauxAssistantOptions::default()).into()],
        Some(host),
        Vec::new(),
        None,
        "loose",
    )
    .await;
    fixture
        .session
        .prompt("inspect", prompt_options_with_image())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    assert_eq!(
        fixture.session.model().expect("model").id,
        "strict",
        "hook selected the strict model"
    );
    let (text, images) = first_user_content(&fixture.session);
    assert_eq!(images.len(), 1, "prompt image survived");
    assert!(
        text.contains("displayed at 8x4"),
        "normalization used the hook-selected profile: {text}"
    );
    assert!(text.contains("original 40x20"), "dimension note: {text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_images_without_a_profile_are_byte_identical() {
    // Zero-regression red line: no `inputLimits.images.resize` → the
    // historical global defaults and an untouched payload (R2.3.2).
    let fixture = session_fixture(
        vec![faux_assistant_message("done", FauxAssistantOptions::default()).into()],
        None,
        Vec::new(),
        None,
        "loose",
    )
    .await;
    fixture
        .session
        .prompt("inspect", prompt_options_with_image())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    let (text, images) = first_user_content(&fixture.session);
    assert_eq!(images.len(), 1);
    assert_eq!(
        images[0].data, IMAGE_40X20_PNG_BASE64,
        "no profile → byte-identical image"
    );
    assert!(
        !text.contains("displayed at"),
        "no resize hint without a profile: {text}"
    );
}

// ---------------------------------------------------------------------------
// FR-H entry (3): tool-result images / current model
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_result_images_use_the_current_model_profile() {
    let responses = vec![
        faux_assistant_message(
            vec![faux_tool_call("image_tool", serde_json::Map::new(), None)],
            FauxAssistantOptions {
                stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
                ..Default::default()
            },
        )
        .into(),
        faux_assistant_message("done", FauxAssistantOptions::default()).into(),
    ];
    let fixture = session_fixture(
        responses,
        None,
        vec![Arc::new(ImageTool)],
        Some(vec!["image_tool".to_owned()]),
        "strict",
    )
    .await;
    fixture
        .session
        .prompt("take a screenshot", PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    let tool_result = fixture
        .session
        .messages()
        .into_iter()
        .find_map(|message| match message {
            AgentMessage::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("tool result message");
    let text: String = tool_result
        .content
        .iter()
        .filter_map(|block| match block {
            rpi_ai::types::ToolResultContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("displayed at 8x4"),
        "tool-result normalization used the current model profile: {text}"
    );
}
