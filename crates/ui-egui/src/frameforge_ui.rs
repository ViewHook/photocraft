//! Window › FrameForge: a FrameForge server turns a brief and up to four images into two or
//! three thumbnail concepts; the one picked comes back as a `.frameforge` project, which the
//! importer (`photocraft-frameforge`) opens as a new document of image layers and editable
//! type. The server does the AI work. Its contract is youtube-thumbnail-app
//! `docs/native-client-api.md`; this panel is its client, through [`crate::Services::http`].
//!
//! Every button runs a shell command (`frameforge.connect`, `.develop`, `.create`, `.cancel`), so
//! the control channel and tests drive the panel the same way. Requests run in the background
//! and answer over a channel drained every frame ([`poll`]), like the histogram job. Only the
//! server URL and the channel persist: the access token, images, concepts and requests stay in
//! memory and are never serialized.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};

use egui::{Color32, RichText, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use crate::frameforge_images::BriefImage;
use crate::frameforge_images::limit;
use crate::i18n::fmt;
use crate::theme::Tokens;
use crate::{HttpRequest, HttpResponse, PhotocraftApp};

pub const PANEL: &str = "window.panel.frameforge";
pub const CONNECT: &str = "frameforge.connect";
pub const DEVELOP: &str = "frameforge.develop";
pub const CREATE: &str = "frameforge.create";
pub const CANCEL: &str = "frameforge.cancel";

/// Brief field lengths the server accepts, in characters.
pub(crate) const CHANNEL_NAME: usize = 120;
pub(crate) const CHANNEL_URL: usize = 2048;
pub(crate) const CHANNEL_NOTES: usize = 2000;
pub(crate) const TITLE: usize = 200;
pub(crate) const SUMMARY: usize = 6000;
/// Request bodies the server accepts (`/concepts`, `/materialize`).
const CONCEPTS_BODY: usize = 10 * 1024 * 1024;
const MATERIALIZE_BODY: usize = 64 * 1024 * 1024;
/// The largest answers read: JSON, and a project archive (the importer's own container cap).
const JSON_ANSWER: usize = 16 * 1024 * 1024;
const ARCHIVE_ANSWER: usize = 128 * 1024 * 1024;
/// Seconds Create waits for the concept's fonts once the archive is in: fonts never block it.
pub(crate) const FONT_GRACE: f64 = 10.0;
/// `X-FrameForge-Warnings`: at most 20 strings of 200 characters.
const WARNINGS: usize = 20;
const WARNING_CHARS: usize = 200;

/// Shown instead of the panel when the build has no HTTP service.
fn needs_network() -> &'static str {
    tl!("FrameForge needs a network-enabled build.")
}

/// Window › FrameForge state. Only `open`, the server URL and the channel are serialized.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameForgeUi {
    pub open: bool,
    /// `https://host[:port]`, without the API path.
    #[serde(default)]
    pub server_url: String,
    #[serde(default)]
    pub channel_name: String,
    #[serde(default)]
    pub channel_url: String,
    #[serde(default)]
    pub channel_notes: String,
    #[serde(skip)]
    pub video_title: String,
    #[serde(skip)]
    pub video_summary: String,
    #[serde(skip)]
    pub token: Token,
    #[serde(skip)]
    pub images: Vec<BriefImage>,
    #[serde(skip)]
    pub concepts: Option<Concepts>,
    #[serde(skip)]
    pub request: Option<Request>,
    /// What `/info` said on the last successful connection.
    #[serde(skip)]
    pub server: Option<ServerInfo>,
    /// The last outcome: (text, is an error).
    #[serde(skip)]
    pub message: Option<(String, bool)>,
    /// Warnings from the last Develop or Create (server and importer).
    #[serde(skip)]
    pub warnings: Vec<String>,
    /// Converted server fonts by file name, for this session.
    #[serde(skip)]
    pub fonts: FontCache,
    /// What to scroll into view on the next draw.
    #[serde(skip)]
    pub reveal: Reveal,
}

/// A part of the panel to scroll into view once an answer arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reveal {
    #[default]
    Nothing,
    /// New concept cards.
    Concepts,
    /// The outcome line and warnings (an error, or a created document).
    Outcome,
}

/// The access token and the server origin (`scheme://host[:port]`) it was entered for: it is only
/// ever sent there. Memory only: not serializable, and `Debug` never shows it.
#[derive(Clone, Default, PartialEq)]
pub struct Token {
    secret: String,
    origin: Option<String>,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token").field("set", &self.is_set()).field("origin", &self.origin).finish()
    }
}

impl Token {
    /// `token` for the server at `server_url` (unbound while that isn't a valid URL: the panel's
    /// buttons bind it to the URL they use).
    pub fn new(token: impl Into<String>, server_url: &str) -> Token {
        Token { secret: token.into(), origin: base_url(server_url).ok().map(|b| origin(&b).to_string()) }
    }
    pub fn is_set(&self) -> bool {
        !self.secret.trim().is_empty()
    }
    /// The origin the token belongs to, if any.
    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }
    /// The `Authorization` value for a request to `base`: only on the token's own origin.
    fn bearer_for(&self, base: &str) -> Option<String> {
        (self.is_set() && self.origin.as_deref() == Some(origin(base))).then(|| format!("Bearer {}", self.secret.trim()))
    }
}

/// `scheme://host[:port]` of a [`base_url`].
pub(crate) fn origin(base: &str) -> &str {
    let host = base.find("://").map_or(0, |i| i + 3);
    base.get(host..).and_then(|rest| rest.find('/')).and_then(|end| base.get(..host + end)).unwrap_or(base)
}

/// A token entered for another server is never sent to `base`: it is dropped, and the user asked
/// for the token of this one.
fn guard_token(f: &mut FrameForgeUi, base: &str) -> Result<(), String> {
    if f.token.is_set() && f.token.origin() != Some(origin(base)) {
        f.token = Token::default();
        return Err(fmt(tl!("Enter the access token for {origin}."), &[("origin", origin(base))]));
    }
    Ok(())
}

/// Concepts from `/concepts`, with the brief they answer: their upload indices refer to `uploads`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Concepts {
    /// As the server sent them (materialize takes one back unchanged).
    pub list: Vec<Value>,
    pub channel_summary: String,
    channel: Value,
    video: Value,
    uploads: Vec<BriefImage>,
}

impl Concepts {
    pub fn name(&self, index: usize) -> &str {
        self.list.get(index).and_then(|c| c.get("name")).and_then(Value::as_str).unwrap_or_default()
    }
}

/// What a request is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Connect,
    Develop,
    /// Create a document from concept N.
    Create(usize),
}

