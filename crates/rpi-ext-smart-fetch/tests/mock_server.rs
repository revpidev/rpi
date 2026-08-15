//! Pipeline-layer mock-server tests (design §5.3): the REAL wreq fingerprint
//! client through the default pipeline against a local HTTP responder —
//! status codes, content-type branches, redirect following, request headers,
//! decompression-free large bodies and true transport timeouts. TE07 adds
//! the FR-P1 surfaces: meta-refresh recursion, alternate fallback, streaming
//! downloads and the batch worker pool's concurrency bound.

mod common;

use common::{ConcurrentResponder, Responder, HTML_PAGE};
use rpi_ext_smart_fetch::batch;
use rpi_ext_smart_fetch::pipeline::FetchPipeline;
use rpi_ext_smart_fetch::types::FetchToolConfig;
use rpi_ext_smart_fetch::types::{FetchOptions, FetchOutcome, WebFetchParams};

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

// ---------------------------------------------------------------------------
// TE07: FR-P1-2 meta refresh / FR-P1-3 alternate / FR-P1-4 download
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn meta_refresh_follows_to_final_page() {
    let refresh_page = "<html><head><meta http-equiv=\"refresh\" content=\"0;url=/landed\"></head><body>redirecting</body></html>";
    let server = Responder::start(vec![
        (
            "200 OK".to_string(),
            vec![("Content-Type", "text/html".to_string())],
            refresh_page.to_string(),
        ),
        (
            "200 OK".to_string(),
            vec![("Content-Type", "text/html".to_string())],
            "<html><head><title>Landed</title></head><body><article><h1>Final Destination</h1><p>The landed article body carries several sentences of real prose so the readability extractor produces a content result on the follow-up page after the client-side redirect fires.</p></article></body></html>".to_string(),
        ),
    ]);
    let FetchOutcome::Result(result) = fetch(server.url("/start")).await else {
        panic!("expected content result");
    };
    assert!(
        result.final_url.ends_with("/landed"),
        "final url: {}",
        result.final_url
    );
    assert_eq!(result.title, "Landed");
    assert!(result.content.contains("Final Destination"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn meta_refresh_loop_reports_too_many_redirects() {
    // every page redirects to the next; the chain exceeds the budget of 5
    #[allow(clippy::type_complexity)]
    let script: Vec<(String, Vec<(&'static str, String)>, String)> = (0..6)
        .map(|hop| {
            (
                "200 OK".to_string(),
                vec![("Content-Type", "text/html".to_string())],
                format!(
                    "<html><head><meta http-equiv=\"refresh\" content=\"0;url=/hop-{}\"></head><body>loop</body></html>",
                    hop + 1
                ),
            )
        })
        .collect();
    let server = Responder::start(script);
    let FetchOutcome::Error(error) = fetch(server.url("/hop-0")).await else {
        panic!("expected too_many_redirects");
    };
    assert_eq!(
        error.code.map(|c| c.as_str()),
        Some("too_many_redirects"),
        "{}",
        error.error
    );
    assert!(error
        .error
        .contains("Client-side redirect limit (5) exceeded"));
    assert_eq!(error.phase.map(|p| p.as_str()), Some("loading"));
    assert_eq!(error.retryable, Some(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thin_content_follows_alternate_markdown_link() {
    // a 3-word extraction (< 30) with a qualified alternate link → follow
    let thin_page = "<html><head><title>Thin</title>\
        <link rel=\"alternate\" type=\"text/markdown\" href=\"/full.md\">\
        </head><body><article><p>too thin</p></article></body></html>";
    let full_markdown = "# Full Markdown\n\nThis markdown alternate response supplies a body with plenty of words so the thin-content fallback accepts it as the final extraction result.";
    let server = Responder::start(vec![
        (
            "200 OK".to_string(),
            vec![("Content-Type", "text/html".to_string())],
            thin_page.to_string(),
        ),
        (
            "200 OK".to_string(),
            vec![("Content-Type", "text/markdown".to_string())],
            full_markdown.to_string(),
        ),
    ]);
    let FetchOutcome::Result(result) = fetch(server.url("/thin")).await else {
        panic!("expected content result");
    };
    assert!(
        result.final_url.ends_with("/full.md"),
        "final url: {}",
        result.final_url
    );
    assert!(
        result.content.contains("Full Markdown"),
        "{}",
        result.content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thin_content_without_alternate_keeps_result() {
    // thin extraction, no alternate link → the thin result is KEPT
    // (extract.ts:1653-1654: `if (alternateResult) return` — null falls
    // through to the normal output), it is not a no_content error
    let thin_page = "<html><head><title>Only Thin</title></head><body><article><p>too thin</p></article></body></html>";
    let server = Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "text/html".to_string())],
        thin_page.to_string(),
    )]);
    let FetchOutcome::Result(result) = fetch(server.url("/thin-alone")).await else {
        panic!("expected a kept thin result");
    };
    assert!(result.content.contains("too thin"), "{}", result.content);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attachment_downloads_to_temp_dir() {
    let body = "PDF-BYTES-0123456789".repeat(64);
    let server = Responder::start(vec![(
        "200 OK".to_string(),
        vec![
            ("Content-Type", "application/pdf".to_string()),
            (
                "Content-Disposition",
                "attachment; filename=\"Mock Report.pdf\"".to_string(),
            ),
        ],
        body.clone(),
    )]);
    let temp_dir =
        std::env::temp_dir().join(format!("rpi-sf-dl-{}-attachment", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);

    let mut options = opts(server.url("/files/report.pdf"));
    options.temp_dir = Some(temp_dir.to_string_lossy().into_owned());
    let FetchOutcome::Result(result) = FetchPipeline::default().fetch(&options).await else {
        panic!("expected file result");
    };
    assert_eq!(result.kind, "file");
    assert_eq!(result.mime_type.as_deref(), Some("application/pdf"));
    assert_eq!(result.file_size, Some(body.len() as u64));
    assert_eq!(result.content, "");
    let file_path = result.file_path.expect("filePath set");
    assert_eq!(
        std::path::Path::new(&file_path)
            .file_name()
            .and_then(|n| n.to_str()),
        Some("Mock-Report.pdf"),
        "sanitized disposition filename: {file_path}"
    );
    let on_disk = std::fs::read(&file_path).expect("downloaded file");
    assert_eq!(String::from_utf8_lossy(&on_disk), body);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&file_path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "downloaded files are 0600");
    }
    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_eexist_retries_with_suffix() {
    let body = "BINARY-DATA-1234567890".repeat(32);
    let server = Responder::start(vec![(
        "200 OK".to_string(),
        vec![
            ("Content-Type", "application/octet-stream".to_string()),
            (
                "Content-Disposition",
                "attachment; filename=\"data.bin\"".to_string(),
            ),
        ],
        body.clone(),
    )]);
    let temp_dir = std::env::temp_dir().join(format!("rpi-sf-dl-{}-eexist", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();
    // pre-create the target name → the wx open collides → `<base>-1<ext>`
    std::fs::write(temp_dir.join("data.bin"), "occupied").unwrap();

    let mut options = opts(server.url("/blob"));
    options.temp_dir = Some(temp_dir.to_string_lossy().into_owned());
    let FetchOutcome::Result(result) = FetchPipeline::default().fetch(&options).await else {
        panic!("expected file result");
    };
    let file_path = result.file_path.expect("filePath set");
    assert!(
        file_path.ends_with("data-1.bin"),
        "EEXIST retry name: {file_path}"
    );
    assert_eq!(result.file_size, Some(body.len() as u64));
    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_binary_streams_to_disk() {
    // ~8 MB body through the streaming download path (never buffered into
    // the text pipeline)
    let chunk = "0123456789abcdef".repeat(64); // 1 KiB
    let body = chunk.repeat(8 * 1024); // 8 MiB
    let expected_len = body.len();
    let server = Responder::start(vec![(
        "200 OK".to_string(),
        vec![("Content-Type", "application/zip".to_string())],
        body,
    )]);
    let temp_dir = std::env::temp_dir().join(format!("rpi-sf-dl-{}-large", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);

    let mut options = opts(server.url("/big-archive.zip"));
    options.temp_dir = Some(temp_dir.to_string_lossy().into_owned());
    let FetchOutcome::Result(result) = FetchPipeline::default().fetch(&options).await else {
        panic!("expected file result");
    };
    assert_eq!(result.file_size, Some(expected_len as u64));
    assert!(
        (expected_len as u64) > 4 * 1024 * 1024,
        "body exceeds any sane buffering threshold"
    );
    let file_path = result.file_path.expect("filePath set");
    assert_eq!(
        std::fs::metadata(&file_path).expect("on disk").len(),
        expected_len as u64
    );
    let _ = std::fs::remove_dir_all(&temp_dir);
}

// ---------------------------------------------------------------------------
// TE07: FR-P1-1 batch worker pool through the real client
// ---------------------------------------------------------------------------

fn web_fetch_params(url: String) -> WebFetchParams {
    WebFetchParams {
        url,
        browser: None,
        os: None,
        headers: None,
        max_chars: None,
        timeout_ms: None,
        format: None,
        remove_images: None,
        include_replies: None,
        proxy: None,
        verbose: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn batch_bounded_concurrency_order_and_isolation() {
    // 12 requests, concurrency 4, each server response delayed 60ms:
    // in-flight must never exceed 4 (and must exceed 1 — real overlap).
    let server = ConcurrentResponder::start(
        "200 OK",
        vec![("Content-Type".to_string(), "text/plain".to_string())],
        "batch item body".to_string(),
        60,
    );
    let requests: Vec<WebFetchParams> = (0..12)
        .map(|index| web_fetch_params(server.url(&format!("/item-{index}"))))
        .collect();
    // one poisoned entry in the middle — per-item error isolation
    let mut mixed = requests.clone();
    mixed.insert(6, web_fetch_params("not a url".to_string()));

    let defaults = batch::resolve_fetch_tool_defaults(&FetchToolConfig {
        batch_concurrency: Some(4.0),
        ..FetchToolConfig::default()
    });
    let pipeline = FetchPipeline::default();
    let result = batch::execute_batch_fetch(&pipeline, &mixed, &defaults).await;

    assert_eq!(result.total, 13);
    assert_eq!(result.succeeded, 12);
    assert_eq!(result.failed, 1);
    assert_eq!(result.batch_concurrency, 4);
    // input order: item 6 (0-based) is the invalid URL
    assert_eq!(mixed[6].url, "not a url");
    assert_eq!(result.items[6].status, "error");
    assert!(
        result.items[6]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Invalid URL: not a url")),
        "{:?}",
        result.items[6].error
    );
    for (index, item) in result.items.iter().enumerate() {
        if index != 6 {
            assert_eq!(
                item.status, "done",
                "item {index} isolated from the failure"
            );
        }
    }
    // every request hit the server; the gauge never exceeded the bound
    let max_in_flight = server.max_in_flight();
    server.stop();
    assert!(
        max_in_flight <= 4,
        "in-flight high-water mark {max_in_flight} exceeded the bound of 4"
    );
    assert!(
        max_in_flight >= 2,
        "no overlap observed (max in-flight {max_in_flight}) — concurrency not exercised"
    );
}
