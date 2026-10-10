//! Images for Window › FrameForge (`frameforge_ui`): checked when added, decoded once off the UI
//! thread ([`Decoding`]), then sent as data URLs. `/concepts` gets a copy downscaled to fit its
//! 2 MiB data-URL limit; `/materialize` gets the original file when the server accepts it as it
//! is, otherwise a re-encode within its 12 MiB / 8192 px / 40-megapixel limits. Encoding goes
//! through `photocraft-codecs`.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use photocraft_codecs::{ChannelLayout, DecodeOptions, EncodeOptions, Format, Image, Limits};

/// The server's limits (youtube-thumbnail-app `docs/native-client-api.md`, "Limits and errors").
pub(crate) mod limit {
    /// Images per brief (both endpoints).
    pub const IMAGES: usize = 4;
    /// `/concepts`: each data URL, encoded.
    pub const CONCEPT_DATA_URL: usize = 2 * 1024 * 1024;
    /// `/concepts`: the long edge of the downscaled copy.
    pub const CONCEPT_EDGE: u32 = 1600;
    /// `/materialize`: each upload, decoded.
    pub const UPLOAD_BYTES: usize = 12 * 1024 * 1024;
    pub const UPLOAD_SIDE: u32 = 8192;
    pub const UPLOAD_PIXELS: u64 = 40_000_000;
    /// Files larger than this aren't read (a FrameForge brief is a few stills).
    pub const INPUT_BYTES: usize = 256 * 1024 * 1024;
    /// Decoded images larger than this are refused before allocating (400 MB as RGBA8).
    pub const INPUT_PIXELS: u64 = 100_000_000;
}

/// The long edge of panel thumbnails, in pixels.
const THUMB_EDGE: u32 = 96;

/// An image attached to a FrameForge brief.
#[derive(Clone)]
pub struct BriefImage {
    pub name: String,
    /// Upright size in pixels.
    pub width: u32,
    pub height: u32,
    /// The bytes as added (a file, or the flattened document as PNG).
    original: Arc<Vec<u8>>,
    /// The MIME type the original goes to `/materialize` as, when it can go as it is.
    as_is: Option<&'static str>,
    /// The `/concepts` copy (a data URL), or why there is none.
    pub(crate) concept: Result<Arc<String>, String>,
    /// Panel thumbnail (RGBA) and its texture, made on first draw.
    pub(crate) thumb: Arc<egui::ColorImage>,
    pub(crate) texture: Option<egui::TextureHandle>,
}

impl PartialEq for BriefImage {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && Arc::ptr_eq(&self.original, &other.original)
    }
}

impl std::fmt::Debug for BriefImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BriefImage")
            .field("name", &self.name)
            .field("size", &(self.width, self.height))
            .field("bytes", &self.original.len())
            .field("concept", &self.concept.as_ref().map(|u| u.len()))
            .finish()
    }
}

impl BriefImage {
    /// Decode `bytes` (PNG, JPEG or WebP: what the server takes) and prepare its copies, here and
    /// now. The panel decodes through [`Decoding`] instead.
    pub fn new(name: &str, bytes: Vec<u8>) -> Result<BriefImage, String> {
        Checked::new(name, bytes)?.decode()
    }

    /// The `/concepts` data URL.
    pub fn concept_data_url(&self) -> Result<Arc<String>, String> {
        self.concept.clone()
    }

    /// The `/materialize` data URL: the original when the server takes it as it is, otherwise
    /// a re-encode within the upload limits.
    pub fn upload_data_url(&self) -> Result<String, String> {
        if let Some(mime) = self.as_is {
            return Ok(data_url(mime, &self.original));
        }
        let (rgba, width, height, _) = decode(&self.original)?;
        let (mime, bytes) = fit(&rgba, width, height, limit::UPLOAD_SIDE, limit::UPLOAD_PIXELS, limit::UPLOAD_BYTES, true)
            .map_err(|_| crate::i18n::fmt(tl!("{name} is too large to send (at most 12 MiB and 8192 pixels)."), &[("name", &self.name)]))?;
        Ok(data_url(mime, &bytes))
    }
}

/// A file accepted for a brief (its size, and PNG, JPEG or WebP by its signature) but not decoded
/// yet: the check is cheap and runs where the file arrives; [`Checked::decode`] is the slow part.
pub struct Checked {
    name: String,
    bytes: Vec<u8>,
    mime: &'static str,
}

