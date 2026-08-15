//! Shared mini HTTP responder for the TE06 mock-server tests (design §5.3).
//!
//! Hand-rolled on a thread + TcpListener: the parity fixtures already cover
//! pipeline logic through the transport seam — what these tests add is the
//! REAL wreq client (fingerprint, redirects, decompression, timeout) against
//! a local server, plus server-side request-header assertions.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{channel, Receiver};
use std::thread;

/// One scripted response: (status line, response headers, body).
pub type Script = Vec<(String, Vec<(&'static str, String)>, String)>;

// shared by mock_server.rs (full surface) and e2e_real_rpi.rs (start/url
// only) — per-target compilation makes unused members lint-visible
#[allow(dead_code)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

#[allow(dead_code)]
pub struct Responder {
    pub port: u16,
    requests: Receiver<CapturedRequest>,
}

impl Responder {
    /// Serve exactly `script.len()` requests on the listener, then stop.
    /// Each script entry is (status_line, response_headers, body); the special
    /// body marker "<delay:ms>" stalls the response to exercise timeouts.
    pub fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local port");
        let port = listener.local_addr().expect("addr").port();
        let (tx, requests) = channel();
        thread::spawn(move || {
            for (status_line, response_headers, body) in script {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0u8; 8192];
                let mut read = 0usize;
                // read until end of headers (requests are header-only GETs)
                while let Ok(n) = stream.read(&mut buffer[read..]) {
                    read += n;
                    if n == 0
                        || read >= buffer.len()
                        || buffer[..read].windows(4).any(|w| w == b"\r\n\r\n")
                    {
                        break;
                    }
                }
                let raw = String::from_utf8_lossy(&buffer[..read]).to_string();
                let mut lines = raw.split("\r\n");
                let request_line = lines.next().unwrap_or_default().to_string();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let request_headers: Vec<(String, String)> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_string()))
                    .collect();
                let _ = tx.send(CapturedRequest {
                    method,
                    path,
                    headers: request_headers,
                });

                if let Some(delay) = body.strip_prefix("<delay:") {
                    let millis: u64 = delay.trim_end_matches('>').parse().unwrap_or_default();
                    thread::sleep(std::time::Duration::from_millis(millis));
                    // the client times out mid-wait; write nothing durable
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    );
                    continue;
                }
                let mut response = format!("HTTP/1.1 {status_line}\r\n");
                let mut has_length = false;
                for (name, value) in &response_headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                    if name.eq_ignore_ascii_case("content-length") {
                        has_length = true;
                    }
                }
                if !has_length {
                    response.push_str(&format!("Content-Length: {}\r\n", body.len()));
                }
                response.push_str("Connection: close\r\n\r\n");
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });
        Responder { port, requests }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    #[allow(dead_code)]
    pub fn next_request(&self) -> CapturedRequest {
        self.requests
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("server captured a request")
    }

    #[allow(dead_code)]
    pub fn header<'a>(&self, request: &'a CapturedRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[allow(dead_code)]
pub const HTML_PAGE: &str = "<html><head><title>Mock Page</title></head><body><article><h1>Mock Article</h1><p>This mock article carries enough sentences and words for readability scoring so the extraction path produces a real content result in the pipeline integration tests without any network access whatsoever.</p><p>A second paragraph keeps the candidate score comfortably above the thresholds used by the extraction engine.</p></article></body></html>";