/// Which call an answer belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Info,
    Concepts,
    Materialize,
    Font(String),
}

type Answer = (Call, Result<HttpResponse, String>);

/// A request in flight: one or more calls answering on one channel. Dropping it (Cancel)
/// drops whatever arrives later.
#[derive(Clone)]
pub struct Request {
    pub kind: Kind,
    /// egui time it started at, for the elapsed seconds.
    pub started: f64,
    base: String,
    pending: usize,
    tx: Sender<Answer>,
    rx: Arc<Mutex<Receiver<Answer>>>,
    /// Develop: the channel, video and images it was sent with.
    brief: Option<(Value, Value, Vec<BriefImage>)>,
    /// Create: the materialize answer, held until the fonts are in.
    archive: Option<Result<HttpResponse, String>>,
    /// Create: egui time the materialize answer arrived (see [`FONT_GRACE`]).
    archive_at: Option<f64>,
    /// Create: the (family, weight) faces the concept's type uses.
    faces: Vec<(String, u16)>,
}

impl PartialEq for Request {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.started == other.started && Arc::ptr_eq(&self.rx, &other.rx)
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request").field("kind", &self.kind).field("started", &self.started).field("pending", &self.pending).finish()
    }
}

impl Request {
    fn new(kind: Kind, started: f64, base: String) -> Request {
        let (tx, rx) = std::sync::mpsc::channel();
        Request { kind, started, base, pending: 0, tx, rx: Arc::new(Mutex::new(rx)), brief: None, archive: None, archive_at: None, faces: Vec::new() }
    }
}

/// `/info`: the API version and the server's fonts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerInfo {
    pub api: u64,
    pub fonts: Vec<ServerFont>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerFont {
    pub family: String,
    pub weight: u16,
    pub file: String,
}

/// Server fonts converted to TrueType/OpenType (or why not), by file name.
#[derive(Clone, Default, PartialEq)]
pub struct FontCache(BTreeMap<String, Result<Arc<Vec<u8>>, String>>);

impl std::fmt::Debug for FontCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.0.iter().map(|(k, v)| (k, v.as_ref().map(|b| b.len())))).finish()
    }
}

/// Use `token` for FrameForge requests to the server URL the panel has now (memory only; see
/// [`Token`]). The desktop app passes `FRAMEFORGE_ACCESS_TOKEN` from its environment.
pub fn set_access_token(app: &mut PhotocraftApp, token: String) {
    app.ui.frameforge.token = Token::new(token, &app.ui.frameforge.server_url);
}

/// Menu checkmark for Window › FrameForge.
pub fn checked(app: &PhotocraftApp, id: &str) -> Option<bool> {
    (id == PANEL).then_some(app.ui.frameforge.open)
}

/// Ids this module owns.
pub fn handles(id: &str) -> bool {
    matches!(id, PANEL | CONNECT | DEVELOP | CREATE | CANCEL)
}

pub fn is_enabled(app: &PhotocraftApp, id: &str) -> Option<bool> {
    let f = &app.ui.frameforge;
    let idle = f.request.is_none() && app.services.http.is_some();
    Some(match id {
        PANEL => true,
        CONNECT | DEVELOP => idle,
        CREATE => idle && f.concepts.is_some(),
        CANCEL => f.request.is_some(),
        _ => return None,
    })
}

/// The panel toggle and the `frameforge.*` commands.
pub fn menu(app: &mut PhotocraftApp, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    let r = match id {
        PANEL => {
            app.ui.frameforge.open = !app.ui.frameforge.open;
            return Some(Ok(json!({ "open": app.ui.frameforge.open })));
        }
        CONNECT => connect(app, ctx, params),
        DEVELOP => develop(app, ctx, params),
        CREATE => create(app, ctx, params),
        CANCEL => Ok(cancel(app)),
        _ => return None,
    };
    // The panel shows why a command failed, whoever ran it.
    if let Err(e) = &r {
        app.ui.frameforge.message = Some((e.clone(), true));
    }
    Some(r)
}

/// `frameforge.connect {url?, token?}`: check the server with `GET /info`. A `token` given here
/// belongs to this call's server.
fn connect(app: &mut PhotocraftApp, ctx: &egui::Context, p: &Value) -> Result<Value, String> {
    idle(app)?;
    if let Some(url) = text_param(p, "url")? {
        app.ui.frameforge.server_url = url.trim().to_string();
    }
    let base = base_url(&app.ui.frameforge.server_url)?;
    if let Some(token) = text_param(p, "token")? {
        app.ui.frameforge.token = Token::new(token, &base);
    }
    guard_token(&mut app.ui.frameforge, &base)?;
    let mut req = Request::new(Kind::Connect, now(ctx), base);
    send(app, ctx, &mut req, Call::Info, None)?;
    app.ui.frameforge.request = Some(req);
    app.ui.frameforge.message = Some((tl!("Connecting…").into(), false));
    Ok(json!({ "request": "connect" }))
}

/// `frameforge.develop {channel?: {name, url?, notes?}, video?: {title, summary?}, images?: [..]}`:
/// `POST /concepts` with the brief. Given fields replace the panel's; `images` entries are data
/// URLs, base64 file bytes, or `"document"` (the active document, flattened).
fn develop(app: &mut PhotocraftApp, ctx: &egui::Context, p: &Value) -> Result<Value, String> {
    idle(app)?;
    if let Some(c) = p.get("channel") {
        let c = c.as_object().ok_or(tl!("Brief fields must be text."))?;
        let f = &mut app.ui.frameforge;
        for (key, field) in [("name", &mut f.channel_name), ("url", &mut f.channel_url), ("notes", &mut f.channel_notes)] {
            if let Some(v) = c.get(key) {
                *field = v.as_str().ok_or(tl!("Brief fields must be text."))?.to_string();
            }
        }
    }
    if let Some(v) = p.get("video") {
        let v = v.as_object().ok_or(tl!("Brief fields must be text."))?;
        let f = &mut app.ui.frameforge;
        for (key, field) in [("title", &mut f.video_title), ("summary", &mut f.video_summary)] {
            if let Some(v) = v.get(key) {
                *field = v.as_str().ok_or(tl!("Brief fields must be text."))?.to_string();
            }
        }
    }
    if let Some(list) = p.get("images") {
        let list = list.as_array().ok_or(tl!("Images must be data URLs, base64 file bytes or \"document\"."))?;
        if list.len() > limit::IMAGES {
            return Err(tl!("Attach at most 4 images.").into());
        }
        let images = list.iter().enumerate().map(|(i, v)| image_param(app, i, v)).collect::<Result<Vec<_>, _>>()?;
        app.ui.frameforge.images = images;
    }
    let base = base_url(&app.ui.frameforge.server_url)?;
    guard_token(&mut app.ui.frameforge, &base)?;
    let (channel, video) = brief(&app.ui.frameforge)?;
    let images = app.ui.frameforge.images.clone();
    if images.len() > limit::IMAGES {
        return Err(tl!("Attach at most 4 images.").into());
    }
    let urls = images.iter().map(|i| i.concept_data_url().map(|u| u.as_str().to_string())).collect::<Result<Vec<_>, _>>()?;
    let body = serde_json::to_vec(&json!({ "channel": channel, "video": video, "images": urls })).map_err(|e| e.to_string())?;
    if body.len() > CONCEPTS_BODY {
        return Err(tl!("The brief is larger than the server accepts (10 MiB).").into());
    }
    let mut req = Request::new(Kind::Develop, now(ctx), base);
    req.brief = Some((channel, video, images.clone()));
    send(app, ctx, &mut req, Call::Concepts, Some(body))?;
    let f = &mut app.ui.frameforge;
    f.request = Some(req);
    f.warnings.clear();
    f.message = Some((tl!("Developing concepts…").into(), false));
    Ok(json!({ "request": "develop", "images": images.len() }))
}

