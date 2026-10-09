//! Bounded ZIP central-directory reader. No extraction to disk.
use crate::{Archive, Result};
use serde_json::Value;
use std::{collections::BTreeMap, io::Read};
const ENTRY_LIMIT: usize = 32 * 1024 * 1024;
const TOTAL_LIMIT: usize = 128 * 1024 * 1024;
fn slice(b: &[u8], at: usize, n: usize) -> Result<&[u8]> {
    b.get(at..at.checked_add(n).ok_or("ZIP offset overflow")?).ok_or_else(|| "truncated ZIP".into())
}
fn u16le(b: &[u8], at: usize) -> Result<usize> {
    let a: [u8; 2] = slice(b, at, 2)?.try_into().map_err(|_| "truncated ZIP")?;
    Ok(u16::from_le_bytes(a).into())
}
fn u32le(b: &[u8], at: usize) -> Result<usize> {
    let a: [u8; 4] = slice(b, at, 4)?.try_into().map_err(|_| "truncated ZIP")?;
    Ok(u32::from_le_bytes(a) as usize)
}
fn asset_path(s: &str) -> bool {
    s.strip_prefix("assets/asset-").is_some_and(|s| s.len() >= 4 && s.bytes().all(|c| c.is_ascii_digit()))
}
pub fn read_archive(bytes: &[u8]) -> Result<Archive> {
    if bytes.len() > TOTAL_LIMIT {
        return Err("archive container exceeds 128 MiB".into());
    }
    let end = (bytes.len().saturating_sub(65557)..bytes.len().saturating_sub(21))
        .rev()
        .find(|&at| bytes.get(at..at + 4) == Some(b"PK\x05\x06") && u16le(bytes, at + 20).is_ok_and(|n| at + 22 + n == bytes.len()))
        .ok_or("missing ZIP end directory")?;
    if u16le(bytes, end + 4)? != 0 || u16le(bytes, end + 6)? != 0 {
        return Err("multi-disk ZIP is unsupported".into());
    }
    let count = u16le(bytes, end + 10)?;
    if count > 256 || count != u16le(bytes, end + 8)? {
        return Err("ZIP entry count exceeds 256 or is inconsistent".into());
    }
    let cd = u32le(bytes, end + 16)?;
    let cd_size = u32le(bytes, end + 12)?;
    if cd.checked_add(cd_size) != Some(end) {
        return Err("invalid ZIP directory bounds".into());
    }
    let mut at = cd;
    let mut entries = BTreeMap::new();
    let mut total = 0usize;
    let mut spans = Vec::new();
    for _ in 0..count {
        if slice(bytes, at, 4)? != b"PK\x01\x02" {
            return Err("invalid ZIP directory entry".into());
        }
        let flags = u16le(bytes, at + 8)?;
        let method = u16le(bytes, at + 10)?;
        if flags & (1 | 64 | 8192) != 0 {
            return Err("encrypted ZIP entry".into());
        }
        if !matches!(method, 0 | 8) {
            return Err("unsupported ZIP compression".into());
        }
        let crc = u32le(bytes, at + 16)? as u32;
        let compressed = u32le(bytes, at + 20)?;
        let expanded = u32le(bytes, at + 24)?;
        if expanded > ENTRY_LIMIT || (method == 0 && compressed > ENTRY_LIMIT) {
            return Err("ZIP entry exceeds 32 MiB".into());
        }
        let name_len = u16le(bytes, at + 28)?;
        let extra_len = u16le(bytes, at + 30)?;
        let comment_len = u16le(bytes, at + 32)?;
        if u16le(bytes, at + 34)? != 0 {
            return Err("multi-disk ZIP entry".into());
        }
        let local = u32le(bytes, at + 42)?;
        let name = std::str::from_utf8(slice(bytes, at + 46, name_len)?).map_err(|_| "non UTF-8 ZIP path")?.to_string();
        if name != "manifest.json" && !asset_path(&name) {
            return Err(format!("unexpected archive path: {name}"));
        }
        if entries.contains_key(&name) {
            return Err(format!("duplicate ZIP path: {name}"));
        }
        at = at.checked_add(46 + name_len + extra_len + comment_len).filter(|n| *n <= end).ok_or("invalid directory lengths")?;
        if slice(bytes, local, 4)? != b"PK\x03\x04" || u16le(bytes, local + 6)? != flags || u16le(bytes, local + 8)? != method {
            return Err("inconsistent ZIP local header".into());
        }
        let ln = u16le(bytes, local + 26)?;
        let le = u16le(bytes, local + 28)?;
        if slice(bytes, local + 30, ln)? != name.as_bytes() {
            return Err("inconsistent ZIP local name".into());
        }
        let start = local.checked_add(30 + ln + le).ok_or("invalid ZIP data offset")?;
        let finish = start.checked_add(compressed).filter(|n| *n <= cd).ok_or("ZIP payload overlaps directory")?;
        if spans.iter().any(|&(a, b)| local < b && finish > a) {
            return Err("overlapping ZIP entries".into());
        }
        spans.push((local, finish));
        let data = slice(bytes, start, compressed)?;
        let mut out = Vec::new();
        let remaining = TOTAL_LIMIT.saturating_sub(total);
        if method == 0 {
            if data.len() > remaining {
                return Err("ZIP total exceeds 128 MiB".into());
            }
            out.extend_from_slice(data);
        } else {
            // A lying uncompressed-size header must not allow a deflate bomb.
            let mut decoder = flate2::read::DeflateDecoder::new(data);
            (&mut decoder).take((ENTRY_LIMIT.min(remaining) + 1) as u64).read_to_end(&mut out).map_err(|e| format!("ZIP deflate: {e}"))?;
            if out.len() > remaining {
                return Err("ZIP total exceeds 128 MiB".into());
            }
            if out.len() > ENTRY_LIMIT {
                return Err("ZIP inflate exceeds 32 MiB".into());
            }
            if decoder.total_in() != compressed as u64 {
                return Err("trailing or truncated deflate data".into());
            }
        }
        total = total.checked_add(out.len()).filter(|n| *n <= TOTAL_LIMIT).ok_or("ZIP total exceeds 128 MiB")?;
        if out.len() != expanded || crc32fast::hash(&out) != crc {
            return Err("ZIP size or CRC mismatch".into());
        }
        entries.insert(name, out);
    }
    if at != end {
        return Err("ZIP directory size mismatch".into());
    }
    let manifest: Value = serde_json::from_slice(entries.get("manifest.json").ok_or("missing manifest.json")?).map_err(|e| format!("manifest JSON: {e}"))?;
    if manifest.get("format").and_then(Value::as_str) != Some("frameforge-project") || manifest.get("formatVersion").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported FrameForge format or formatVersion".into());
    }
    let project = manifest.get("project").filter(|p| p.is_object()).ok_or("missing project")?.clone();
    let layers = project.get("layers").and_then(Value::as_array).ok_or("missing project layers")?;
    if layers.len() > 500 || layers.iter().any(|l| !l.is_object()) {
        return Err("invalid layers or layer count exceeds 500".into());
    }
    let mut assets = BTreeMap::new();
    for a in manifest.get("assets").and_then(Value::as_array).ok_or("missing assets")? {
        let path = a.get("path").and_then(Value::as_str).ok_or("missing asset path")?;
        let reference = a.get("ref").and_then(Value::as_str).ok_or("missing asset ref")?;
        if !asset_path(path) || path.strip_prefix("assets/") != Some(reference) {
            return Err("invalid asset path/ref".into());
        }
        let data = entries.remove(path).ok_or_else(|| format!("missing asset {path}"))?;
        if a.get("size").and_then(Value::as_u64) != Some(data.len() as u64) {
            return Err("asset size mismatch".into());
        }
        if assets.insert(reference.to_string(), data).is_some() {
            return Err("duplicate asset ref".into());
        }
    }
    entries.remove("manifest.json");
    if !entries.is_empty() {
        return Err("unlisted archive assets".into());
    }
    Ok(Archive { project, assets })
}
