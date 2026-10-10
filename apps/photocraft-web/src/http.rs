//! `Services::http` for Window › FrameForge on the desktop, through ureq: one worker thread per
//! request, with real deadlines (connect, then the whole exchange). [`HttpAbort`] only sets a flag
//! the worker reads before each body chunk; ureq can't interrupt a blocked call from another
//! thread. So an abort while connecting, sending, or waiting for the status line and headers does
//! nothing until the headers arrive or the deadline passes (up to 300 s for a model call); during
//! the body it takes effect when the read in progress returns. Then the worker drops the
//! connection and calls `done` with an error (or with the answer, if the last read finished
//! first). Until then the thread and the connection linger; the caller has stopped waiting.
//! This file is both `apps/photocraft/src/http.rs` (the desktop app's service) and
//! `apps/photocraft-web/src/http.rs`, where its loopback tests run natively (a test keeps the two
//! copies identical). The browser uses `fetch.rs` there instead.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use photocraft_ui_egui::{HttpAbort, HttpDone, HttpFn, HttpRequest, HttpResponse};

/// The deadline when the request names none: FrameForge's model calls take up to 240 seconds,
/// plus the transfer.
const TIMEOUT: Duration = Duration::from_secs(300);
/// Opening the connection (DNS, TCP, TLS) never gets more than this, whatever the deadline.
const CONNECT: Duration = Duration::from_secs(30);
/// The body is read this much at a time; an abort is noticed between reads.
const CHUNK: usize = 64 * 1024;
/// What `done` hears after [`HttpAbort::abort`] (nobody listens by then; it ends the worker).
const ABORTED: &str = "the request was aborted";

pub fn service() -> HttpFn {
    Box::new(|request: HttpRequest, done: HttpDone| {
        if !matches!(request.method.as_str(), "GET" | "POST") {
            done(Err(format!("unsupported HTTP method {}", request.method)));
            return HttpAbort::none();
        }
        let aborted = Arc::new(AtomicBool::new(false));
        // `done` is called once: by the worker, or here when no worker could start.
        let slot = Arc::new(Mutex::new(Some(done)));
        let (flag, worker_slot) = (aborted.clone(), slot.clone());
        let worker = std::thread::Builder::new().name("frameforge-http".into()).spawn(move || {
            let outcome = exchange(&request, &flag);
            if let Some(done) = take(&worker_slot) {
                done(outcome);
            }
        });
        if let Err(e) = worker {
            if let Some(done) = take(&slot) {
                done(Err(format!("couldn't start the HTTP request: {e}")));
            }
            return HttpAbort::none();
        }
        HttpAbort::new(move || aborted.store(true, Ordering::Relaxed))
    })
}

fn take(slot: &Mutex<Option<HttpDone>>) -> Option<HttpDone> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// One request on the worker thread. The agent is the request's own, so dropping it (after an
/// abort, the size cap or the deadline) closes the connection rather than pooling it.
fn exchange(request: &HttpRequest, aborted: &AtomicBool) -> Result<HttpResponse, String> {
    let timeout = request.timeout.unwrap_or(TIMEOUT);
    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(timeout)).timeout_connect(Some(timeout.min(CONNECT))).http_status_as_error(false).build().into();
    let response = if request.method == "POST" {
        let mut call = agent.post(&request.url);
        for (name, value) in &request.headers {
            call = call.header(name.as_str(), value.as_str());
        }
        call.send(request.body.as_slice())
    } else {
        let mut call = agent.get(&request.url);
        for (name, value) in &request.headers {
            call = call.header(name.as_str(), value.as_str());
        }
        call.call()
    }
    .map_err(|e| e.to_string())?;
    let max = request.max_response_bytes;
    let (head, body) = response.into_parts();
    // The body is read in chunks and reading stops past `max` (or when `Content-Length`
    // announces more), so a server can't make the app allocate more than it asked for.
    let announced = head.headers.get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    if announced.is_some_and(|n| n > max as u64) {
        return Err(too_large(max));
    }
    let headers = head.headers.iter().map(|(name, value)| (name.as_str().to_string(), String::from_utf8_lossy(value.as_bytes()).into_owned())).collect();
    let mut reader = body.into_reader();
    let mut out = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        if aborted.load(Ordering::Relaxed) {
            return Err(ABORTED.to_string());
        }
        let n = reader.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > max {
            return Err(too_large(max));
        }
        out.extend_from_slice(&chunk[..n]);
    }
    Ok(HttpResponse { status: head.status.as_u16(), headers, body: out })
}

