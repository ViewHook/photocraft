//! Window › FrameForge against a fake FrameForge server (an injected `HttpFn` with canned
//! answers). No network: every answer below is made up in this file, and archives are
//! synthetic (`photocraft_frameforge::testing`).

use std::sync::{Arc, Mutex};

use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use crate::frameforge_ui::{self as ff, Token};
use crate::{HttpDone, HttpFn, HttpRequest, HttpResponse, PhotocraftApp, Services};

type Answer = Result<HttpResponse, String>;

/// A fake server: answers by URL suffix, synchronously (or holds the answers while `hold`, or for
/// URLs ending in one of `hold_only`). With `cap`, a body over the request's `max_response_bytes`
/// is an `Err`, as the app adapters do.
#[derive(Clone, Default)]
struct Fake {
    routes: Arc<Mutex<Vec<(String, Answer)>>>,
    log: Arc<Mutex<Vec<HttpRequest>>>,
    hold: Arc<Mutex<bool>>,
    hold_only: Arc<Mutex<Vec<String>>>,
    held: Arc<Mutex<Vec<(String, HttpDone)>>>,
    cap: Arc<Mutex<bool>>,
}

impl Fake {
    fn route(&self, suffix: &str, answer: Result<HttpResponse, String>) {
        let mut routes = self.routes.lock().unwrap();
        routes.retain(|(s, _)| s != suffix);
        routes.push((suffix.to_string(), answer));
    }
    fn service(&self) -> HttpFn {
        let fake = self.clone();
        Box::new(move |req: HttpRequest, done: HttpDone| {
            let (url, max) = (req.url.clone(), req.max_response_bytes);
            fake.log.lock().unwrap().push(req);
            if *fake.hold.lock().unwrap() || fake.hold_only.lock().unwrap().iter().any(|s| url.ends_with(s.as_str())) {
                fake.held.lock().unwrap().push((url, done));
                return;
            }
            let answer = fake.routes.lock().unwrap().iter().find(|(s, _)| url.ends_with(s.as_str())).map(|(_, a)| a.clone());
            let answer = answer.unwrap_or_else(|| Err(format!("no route for {url}")));
            if *fake.cap.lock().unwrap() && answer.as_ref().is_ok_and(|r| r.body.len() > max) {
                return done(Err(format!("the response is larger than {max} bytes")));
            }
            done(answer);
        })
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.log.lock().unwrap().clone()
    }
    fn release(&self, answer: Result<HttpResponse, String>) {
        for (_, done) in self.held.lock().unwrap().drain(..) {
            done(answer.clone());
        }
    }
    /// Answer the oldest held request.
    fn release_first(&self, answer: Result<HttpResponse, String>) {
        let (_, done) = self.held.lock().unwrap().remove(0);
        done(answer);
    }
}

fn services(fake: &Fake) -> Services {
    Services {
        http: Some(fake.service()),
        export: Some(Box::new(|doc: &photocraft_doc::Document, path: &str, _: &crate::ExportSettings| {
            photocraft_io::export(doc, path, &Default::default()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
        })),
        ..Default::default()
    }
}

fn app(fake: &Fake) -> PhotocraftApp {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services(fake));
    let f = &mut app.ui.frameforge;
    f.server_url = "http://frameforge.test/".into();
    f.token = Token::new("test-token", "http://frameforge.test/");
    f.channel_name = "Test Channel".into();
    f.video_title = "I tried the thing".into();
    f.video_summary = "A short brief.".into();
    app
}

fn run(app: &mut PhotocraftApp, id: &str, params: Value) -> Result<Value, String> {
    let ctx = egui::Context::default();
    let r = crate::menus::invoke(app, &ctx, id, params);
    ff::poll(app, &ctx);
    r
}

fn poll(app: &mut PhotocraftApp) {
    ff::poll(app, &egui::Context::default());
}

fn answer(status: u16, body: Vec<u8>, headers: &[(&str, &str)]) -> Result<HttpResponse, String> {
    Ok(HttpResponse { status, headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body })
}

fn json_answer(status: u16, v: Value) -> Result<HttpResponse, String> {
    answer(status, serde_json::to_vec(&v).unwrap(), &[("content-type", "application/json")])
}

fn info() -> Value {
    json!({"api": 1, "limits": {"concepts": {}, "materialize": {}}, "fonts": [
        {"family": "Anton", "weight": 400, "file": "anton-400.woff2"},
        {"family": "Montserrat", "weight": 400, "file": "montserrat-400.woff2"},
        {"family": "Montserrat", "weight": 900, "file": "montserrat-900.woff2"},
        {"family": "Inter", "weight": 900, "file": "inter-900.woff2"},
        {"family": "Bad", "weight": 400, "file": "../etc/passwd"},
    ]})
}

fn concept(name: &str) -> Value {
    json!({
        "name": name, "rationale": "A big promise over the original still.", "backgroundPrompt": "", "backgroundUploadIndex": 0,
        "texts": [
            {"text": "NO WAY", "x": 5, "y": 8, "w": 48, "h": 30, "fontSize": 132, "fill": "#ffd400", "align": "left", "shadow": null, "font": "Anton", "fontWeight": 400, "stroke": {"color": "#000000", "width": 0.04}},
            {"text": "he said yes", "x": 5, "y": 42, "w": 44, "h": 14, "fontSize": 56, "fill": "#ffffff", "align": "left", "shadow": null, "font": "Montserrat", "fontWeight": 900, "stroke": null}
        ],
        "images": [{"source": "upload", "uploadIndex": 1, "prompt": "", "removeBackground": true, "x": 52, "y": 10, "w": 44, "h": 88}]
    })
}

fn concepts(n: usize) -> Value {
    let list: Vec<Value> = (0..n).map(|i| concept(&format!("Concept {}", i + 1))).collect();
    json!({"result": {"channelSummary": "Bold yellow headlines, faces right.", "concepts": list}, "channel": {"name": "Test Channel", "url": "", "notes": "", "summary": "", "samples": []}, "warnings": ["Channel references were unavailable."]})
}

