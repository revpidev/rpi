use std::sync::{Arc, Mutex};

use rpi_ai::models::{
    CreateModelsOptions, CreateProviderOptions, Models, ModelsStreamOptions, Provider, ProviderApi,
    ProviderStreams, create_provider,
};
use rpi_ai::types::{
    ApiKind, Context, Model, ProviderHeaders, SimpleStreamOptions, StopReason, StreamOptions,
    TranscriptContext,
};
use rpi_ai::utils::event_stream::AssistantMessageEventStream;
use rpi_ai::utils::identity::{ClientIdentity, UserAgentMode, with_client_identity};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug)]
struct CapturedRequest {
    headers: Vec<(String, String)>,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

async fn serve_completion() -> (String, tokio::sync::oneshot::Receiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        let header_end = loop {
            let n = socket.read(&mut buffer).await.expect("read");
            assert!(n > 0, "connection closed before request headers");
            request.extend_from_slice(&buffer[..n]);
            if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let head = String::from_utf8_lossy(&request[..header_end]);
        let headers = head
            .lines()
            .skip(1)
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_lowercase(), value.trim().to_owned()))
            })
            .collect();
        tx.send(CapturedRequest { headers }).expect("capture");
        let body = concat!(
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,",
            "\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    (format!("http://{addr}"), rx)
}

fn completions_model(provider: &Arc<dyn Provider>) -> Model {
    provider
        .get_models()
        .into_iter()
        .find(|model| model.api == ApiKind::from(ApiKind::OPENAI_COMPLETIONS))
        .expect("OpenAI completions model")
}

async fn execute(
    provider: Arc<dyn Provider>,
    identity: ClientIdentity,
    request_headers: ProviderHeaders,
    mutate_model: impl FnOnce(&mut Model),
    before_headers: Option<Arc<Mutex<Option<ProviderHeaders>>>>,
    session_id: Option<&str>,
) -> CapturedRequest {
    let (base_url, captured) = serve_completion().await;
    let mut model = completions_model(&provider);
    model.base_url = base_url;
    mutate_model(&mut model);
    let models = Models::new(Some(CreateModelsOptions {
        identity: Some(identity),
        credentials: None,
        auth_context: None,
        models_store: None,
    }));
    models.set_provider(provider);
    let transform_headers = before_headers.map(|captured| {
        Arc::new(
            move |headers: ProviderHeaders| -> futures::future::BoxFuture<'static, ProviderHeaders> {
                *captured.lock().expect("capture hook") = Some(headers.clone());
                Box::pin(async move { headers })
            },
        ) as rpi_ai::models::ModelsRequestTransforms
    });
    let message = models
        .stream(
            &model,
            &Context::default(),
            Some(ModelsStreamOptions {
                stream: StreamOptions {
                    request: rpi_ai::types::ProviderRequestOptions {
                        api_key: Some("test-key".to_owned()),
                        headers: Some(request_headers),
                        ..Default::default()
                    },
                    session_id: session_id.map(str::to_owned),
                    ..Default::default()
                },
                transform_headers,
            }),
        )
        .result()
        .await
        .expect("completion");
    assert_eq!(message.stop_reason, StopReason::Stop, "{message:?}");
    captured.await.expect("captured request")
}

#[tokio::test]
async fn openrouter_identity_is_below_headers_and_invisible_to_transform() {
    let before_headers = Arc::new(Mutex::new(None));
    let request = execute(
        rpi_ai::providers::openrouter::openrouter_provider(),
        ClientIdentity::new("9.8.7", true),
        [
            ("user-agent".to_owned(), None),
            ("X-OpenRouter-Title".to_owned(), None),
            (
                "HTTP-Referer".to_owned(),
                Some("https://request.test".to_owned()),
            ),
        ]
        .into(),
        |model| {
            model.headers = Some(
                [(
                    "X-OpenRouter-Categories".to_owned(),
                    "model-category".to_owned(),
                )]
                .into(),
            );
        },
        Some(before_headers.clone()),
        None,
    )
    .await;
    assert_eq!(request.header("user-agent"), None);
    assert_eq!(request.header("x-openrouter-title"), None);
    assert_eq!(request.header("http-referer"), Some("https://request.test"));
    assert_eq!(
        request.header("x-openrouter-categories"),
        Some("model-category")
    );
    let before = before_headers.lock().expect("hook");
    let before = before.as_ref().expect("transform ran");
    assert_eq!(before.get("user-agent"), Some(&None));
    assert_eq!(before.get("X-OpenRouter-Title"), Some(&None));
    assert_eq!(
        before.get("HTTP-Referer"),
        Some(&Some("https://request.test".to_owned()))
    );
    assert_eq!(
        before.get("X-OpenRouter-Categories"),
        Some(&Some("model-category".to_owned()))
    );
}

