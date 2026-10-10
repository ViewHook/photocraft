//! FrameForge serves its bundled fonts as WOFF2 (`GET /api/native/v1/fonts/<file>`); PhotoCraft's
//! type engine reads TrueType/OpenType (sfnt). [`woff2_to_sfnt`] converts one to the other.
use crate::Result;

/// The largest WOFF2 file accepted. FrameForge's bundled latin subsets are tens of KiB.
pub const MAX_WOFF2_BYTES: usize = 8 * 1024 * 1024;

/// Decode WOFF2 bytes to the sfnt font they compress. Malformed input is an `Err`.
pub fn woff2_to_sfnt(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() > MAX_WOFF2_BYTES {
        return Err("font file exceeds 8 MiB".into());
    }
    if bytes.get(..4) != Some(b"wOF2".as_slice()) {
        return Err("not a WOFF2 font".into());
    }
    let sfnt = decode(bytes)?;
    match sfnt.get(..4) {
        Some([0, 1, 0, 0] | b"OTTO" | b"true") => Ok(sfnt),
        _ => Err("WOFF2 did not contain a TrueType or OpenType font".into()),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn decode(bytes: &[u8]) -> Result<Vec<u8>> {
    // Server bytes are untrusted: a decoder panic must not take the open documents with it.
    std::panic::catch_unwind(|| wuff::decompress_woff2(bytes))
        .map_err(|_| "the WOFF2 decoder failed".to_string())?
        .map_err(|e| format!("invalid WOFF2 font: {e}"))
}

#[cfg(target_arch = "wasm32")]
fn decode(bytes: &[u8]) -> Result<Vec<u8>> {
    wuff::decompress_woff2(bytes).map_err(|e| format!("invalid WOFF2 font: {e}"))
}