/// A generated (not photographed) RGB image as PNG.
fn png(w: u32, h: u32) -> Vec<u8> {
    let px: Vec<u8> = (0..w * h).flat_map(|i| [(i % w * 255 / w) as u8, (i / w * 255 / h) as u8, 160]).collect();
    let img = photocraft_codecs::Image::from_u8(w, h, photocraft_codecs::ChannelLayout::Rgb, px).unwrap();
    photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &Default::default()).unwrap()
}

fn data_url(bytes: &[u8], mime: &str) -> String {
    use base64::Engine as _;
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// A synthetic `.frameforge`: two type layers over an image and the background still.
fn archive() -> Vec<u8> {
    let layers = json!([
        {"id": "t1", "name": "NO WAY", "type": "text", "text": "NO WAY", "x": 5, "y": 8, "w": 48, "h": 30, "size": 132, "color": "#ffd400", "font": "Anton", "fontWeight": 400, "bold": false, "stroke": true, "strokeColor": "#000000", "strokeWidth": 0.04, "shadow": false, "visible": true, "role": "headline", "semanticRole": "headline", "rotation": 0, "outerStroke": "#ff0000"},
        {"id": "t2", "name": "he said yes", "type": "text", "text": "he said yes", "x": 5, "y": 42, "w": 44, "h": 14, "size": 56, "color": "#ffffff", "font": "Montserrat", "fontWeight": 900, "bold": true, "stroke": false, "shadow": false, "visible": true, "role": "text", "semanticRole": "text", "rotation": 0},
        {"id": "i1", "name": "Uploaded image", "type": "image", "assetRef": "asset-0001", "x": 52, "y": 10, "w": 44, "h": 88, "fit": "contain", "preserveAspectRatio": true, "role": "image", "semanticRole": "subject", "assetKind": "person-cutout", "generated": false, "sourceWidth": 64, "sourceHeight": 48, "hasAlpha": true, "backgroundRemoved": true, "busyZones": [], "maskBounds": {"x": 0, "y": 0, "w": 64, "h": 48}, "rotation": 0, "visible": true, "locked": false},
        {"id": "bg", "name": "Original video still", "type": "image", "assetRef": "asset-0002", "x": 0, "y": 0, "w": 100, "h": 100, "fit": "contain", "preserveAspectRatio": true, "role": "background", "semanticRole": "background", "assetKind": "scene", "generated": false, "sourceWidth": 160, "sourceHeight": 90, "hasAlpha": false, "backgroundRemoved": false, "rotation": 0, "visible": true, "locked": true}
    ]);
    let (a, b) = (png(64, 48), png(160, 90));
    let manifest = json!({"format": "frameforge-project", "formatVersion": 1, "project": {"name": "I tried the thing", "layers": layers}, "assets": [
        {"ref": "asset-0001", "path": "assets/asset-0001", "size": a.len()}, {"ref": "asset-0002", "path": "assets/asset-0002", "size": b.len()}
    ]});
    photocraft_frameforge::testing::zip(&[
        ("manifest.json", serde_json::to_vec(&manifest).unwrap(), true),
        ("assets/asset-0001", a, false),
        ("assets/asset-0002", b, false),
    ])
}

fn woff2() -> Vec<u8> {
    photocraft_frameforge::testing::woff2_stored(include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf")).unwrap()
}

/// An app with two concepts developed from two images.
fn developed(fake: &Fake) -> PhotocraftApp {
    let mut app = app(fake);
    fake.route("/api/native/v1/concepts", json_answer(200, concepts(2)));
    let images = json!([data_url(&png(160, 90), "image/png"), {"name": "subject.png", "data": data_url(&png(64, 48), "image/png")}]);
    run(&mut app, ff::DEVELOP, json!({"images": images})).unwrap();
    assert_eq!(app.ui.frameforge.concepts.as_ref().map(|c| c.list.len()), Some(2), "{:?}", app.ui.frameforge.message);
    app
}

fn message(app: &PhotocraftApp) -> (String, bool) {
    app.ui.frameforge.message.clone().unwrap_or_default()
}

#[test]
fn frameforge_connect_succeeds_then_shows_the_servers_401() {
    let fake = Fake::default();
    let mut app = app(&fake);
    fake.route("/api/native/v1/info", json_answer(200, info()));
    run(&mut app, ff::CONNECT, json!({})).unwrap();
    assert_eq!(message(&app), ("Connected (api 1)".into(), false));
    let server = app.ui.frameforge.server.clone().unwrap();
    assert_eq!(server.api, 1);
    assert!(server.fonts.iter().all(|f| f.file != "../etc/passwd"), "unsafe font names are dropped");
    let req = fake.requests().pop().unwrap();
    assert_eq!((req.method.as_str(), req.url.as_str()), ("GET", "http://frameforge.test/api/native/v1/info"));
    assert!(req.headers.contains(&("Authorization".into(), "Bearer test-token".into())));
    assert!(!format!("{req:?}").contains("test-token"), "request Debug hides header values");

    fake.route("/api/native/v1/info", json_answer(401, json!({"error": "Sign in to use the design model."})));
    run(&mut app, ff::CONNECT, json!({})).unwrap();
    assert_eq!(message(&app), ("Sign in to use the design model.".into(), true));
    assert!(app.ui.frameforge.server.is_none());
}

#[test]
fn frameforge_connect_rejects_bad_urls_and_parameters() {
    let fake = Fake::default();
    let mut app = app(&fake);
    for url in ["ftp://frameforge.test", "frameforge.test", "https://", "https://a b", ""] {
        assert!(run(&mut app, ff::CONNECT, json!({"url": url})).is_err(), "{url}");
    }
    assert!(run(&mut app, ff::CONNECT, json!({"url": 42})).is_err());
    assert!(fake.requests().is_empty());
    assert!(message(&app).1, "the panel shows the error");
}

#[test]
fn frameforge_develop_sends_the_contract_body() {
    let fake = Fake::default();
    let mut app = app(&fake);
    app.ui.frameforge.channel_url = "https://www.youtube.com/@test".into();
    app.run("file.new", json!({"width": 320, "height": 180, "name": "Still"})).unwrap();
    fake.route("/api/native/v1/concepts", json_answer(200, concepts(3)));
    let r = run(&mut app, ff::DEVELOP, json!({"video": {"title": "New title"}, "images": ["document", data_url(&png(64, 48), "image/png")]})).unwrap();
    assert_eq!(r["images"], 2);
    let req = fake.requests().pop().unwrap();
    assert_eq!((req.method.as_str(), req.url.as_str()), ("POST", "http://frameforge.test/api/native/v1/concepts"));
    assert!(req.headers.contains(&("Content-Type".into(), "application/json".into())));
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["channel", "images", "video"]);
    assert_eq!(body["channel"], json!({"name": "Test Channel", "url": "https://www.youtube.com/@test"}));
    assert_eq!(body["video"], json!({"title": "New title", "summary": "A short brief."}));
    let images = body["images"].as_array().unwrap();
    assert_eq!(images.len(), 2);
    assert!(images.iter().all(|i| i.as_str().is_some_and(|s| s.starts_with("data:image/jpeg;base64,") || s.starts_with("data:image/png;base64,"))));
    // The answer: three concepts and the server's warnings.
    let c = app.ui.frameforge.concepts.clone().unwrap();
    assert_eq!(c.list.len(), 3);
    assert_eq!(c.name(2), "Concept 3");
    assert_eq!(app.ui.frameforge.warnings, ["Channel references were unavailable."]);
    assert_eq!(message(&app), ("3 concepts are ready.".into(), false));
    assert!(app.ui.frameforge.request.is_none());
}

#[test]
fn frameforge_develop_downscales_images_under_two_mib() {
    // Noise doesn't compress: the hardest case for the 2 MiB data URL.
    let (w, h) = (2400u32, 1600u32);
    let mut seed = 0x1234_5678u32;
    let px: Vec<u8> = (0..w * h * 3)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    let img = photocraft_codecs::Image::from_u8(w, h, photocraft_codecs::ChannelLayout::Rgb, px).unwrap();
    let opts = photocraft_codecs::EncodeOptions { jpeg_quality: 98, ..Default::default() };
    let big = photocraft_codecs::encode(&img, photocraft_codecs::Format::Jpeg, &opts).unwrap();
    assert!(big.len() > 2 * 1024 * 1024, "the original is over the limit: {}", big.len());
    let fake = Fake::default();
    let mut app = app(&fake);
    fake.route("/api/native/v1/concepts", json_answer(200, concepts(2)));
    run(&mut app, ff::DEVELOP, json!({"images": [{"name": "noise.jpg", "data": data_url(&big, "image/jpeg")}]})).unwrap();
    let body: Value = serde_json::from_slice(&fake.requests().pop().unwrap().body).unwrap();
    let url = body["images"][0].as_str().unwrap();
    assert!(url.len() <= 2 * 1024 * 1024, "{} bytes", url.len());
    use base64::Engine as _;
    let sent = base64::engine::general_purpose::STANDARD.decode(url.split_once(',').unwrap().1).unwrap();
    let decoded = photocraft_codecs::decode(&sent).unwrap();
    assert_eq!(decoded.dimensions(), (1600, 1066));
    // Materialize gets the original file, unchanged.
    assert_eq!(app.ui.frameforge.images[0].upload_data_url().unwrap(), data_url(&big, "image/jpeg"));
}

#[test]
fn frameforge_in_flight_request_blocks_a_double_submit() {
    let fake = Fake::default();
    let mut app = app(&fake);
    *fake.hold.lock().unwrap() = true;
    run(&mut app, ff::DEVELOP, json!({})).unwrap();
    assert!(app.ui.frameforge.request.is_some());
    assert!(!crate::menus::is_enabled(&app, ff::DEVELOP) && !crate::menus::is_enabled(&app, ff::CONNECT));
    assert!(crate::menus::is_enabled(&app, ff::CANCEL));
    let err = run(&mut app, ff::DEVELOP, json!({})).unwrap_err();
    assert!(err.contains("already running"), "{err}");
    assert!(run(&mut app, ff::CONNECT, json!({})).is_err());
    assert_eq!(fake.requests().len(), 1);
    fake.release(json_answer(200, concepts(2)));
    poll(&mut app);
    assert!(app.ui.frameforge.request.is_none());
    assert_eq!(app.ui.frameforge.concepts.as_ref().map(|c| c.list.len()), Some(2));
}

#[test]
fn frameforge_cancel_drops_a_late_result() {
    let fake = Fake::default();
    let mut app = app(&fake);
    *fake.hold.lock().unwrap() = true;
    run(&mut app, ff::DEVELOP, json!({})).unwrap();
    assert_eq!(run(&mut app, ff::CANCEL, json!({})).unwrap(), json!({"cancelled": true}));
    assert_eq!(message(&app), ("Cancelled.".into(), false));
    fake.release(json_answer(200, concepts(2)));
    poll(&mut app);
    assert!(app.ui.frameforge.concepts.is_none(), "the late answer is dropped");
    assert_eq!(message(&app), ("Cancelled.".into(), false));
    assert_eq!(run(&mut app, ff::CANCEL, json!({})).unwrap(), json!({"cancelled": false}));
}

#[test]
fn frameforge_server_errors_are_shown_verbatim() {
    for (status, error, retry) in [
        (503, "The design model is busy. Try again shortly.", Some("5")),
        (504, "The materialization request timed out. Try again.", None),
        (400, "Enter a video title and a brief under 6,000 characters.", None),
    ] {
        let fake = Fake::default();
        let mut app = app(&fake);
        let headers: Vec<(&str, &str)> = retry.iter().map(|r| ("Retry-After", *r)).collect();
        fake.route("/api/native/v1/concepts", answer(status, serde_json::to_vec(&json!({"error": error})).unwrap(), &headers));
        run(&mut app, ff::DEVELOP, json!({})).unwrap();
        let (text, is_error) = message(&app);
        assert!(is_error && text.starts_with(error), "{status}: {text}");
        assert_eq!(text.contains("Try again in 5 s."), retry.is_some(), "{text}");
        assert!(app.ui.frameforge.concepts.is_none());
    }
    // No JSON body, or no server at all.
    let fake = Fake::default();
    let mut app = app(&fake);
    fake.route("/api/native/v1/concepts", answer(502, b"<html>Bad gateway</html>".to_vec(), &[]));
    run(&mut app, ff::DEVELOP, json!({})).unwrap();
    assert_eq!(message(&app), ("The FrameForge server answered HTTP 502.".into(), true));
    fake.route("/api/native/v1/concepts", Err("connection refused".into()));
    run(&mut app, ff::DEVELOP, json!({})).unwrap();
    assert_eq!(message(&app), ("Couldn't reach the FrameForge server: connection refused".into(), true));
}

#[test]
fn frameforge_malformed_answers_are_errors() {
    let fake = Fake::default();
    let mut app = app(&fake);
    for body in [&b"not json"[..], b"{\"result\":", b"[]", b"{\"result\": {\"concepts\": \"three\"}}", b"{\"result\": {\"concepts\": [1, 2]}}"] {
        fake.route("/api/native/v1/concepts", answer(200, body.to_vec(), &[]));
        run(&mut app, ff::DEVELOP, json!({})).unwrap();
        assert!(message(&app).1, "{}", String::from_utf8_lossy(body));
        assert!(app.ui.frameforge.concepts.is_none());
    }
    for body in [json!({"api": 2, "fonts": []}), json!({"fonts": []}), json!({"api": "1"})] {
        fake.route("/api/native/v1/info", json_answer(200, body.clone()));
        run(&mut app, ff::CONNECT, json!({})).unwrap();
        assert!(message(&app).1, "{body}");
        assert!(app.ui.frameforge.server.is_none());
    }
}

#[test]
fn frameforge_zero_or_nine_concepts_are_errors() {
    for n in [0, 1, 4, 9] {
        let fake = Fake::default();
        let mut app = app(&fake);
        fake.route("/api/native/v1/concepts", json_answer(200, concepts(n)));
        run(&mut app, ff::DEVELOP, json!({})).unwrap();
        let (text, is_error) = message(&app);
        assert!(is_error && text.contains(&format!("{n} concepts")), "{text}");
        assert!(app.ui.frameforge.concepts.is_none());
    }
}

/// Every call tells the HTTP service its size cap (JSON 16 MiB, archive 128 MiB, font 8 MiB). An
/// answer over it is an error: refused by the service, or by the panel itself if a service lets
/// it through (a huge `Content-Length`, or the body).
#[test]
fn frameforge_answers_over_the_size_cap_are_errors() {
    let fake = Fake::default();
    let mut app = app(&fake);
    // The service enforces the cap, as the app adapters do: a clean error, no panic.
    *fake.cap.lock().unwrap() = true;
    fake.route("/api/native/v1/concepts", answer(200, vec![b' '; 16 * 1024 * 1024 + 1], &[]));
    run(&mut app, ff::DEVELOP, json!({})).unwrap();
    assert_eq!(message(&app), ("Couldn't reach the FrameForge server: the response is larger than 16777216 bytes".into(), true));
    assert_eq!(fake.requests().last().unwrap().max_response_bytes, 16 * 1024 * 1024);
    // A service that lets it through: the panel refuses the body, or the announced length.
    *fake.cap.lock().unwrap() = false;
    run(&mut app, ff::DEVELOP, json!({})).unwrap();
    let (text, is_error) = message(&app);
    assert!(is_error && text.contains("larger than 16 MiB"), "{text}");
    fake.route("/api/native/v1/concepts", answer(200, serde_json::to_vec(&concepts(2)).unwrap(), &[("Content-Length", "99999999999")]));
    run(&mut app, ff::DEVELOP, json!({})).unwrap();
    let (text, is_error) = message(&app);
    assert!(is_error && text.contains("larger than 16 MiB"), "{text}");
    let mut app = developed(&fake);
    fake.route("/api/native/v1/info", json_answer(200, info()));
    fake.route("/api/native/v1/materialize", answer(200, archive(), &[("content-length", "1099511627776")]));
    run(&mut app, ff::CREATE, json!({"concept": 0})).unwrap();
    let (text, is_error) = message(&app);
    assert!(is_error && text.contains("larger than 128 MiB"), "{text}");
    assert!(app.session.documents().is_empty());
    let caps = |suffix: &str| fake.requests().iter().rev().find(|r| r.url.ends_with(suffix)).map(|r| r.max_response_bytes);
    assert_eq!(caps("/materialize"), Some(128 * 1024 * 1024));
    assert_eq!(caps("/info"), Some(16 * 1024 * 1024));
    assert_eq!(caps("/fonts/anton-400.woff2"), Some(8 * 1024 * 1024));
}

#[test]
fn frameforge_create_opens_the_archive_as_a_new_named_document() {
    let fake = Fake::default();
    let mut app = developed(&fake);
    app.run("file.new", json!({"width": 64, "height": 64, "name": "Other"})).unwrap();
    fake.route("/api/native/v1/info", json_answer(200, info()));
    for file in ["anton-400.woff2", "montserrat-900.woff2"] {
        fake.route(&format!("/api/native/v1/fonts/{file}"), answer(200, woff2(), &[("content-type", "font/woff2")]));
    }
    let warnings = "[\"Background generated at 1536x1024.\"]";
    fake.route(
        "/api/native/v1/materialize",
        answer(200, archive(), &[("X-FrameForge-Warnings", warnings), ("Content-Type", "application/x-frameforge-project")]),
    );
    let r = run(&mut app, ff::CREATE, json!({"concept": 1})).unwrap();
    assert_eq!(r["name"], "Concept 2");
    assert_eq!(message(&app), ("Created Concept 2".into(), false));
    // A new document after the one already open, named after the concept.
    assert_eq!(app.session.documents().len(), 2);
    let doc = &app.session.active().unwrap().doc;
    assert_eq!(doc.name, "Concept 2");
    let kinds: Vec<&str> = doc.layers.iter().map(|l| l.content.kind_name()).collect();
    assert_eq!(kinds, ["Pixel", "Smart Object", "Smart Object", "Type", "Type"]);
    assert_eq!(doc.layers.last().unwrap().name, "NO WAY");
    // Server warnings and import warnings, side by side; layout metadata is not one.
    let w = &app.ui.frameforge.warnings;
    assert!(w.iter().any(|w| w == "Background generated at 1536x1024."), "{w:?}");
    assert!(w.iter().any(|w| w.contains("outerStroke")), "{w:?}");
    assert!(!w.iter().any(|w| w.contains("busyZones") || w.contains("sourceWidth")), "{w:?}");
    // The materialize body: the concept unchanged, the brief's original images, channel and video.
    let reqs = fake.requests();
    let m = reqs.iter().find(|r| r.url.ends_with("/materialize")).unwrap();
    assert_eq!(m.method, "POST");
    let body: Value = serde_json::from_slice(&m.body).unwrap();
    assert_eq!(body["concept"], concept("Concept 2"));
    assert_eq!(body["uploads"], json!([data_url(&png(160, 90), "image/png"), data_url(&png(64, 48), "image/png")]));
    assert_eq!(body["video"], json!({"title": "I tried the thing", "summary": "A short brief."}));
    assert_eq!(body["channel"]["name"], "Test Channel");
    // Only the fonts the concept uses were fetched, and they converted.
    let mut fonts: Vec<&str> = reqs.iter().filter_map(|r| r.url.split_once("/fonts/").map(|(_, f)| f)).collect();
    fonts.sort_unstable();
    assert_eq!(fonts, ["anton-400.woff2", "montserrat-900.woff2"]);
    assert!(format!("{:?}", app.ui.frameforge.fonts).contains("anton-400.woff2\": Ok("));
}

#[test]
fn frameforge_a_non_archive_answer_is_an_error() {
    let fake = Fake::default();
    let mut app = developed(&fake);
    fake.route("/api/native/v1/info", json_answer(200, info()));
    for body in [b"<html>Sign in</html>".to_vec(), Vec::new(), b"PK\x05\x06".to_vec()] {
        fake.route("/api/native/v1/materialize", answer(200, body, &[]));
        run(&mut app, ff::CREATE, json!({"concept": 0})).unwrap();
        let (text, is_error) = message(&app);
        assert!(is_error && text.starts_with("The server's answer isn't a FrameForge project"), "{text}");
        assert!(app.session.documents().is_empty());
        assert!(app.ui.frameforge.request.is_none());
    }
}

#[test]
fn frameforge_concept_index_out_of_range_is_an_error() {
    let fake = Fake::default();
    let mut app = app(&fake);
    assert!(run(&mut app, ff::CREATE, json!({"concept": 0})).unwrap_err().contains("Develop concepts first"));
    let mut app = developed(&fake);
    let before = fake.requests().len();
    for p in [json!({"concept": 2}), json!({"concept": 9}), json!({"concept": u64::MAX}), json!({"concept": -1}), json!({"concept": "0"}), json!({})] {
        assert!(run(&mut app, ff::CREATE, p.clone()).is_err(), "{p}");
    }
    assert_eq!(fake.requests().len(), before, "nothing was sent");
}

#[test]
fn frameforge_font_decode_failure_falls_back_with_a_warning() {
    let fake = Fake::default();
    let mut app = developed(&fake);
    fake.route("/api/native/v1/info", json_answer(200, info()));
    fake.route("/api/native/v1/fonts/anton-400.woff2", answer(200, b"wOF2 but not really a font".to_vec(), &[]));
    fake.route("/api/native/v1/fonts/montserrat-900.woff2", json_answer(404, json!({"error": "Font not found."})));
    fake.route("/api/native/v1/materialize", answer(200, archive(), &[]));
    run(&mut app, ff::CREATE, json!({"concept": 0})).unwrap();
    assert_eq!(message(&app), ("Created Concept 1".into(), false));
    assert_eq!(app.session.documents().len(), 1, "fonts never block the import");
    let w = &app.ui.frameforge.warnings;
    assert!(w.iter().any(|w| w.starts_with("Font Anton 400: invalid WOFF2 font")), "{w:?}");
    assert!(w.iter().any(|w| w == "Font Montserrat 900: Font not found."), "{w:?}");
}

#[test]
fn frameforge_woff2_round_trip_registers_the_font() {
    let ttf = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");
    let sfnt = photocraft_frameforge::fonts::woff2_to_sfnt(&woff2()).unwrap();
    assert_eq!(&sfnt[..4], &ttf[..4]);
    let families = photocraft_text::shared().lock().unwrap().fonts.register_font_data(sfnt);
    assert!(families.iter().any(|f| f == "JetBrains Mono"), "{families:?}");
}

#[test]
fn frameforge_token_is_never_serialized() {
    let fake = Fake::default();
    let mut app = developed(&fake);
    let ui = serde_json::to_value(&app.ui).unwrap();
    let text = ui.to_string();
    assert!(!text.contains("test-token"));
    assert!(!format!("{:?}", app.ui).contains("test-token"));
    assert!(!crate::control::inspect(&app, &egui::Context::default()).to_string().contains("test-token"));
    // Only the server and the channel persist.
    assert_eq!(
        ui["frameforge"],
        json!({"open": false, "server_url": "http://frameforge.test/", "channel_name": "Test Channel", "channel_url": "", "channel_notes": ""})
    );
    let back: crate::UiState = serde_json::from_value(ui).unwrap();
    assert!(!back.frameforge.token.is_set() && back.frameforge.images.is_empty() && back.frameforge.concepts.is_none());
    // The env/automation path sets it in memory only.
    ff::set_access_token(&mut app, "test-token-2".into());
    assert!(!serde_json::to_string(&app.ui).unwrap().contains("test-token-2"));
}

#[test]
fn frameforge_menu_id_is_wired() {
    let fake = Fake::default();
    let mut app = app(&fake);
    assert!(ff::handles(ff::PANEL) && crate::menus::is_live(ff::PANEL));
    assert!(crate::menu_catalog::CATALOG.iter().any(|(path, _, _, id)| *id == ff::PANEL && *path == ["Window"].as_slice()));
    let item = |app: &PhotocraftApp| crate::menus::menu_items(app).into_iter().find(|i| i.id == ff::PANEL).unwrap();
    assert_eq!((item(&app).enabled, item(&app).checked), (true, Some(false)));
    assert_eq!(run(&mut app, ff::PANEL, json!({})).unwrap(), json!({"open": true}));
    assert_eq!(item(&app).checked, Some(true));
    assert_eq!(run(&mut app, ff::PANEL, json!({})).unwrap(), json!({"open": false}));
    assert_eq!(item(&app).checked, Some(false));
    for id in [ff::CONNECT, ff::DEVELOP, ff::CREATE, ff::CANCEL] {
        assert!(crate::menus::is_live(id), "{id}");
    }
}

#[test]
fn frameforge_without_a_network_service_says_so() {
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), Services::default());
    app.ui.frameforge.server_url = "http://frameforge.test".into();
    for id in [ff::CONNECT, ff::DEVELOP] {
        assert_eq!(run(&mut app, id, json!({})).unwrap_err(), "FrameForge needs a network-enabled build.");
    }
    app.ui.frameforge.open = true;
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_ui_state(
        |ui, app: &mut PhotocraftApp| {
            let ctx = ui.ctx().clone();
            // Fonts set up after the first frame only apply from the next one.
            if ctx.fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                ff::windows(app, &ctx);
            }
        },
        app,
    );
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::ALL[0]);
    h.run_steps(3);
    assert!(h.query_by_label("FrameForge needs a network-enabled build.").is_some());
}