#[tokio::test]
async fn nvidia_and_opencode_send_their_attribution_headers() {
    let nvidia = execute(
        rpi_ai::providers::nvidia::nvidia_provider(),
        ClientIdentity::new("9.8.7", true),
        ProviderHeaders::new(),
        |_| {},
        None,
        None,
    )
    .await;
    assert_eq!(nvidia.header("x-billing-invoke-origin"), Some("Rpi"));
    assert!(
        nvidia
            .header("user-agent")
            .is_some_and(|ua| ua.starts_with("rpi/9.8.7 "))
    );
    assert_eq!(nvidia.header("x-opencode-client"), None);

    let before_headers = Arc::new(Mutex::new(None));
    let opencode = execute(
        rpi_ai::providers::opencode::opencode_provider(),
        ClientIdentity::new("9.8.7", true),
        ProviderHeaders::new(),
        |_| {},
        Some(before_headers.clone()),
        Some("session-test"),
    )
    .await;
    assert_eq!(opencode.header("x-opencode-client"), Some("rpi"));
    assert_eq!(opencode.header("x-opencode-session"), Some("session-test"));
    assert_eq!(opencode.header("x-billing-invoke-origin"), None);
    let before = before_headers.lock().expect("hook");
    let before = before.as_ref().expect("transform ran");
    assert_eq!(
        before.get("x-opencode-session"),
        Some(&Some("session-test".to_owned()))
    );
    assert!(!before.keys().any(|key| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "user-agent" | "x-opencode-client"
        )
    }));
}

#[derive(Clone, Default)]
struct CaptureIdentityStreams {
    identity: Arc<Mutex<Option<ClientIdentity>>>,
}

impl ProviderStreams for CaptureIdentityStreams {
    fn stream(
        &self,
        model: &Model,
        _context: &TranscriptContext,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        *self.identity.lock().expect("identity") =
            options.and_then(|options| options.identity.clone());
        let stream = AssistantMessageEventStream::new();
        stream.push(rpi_ai::types::StreamEvent::Error {
            reason: rpi_ai::types::ErrorReason::Error,
            error: rpi_ai::api::lazy::create_setup_error_message_for_model(
                model,
                "identity captured",
            ),
        });
        stream.end(None);
        stream
    }

    fn stream_simple(
        &self,
        _model: &Model,
        _context: &TranscriptContext,
        _options: Option<SimpleStreamOptions>,
    ) -> Result<AssistantMessageEventStream, String> {
        Err("not used".to_owned())
    }
}

#[tokio::test]
async fn models_wrapper_and_request_merge_identity_with_an_explicit_off_switch() {
    let provider = rpi_ai::providers::openrouter::openrouter_provider();
    let model = completions_model(&provider);
    let streams = CaptureIdentityStreams::default();
    let models = Models::new(Some(CreateModelsOptions {
        identity: Some(ClientIdentity::new("base", true)),
        ..Default::default()
    }));
    models.set_provider(create_provider(CreateProviderOptions {
        id: model.provider.clone(),
        auth: provider.auth().clone(),
        models: vec![model.clone()],
        api: ProviderApi::Single(Arc::new(streams.clone())),
        ..Default::default()
    }));
    let models = with_client_identity(
        &models,
        ClientIdentity {
            user_agent: Some(UserAgentMode::Short),
            version: Some("wrapper".to_owned()),
            ..Default::default()
        },
    );
    let _ = models
        .stream(
            &model,
            &Context::default(),
            Some(ModelsStreamOptions {
                stream: StreamOptions {
                    request: rpi_ai::types::ProviderRequestOptions {
                        api_key: Some("test-key".to_owned()),
                        identity: Some(ClientIdentity {
                            version: Some("request".to_owned()),
                            attribution: Some(None),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                transform_headers: None,
            }),
        )
        .result()
        .await;
    let identity = streams
        .identity
        .lock()
        .expect("identity")
        .clone()
        .expect("merged identity");
    assert_eq!(identity.user_agent, Some(UserAgentMode::Short));
    assert_eq!(identity.version.as_deref(), Some("request"));
    assert_eq!(identity.attribution, Some(None));
}