/// `frameforge.create {concept: index}`: `POST /materialize` with that concept and the brief's
/// original images, fetch the fonts its type uses, then open the project as a new document.
fn create(app: &mut PhotocraftApp, ctx: &egui::Context, p: &Value) -> Result<Value, String> {
    idle(app)?;
    let base = base_url(&app.ui.frameforge.server_url)?;
    guard_token(&mut app.ui.frameforge, &base)?;
    let concepts = app.ui.frameforge.concepts.as_ref().ok_or(tl!("Develop concepts first."))?;
    let index = p.get("concept").and_then(Value::as_u64).ok_or(tl!("Choose a concept to create."))?;
    let (index, concept) = usize::try_from(index)
        .ok()
        .and_then(|i| Some((i, concepts.list.get(i)?)))
        .ok_or_else(|| fmt(tl!("There is no concept {n}."), &[("n", &index.to_string())]))?;
    let uploads = concepts.uploads.iter().map(BriefImage::upload_data_url).collect::<Result<Vec<_>, _>>()?;
    let body = json!({ "concept": concept, "uploads": uploads, "channel": concepts.channel, "video": concepts.video });
    let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    if body.len() > MATERIALIZE_BODY {
        return Err(tl!("The images are larger than the server accepts (64 MiB in all).").into());
    }
    let name = concepts.name(index).to_string();
    let mut req = Request::new(Kind::Create(index), now(ctx), base);
    req.faces = faces(concept);
    send(app, ctx, &mut req, Call::Materialize, Some(body))?;
    // Fonts load while the server works; the font list comes from /info.
    if app.ui.frameforge.server.is_some() {
        request_fonts(app, ctx, &mut req);
    } else {
        send(app, ctx, &mut req, Call::Info, None)?;
    }
    let f = &mut app.ui.frameforge;
    f.request = Some(req);
    f.warnings.clear();
    f.message = Some((fmt(tl!("Creating {name}…"), &[("name", &name)]), false));
    Ok(json!({ "request": "create", "concept": index, "name": name }))
}

/// `frameforge.cancel`: drop the request in flight; its answer is ignored when it comes.
fn cancel(app: &mut PhotocraftApp) -> Value {
    let cancelled = app.ui.frameforge.request.take().is_some();
    if cancelled {
        app.ui.frameforge.message = Some((tl!("Cancelled.").into(), false));
    }
    json!({ "cancelled": cancelled })
}

fn idle(app: &PhotocraftApp) -> Result<(), String> {
    if app.ui.frameforge.request.is_some() {
        return Err(tl!("A FrameForge request is already running.").into());
    }
    if app.services.http.is_none() {
        return Err(needs_network().into());
    }
    Ok(())
}

fn now(ctx: &egui::Context) -> f64 {
    ctx.input(|i| i.time)
}

fn text_param<'a>(p: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_str().map(Some).ok_or_else(|| tl!("Brief fields must be text.").into()),
    }
}

/// `https://host[:port][/prefix]` without a trailing slash.
/// Plain `http://` only reaches this computer (`localhost`, `127.0.0.0/8`, `[::1]`): anywhere else
/// the access token, the brief and the images would cross the network in cleartext.
pub(crate) fn base_url(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    let invalid = || tl!("Enter the FrameForge server URL (http:// or https://).").to_string();
    let (https, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        _ => return Err(invalid()),
    };
    if url.len() > 2048 || url.chars().any(|c| c.is_whitespace() || c.is_control() || c == '?' || c == '#') {
        return Err(invalid());
    }
    // host[:port], without user info (`http://localhost@elsewhere` is a request to elsewhere).
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((h, "")) => (h, None),
            Some((h, p)) => (h, Some(p.strip_prefix(':').ok_or_else(invalid)?)),
            None => return Err(invalid()),
        },
        None => authority.split_once(':').map_or((authority, None), |(h, p)| (h, Some(p))),
    };
    if host.is_empty() || authority.contains('@') || port.is_some_and(|p| p.parse::<u16>().is_err()) {
        return Err(invalid());
    }
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        || (authority.starts_with('[') && host.parse::<std::net::Ipv6Addr>().is_ok_and(|ip| ip.is_loopback()));
    if !https && !loopback {
        return Err(tl!("Use https:// for servers other than localhost.").into());
    }
    Ok(url.to_string())
}

/// The brief as the server takes it: (channel, video), checked against its limits.
fn brief(f: &FrameForgeUi) -> Result<(Value, Value), String> {
    let name = f.channel_name.trim();
    let title = f.video_title.trim();
    if name.is_empty() {
        return Err(tl!("Enter a channel name.").into());
    }
    if title.is_empty() {
        return Err(tl!("Enter a video title.").into());
    }
    for (label, value, max) in [
        (tl!("Channel name"), name, CHANNEL_NAME),
        (tl!("Channel URL"), f.channel_url.trim(), CHANNEL_URL),
        (tl!("Brand notes"), f.channel_notes.trim(), CHANNEL_NOTES),
        (tl!("Video title"), title, TITLE),
        (tl!("Video summary"), f.video_summary.as_str(), SUMMARY),
    ] {
        // The server counts UTF-16 code units (JavaScript `length`): an emoji is two.
        if value.encode_utf16().count() > max {
            return Err(fmt(tl!("{field} is too long: at most {max} characters."), &[("field", label), ("max", &max.to_string())]));
        }
    }
    let mut channel = json!({ "name": name });
    if !f.channel_url.trim().is_empty() {
        channel["url"] = json!(f.channel_url.trim());
    }
    if !f.channel_notes.trim().is_empty() {
        channel["notes"] = json!(f.channel_notes.trim());
    }
    Ok((channel, json!({ "title": title, "summary": f.video_summary })))
}