#[test]
fn frameforge_brief_and_image_parameters_are_checked() {
    let fake = Fake::default();
    let mut app = app(&fake);
    let long = "x".repeat(201);
    for (p, why) in [
        (json!({"channel": {"name": ""}}), "Enter a channel name."),
        (json!({"channel": {"name": "Ok"}, "video": {"title": " "}}), "Enter a video title."),
        (json!({"video": {"title": long}}), "Video title is too long: at most 200 characters."),
        // 101 characters, but 202 UTF-16 units: the server counts those.
        (json!({"video": {"title": "😀".repeat(101)}}), "Video title is too long: at most 200 characters."),
        (json!({"video": {"title": 7}}), "Brief fields must be text."),
        (json!({"images": [42]}), "Images must be"),
        (json!({"images": ["%%%"]}), "Images must be"),
        (json!({"images": ["data:text/plain;base64,aGk="]}), "can't be used"),
        (json!({"images": ["document"]}), "Open a document first."),
        (json!({"images": ["a", "b", "c", "d", "e"]}), "Attach at most 4 images."),
    ] {
        let err = run(&mut app, ff::DEVELOP, p.clone()).unwrap_err();
        assert!(err.contains(why), "{p}: {err}");
    }
    assert!(fake.requests().is_empty());
}