impl Checked {
    pub fn new(name: &str, bytes: Vec<u8>) -> Result<Checked, String> {
        if bytes.len() > limit::INPUT_BYTES {
            return Err(crate::i18n::fmt(tl!("{name} is larger than {max} MiB."), &[("name", name), ("max", &(limit::INPUT_BYTES >> 20).to_string())]));
        }
        // Checked before any decoder sees the bytes.
        let mime = match photocraft_codecs::detect(&bytes) {
            Some(Format::Png) => "image/png",
            Some(Format::Jpeg) => "image/jpeg",
            Some(Format::WebP) => "image/webp",
            _ => return Err(crate::i18n::fmt(tl!("{name} isn't a PNG, JPEG or WebP image."), &[("name", name)])),
        };
        Ok(Checked { name: name.to_string(), bytes, mime })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Decode and prepare the copies: the `/concepts` data URL and the panel thumbnail.
    pub fn decode(self) -> Result<BriefImage, String> {
        let Checked { name, bytes, mime } = self;
        let (rgba, width, height, upright) = decode(&bytes)?;
        let fits = bytes.len() <= limit::UPLOAD_BYTES
            && width <= limit::UPLOAD_SIDE
            && height <= limit::UPLOAD_SIDE
            && u64::from(width) * u64::from(height) <= limit::UPLOAD_PIXELS;
        let as_is = (fits && upright).then_some(mime);
        let concept = fit(&rgba, width, height, limit::CONCEPT_EDGE, u64::MAX, concept_bytes(), false)
            .map(|(mime, b)| Arc::new(data_url(mime, &b)))
            .map_err(|_| crate::i18n::fmt(tl!("{name} doesn't fit the server's 2 MiB limit, even at 1600 pixels."), &[("name", &name)]));
        let (tw, th) = scaled(width, height, THUMB_EDGE, u64::MAX);
        let thumb = egui::ColorImage::from_rgba_unmultiplied([tw as usize, th as usize], &resize(&rgba, width, height, tw, th));
        Ok(BriefImage { name, width, height, original: Arc::new(bytes), as_is, concept, thumb: Arc::new(thumb), texture: None })
    }
}

/// How the panel decodes added images.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeMode {
    /// On a worker thread; the result comes back over a channel drained every frame (native).
    Thread,
    /// On the UI thread, but on a frame after the one that drew the placeholder, so the panel
    /// repaints first (the web: wasm has no threads).
    NextFrame,
    /// Only when the test releases it.
    #[cfg(test)]
    Held,
}

impl Default for DecodeMode {
    fn default() -> Self {
        if cfg!(target_arch = "wasm32") { DecodeMode::NextFrame } else { DecodeMode::Thread }
    }
}

enum Job {
    /// Not on wasm, which has no threads.
    #[cfg(not(target_arch = "wasm32"))]
    Thread(Receiver<Result<BriefImage, String>>),
    /// Frames to wait before decoding on the UI thread.
    NextFrame(Option<Checked>, u8),
    #[cfg(test)]
    Held(Option<Checked>),
    Done(Result<BriefImage, String>),
}

/// An image being decoded: the panel shows a placeholder row until it is [`Decoding::done`].
/// Dropping it (the row's ×, Cancel, a new `images` list) drops the result: a worker still
/// decoding finishes and its answer goes nowhere.
#[derive(Clone)]
pub struct Decoding {
    pub name: String,
    job: Arc<Mutex<Job>>,
}

impl PartialEq for Decoding {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && Arc::ptr_eq(&self.job, &other.job)
    }
}

impl std::fmt::Debug for Decoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoding").field("name", &self.name).finish()
    }
}

impl Decoding {
    /// Start decoding `image`; `ctx` repaints when a worker is done.
    pub fn start(image: Checked, mode: DecodeMode, ctx: &egui::Context) -> Decoding {
        let name = image.name.clone();
        let job = match mode {
            DecodeMode::Thread => thread(image, ctx),
            DecodeMode::NextFrame => Job::NextFrame(Some(image), 1),
            #[cfg(test)]
            DecodeMode::Held => Job::Held(Some(image)),
        };
        Decoding { name, job: Arc::new(Mutex::new(job)) }
    }