/// One `images` entry of `frameforge.develop`.
fn image_param(app: &PhotocraftApp, index: usize, v: &Value) -> Result<BriefImage, String> {
    let invalid = || tl!("Images must be data URLs, base64 file bytes or \"document\".").to_string();
    let (name, data) = match v {
        Value::String(s) if s == "document" => return document_image(app),
        Value::String(s) => (format!("image-{}", index + 1), s.as_str()),
        Value::Object(o) => (
            o.get("name").and_then(Value::as_str).map_or_else(|| format!("image-{}", index + 1), str::to_string),
            o.get("data").and_then(Value::as_str).ok_or_else(invalid)?,
        ),
        _ => return Err(invalid()),
    };
    let encoded = match data.split_once(',') {
        Some((head, rest)) if head.starts_with("data:") && head.ends_with(";base64") => rest,
        Some(_) => return Err(invalid()),
        None => data,
    };
    if encoded.len() > limit::INPUT_BYTES / 3 * 4 + 4 {
        return Err(invalid());
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).map_err(|_| invalid())?;
    BriefImage::new(&name, bytes).map_err(|e| fmt(tl!("{name} can't be used: {error}"), &[("name", &name), ("error", &e)]))
}

/// The active document flattened to PNG through the export service.
fn document_image(app: &PhotocraftApp) -> Result<BriefImage, String> {
    let st = app.session.active().ok_or(tl!("Open a document first."))?;
    let export = app.services.export.as_ref().ok_or(tl!("This build can't export documents."))?;
    // No XMP: it can carry the text of every type layer (#647).
    let settings = crate::ExportSettings { xmp_all: false, ..Default::default() };
    let (bytes, _) = export(&st.doc, "frameforge.png", &settings)?;
    BriefImage::new(&st.doc.name, bytes).map_err(|e| fmt(tl!("{name} can't be used: {error}"), &[("name", &st.doc.name), ("error", &e)]))
}

/// Add an image to the brief (Add image…, Add current document).
fn add_image(app: &mut PhotocraftApp, image: Result<BriefImage, String>) -> Result<Value, String> {
    let f = &mut app.ui.frameforge;
    if f.images.len() >= limit::IMAGES {
        return Err(tl!("Attach at most 4 images.").into());
    }
    let image = image?;
    let r = json!({ "name": image.name, "width": image.width, "height": image.height });
    // An image too large for /concepts stays listed with the reason; Develop reports it.
    if let Err(e) = &image.concept {
        f.message = Some((e.clone(), true));
    }
    f.images.push(image);
    Ok(r)
}

/// Start `call` on the HTTP service. The answer arrives on `req`'s channel and wakes the UI.
fn send(app: &PhotocraftApp, ctx: &egui::Context, req: &mut Request, call: Call, body: Option<Vec<u8>>) -> Result<(), String> {
    let http = app.services.http.as_ref().ok_or(needs_network())?;
    let (path, max_response_bytes) = match &call {
        Call::Info => ("info".to_string(), JSON_ANSWER),
        Call::Concepts => ("concepts".to_string(), JSON_ANSWER),
        Call::Materialize => ("materialize".to_string(), ARCHIVE_ANSWER),
        Call::Font(file) => (format!("fonts/{file}"), photocraft_frameforge::fonts::MAX_WOFF2_BYTES),
    };
    let mut headers = Vec::new();
    if let Some(bearer) = app.ui.frameforge.token.bearer_for(&req.base) {
        headers.push(("Authorization".to_string(), bearer));
    }
    if body.is_some() {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
    let method = if body.is_some() { "POST" } else { "GET" };
    let request =
        HttpRequest { method: method.into(), url: format!("{}/api/native/v1/{path}", req.base), headers, body: body.unwrap_or_default(), max_response_bytes };
    let (tx, ctx) = (req.tx.clone(), ctx.clone());
    req.pending += 1;
    http(
        request,
        Box::new(move |answer| {
            // Nobody listens any more after Cancel.
            let _ = tx.send((call, answer));
            ctx.request_repaint();
        }),
    );
    Ok(())
}

/// The (family, weight) faces a concept's type uses, named as the importer asks for them.
fn faces(concept: &Value) -> Vec<(String, u16)> {
    let mut out: Vec<(String, u16)> = Vec::new();
    for t in concept.get("texts").and_then(Value::as_array).into_iter().flatten() {
        // Materialize writes `font` (default Inter) and `fontWeight` (default 900).
        let font = t.get("font").and_then(Value::as_str).unwrap_or("Inter");
        let weight = t.get("fontWeight").and_then(Value::as_f64).unwrap_or(900.);
        let Ok(style) = photocraft_frameforge::convert::text_style(&json!({ "font": font, "fontWeight": weight })) else { continue };
        let family = style.get("font").and_then(Value::as_str).unwrap_or(font).to_string();
        let weight = style.get("weight").and_then(Value::as_f64).unwrap_or(400.) as u16;
        if !out.iter().any(|(f, w)| f.eq_ignore_ascii_case(&family) && *w == weight) {
            out.push((family, weight));
        }
    }
    out
}

/// The server font for a face.
fn server_font<'a>(info: &'a ServerInfo, family: &str, weight: u16) -> Option<&'a ServerFont> {
    info.fonts.iter().find(|f| f.family.eq_ignore_ascii_case(family) && f.weight == weight)
}

/// Fetch the faces `req`'s concept uses that aren't converted yet, and nothing else.
fn request_fonts(app: &mut PhotocraftApp, ctx: &egui::Context, req: &mut Request) {
    let Some(info) = app.ui.frameforge.server.clone() else { return };
    for (family, weight) in req.faces.clone() {
        let Some(font) = server_font(&info, &family, weight) else { continue };
        if matches!(app.ui.frameforge.fonts.0.get(&font.file), Some(Ok(_))) {
            continue;
        }
        if send(app, ctx, req, Call::Font(font.file.clone()), None).is_err() {
            return;
        }
    }
}

/// Apply the answers that have arrived. Runs every frame (the panel need not be open).
pub fn poll(app: &mut PhotocraftApp, ctx: &egui::Context) {
    poll_at(app, ctx, now(ctx));
}