#[test]
fn frameforge_panel_draws_the_brief_and_concept_cards() {
    let fake = Fake::default();
    let mut app = developed(&fake);
    app.ui.frameforge.open = true;
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 1600.0)).build_ui_state(
        |ui, app: &mut PhotocraftApp| {
            let ctx = ui.ctx().clone();
            app.last_canvas_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 1600.0));
            if ctx.fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                ff::windows(app, &ctx);
            }
        },
        app,
    );
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::ALL[0]);
    h.run_steps(4);
    for label in ["Develop concepts", "Connect", "Add image…", "Add current document"] {
        assert!(h.query_by_label(label).is_some(), "{label}");
    }
    assert_eq!(h.query_all_by_label("Create").count(), 2);
    assert!(h.query_by_label("Concept 2").is_some());
    // New concepts scroll into view; Create runs the same command as the control channel.
    fake.route("/api/native/v1/info", json_answer(200, info()));
    fake.route("/api/native/v1/materialize", answer(200, archive(), &[]));
    let panel = h.ctx.memory(|m| m.area_rect(egui::Id::new("frameforge"))).unwrap();
    let create = h.query_all_by_label("Create").nth(1).unwrap();
    assert!(panel.contains_rect(create.rect()), "{panel:?} {:?}", create.rect());
    create.click();
    h.run_steps(4);
    assert_eq!(h.state().session.active().map(|d| d.doc.name.clone()).as_deref(), Some("Concept 2"));
    assert_eq!(h.state().ui.frameforge.message, Some(("Created Concept 2".into(), false)));
}