    /// Move the decode on (one call per frame) and say whether its result is in. `ui_thread` is
    /// whether this frame may still decode an image on the UI thread (one per frame).
    pub fn step(&self, ctx: &egui::Context, ui_thread: &mut bool) -> bool {
        let mut job = self.job.lock().unwrap_or_else(PoisonError::into_inner);
        let next = match &mut *job {
            #[cfg(not(target_arch = "wasm32"))]
            Job::Thread(rx) => match rx.try_recv() {
                Ok(result) => Job::Done(result),
                Err(TryRecvError::Empty) => return false,
                // The worker died without an answer.
                Err(TryRecvError::Disconnected) => Job::Done(Err(tl!("the image decoder failed").into())),
            },
            Job::NextFrame(image, frames) => {
                // Not in the frame that adds the image: that one draws its placeholder first.
                if *frames > 0 || !*ui_thread {
                    *frames = frames.saturating_sub(1);
                    ctx.request_repaint();
                    return false;
                }
                match image.take() {
                    Some(image) => {
                        *ui_thread = false;
                        Job::Done(image.decode())
                    }
                    None => return false,
                }
            }
            #[cfg(test)]
            Job::Held(_) => return false,
            Job::Done(_) => return true,
        };
        *job = next;
        true
    }

    /// The result, once [`Decoding::step`] said it is in.
    pub fn take(&self) -> Option<Result<BriefImage, String>> {
        let mut job = self.job.lock().unwrap_or_else(PoisonError::into_inner);
        match std::mem::replace(&mut *job, Job::NextFrame(None, 0)) {
            Job::Done(result) => Some(result),
            other => {
                *job = other;
                None
            }
        }
    }

    /// Tests: decode a held image now.
    #[cfg(test)]
    pub fn release(&self) {
        let mut job = self.job.lock().unwrap_or_else(PoisonError::into_inner);
        if let Job::Held(image) = &mut *job
            && let Some(image) = image.take()
        {
            *job = Job::Done(image.decode());
        }
    }
}

/// Decode on a worker thread, like the histogram job (`tone.rs`); when no thread can start,
/// fall back to [`DecodeMode::NextFrame`].
fn thread(image: Checked, ctx: &egui::Context) -> Job {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        // Shared so a failed spawn (which drops the closure) leaves the image here.
        let slot = Arc::new(Mutex::new(Some(image)));
        let (worker, ctx) = (slot.clone(), ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("frameforge-decode".into())
            .spawn(move || {
                let image = worker.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some(image) = image {
                    // The receiver is gone when the image was removed meanwhile: dropped.
                    let _ = tx.send(image.decode());
                    ctx.request_repaint();
                }
            })
            .is_ok();
        if spawned {
            return Job::Thread(rx);
        }
        // No thread: the closure was dropped unrun and the image is still in the slot.
        let left = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
        match left {
            Some(image) => Job::NextFrame(Some(image), 1),
            None => Job::Done(Err(tl!("the image decoder failed").into())),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = ctx;
        Job::NextFrame(Some(image), 1)
    }
}

/// The largest encoded image whose data URL fits the `/concepts` limit.
fn concept_bytes() -> usize {
    (limit::CONCEPT_DATA_URL - "data:image/jpeg;base64,".len()) / 4 * 3
}

pub fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Decoded upright RGBA8 pixels, the size, and whether the stored pixels already were upright.
/// The bytes come from anywhere: a decoder panic is an `Err` (native; wasm aborts on panics).
fn decode(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32, bool), String> {
    #[cfg(not(target_arch = "wasm32"))]
    return std::panic::catch_unwind(|| decode_unguarded(bytes)).unwrap_or_else(|_| Err(tl!("the image decoder failed").into()));
    #[cfg(target_arch = "wasm32")]
    decode_unguarded(bytes)
}

fn decode_unguarded(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32, bool), String> {
    let opts = DecodeOptions {
        limits: Limits { max_width: 1 << 16, max_height: 1 << 16, max_pixels: limit::INPUT_PIXELS, max_alloc: limit::INPUT_PIXELS * 16 },
        keep_orientation: true,
    };
    let image = photocraft_codecs::decode_with(bytes, &opts).map_err(|e| e.to_string())?;
    let orientation = image.meta.exif.as_deref().map_or(1, photocraft_codecs::exif_orientation);
    let image = if orientation == 1 { image } else { image.oriented(orientation).map_err(|e| e.to_string())? };
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Err("empty image".into());
    }
    Ok((image.to_rgba8(), width, height, orientation == 1))
}

