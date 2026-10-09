//! Synthetic `.frameforge` archives for tests (this crate's and the PhotoCraft shell's). Not
//! part of the importer: it writes the ZIP shape [`crate::read_archive`] reads.
use std::io::Write;

/// A ZIP of `(path, bytes, deflate)` entries. Deflate falls back to stored if it fails.
pub fn zip(entries: &[(&str, Vec<u8>, bool)]) -> Vec<u8> {
    let le32 = |n: usize| u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes();
    let le16 = |n: usize| u16::try_from(n).unwrap_or(u16::MAX).to_le_bytes();
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for (name, data, deflate) in entries {
        let local = out.len();
        let packed = deflate
            .then(|| {
                let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(data).ok()?;
                e.finish().ok()
            })
            .flatten();
        let method = if packed.is_some() { 8 } else { 0 };
        let compressed = packed.unwrap_or_else(|| data.clone());
        let crc = crc32fast::hash(data).to_le_bytes();
        out.extend_from_slice(b"PK\x03\x04");
        for n in [20, 0, method, 0, 0] {
            out.extend(le16(n));
        }
        out.extend(crc);
        out.extend(le32(compressed.len()));
        out.extend(le32(data.len()));
        out.extend(le16(name.len()));
        out.extend(le16(0));
        out.extend(name.as_bytes());
        out.extend(&compressed);
        directory.extend_from_slice(b"PK\x01\x02");
        for n in [20, 20, 0, method, 0, 0] {
            directory.extend(le16(n));
        }
        directory.extend(crc);
        directory.extend(le32(compressed.len()));
        directory.extend(le32(data.len()));
        for n in [name.len(), 0, 0, 0, 0] {
            directory.extend(le16(n));
        }
        directory.extend(le32(0));
        directory.extend(le32(local));
        directory.extend(name.as_bytes());
    }
    let at = out.len();
    out.extend(&directory);
    out.extend_from_slice(b"PK\x05\x06");
    for n in [0, 0, entries.len(), entries.len()] {
        out.extend(le16(n));
    }
    out.extend(le32(directory.len()));
    out.extend(le32(at));
    out.extend(le16(0));
    out
}

/// WOFF2 for a TrueType/OpenType font, written without a Brotli encoder: every table uses the
/// null transform and the compressed stream is a run of uncompressed Brotli meta-blocks
/// (RFC 7932 §9.2), which WOFF2 decoders read like any other stream. `None` if `sfnt` isn't one.
pub fn woff2_stored(sfnt: &[u8]) -> Option<Vec<u8>> {
    let be16 = |at: usize| Some(u16::from_be_bytes(sfnt.get(at..at + 2)?.try_into().ok()?));
    let be32 = |at: usize| Some(u32::from_be_bytes(sfnt.get(at..at + 4)?.try_into().ok()?));
    let flavor = be32(0)?;
    let count = usize::from(be16(4)?);
    let mut directory = Vec::new();
    let mut data = Vec::new();
    let mut padded = 0;
    for i in 0..count {
        let record = 12 + 16 * i;
        let tag = sfnt.get(record..record + 4)?;
        let (offset, length) = (usize::try_from(be32(record + 8)?).ok()?, usize::try_from(be32(record + 12)?).ok()?);
        // glyf/loca: transform version 3 is their null transform; other tables: arbitrary tag
        // (63) with version 0.
        match tag {
            b"glyf" => directory.push(0xC0 | 10),
            b"loca" => directory.push(0xC0 | 11),
            _ => {
                directory.push(63);
                directory.extend_from_slice(tag);
            }
        }
        base128(&mut directory, u32::try_from(length).ok()?);
        data.extend_from_slice(sfnt.get(offset..offset.checked_add(length)?)?);
        padded += length.next_multiple_of(4);
    }
    let stream = brotli_stored(&data);
    let sfnt_size = 12 + 16 * count + padded;
    let total = (48 + directory.len() + stream.len()).next_multiple_of(4);
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"wOF2");
    out.extend(flavor.to_be_bytes());
    out.extend(u32::try_from(total).ok()?.to_be_bytes());
    out.extend(u16::try_from(count).ok()?.to_be_bytes());
    out.extend(0u16.to_be_bytes());
    out.extend(u32::try_from(sfnt_size).ok()?.to_be_bytes());
    out.extend(u32::try_from(stream.len()).ok()?.to_be_bytes());
    out.extend([0u8; 4]); // version 0.0
    out.extend([0u8; 20]); // no metadata or private blocks
    out.extend(directory);
    out.extend(stream);
    out.resize(total, 0);
    Some(out)
}

/// WOFF2's UIntBase128.
fn base128(out: &mut Vec<u8>, n: u32) {
    let mut groups = vec![(n & 0x7f) as u8];
    let mut rest = n >> 7;
    while rest > 0 {
        groups.push(0x80 | (rest & 0x7f) as u8);
        rest >>= 7;
    }
    out.extend(groups.into_iter().rev());
}

/// A Brotli stream holding `data` in uncompressed meta-blocks of at most 64 KiB.
fn brotli_stored(data: &[u8]) -> Vec<u8> {
    let mut w = Bits::default();
    w.put(0, 1); // WBITS = 16
    for chunk in data.chunks(65536) {
        // ISLAST = 0, MNIBBLES = 4, MLEN - 1 in 16 bits, ISUNCOMPRESSED = 1, then byte-aligned bytes.
        w.put(0, 1);
        w.put(0, 2);
        w.put(chunk.len().saturating_sub(1) as u64, 16);
        w.put(1, 1);
        w.align();
        w.out.extend_from_slice(chunk);
    }
    w.put(0b11, 2); // ISLAST = 1, ISLASTEMPTY = 1
    w.align();
    w.out
}

/// LSB-first bit writer.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u64, bits: u32) {
        self.acc |= value << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push((self.acc & 0xff) as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    fn align(&mut self) {
        if self.n > 0 {
            self.put(0, 8 - self.n);
        }
    }
}
