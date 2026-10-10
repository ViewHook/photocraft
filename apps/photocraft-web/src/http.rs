//! `Services::http` for Window › FrameForge, through ehttp: the browser's `fetch` here (the
//! FrameForge server must list this page's origin in `FRAMEFORGE_NATIVE_CORS_ORIGINS`).
//! `apps/photocraft/src/http.rs` is the same adapter for the desktop app.

use std::ops::ControlFlow;
use std::sync::{Mutex, PoisonError};

use ehttp::streaming::Part;
use photocraft_ui_egui::{HttpDone, HttpFn, HttpRequest, HttpResponse};

/// FrameForge's model calls have a 240-second deadline; leave room for the transfer.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub fn service() -> HttpFn {
    Box::new(|request: HttpRequest, done: HttpDone| {
        let method = match request.method.as_str() {
            "GET" => ehttp::Method::GET,
            "POST" => ehttp::Method::POST,
            other => return done(Err(format!("unsupported HTTP method {other}"))),
        };
        let max = request.max_response_bytes;
        let mut req = ehttp::Request::new(method, request.url, ehttp::Headers { headers: request.headers });
        req.body = request.body;
        req.timeout = Some(TIMEOUT);
        // The body arrives in chunks and reading stops past `max` (or when `Content-Length`
        // announces more), so a server can't make the app allocate more than it asked for.
        let state: Mutex<(Option<HttpDone>, Option<HttpResponse>)> = Mutex::new((Some(done), None));
        ehttp::streaming::fetch(req, move |part| {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            let (done, response) = &mut *state;
            if done.is_none() {
                return ControlFlow::Break(());
            }
            let outcome = match (part, response.as_mut()) {
                (Err(e), _) => Err(e),
                (Ok(Part::Response(head)), None) => {
                    if head.headers.get("content-length").and_then(|v| v.trim().parse::<u64>().ok()).is_some_and(|n| n > max as u64) {
                        Err(too_large(max))
                    } else {
                        *response = Some(HttpResponse { status: head.status, headers: head.headers.headers, body: Vec::new() });
                        return ControlFlow::Continue(());
                    }
                }
                // An empty chunk ends the body.
                (Ok(Part::Chunk(chunk)), Some(r)) if chunk.is_empty() => Ok(std::mem::take(r)),
                (Ok(Part::Chunk(chunk)), Some(r)) => {
                    if r.body.len().saturating_add(chunk.len()) > max {
                        Err(too_large(max))
                    } else {
                        r.body.extend_from_slice(&chunk);
                        return ControlFlow::Continue(());
                    }
                }
                _ => Err("unexpected HTTP response stream".to_string()),
            };
            if let Some(done) = done.take() {
                done(outcome);
            }
            ControlFlow::Break(())
        });
    })
}

fn too_large(max: usize) -> String {
    format!("the response is larger than {max} bytes")
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    /// One request to a loopback stand-in for the server (no FrameForge server involved):
    /// returns what it received, and answers with `answer`.
    fn serve(answer: &'static [u8]) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            // Headers, then the body its Content-Length announces.
            loop {
                let n = stream.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if got.len() >= end + 4 + length {
                        break;
                    }
                }
                assert!(n > 0, "connection closed early");
            }
            // The client may hang up early (a refused answer): that's fine here.
            let _ = stream.write_all(answer);
            got
        });
        (url, server)
    }

    fn call(request: HttpRequest) -> Result<HttpResponse, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        service()(request, Box::new(move |answer| tx.send(answer).unwrap()));
        rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap()
    }

    #[test]
    fn posts_and_returns_status_headers_and_body() {
        let (url, server) = serve(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nRetry-After: 5\r\nX-FrameForge-Warnings: [\"w\"]\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"error\":\"busy\"}",
        );
        let r = call(HttpRequest {
            method: "POST".into(),
            url: format!("{url}/api/native/v1/concepts"),
            headers: vec![("Authorization".into(), "Bearer test-token".into()), ("Content-Type".into(), "application/json".into())],
            body: b"{\"video\":{}}".to_vec(),
            max_response_bytes: 1024,
        })
        .unwrap();
        assert_eq!(r.status, 503);
        assert_eq!(r.header("retry-after"), Some("5"));
        assert_eq!(r.header("x-frameforge-warnings"), Some("[\"w\"]"));
        assert_eq!(r.body, b"{\"error\":\"busy\"}");
        let got = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(got.starts_with("POST /api/native/v1/concepts HTTP/1.1\r\n"), "{got}");
        assert!(got.to_ascii_lowercase().contains("authorization: bearer test-token\r\n"), "{got}");
        assert!(got.ends_with("\r\n\r\n{\"video\":{}}"), "{got}");
    }

    #[test]
    fn gets_and_reports_transport_errors() {
        let (url, server) = serve(b"HTTP/1.1 200 OK\r\nContent-Type: font/woff2\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwOF2");
        let r =
            call(HttpRequest { method: "GET".into(), url: format!("{url}/api/native/v1/fonts/a.woff2"), max_response_bytes: 4, ..Default::default() }).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, b"wOF2".as_slice()));
        assert!(String::from_utf8(server.join().unwrap()).unwrap().starts_with("GET /api/native/v1/fonts/a.woff2 HTTP/1.1"));
        // Port 0 never accepts a connection: a transport error, not a response.
        assert!(call(HttpRequest { method: "GET".into(), url: "http://127.0.0.1:0/api/native/v1/info".into(), ..Default::default() }).is_err());
        assert!(call(HttpRequest { method: "PUT".into(), url, ..Default::default() }).unwrap_err().contains("unsupported"));
    }

    #[test]
    fn stops_reading_past_the_response_cap() {
        // Chunked, so no Content-Length warns in advance: two 32-byte chunks.
        const CHUNKED: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n20\r\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n20\r\nbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\r\n0\r\n\r\n";
        let get = |url: String, max| call(HttpRequest { method: "GET".into(), url, max_response_bytes: max, ..Default::default() });
        let (url, server) = serve(CHUNKED);
        assert_eq!(get(format!("{url}/a"), 16).unwrap_err(), "the response is larger than 16 bytes");
        server.join().unwrap();
        let (url, server) = serve(CHUNKED);
        assert_eq!(get(format!("{url}/a"), 64).unwrap().body.len(), 64, "exactly the cap is fine");
        server.join().unwrap();
        // A Content-Length over the cap is refused before the body is read.
        let (url, server) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 1099511627776\r\nConnection: close\r\n\r\nshort");
        assert_eq!(get(format!("{url}/a"), 1024).unwrap_err(), "the response is larger than 1024 bytes");
        server.join().unwrap();
    }
}