/// `(width, height)` scaled down (never up) to fit `edge` on the long side and `pixels` in area.
pub(crate) fn scaled(width: u32, height: u32, edge: u32, pixels: u64) -> (u32, u32) {
    let (w, h) = (u64::from(width.max(1)), u64::from(height.max(1)));
    let (edge, long) = (u64::from(edge.max(1)), w.max(h));
    // The long side lands exactly on `edge`.
    let (mut w, mut h) = if long > edge { (w * edge / long, h * edge / long) } else { (w, h) };
    if w * h > pixels {
        let s = (pixels as f64 / (w * h) as f64).sqrt();
        (w, h) = ((w as f64 * s).floor() as u64, (h as f64 * s).floor() as u64);
    }
    (w.clamp(1, u64::from(u32::MAX)) as u32, h.clamp(1, u64::from(u32::MAX)) as u32)
}

/// Encode `rgba` at the largest size within `edge`/`pixels` as PNG (when it has transparency
/// and `keep_alpha`) or JPEG, trying lower JPEG qualities until it is at most `max_bytes`.
fn fit(rgba: &[u8], width: u32, height: u32, edge: u32, pixels: u64, max_bytes: usize, keep_alpha: bool) -> Result<(&'static str, Vec<u8>), String> {
    let (w, h) = scaled(width, height, edge, pixels);
    let px = if (w, h) == (width, height) { rgba.to_vec() } else { resize(rgba, width, height, w, h) };
    let translucent = px.as_chunks::<4>().0.iter().any(|[.., a]| *a < 255);
    if translucent {
        let png = encode(Image::from_u8(w, h, ChannelLayout::Rgba, px.clone()), Format::Png, 90)?;
        if png.len() <= max_bytes {
            return Ok(("image/png", png));
        }
        if keep_alpha {
            return Err("too large".into());
        }
    }
    // JPEG has no alpha: composite on white, as a viewer shows a transparent PNG.
    let rgb: Vec<u8> = px
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let [r, g, b, a] = p.map(u32::from);
            [r, g, b].map(|c| ((c * a + 255 * (255 - a) + 127) / 255) as u8)
        })
        .collect();
    for quality in [90, 80, 65, 50] {
        let jpeg = encode(Image::from_u8(w, h, ChannelLayout::Rgb, rgb.clone()), Format::Jpeg, quality)?;
        if jpeg.len() <= max_bytes {
            return Ok(("image/jpeg", jpeg));
        }
    }
    Err("too large".into())
}

fn encode(image: Result<Image, photocraft_codecs::CodecError>, format: Format, quality: u8) -> Result<Vec<u8>, String> {
    let image = image.map_err(|e| e.to_string())?;
    // No metadata: the copies carry pixels only.
    let opts = EncodeOptions { jpeg_quality: quality, embed_icc: false, embed_metadata: false, ..Default::default() };
    photocraft_codecs::encode(&image, format, &opts).map_err(|e| e.to_string())
}

/// Area-average downscale of RGBA8 (alpha-weighted, so transparent pixels don't bleed).
pub(crate) fn resize(rgba: &[u8], width: u32, height: u32, nw: u32, nh: u32) -> Vec<u8> {
    let (width, height, nw, nh) = (width as usize, height as usize, nw.max(1) as usize, nh.max(1) as usize);
    let mut out = Vec::with_capacity(nw * nh * 4);
    for oy in 0..nh {
        let y0 = oy * height / nh;
        let y1 = ((oy + 1) * height / nh).max(y0 + 1).min(height);
        for ox in 0..nw {
            let x0 = ox * width / nw;
            let x1 = ((ox + 1) * width / nw).max(x0 + 1).min(width);
            let mut sum = [0u64; 4];
            let mut n = 0u64;
            for y in y0..y1 {
                let row = rgba.get(y * width * 4 + x0 * 4..y * width * 4 + x1 * 4).unwrap_or_default();
                for &[r, g, b, a] in row.as_chunks::<4>().0 {
                    let a64 = u64::from(a);
                    sum[0] += u64::from(r) * a64;
                    sum[1] += u64::from(g) * a64;
                    sum[2] += u64::from(b) * a64;
                    sum[3] += a64;
                    n += 1;
                }
            }
            let alpha = sum[3];
            let c = |s: u64| (s + alpha / 2).checked_div(alpha).unwrap_or(0).min(255) as u8;
            out.extend([c(sum[0]), c(sum[1]), c(sum[2]), (alpha + n / 2).checked_div(n).unwrap_or(0).min(255) as u8]);
        }
    }
    out
}