/// [`poll`] at egui time `now`.
pub(crate) fn poll_at(app: &mut PhotocraftApp, ctx: &egui::Context, now: f64) {
    let Some(mut req) = app.ui.frameforge.request.take() else { return };
    loop {
        let next = req.rx.lock().unwrap_or_else(PoisonError::into_inner).try_recv();
        let (call, answer) = match next {
            Ok(a) => a,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        };
        req.pending = req.pending.saturating_sub(1);
        answered(app, ctx, &mut req, call, answer);
    }
    // Create imports once the fonts settle, or FONT_GRACE after the archive arrived (fonts still
    // missing then fall back, and their late answers are dropped with the request). A failed
    // materialize answer has nothing to wait for.
    let create = match req.kind {
        Kind::Create(index) => Some(index),
        _ => None,
    };
    let failed = req.archive.as_ref().is_some_and(|a| !a.as_ref().is_ok_and(|r| (200..300).contains(&r.status)));
    if req.archive.is_some() && req.archive_at.is_none() {
        req.archive_at = Some(now);
    }
    let overdue = req.archive_at.is_some_and(|at| now - at >= FONT_GRACE);
    match create {
        Some(index) if req.pending == 0 || failed || overdue => finish_create(app, req, index),
        _ if req.pending > 0 => app.ui.frameforge.request = Some(req),
        _ => {}
    }
}

fn answered(app: &mut PhotocraftApp, ctx: &egui::Context, req: &mut Request, call: Call, answer: Result<HttpResponse, String>) {
    let f = &mut app.ui.frameforge;
    match (req.kind, call) {
        (Kind::Connect, Call::Info) => match parse_info(answer) {
            Ok(info) => {
                f.message = Some((fmt(tl!("Connected (api {api})"), &[("api", &info.api.to_string())]), false));
                f.server = Some(info);
            }
            Err(e) => {
                f.server = None;
                f.message = Some((e, true));
                f.reveal = Reveal::Outcome;
            }
        },
        (Kind::Develop, Call::Concepts) => match parse_concepts(answer) {
            Ok(parsed) => {
                let (channel, video, uploads) = req.brief.take().unwrap_or_default();
                let channel = parsed.channel.unwrap_or(channel);
                f.message = Some((fmt(tl!("{count} concepts are ready."), &[("count", &parsed.concepts.len().to_string())]), false));
                f.warnings = parsed.warnings;
                f.concepts = Some(Concepts { list: parsed.concepts, channel_summary: parsed.summary, channel, video, uploads });
                f.reveal = Reveal::Concepts;
            }
            Err(e) => {
                f.message = Some((e, true));
                f.reveal = Reveal::Outcome;
            }
        },
        (Kind::Create(_), Call::Materialize) => req.archive = Some(answer),
        (Kind::Create(_), Call::Info) => match parse_info(answer) {
            Ok(info) => {
                f.server = Some(info);
                request_fonts(app, ctx, req);
            }
            Err(e) => f.warnings.push(fmt(tl!("Server fonts are unavailable: {error}"), &[("error", &e)])),
        },
        (Kind::Create(_), Call::Font(file)) => {
            let font = answer_bytes(answer, photocraft_frameforge::fonts::MAX_WOFF2_BYTES).and_then(|b| photocraft_frameforge::fonts::woff2_to_sfnt(&b));
            f.fonts.0.insert(file, font.map(Arc::new));
        }
        _ => {}
    }
}

/// The materialize answer is in and the fonts settled: open the project as a new document.
fn finish_create(app: &mut PhotocraftApp, req: Request, index: usize) {
    // Calls still unanswered: fonts (or /info) that missed the grace period.
    let late = req.pending > 0;
    let name = app.ui.frameforge.concepts.as_ref().map(|c| c.name(index).to_string()).unwrap_or_default();
    let mut warnings = std::mem::take(&mut app.ui.frameforge.warnings);
    let opened = (|| -> Result<photocraft_frameforge::ImportReport, String> {
        let answer = req.archive.ok_or(tl!("The server didn't answer."))?;
        let (bytes, server_warnings) = materialized(answer)?;
        warnings.extend(server_warnings);
        let mut archive =
            photocraft_frameforge::read_archive(&bytes).map_err(|e| fmt(tl!("The server's answer isn't a FrameForge project: {error}"), &[("error", &e)]))?;
        // A new document named after the concept.
        if let Some(project) = archive.project.as_object_mut()
            && !name.is_empty()
        {
            project.insert("name".into(), json!(name));
        }
        // Converted server fonts for the faces the concept uses; failures fall back in the importer.
        let mut faces: BTreeMap<(String, u16), Arc<Vec<u8>>> = BTreeMap::new();
        if let Some(info) = &app.ui.frameforge.server {
            for (family, weight) in &req.faces {
                let Some(font) = server_font(info, family, *weight) else { continue };
                match app.ui.frameforge.fonts.0.get(&font.file) {
                    Some(Ok(sfnt)) => {
                        faces.insert((family.to_ascii_lowercase(), *weight), sfnt.clone());
                    }
                    Some(Err(e)) => {
                        warnings.push(fmt(tl!("Font {family} {weight}: {error}"), &[("family", family), ("weight", &weight.to_string()), ("error", e)]))
                    }
                    None if late => warnings.push(fmt(
                        tl!("Font {family} {weight}: {error}"),
                        &[("family", family), ("weight", &weight.to_string()), ("error", tl!("The server didn't answer."))],
                    )),
                    None => {}
                }
            }
        } else if late {
            warnings.push(fmt(tl!("Server fonts are unavailable: {error}"), &[("error", tl!("The server didn't answer."))]));
        }
        let resolver = move |family: &str, weight: u16| faces.get(&(family.to_ascii_lowercase(), weight)).map(|b| b.as_ref().clone());
        let options = photocraft_frameforge::ImportOptions { font_resolver: Some(&resolver) };
        crate::frameforge_open::import(app, &archive, &options)
    })();
    let f = &mut app.ui.frameforge;
    match opened {
        Ok(report) => {
            warnings.extend(report.warnings);
            f.message = Some((fmt(tl!("Created {name}"), &[("name", &name)]), false));
            app.ui.status = fmt(tl!("Created {name}"), &[("name", &name)]);
            app.ui.status_error = false;
        }
        Err(e) => f.message = Some((e, true)),
    }
    app.ui.frameforge.warnings = warnings;
    app.ui.frameforge.reveal = Reveal::Outcome;
}