/// A token belongs to the server it was entered for: it is never sent anywhere else. Pointing the
/// panel at another server drops it and asks for that server's token.
#[test]
fn frameforge_token_is_bound_to_its_server() {
    let fake = Fake::default();
    let mut app = app(&fake);
    fake.route("/api/native/v1/info", json_answer(200, info()));
    let bearer = |r: &HttpRequest| r.headers.iter().find(|(k, _)| k == "Authorization").map(|(_, v)| v.clone());
    run(&mut app, ff::CONNECT, json!({})).unwrap();
    assert_eq!(bearer(&fake.requests()[0]).as_deref(), Some("Bearer test-token"));
    // Another server (the control channel, or an edited URL): nothing is sent, the token is gone.
    let err = run(&mut app, ff::CONNECT, json!({"url": "http://other.test:8080/"})).unwrap_err();
    assert_eq!(err, "Enter the access token for http://other.test:8080.");
    assert_eq!(message(&app), (err, true));
    assert_eq!(fake.requests().len(), 1, "nothing went to the other server");
    assert!(!app.ui.frameforge.token.is_set());
    // A token given with the call belongs to that call's server.
    run(&mut app, ff::CONNECT, json!({"url": "http://other.test:8080", "token": "test-token-b"})).unwrap();
    let r = fake.requests().pop().unwrap();
    assert_eq!((r.url.as_str(), bearer(&r).as_deref()), ("http://other.test:8080/api/native/v1/info", Some("Bearer test-token-b")));
    // Back to the first server: B's token stays home.
    app.ui.frameforge.server_url = "http://frameforge.test".into();
    let err = run(&mut app, ff::DEVELOP, json!({})).unwrap_err();
    assert_eq!(err, "Enter the access token for http://frameforge.test.");
    assert_eq!(fake.requests().len(), 2);
    // Entered again for it: sent again.
    ff::set_access_token(&mut app, "test-token".into());
    run(&mut app, ff::CONNECT, json!({})).unwrap();
    assert_eq!(bearer(&fake.requests()[2]).as_deref(), Some("Bearer test-token"));
    assert!(fake.requests().iter().all(|r| r.url.starts_with("http://other.test") == (bearer(r).as_deref() == Some("Bearer test-token-b"))));
    // A token entered before there was a URL belongs to no server: commands never bind it.
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services(&fake));
    ff::set_access_token(&mut app, "test-token".into());
    assert_eq!(app.ui.frameforge.token.origin(), None);
    assert!(run(&mut app, ff::CONNECT, json!({"url": "http://frameforge.test"})).is_err());
    assert_eq!(fake.requests().len(), 3);
    assert_eq!(ff::origin("https://Host.test:8443/prefix/api"), "https://Host.test:8443");
}

