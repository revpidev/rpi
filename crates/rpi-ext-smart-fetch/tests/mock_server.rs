//! Pipeline-layer mock-server tests (design §5.3): the REAL wreq fingerprint
//! client through the default pipeline against a local HTTP responder —
//! status codes, content-type branches, redirect following, request headers,
//! decompression-free large bodies and true transport timeouts.

mod common;

use common::{Responder, HTML_PAGE};
use rpi_ext_smart_fetch::pipeline::FetchPipeline;
use rpi_ext_smart_fetch::types::{FetchOptions, FetchOutcome};

fn opts(url: String) -> FetchOptions {
    FetchOptions {
        url,
        browser: None,
        os: None,
        headers: None,
        format: None,
        max_chars: None,
        remove_images: None,
        include_replies: None,
        proxy: None,
        timeout_ms: Some(8_000),
        temp_dir: None,
    }
}

async fn fetch(url: String) -> FetchOutcome {
    FetchPipeline::default().fetch(&opts(url)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn html_page_extracts_and_sends_default_headers() {
    let server = Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "text/html; charset=utf-8".to_string())],
        HTML_PAGE.to_string(),
    )]);
    let url = server.url("/article");
    let outcome = fetch(url.clone()).await;
    let FetchOutcome::Result(result) = outcome else {
        panic!("expected content result, got {outcome:?}");
    };
    assert_eq!(result.title, "Mock Page");
    assert!(
        result.content.contains("Mock Article"),
        "{}",
        result.content
    );
    assert!(result.word_count > 10);

    let request = server.next_request();
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/article");
    // FR-P0-4: format=markdown Accept + default Accept-Language
    assert_eq!(
        server.header(&request, "accept"),
        Some("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
    );
    assert_eq!(
        server.header(&request, "accept-language"),
        Some("en-US,en;q=0.9")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_endpoint_pretty_prints_in_markdown_mode() {
    let server = Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "application/json".to_string())],
        r#"{"b":1,"a":[1,2]}"#.to_string(),
    )]);
    let FetchOutcome::Result(result) = fetch(server.url("/api")).await else {
        panic!("expected result");
    };
    assert!(result.content.contains("```json"));
    assert!(result.content.contains("\"b\": 1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_error_maps_to_template() {
    let server = Responder::start(vec![(
        "404 Not Found".to_string(),
        vec![("Content-Type", "text/html".to_string())],
        "gone".to_string(),
    )]);
    let FetchOutcome::Error(error) = fetch(server.url("/missing")).await else {
        panic!("expected error");
    };
    assert_eq!(error.code.map(|c| c.as_str()), Some("http_error"));
    assert!(error.error.contains("Server returned HTTP 404 Not Found"));
    assert_eq!(error.retryable, Some(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirect_following_records_final_url() {
    let server = Responder::start(vec![
        (
            "302 Found".to_string(),
            vec![("Location", "/final".to_string())],
            String::new(),
        ),
        (
            "200 OK".to_string(),
            vec![("Content-Type", "text/plain".to_string())],
            "landed".to_string(),
        ),
    ]);
    let outcome = fetch(server.url("/start")).await;
    let FetchOutcome::Result(result) = outcome else {
        panic!("expected result, got {outcome:?}");
    };
    assert!(result.final_url.ends_with("/final"), "{}", result.final_url);
    assert_eq!(result.content, "landed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_headers_override_defaults() {
    let server = Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "text/plain".to_string())],
        "ok".to_string(),
    )]);
    let mut options = opts(server.url("/custom"));
    options.headers = Some(
        [("Accept".to_string(), "text/custom".to_string())]
            .into_iter()
            .collect(),
    );
    let FetchOutcome::Result(_) = FetchPipeline::default().fetch(&options).await else {
        panic!("expected result");
    };
    let request = server.next_request();
    assert_eq!(server.header(&request, "accept"), Some("text/custom"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_server_times_out_with_template_error() {
    let server = Responder::start(vec![(
        "200 OK".to_string(),
        Vec::new(),
        "<delay:4000>".to_string(),
    )]);
    let mut options = opts(server.url("/slow"));
    options.timeout_ms = Some(500);
    let start = std::time::Instant::now();
    let FetchOutcome::Error(error) = FetchPipeline::default().fetch(&options).await else {
        panic!("expected timeout error");
    };
    assert!(
        start.elapsed() < std::time::Duration::from_secs(3),
        "timed out promptly"
    );
    assert_eq!(error.code.map(|c| c.as_str()), Some("timeout"));
    assert!(
        error.error.contains("Timeout of 500ms exceeded"),
        "{}",
        error.error
    );
    assert_eq!(error.retryable, Some(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_text_body_streams_to_string() {
    // ~2 MB body through the full client + pipeline (no decompression involved)
    let body = "0123456789abcdef".repeat(128 * 1024);
    let expected_len = body.len();
    let server = Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "text/plain".to_string())],
        body.clone(),
    )]);
    let mut options = opts(server.url("/big"));
    options.max_chars = Some(50);
    let FetchOutcome::Result(result) = FetchPipeline::default().fetch(&options).await else {
        panic!("expected result");
    };
    assert!(expected_len > 1_000_000);
    assert!(result.content.ends_with("[... truncated]"));
    assert!(result.content.len() < 100);
    // 50 truncated chars contain no whitespace → a single word
    assert_eq!(result.word_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_profile_rejected_before_request() {
    let server = Responder::start(Vec::new());
    let mut options = opts(server.url("/never"));
    options.browser = Some("chrome_99".to_string());
    let FetchOutcome::Error(error) = FetchPipeline::default().fetch(&options).await else {
        panic!("expected error");
    };
    assert!(
        error.error.contains("Invalid browser profile: chrome_99"),
        "{}",
        error.error
    );
    assert_eq!(error.code.map(|c| c.as_str()), Some("network_error"));
    assert_eq!(error.phase.map(|p| p.as_str()), Some("connecting"));
}
