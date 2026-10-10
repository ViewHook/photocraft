//! `Services::http` for Window › FrameForge, through ehttp: one background thread per request
//! here. `apps/photocraft-web/src/http.rs` is the same adapter for the browser, where its tests
//! run against a loopback server.

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