/// Typed before the URL (or from the environment), a token is bound by the user's own click.
#[test]
fn frameforge_connect_button_binds_a_token_entered_before_the_url() {
    let fake = Fake::default();
    fake.route("/api/native/v1/info", json_answer(200, info()));
    let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services(&fake));
    ff::set_access_token(&mut app, "test-token".into());
    app.ui.frameforge.server_url = "http://frameforge.test".into();
    app.ui.frameforge.open = true;
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 1000.0)).build_ui_state(
        |ui, app: &mut PhotocraftApp| {
            let ctx = ui.ctx().clone();
            app.last_canvas_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 1000.0));
            if ctx.fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                ff::windows(app, &ctx);
            }
        },
        app,
    );
    PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::ALL[0]);
    h.run_steps(4);
    h.get_by_label("Connect").click();
    h.run_steps(2);
    assert_eq!(h.state().ui.frameforge.message, Some(("Connected (api 1)".into(), false)));
    assert_eq!(h.state().ui.frameforge.token.origin(), Some("http://frameforge.test"));
    let r = fake.requests().pop().unwrap();
    assert!(r.headers.contains(&("Authorization".into(), "Bearer test-token".into())));
}

/// Cancel, then a new request: the cancelled one's late answer must not land in the new one.
#[test]
fn frameforge_a_cancelled_answer_never_reaches_the_next_request() {
    let fake = Fake::default();
    let mut app = app(&fake);
    *fake.hold.lock().unwrap() = true;
    run(&mut app, ff::DEVELOP, json!({})).unwrap();
    run(&mut app, ff::CANCEL, json!({})).unwrap();
    run(&mut app, ff::DEVELOP, json!({})).unwrap();
    assert_eq!(fake.held.lock().unwrap().len(), 2);
    // The first Develop answers while the second waits: ignored.
    fake.release_first(json_answer(200, concepts(2)));
    poll(&mut app);
    assert!(app.ui.frameforge.concepts.is_none());
    assert_eq!(app.ui.frameforge.request.as_ref().map(|r| r.kind), Some(ff::Kind::Develop));
    fake.release_first(json_answer(200, concepts(3)));
    poll(&mut app);
    assert_eq!(app.ui.frameforge.concepts.as_ref().map(|c| c.list.len()), Some(3));
    // Another kind: a cancelled Connect's 401 doesn't land in the Create that follows.
    run(&mut app, ff::CONNECT, json!({})).unwrap();
    run(&mut app, ff::CANCEL, json!({})).unwrap();
    run(&mut app, ff::CREATE, json!({"concept": 0})).unwrap();
    fake.release_first(json_answer(401, json!({"error": "Sign in to use the design model."})));
    poll(&mut app);
    assert_eq!(message(&app), ("Creating Concept 1…".into(), false));
    assert!(matches!(app.ui.frameforge.request.as_ref().map(|r| r.kind), Some(ff::Kind::Create(0))));
}