fn too_large(max: usize) -> String {
    format!("the response is larger than {max} bytes")
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Read, Write};
    use std::net::TcpStream;
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;

    /// A loopback stand-in for the server (no FrameForge server involved) that accepts one
    /// connection and runs `script` on it.
    fn listen<R: Send + 'static>(script: impl FnOnce(TcpStream) -> R + Send + 'static) -> (String, std::thread::JoinHandle<R>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            script(stream)
        });
        (url, server)
    }

    /// The request's headers, then the body its Content-Length announces.
    fn receive(stream: &mut TcpStream) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
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
                    return got;
                }
            }
            assert!(n > 0, "connection closed early");
        }
    }

    /// Returns what it received, and answers with `answer`.
    fn serve(answer: &'static [u8]) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        listen(move |mut stream| {
            let got = receive(&mut stream);
            // The client may hang up early (a refused answer): that's fine here.
            let _ = stream.write_all(answer);
            got
        })
    }

    /// Whether the client hung up: its end is closed, so reading ends (or is reset) instead of
    /// waiting for more.
    fn hung_up(stream: &mut TcpStream) -> bool {
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => return true,
                Ok(_) => continue,
                Err(e) => return matches!(e.kind(), ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe),
            }
        }
    }

    fn start(request: HttpRequest) -> (mpsc::Receiver<Result<HttpResponse, String>>, HttpAbort) {
        let (tx, rx) = mpsc::channel();
        let abort = service()(request, Box::new(move |answer| tx.send(answer).unwrap()));
        (rx, abort)
    }

    fn call(request: HttpRequest) -> Result<HttpResponse, String> {
        start(request).0.recv_timeout(Duration::from_secs(20)).unwrap()
    }

    fn get(url: String, max_response_bytes: usize, timeout: Option<Duration>) -> HttpRequest {
        HttpRequest { method: "GET".into(), url, max_response_bytes, timeout, ..Default::default() }
    }

    #[test]
    fn the_desktop_and_web_copies_of_the_http_adapter_match() {
        assert!(include_str!("../../photocraft/src/http.rs") == include_str!("../../photocraft-web/src/http.rs"), "copy one http.rs over the other");
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
            timeout: Some(Duration::from_secs(20)),
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
        let r = call(get(format!("{url}/api/native/v1/fonts/a.woff2"), 4, None)).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, b"wOF2".as_slice()));
        assert!(String::from_utf8(server.join().unwrap()).unwrap().starts_with("GET /api/native/v1/fonts/a.woff2 HTTP/1.1"));
        // Port 0 never accepts a connection: a transport error, not a response.
        assert!(call(get("http://127.0.0.1:0/api/native/v1/info".into(), 1024, None)).is_err());
        let (rx, abort) = start(HttpRequest { method: "PUT".into(), url, ..Default::default() });
        assert!(rx.recv_timeout(Duration::from_secs(20)).unwrap().unwrap_err().contains("unsupported"));
        assert_eq!(format!("{abort:?}"), "HttpAbort(none)", "nothing started, nothing to abort");
    }

    #[test]
    fn stops_reading_past_the_response_cap() {
        // Chunked, so no Content-Length warns in advance: two 32-byte chunks.
        const CHUNKED: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n20\r\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n20\r\nbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\r\n0\r\n\r\n";
        let (url, server) = serve(CHUNKED);
        assert_eq!(call(get(format!("{url}/a"), 16, None)).unwrap_err(), "the response is larger than 16 bytes");
        server.join().unwrap();
        let (url, server) = serve(CHUNKED);
        assert_eq!(call(get(format!("{url}/a"), 64, None)).unwrap().body.len(), 64, "exactly the cap is fine");
        server.join().unwrap();
        // A Content-Length over the cap is refused before the body is read.
        let (url, server) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 1099511627776\r\nConnection: close\r\n\r\nshort");
        assert_eq!(call(get(format!("{url}/a"), 1024, None)).unwrap_err(), "the response is larger than 1024 bytes");
        server.join().unwrap();
    }

    /// A body that never ends is cut off at the cap, and the connection is closed then (the
    /// server's writes stop being read long before it runs out of body).
    #[test]
    fn an_endless_body_stops_at_the_cap() {
        const MAX: usize = 100 * 1024;
        let (url, server) = listen(|mut stream| {
            receive(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
            let chunk = [b'x'; 0x4000];
            let mut sent = 0usize;
            while sent < 256 << 20 {
                if stream.write_all(b"4000\r\n").and_then(|()| stream.write_all(&chunk)).and_then(|()| stream.write_all(b"\r\n")).is_err() {
                    break;
                }
                sent += chunk.len();
            }
            sent
        });
        assert_eq!(call(get(format!("{url}/endless"), MAX, None)).unwrap_err(), format!("the response is larger than {MAX} bytes"));
        let sent = server.join().unwrap();
        assert!(sent < 256 << 20, "the client hung up: the server stopped after {sent} bytes");
    }

    /// An abort mid-body takes effect when the read in progress returns (here, when the next chunk
    /// arrives): the worker answers `Err` and hangs up, whatever the deadline and however much
    /// body is still to come.
    #[test]
    fn aborting_mid_body_stops_the_read_and_hangs_up() {
        let (sent_tx, sent_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (url, server) = listen(move |mut stream| {
            receive(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: 1048576\r\n\r\n").unwrap();
            stream.write_all(&[b'a'; 1000]).unwrap();
            sent_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            // One more chunk wakes the reader up; then the client's end must close.
            let _ = stream.write_all(&[b'b'; 1000]);
            hung_up(&mut stream)
        });
        let started = Instant::now();
        let (rx, abort) = start(get(format!("{url}/api/native/v1/materialize"), 2 << 20, Some(Duration::from_secs(300))));
        sent_rx.recv_timeout(Duration::from_secs(20)).unwrap();
        abort.abort();
        go_tx.send(()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(20)).unwrap().unwrap_err(), ABORTED);
        assert!(server.join().unwrap(), "the client hung up");
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "done is called once");
    }

    /// A server that accepts the request and then says nothing (before its headers, or in the
    /// middle of its body) fails at the request's deadline instead of hanging forever.
    #[test]
    fn a_silent_server_hits_the_deadline() {
        for midway in [false, true] {
            let (url, server) = listen(move |mut stream| {
                receive(&mut stream);
                if midway {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nhalf").unwrap();
                }
                // Silence, with the connection held open, until the client gives up.
                hung_up(&mut stream)
            });
            let started = Instant::now();
            let answer = call(get(format!("{url}/api/native/v1/info"), 1024, Some(Duration::from_millis(1500))));
            let took = started.elapsed();
            assert!(answer.as_ref().is_err_and(|e| e.contains("timeout")), "midway {midway}: {answer:?}");
            assert!(took >= Duration::from_millis(1400) && took < Duration::from_secs(8), "midway {midway}: {took:?}");
            assert!(server.join().unwrap(), "midway {midway}: the client hung up");
        }
    }
}