/// The body of a successful answer of at most `max` bytes, or the reason there is none: the
/// transport error, the server's `{error}` (verbatim), or the size.
fn answer_bytes(answer: Result<HttpResponse, String>, max: usize) -> Result<Vec<u8>, String> {
    let r = answer.map_err(|e| fmt(tl!("Couldn't reach the FrameForge server: {error}"), &[("error", &e)]))?;
    if !(200..300).contains(&r.status) {
        let error = serde_json::from_slice::<Value>(&r.body)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(|s| s.chars().take(1000).collect::<String>()));
        let mut message = error.unwrap_or_else(|| fmt(tl!("The FrameForge server answered HTTP {status}."), &[("status", &r.status.to_string())]));
        if let Some(seconds) = r.header("retry-after").and_then(|s| s.trim().parse::<u64>().ok()) {
            message.push(' ');
            message.push_str(&fmt(tl!("Try again in {seconds} s."), &[("seconds", &seconds.min(86_400).to_string())]));
        }
        return Err(message);
    }
    let declared = r.header("content-length").and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
    if declared > max as u64 || r.body.len() > max {
        return Err(fmt(tl!("The FrameForge server's answer is larger than {max} MiB."), &[("max", &(max >> 20).to_string())]));
    }
    Ok(r.body)
}

fn answer_json(answer: Result<HttpResponse, String>) -> Result<Value, String> {
    let body = answer_bytes(answer, JSON_ANSWER)?;
    serde_json::from_slice::<Value>(&body).ok().filter(Value::is_object).ok_or_else(|| tl!("The FrameForge server's answer isn't valid JSON.").into())
}

/// `/info`. Fonts with an unusable name or weight are left out.
pub(crate) fn parse_info(answer: Result<HttpResponse, String>) -> Result<ServerInfo, String> {
    let v = answer_json(answer)?;
    let api = v.get("api").and_then(Value::as_u64).ok_or(tl!("The FrameForge server's answer isn't valid JSON."))?;
    if api != 1 {
        return Err(fmt(tl!("This FrameForge server speaks API {api}; PhotoCraft speaks API 1."), &[("api", &api.to_string())]));
    }
    let fonts = v
        .get("fonts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(1000)
        .filter_map(|f| {
            let family = f.get("family")?.as_str()?.trim();
            let weight = u16::try_from(f.get("weight")?.as_u64()?).ok().filter(|w| (1..=1000).contains(w))?;
            let file = f.get("file")?.as_str()?;
            let safe = file.len() <= 128
                && file.ends_with(".woff2")
                && !file.starts_with('.')
                && file.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
            (safe && !family.is_empty() && family.len() <= 100).then(|| ServerFont { family: family.to_string(), weight, file: file.to_string() })
        })
        .collect();
    Ok(ServerInfo { api, fonts })
}

pub(crate) struct ParsedConcepts {
    pub concepts: Vec<Value>,
    pub summary: String,
    pub channel: Option<Value>,
    pub warnings: Vec<String>,
}

/// `/concepts`: `{result: {channelSummary, concepts: [2..3]}, channel, warnings}`.
pub(crate) fn parse_concepts(answer: Result<HttpResponse, String>) -> Result<ParsedConcepts, String> {
    let v = answer_json(answer)?;
    let result = v.get("result").ok_or(tl!("The FrameForge server's answer isn't valid JSON."))?;
    let list = result.get("concepts").and_then(Value::as_array).ok_or(tl!("The FrameForge server's answer isn't valid JSON."))?;
    if !(2..=3).contains(&list.len()) {
        return Err(fmt(tl!("The FrameForge server sent {count} concepts; PhotoCraft expects 2 or 3."), &[("count", &list.len().to_string())]));
    }
    // Upload indices are null or 0..=3 (the contract's four uploads).
    let upload = |v: Option<&Value>| v.is_none_or(|v| v.is_null() || v.as_u64().is_some_and(|i| i < limit::IMAGES as u64));
    let valid = |c: &Value| {
        c.get("name").and_then(Value::as_str).is_some_and(|n| !n.trim().is_empty() && n.chars().count() <= 200)
            && c.get("texts").and_then(Value::as_array).is_some_and(|t| t.len() <= 16)
            && c.get("images").and_then(Value::as_array).is_some_and(|t| t.len() <= 16 && t.iter().all(|i| upload(i.get("uploadIndex"))))
            && upload(c.get("backgroundUploadIndex"))
    };
    if !list.iter().all(valid) {
        return Err(tl!("The FrameForge server sent an invalid concept.").into());
    }
    Ok(ParsedConcepts {
        concepts: list.clone(),
        summary: result.get("channelSummary").and_then(Value::as_str).unwrap_or_default().chars().take(2000).collect(),
        channel: v.get("channel").filter(|c| c.is_object()).cloned(),
        warnings: strings(v.get("warnings")),
    })
}

/// Bounded warning strings.
fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).take(WARNINGS).map(|s| s.chars().take(WARNING_CHARS).collect()).collect()
}

/// A materialize answer: the archive bytes and the server's `X-FrameForge-Warnings`.
pub(crate) fn materialized(answer: Result<HttpResponse, String>) -> Result<(Vec<u8>, Vec<String>), String> {
    let warnings = answer.as_ref().ok().and_then(|r| r.header("x-frameforge-warnings")).and_then(|h| serde_json::from_str::<Value>(h).ok());
    let bytes = answer_bytes(answer, ARCHIVE_ANSWER)?;
    Ok((bytes, strings(warnings.as_ref())))
}

/// For `ui.inspect`: what the panel shows, without the token or image bytes.
pub fn inspect(app: &PhotocraftApp) -> Value {
    let f = &app.ui.frameforge;
    json!({
        "open": f.open,
        "serverUrl": f.server_url,
        "tokenSet": f.token.is_set(),
        "network": app.services.http.is_some(),
        "api": f.server.as_ref().map(|s| s.api),
        "request": f.request.as_ref().map(|r| json!({"kind": r.kind, "pending": r.pending})),
        "message": f.message.as_ref().map(|(text, error)| json!({"text": text, "error": error})),
        "warnings": f.warnings,
        "images": f.images.iter().map(|i| json!({"name": i.name, "width": i.width, "height": i.height, "error": i.concept.as_ref().err()})).collect::<Vec<_>>(),
        "concepts": f.concepts.as_ref().map(|c| c.list.iter().filter_map(|c| c.get("name")).cloned().collect::<Vec<_>>()),
    })
}