/// Fonts never block the import: once the archive is in, a font that doesn't answer within the
/// grace period falls back (with a warning), and its late answer is dropped.
#[test]
fn frameforge_fonts_never_block_the_import() {
    let fake = Fake::default();
    let mut app = developed(&fake);
    fake.route("/api/native/v1/info", json_answer(200, info()));
    fake.route("/api/native/v1/fonts/anton-400.woff2", answer(200, woff2(), &[]));
    fake.route("/api/native/v1/materialize", answer(200, archive(), &[]));
    fake.hold_only.lock().unwrap().push("/fonts/montserrat-900.woff2".into());
    run(&mut app, ff::CREATE, json!({"concept": 0})).unwrap();
    assert!(app.session.documents().is_empty() && app.ui.frameforge.request.is_some(), "the archive waits for the fonts");
    let ctx = egui::Context::default();
    ff::poll_at(&mut app, &ctx, ff::FONT_GRACE - 0.5);
    assert!(app.session.documents().is_empty());
    ff::poll_at(&mut app, &ctx, ff::FONT_GRACE);
    assert_eq!(app.session.documents().len(), 1);
    assert!(app.ui.frameforge.request.is_none());
    assert_eq!(message(&app), ("Created Concept 1".into(), false));
    let w = &app.ui.frameforge.warnings;
    assert!(w.iter().any(|w| w == "Font Montserrat 900: The server didn't answer."), "{w:?}");
    // The font answers after all: nothing changes.
    fake.release(answer(200, woff2(), &[]));
    poll(&mut app);
    assert_eq!(app.session.documents().len(), 1);
    assert!(app.ui.frameforge.request.is_none());
    // /info itself never answering doesn't block it either.
    let mut app = developed(&fake);
    fake.hold_only.lock().unwrap().push("/api/native/v1/info".into());
    run(&mut app, ff::CREATE, json!({"concept": 1})).unwrap();
    ff::poll_at(&mut app, &ctx, ff::FONT_GRACE);
    assert_eq!(message(&app), ("Created Concept 2".into(), false));
    assert!(app.ui.frameforge.warnings.iter().any(|w| w == "Server fonts are unavailable: The server didn't answer."), "{:?}", app.ui.frameforge.warnings);
}

