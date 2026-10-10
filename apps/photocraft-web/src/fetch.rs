//! `Services::http` for Window › FrameForge in the browser: `fetch` with an `AbortController`
//! (the FrameForge server must list this page's origin in `FRAMEFORGE_NATIVE_CORS_ORIGINS`).
//! [`HttpAbort`] and the request's deadline both abort the `fetch`, which also cancels a body
//! still arriving. The body is read from its stream chunk by chunk and reading stops past
//! `max_response_bytes` (or when `Content-Length` announces more), so a server can't make the page
//! allocate more than it asked for. `http.rs` is the desktop's adapter; its loopback tests run
//! natively and can't reach this file, which only builds for wasm32.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use js_sys::{Reflect, Uint8Array};
use photocraft_ui_egui::{HttpAbort, HttpDone, HttpFn, HttpRequest, HttpResponse};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{AbortController, AbortSignal, ReadableStreamDefaultReader, RequestInit, RequestMode, Response};

/// The deadline when the request names none: FrameForge's model calls take up to 240 seconds,
/// plus the transfer.
const TIMEOUT: Duration = Duration::from_secs(300);
/// What `done` hears after [`HttpAbort::abort`] (nobody listens by then).
const ABORTED: &str = "the request was aborted";

pub fn service() -> HttpFn {
    Box::new(|request: HttpRequest, done: HttpDone| {
        if !matches!(request.method.as_str(), "GET" | "POST") {
            done(Err(format!("unsupported HTTP method {}", request.method)));
            return HttpAbort::none();
        }
        let (Some(window), Ok(controller)) = (web_sys::window(), AbortController::new()) else {
            done(Err("this page can't make HTTP requests".to_string()));
            return HttpAbort::none();
        };
        // Why the fetch was aborted, if it was: the deadline or HttpAbort.
        let (timed_out, aborted) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(false)));
        let timeout = request.timeout.unwrap_or(TIMEOUT);
        let timer = {
            let (controller, timed_out) = (controller.clone(), timed_out.clone());
            Closure::<dyn FnMut()>::once(move || {
                timed_out.set(true);
                controller.abort();
            })
        };
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let handle = window.set_timeout_with_callback_and_timeout_and_arguments_0(timer.as_ref().unchecked_ref(), millis).ok();
        let task = {
            let (controller, aborted) = (controller.clone(), aborted.clone());
            async move {
                let outcome = exchange(&window, &request, &controller.signal()).await;
                if let Some(handle) = handle {
                    window.clear_timeout_with_handle(handle);
                }
                drop(timer);
                let outcome = match outcome {
                    Err(_) if aborted.get() => Err(ABORTED.to_string()),
                    Err(_) if timed_out.get() => Err(format!("timeout: no complete answer within {} s", timeout.as_secs())),
                    Err(e) => {
                        // Refused or over the cap: let go of whatever is still arriving.
                        controller.abort();
                        Err(e)
                    }
                    ok => ok,
                };
                done(outcome);
            }
        };
        wasm_bindgen_futures::spawn_local(task);
        HttpAbort::new(move || {
            aborted.set(true);
            controller.abort();
        })
    })
}

async fn exchange(window: &web_sys::Window, request: &HttpRequest, signal: &AbortSignal) -> Result<HttpResponse, String> {
    let init = RequestInit::new();
    init.set_method(&request.method);
    init.set_mode(RequestMode::Cors);
    init.set_signal(Some(signal));
    let headers = web_sys::Headers::new().map_err(js)?;
    for (name, value) in &request.headers {
        headers.append(name, value).map_err(js)?;
    }
    init.set_headers(&headers);
    if request.method == "POST" {
        init.set_body(&Uint8Array::from(request.body.as_slice()));
    }
    let response: Response = JsFuture::from(window.fetch_with_str_and_init(&request.url, &init)).await.map_err(js)?.dyn_into().map_err(js)?;
    let max = request.max_response_bytes;
    let mut head = Vec::new();
    for entry in response.headers().entries() {
        let entry: js_sys::Array = entry.map_err(js)?.unchecked_into();
        if let (Some(name), Some(value)) = (entry.get(0).as_string(), entry.get(1).as_string()) {
            head.push((name, value));
        }
    }
    let announced = head.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse::<u64>().ok());
    if announced.is_some_and(|n| n > max as u64) {
        return Err(too_large(max));
    }
    let mut body = Vec::new();
    if let Some(stream) = response.body() {
        let reader: ReadableStreamDefaultReader = stream.get_reader().unchecked_into();
        loop {
            let part = JsFuture::from(reader.read()).await.map_err(js)?;
            if Reflect::get(&part, &"done".into()).map_err(js)?.as_bool().unwrap_or(true) {
                break;
            }
            let chunk: Uint8Array = Reflect::get(&part, &"value".into()).map_err(js)?.dyn_into().map_err(js)?;
            let n = chunk.length() as usize;
            if body.len().saturating_add(n) > max {
                return Err(too_large(max));
            }
            let start = body.len();
            body.resize(start + n, 0);
            chunk.copy_to(&mut body[start..]);
        }
    }
    Ok(HttpResponse { status: response.status(), headers: head, body })
}

fn too_large(max: usize) -> String {
    format!("the response is larger than {max} bytes")
}

fn js(e: JsValue) -> String {
    e.as_string().or_else(|| e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message()))).unwrap_or_else(|| format!("{e:?}"))
}