/// Draw Window › FrameForge (and apply answers that arrived, open or not).
pub fn windows(app: &mut PhotocraftApp, ctx: &egui::Context) {
    poll(app, ctx);
    if !app.ui.frameforge.open {
        return;
    }
    let t = Tokens::get(ctx);
    let now = now(ctx);
    let network = app.services.http.is_some();
    // Drawn from a copy taken out of the app: the window helper borrows the app.
    let mut f = std::mem::take(&mut app.ui.frameforge);
    for image in &mut f.images {
        if image.texture.is_none() {
            image.texture = Some(ctx.load_texture(format!("frameforge-{}", image.name), (*image.thumb).clone(), egui::TextureOptions::LINEAR));
        }
    }
    let mut act: Option<(&'static str, Value)> = None;
    let (mut close, mut add_file, mut add_doc) = (false, false, false);
    let mut remove = None;
    let busy = f.request.as_ref().map(|r| (r.kind, (now - r.started).max(0.0)));
    // Over the document area, as tall as it allows: the brief, images and concept cards scroll.
    let room = app.last_canvas_rect;
    let height = (room.height() - 48.0).clamp(420.0, 900.0);
    crate::analysis_ui::panel_window_in(app, ctx, "frameforge", tl!("FrameForge"), (room, vec2(0.0, 16.0)), 420.0, Some(height), |ui| {
        close = crate::analysis_ui::title_row(ui, "FrameForge");
        if !network {
            ui.label(RichText::new(needs_network()).color(t.text_dim));
            return;
        }
        egui::ScrollArea::vertical().max_height(height - 40.0).auto_shrink([false, true]).show(ui, |ui| {
            ui.set_width(404.0);
            crate::widgets::section_label(ui, "Server");
            field(ui, tl!("Server URL"), &mut f.server_url, CHANNEL_URL, "https://");
            ui.horizontal(|ui| {
                ui.add_sized([96.0, 18.0], egui::Label::new(RichText::new(tl!("Access token")).color(t.text_dim)));
                // A token typed here belongs to the server URL above (see `Token`).
                if ui.add(egui::TextEdit::singleline(&mut f.token.secret).password(true).desired_width(200.0)).changed() {
                    f.token.origin = base_url(&f.server_url).ok().map(|b| origin(&b).to_string());
                }
                ui.add_enabled_ui(busy.is_none(), |ui| {
                    if crate::widgets::secondary_button(ui, tl!("Connect"), 0.0).clicked() {
                        act = Some((CONNECT, json!({})));
                    }
                });
            });
            ui.label(RichText::new(tl!("The token stays in memory and is never saved.")).size(10.5).color(t.text_faint));
            ui.add_space(6.0);
            crate::widgets::section_label(ui, "Brief");
            field(ui, tl!("Channel name"), &mut f.channel_name, CHANNEL_NAME, "");
            field(ui, tl!("Channel URL"), &mut f.channel_url, CHANNEL_URL, "https://www.youtube.com/@");
            area(ui, tl!("Brand notes"), &mut f.channel_notes, CHANNEL_NOTES, 2);
            field(ui, tl!("Video title"), &mut f.video_title, TITLE, "");
            area(ui, tl!("Video summary"), &mut f.video_summary, SUMMARY, 3);
            ui.add_space(6.0);
            crate::widgets::section_label(ui, "Images");
            ui.horizontal_wrapped(|ui| {
                for (i, image) in f.images.iter().enumerate() {
                    ui.vertical(|ui| {
                        if let Some(tex) = &image.texture {
                            let size = tex.size_vec2();
                            let scale = 72.0 / size.x.max(size.y).max(1.0);
                            ui.add(egui::Image::new((tex.id(), size * scale))).on_hover_text(format!(
                                "{} · {}×{}{}",
                                image.name,
                                image.width,
                                image.height,
                                image.concept.as_ref().err().map(|e| format!("\n{e}")).unwrap_or_default()
                            ));
                        }
                        if crate::icons::button(ui, "x", 18.0, false, tl!("Remove image")).clicked() {
                            remove = Some(i);
                        }
                    });
                }
            });
            ui.horizontal(|ui| {
                ui.add_enabled_ui(busy.is_none() && f.images.len() < limit::IMAGES, |ui| {
                    add_file = crate::widgets::secondary_button(ui, tl!("Add image…"), 0.0).clicked();
                    add_doc = crate::widgets::secondary_button(ui, tl!("Add current document"), 0.0).clicked();
                });
            });
            ui.add_space(8.0);
            let outcome = egui::Rect::from_min_size(ui.cursor().min, vec2(1.0, 16.0));
            ui.horizontal(|ui| {
                ui.add_enabled_ui(busy.is_none(), |ui| {
                    if crate::widgets::primary_button(ui, tl!("Develop concepts"), 140.0).clicked() {
                        act = Some((DEVELOP, json!({})));
                    }
                });
                if let Some((_, seconds)) = busy {
                    ui.add(egui::Spinner::new().size(14.0).color(t.accent));
                    ui.label(RichText::new(fmt(tl!("{seconds} s"), &[("seconds", &(seconds as u64).to_string())])).color(t.text_dim).monospace());
                    if crate::widgets::secondary_button(ui, tl!("Cancel"), 0.0).clicked() {
                        act = Some((CANCEL, json!({})));
                    }
                }
            });
            if let Some((text, error)) = &f.message {
                ui.label(RichText::new(text).color(if *error { t.danger } else { t.text_dim }));
            }
            if !f.warnings.is_empty() {
                crate::widgets::section_label(ui, "Warnings");
                for w in &f.warnings {
                    ui.label(RichText::new(format!("• {w}")).size(11.0).color(t.warning));
                }
            }
            if f.reveal == Reveal::Outcome && !ui.is_sizing_pass() {
                ui.scroll_to_rect(outcome, Some(egui::Align::TOP));
                f.reveal = Reveal::Nothing;
            }
            if let Some(concepts) = &f.concepts {
                ui.add_space(6.0);
                let heading = egui::Rect::from_min_size(ui.cursor().min, vec2(1.0, 16.0));
                crate::widgets::section_label(ui, "Concepts");
                if f.reveal == Reveal::Concepts && !ui.is_sizing_pass() {
                    ui.scroll_to_rect(heading, Some(egui::Align::TOP));
                    f.reveal = Reveal::Nothing;
                }
                if !concepts.channel_summary.is_empty() {
                    ui.label(RichText::new(&concepts.channel_summary).size(11.0).color(t.text_dim));
                }
                for (i, concept) in concepts.list.iter().enumerate() {
                    if card(ui, &t, concept, busy.is_none()) {
                        act = Some((CREATE, json!({ "concept": i })));
                    }
                }
            }
        });
    });
    app.ui.frameforge = f;
    if close {
        app.ui.frameforge.open = false;
    }
    if let Some(i) = remove.filter(|i| *i < app.ui.frameforge.images.len()) {
        app.ui.frameforge.images.remove(i);
    }
    if add_doc {
        let image = document_image(app);
        if let Err(e) = add_image(app, image) {
            app.ui.frameforge.message = Some((e, true));
        }
    }
    if add_file {
        let r = app.pick_file_bytes(|app, name, bytes| {
            let name = crate::file_open::display_name(&name);
            let image = BriefImage::new(&name, bytes).map_err(|e| fmt(tl!("{name} can't be used: {error}"), &[("name", &name), ("error", &e)]));
            let r = add_image(app, image);
            if let Err(e) = &r {
                app.ui.frameforge.message = Some((e.clone(), true));
            }
            r
        });
        if let Err(e) = r.as_ref()
            && e != crate::file_dialog::CANCELLED
        {
            app.ui.frameforge.message = Some((e.clone(), true));
        }
    }
    if let Some((id, params)) = act {
        // The user pressed a button for the URL in the field: a token typed before there was a
        // URL (or one from the environment) belongs to it now. Commands never bind it.
        let f = &mut app.ui.frameforge;
        if f.token.is_set() && f.token.origin.is_none() {
            f.token.origin = base_url(&f.server_url).ok().map(|b| origin(&b).to_string());
        }
        let _ = crate::menus::invoke(app, ctx, id, params);
    }
    if app.ui.frameforge.request.is_some() {
        // The elapsed seconds.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

fn field(ui: &mut egui::Ui, label: &str, text: &mut String, max: usize, hint: &str) {
    let t = Tokens::get(ui.ctx());
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 18.0], egui::Label::new(RichText::new(label).color(t.text_dim)));
        ui.add(egui::TextEdit::singleline(text).char_limit(max).hint_text(hint).desired_width(300.0));
    });
}