/// Only PNG, JPEG and WebP (what the server takes) reach a decoder.
#[test]
fn frameforge_only_png_jpeg_webp_images_are_accepted() {
    let img = photocraft_codecs::Image::from_u8(8, 8, photocraft_codecs::ChannelLayout::Rgb, vec![128; 8 * 8 * 3]).unwrap();
    let tiff = photocraft_codecs::encode(&img, photocraft_codecs::Format::Tiff, &Default::default()).unwrap();
    for (name, bytes) in [("still.tif", tiff), ("still.gif", b"GIF89a\x01\x00\x01\x00".to_vec()), ("notes.txt", b"hello".to_vec())] {
        let err = ff::BriefImage::new(name, bytes.clone()).unwrap_err();
        assert_eq!(err, format!("{name} isn't a PNG, JPEG or WebP image."));
        let fake = Fake::default();
        let mut app = app(&fake);
        let err = run(&mut app, ff::DEVELOP, json!({"images": [{"name": name, "data": data_url(&bytes, "image/tiff")}]})).unwrap_err();
        assert!(err.ends_with("isn't a PNG, JPEG or WebP image."), "{err}");
        assert!(fake.requests().is_empty());
    }
    // A PNG signature over garbage is a decode error, not a crash.
    let mut broken = png(16, 16);
    broken.truncate(40);
    assert!(ff::BriefImage::new("broken.png", broken).is_err());
}

/// Upload indices are null or 0..=3 (the contract's four uploads); anything else is refused.
#[test]
fn frameforge_upload_indices_are_checked() {
    for bad in [json!(4), json!(u64::MAX), json!(-1), json!("0"), json!(1.5)] {
        for key in ["background", "image"] {
            let mut c = concept("Bad");
            if key == "background" {
                c["backgroundUploadIndex"] = bad.clone();
            } else {
                c["images"][0]["uploadIndex"] = bad.clone();
            }
            let fake = Fake::default();
            let mut app = app(&fake);
            fake.route("/api/native/v1/concepts", json_answer(200, json!({"result": {"channelSummary": "", "concepts": [c, concept("Good")]}})));
            run(&mut app, ff::DEVELOP, json!({})).unwrap();
            assert_eq!(message(&app), ("The FrameForge server sent an invalid concept.".into(), true), "{key} {bad}");
            assert!(app.ui.frameforge.concepts.is_none());
        }
    }
    // Generated images and backgrounds have none.
    let mut c = concept("Generated");
    c["backgroundUploadIndex"] = Value::Null;
    c["images"][0] = json!({"source": "generate", "uploadIndex": null, "prompt": "a cat", "removeBackground": false, "x": 1, "y": 1, "w": 10, "h": 10});
    let fake = Fake::default();
    let mut app = app(&fake);
    fake.route("/api/native/v1/concepts", json_answer(200, json!({"result": {"channelSummary": "", "concepts": [c, concept("Upload")]}})));
    run(&mut app, ff::DEVELOP, json!({})).unwrap();
    assert_eq!(app.ui.frameforge.concepts.as_ref().map(|c| c.list.len()), Some(2));
}

/// Visual evidence for review: `FRAMEFORGE_PANEL_EVIDENCE=<dir outside the repo>` writes
/// panel-empty.png, panel-concepts.png and panel-created.png (generated imagery only).
#[test]
#[ignore = "writes PNGs to $FRAMEFORGE_PANEL_EVIDENCE; run explicitly for visual QA"]
fn frameforge_panel_evidence() {
    let dir = std::path::PathBuf::from(std::env::var("FRAMEFORGE_PANEL_EVIDENCE").unwrap());
    std::fs::create_dir_all(&dir).unwrap();
    let fake = Fake::default();
    fake.route("/api/native/v1/info", json_answer(200, info()));
    fake.route("/api/native/v1/concepts", json_answer(200, concepts(3)));
    for file in ["anton-400.woff2", "montserrat-900.woff2"] {
        fake.route(&format!("/api/native/v1/fonts/{file}"), answer(200, woff2(), &[]));
    }
    fake.route("/api/native/v1/materialize", answer(200, archive(), &[("X-FrameForge-Warnings", "[\"Background generated at 1536x1024.\"]")]));
    let services = services(&fake);
    let mut h =
        egui_kittest::Harness::builder().with_size(egui::vec2(1440.0, 1000.0)).with_pixels_per_point(1.0).with_max_steps(64).wgpu().build_eframe(move |cc| {
            PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
            let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), services);
            app.ui.frameforge.server_url = "http://frameforge.test".into();
            app
        });
    let ctx = h.ctx.clone();
    // Opened from Window › FrameForge once the canvas is laid out, as a user would.
    h.run_steps(4);
    crate::menus::invoke(h.state_mut(), &ctx, ff::PANEL, json!({})).unwrap();
    let shoot = |h: &mut egui_kittest::Harness<'_, PhotocraftApp>, name: &str| {
        for _ in 0..12 {
            h.step();
        }
        h.render().unwrap().save(dir.join(name)).unwrap();
    };
    shoot(&mut h, "panel-empty.png");
    let app = h.state_mut();
    ff::set_access_token(app, "test-token".into());
    let f = &mut app.ui.frameforge;
    f.channel_name = "Test Channel".into();
    f.channel_url = "https://www.youtube.com/@test".into();
    f.video_title = "I tried the thing".into();
    f.video_summary = "A short brief about trying the thing.".into();
    crate::menus::invoke(app, &ctx, ff::CONNECT, json!({})).unwrap();
    ff::poll(app, &ctx);
    let images = json!([data_url(&png(320, 180), "image/png"), {"name": "subject.png", "data": data_url(&png(120, 160), "image/png")}]);
    crate::menus::invoke(app, &ctx, ff::DEVELOP, json!({"images": images})).unwrap();
    shoot(&mut h, "panel-concepts.png");
    crate::menus::invoke(h.state_mut(), &ctx, ff::CREATE, json!({"concept": 0})).unwrap();
    shoot(&mut h, "panel-created.png");
}