fn area(ui: &mut egui::Ui, label: &str, text: &mut String, max: usize, rows: usize) {
    let t = Tokens::get(ui.ctx());
    ui.horizontal_top(|ui| {
        ui.add_sized([96.0, 18.0], egui::Label::new(RichText::new(label).color(t.text_dim)));
        ui.add(egui::TextEdit::multiline(text).char_limit(max).desired_rows(rows).desired_width(300.0));
    });
}

/// One concept: name, rationale, texts with their fonts, a schematic and Create. True on Create.
fn card(ui: &mut egui::Ui, t: &Tokens, concept: &Value, enabled: bool) -> bool {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let mut create = false;
    egui::Frame::new().fill(t.card).stroke(egui::Stroke::new(1.0, t.card_border)).corner_radius(t.radius).inner_margin(8.0).show(ui, |ui| {
        ui.set_width(388.0);
        ui.label(RichText::new(s(concept, "name")).strong().color(t.text));
        let rationale = s(concept, "rationale");
        if !rationale.is_empty() {
            ui.label(RichText::new(rationale).size(11.0).color(t.text_dim));
        }
        for text in concept.get("texts").and_then(Value::as_array).into_iter().flatten() {
            let weight = text.get("fontWeight").and_then(Value::as_u64).unwrap_or(900);
            ui.label(
                RichText::new(format!("“{}” · {} {weight}", s(text, "text"), text.get("font").and_then(Value::as_str).unwrap_or("Inter")))
                    .size(11.0)
                    .color(t.text),
            );
        }
        schematic(ui, t, concept, 388.0);
        ui.add_enabled_ui(enabled, |ui| {
            create = crate::widgets::primary_button(ui, tl!("Create"), 96.0).clicked();
        });
    });
    ui.add_space(4.0);
    create
}

/// The concept at FrameForge's percent geometry on a 16:9 frame: #111 like FrameForge's empty
/// canvas, image slots outlined and labelled, text in its fill colour; `layers[0]` (the first
/// text) on top, as materialize stacks them (texts, then images, then the background).
fn schematic(ui: &mut egui::Ui, t: &Tokens, concept: &Value, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, width * 9.0 / 16.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    // FrameForge's canvas colour: data, not a theme colour.
    painter.rect_filled(rect, 0.0, Color32::from_rgb(0x11, 0x11, 0x11));
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0).clamp(-100.0, 200.0) as f32;
    let at = |v: &Value| {
        egui::Rect::from_min_size(
            rect.min + vec2(num(v, "x") / 100.0 * rect.width(), num(v, "y") / 100.0 * rect.height()),
            vec2(num(v, "w").max(0.0) / 100.0 * rect.width(), num(v, "h").max(0.0) / 100.0 * rect.height()),
        )
    };
    let outline = egui::Stroke::new(1.0, t.text_dim);
    let label = |r: egui::Rect, text: String| {
        painter.rect_stroke(r.shrink(0.5), 0.0, outline, egui::StrokeKind::Inside);
        painter.text(r.min + vec2(4.0, 3.0), egui::Align2::LEFT_TOP, text, egui::FontId::proportional(10.0), t.text_dim);
    };
    let upload = |i: u64| fmt(tl!("upload {n}"), &[("n", &i.saturating_add(1).to_string())]);
    match concept.get("backgroundUploadIndex").and_then(Value::as_u64) {
        Some(i) => label(rect, upload(i)),
        None => label(rect, tl!("generated").to_string()),
    }
    let images: Vec<&Value> = concept.get("images").and_then(Value::as_array).into_iter().flatten().collect();
    for image in images.iter().rev() {
        let text = match image.get("uploadIndex").and_then(Value::as_u64) {
            Some(i) if image.get("source").and_then(Value::as_str) != Some("generate") => upload(i),
            _ => tl!("generated").to_string(),
        };
        label(at(image), text);
    }
    let texts: Vec<&Value> = concept.get("texts").and_then(Value::as_array).into_iter().flatten().collect();
    for text in texts.iter().rev() {
        let r = at(text);
        let fill =
            text.get("fill").and_then(Value::as_str).and_then(|c| photocraft_frameforge::convert::color(c).ok()).map_or(Color32::WHITE, |[r, g, b, a]| {
                Color32::from_rgba_unmultiplied((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8, (a * 255.0) as u8)
            });
        // fontSize is in FrameForge's 1280-pixel-wide canvas.
        let size = (num(text, "fontSize") * rect.width() / 1280.0).clamp(6.0, 64.0);
        let align = match text.get("align").and_then(Value::as_str) {
            Some("center") => egui::Align2::CENTER_CENTER,
            Some("right") => egui::Align2::RIGHT_CENTER,
            _ => egui::Align2::LEFT_CENTER,
        };
        let anchor = match align {
            egui::Align2::CENTER_CENTER => r.center(),
            egui::Align2::RIGHT_CENTER => r.right_center(),
            _ => r.left_center(),
        };
        let string = text.get("text").and_then(Value::as_str).unwrap_or_default();
        painter.with_clip_rect(r.intersect(rect)).text(anchor, align, string, egui::FontId::proportional(size), fill);
        painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, t.text_faint), egui::StrokeKind::Inside);
    }
}
