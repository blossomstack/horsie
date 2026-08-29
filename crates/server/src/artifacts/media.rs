//! What a pile of bytes actually is, and how big it is.
//!
//! Everything here is a pure function over a byte slice — no I/O, no database,
//! no configuration — so the rules are testable against real file headers
//! rather than against a mock.
//!
//! **Nothing here trusts a declared type.** A browser upload's `Content-Type`
//! is set by the client and is routinely wrong or empty; an MCP tool result's
//! `mimeType` is whatever the tool server chose to say. Both are claims about
//! the bytes, and this module answers with the bytes themselves.

/// A media type horsie will store, and what shape it is.
///
/// The allow-list *is* this enum: anything that does not sniff to one of these
/// is refused at the door, so an unsupported file is never stored and can never
/// reach a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniffed {
    Png,
    Jpeg,
    Gif,
    Webp,
    Pdf,
}

impl Sniffed {
    /// The canonical media type. This is what gets stored and what a provider
    /// is told — never the caller's claim.
    pub fn media_type(self) -> &'static str {
        match self {
            Sniffed::Png => "image/png",
            Sniffed::Jpeg => "image/jpeg",
            Sniffed::Gif => "image/gif",
            Sniffed::Webp => "image/webp",
            Sniffed::Pdf => "application/pdf",
        }
    }

    /// `"image"` or `"document"` — the `ArtifactKind` discriminant.
    pub fn kind(self) -> &'static str {
        match self {
            Sniffed::Png | Sniffed::Jpeg | Sniffed::Gif | Sniffed::Webp => "image",
            Sniffed::Pdf => "document",
        }
    }

    pub fn is_image(self) -> bool {
        self.kind() == "image"
    }
}

/// Identify `bytes`, or `None` if they are not something we accept.
pub fn sniff(bytes: &[u8]) -> Option<Sniffed> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(Sniffed::Png);
    }
    // Start-of-image plus the first marker's prefix. Three bytes rather than
    // two, because `FF D8` alone also prefixes a truncated or empty file.
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Sniffed::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(Sniffed::Gif);
    }
    // RIFF container, then the form type at offset 8. The length field between
    // them is not checked: a truncated WebP is still a WebP, and rejecting it
    // here would report the wrong problem.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(Sniffed::Webp);
    }
    if bytes.starts_with(b"%PDF-") {
        return Some(Sniffed::Pdf);
    }
    None
}

/// An image's pixel dimensions, read from its header.
///
/// Header-only by design: the width and height of every format here live in the
/// first few dozen bytes, so this never decodes pixels. Dimensions are a layout
/// hint — the UI reserves space so the transcript does not jump when a
/// thumbnail loads — which is why every unparseable case returns `None` rather
/// than guessing. A missing dimension costs a small layout shift; a wrong one
/// renders the image at the wrong size.
pub fn dimensions(kind: Sniffed, bytes: &[u8]) -> Option<(u32, u32)> {
    match kind {
        Sniffed::Png => png(bytes),
        Sniffed::Jpeg => jpeg(bytes),
        Sniffed::Gif => gif(bytes),
        Sniffed::Webp => webp(bytes),
        // A page count would need a real PDF parser. A document renders as a
        // file chip rather than a thumbnail, so nothing needs one.
        Sniffed::Pdf => None,
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// IHDR is mandated to be the first chunk, at a fixed offset.
fn png(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || &b[12..16] != b"IHDR" {
        return None;
    }
    Some((be32(&b[16..20]), be32(&b[20..24])))
}

/// Walk the marker segments to the start-of-frame, which carries the size.
///
/// The size is not at a fixed offset: an ordinary photo has EXIF, ICC and
/// quantization segments of arbitrary length ahead of it.
fn jpeg(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2; // past FF D8
    while i + 9 < b.len() {
        if b[i] != 0xFF {
            // Not on a marker boundary — a malformed or unusual file. Stop
            // rather than resynchronising and risking a wrong answer.
            return None;
        }
        let marker = b[i + 1];
        // Standalone markers: no length, no payload.
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        // Every start-of-frame except DHT (C4), DAC (CC) and the restart
        // markers carries the dimensions in the same place.
        let is_sof = (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xCC;
        if is_sof {
            // length, precision, then height before width.
            return Some((
                u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32,
                u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32,
            ));
        }
        if len < 2 {
            return None;
        }
        i += 2 + len;
    }
    None
}

/// Little-endian, in the logical screen descriptor right after the signature.
fn gif(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 10 {
        return None;
    }
    Some((
        u16::from_le_bytes([b[6], b[7]]) as u32,
        u16::from_le_bytes([b[8], b[9]]) as u32,
    ))
}

/// Three sub-formats, each storing the size differently.
fn webp(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 16 {
        return None;
    }
    match &b[12..16] {
        // Lossy: a VP8 keyframe header, 14 bytes in, 14 bits each.
        b"VP8 " => {
            if b.len() < 30 {
                return None;
            }
            let w = u16::from_le_bytes([b[26], b[27]]) & 0x3FFF;
            let h = u16::from_le_bytes([b[28], b[29]]) & 0x3FFF;
            Some((w as u32, h as u32))
        }
        // Lossless: 14 bits each, packed across four bytes, both minus one.
        b"VP8L" => {
            if b.len() < 25 {
                return None;
            }
            let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        // Extended: 24-bit little-endian canvas size, both minus one.
        b"VP8X" => {
            if b.len() < 30 {
                return None;
            }
            let w = u32::from_le_bytes([b[24], b[25], b[26], 0]) + 1;
            let h = u32::from_le_bytes([b[27], b[28], b[29], 0]) + 1;
            Some((w, h))
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A real 1x1 PNG.
    fn png_1x1() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // "IHDR"
            0x00, 0x00, 0x00, 0x01, // width 1
            0x00, 0x00, 0x00, 0x01, // height 1
            0x08, 0x06, 0x00, 0x00, 0x00,
        ]
    }

    #[test]
    fn sniffs_each_accepted_format() {
        assert_eq!(sniff(&png_1x1()), Some(Sniffed::Png));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Sniffed::Jpeg));
        assert_eq!(sniff(b"GIF89a....."), Some(Sniffed::Gif));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(Sniffed::Webp));
        assert_eq!(sniff(b"%PDF-1.7\n"), Some(Sniffed::Pdf));
    }

    #[test]
    fn refuses_anything_else() {
        for bytes in [
            &b"not a file at all"[..],
            &b""[..],
            &b"\xFF\xD8"[..],                 // truncated JPEG signature
            &b"RIFF\0\0\0\0WAVE"[..],         // a RIFF that is not a WebP
            &b"<!DOCTYPE html><html>"[..],    // HTML, a plausible upload mistake
            &b"MZ\x90\x00"[..],               // a Windows executable
        ] {
            assert_eq!(sniff(bytes), None, "should refuse: {bytes:?}");
        }
    }

    /// The whole reason this module exists: the caller's claim never wins.
    #[test]
    fn the_sniffed_type_is_independent_of_any_claim() {
        // A PDF uploaded as `image/png`, which is what a confused client sends.
        let sniffed = sniff(b"%PDF-1.7\n").unwrap();
        assert_eq!(sniffed.media_type(), "application/pdf");
        assert_eq!(sniffed.kind(), "document");

        // An executable renamed `photo.png` is refused, not stored as an image.
        assert_eq!(sniff(b"MZ\x90\x00"), None);
    }

    #[test]
    fn reads_png_dimensions() {
        assert_eq!(dimensions(Sniffed::Png, &png_1x1()), Some((1, 1)));
    }

    #[test]
    fn reads_jpeg_dimensions_past_a_leading_segment() {
        // SOI, an APP0/JFIF segment, then SOF0 with 16x32. The point is that
        // the size is not at a fixed offset.
        let mut b = vec![0xFF, 0xD8];
        b.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x06, 1, 2, 3, 4]); // APP0, len 6
        b.extend_from_slice(&[
            0xFF, 0xC0, // SOF0
            0x00, 0x11, // length
            0x08, // precision
            0x00, 0x20, // height 32
            0x00, 0x10, // width 16
            0x03,
        ]);
        assert_eq!(dimensions(Sniffed::Jpeg, &b), Some((16, 32)));
    }

    #[test]
    fn reads_gif_dimensions_little_endian() {
        let b = b"GIF89a\x20\x00\x10\x00\x00\x00";
        assert_eq!(dimensions(Sniffed::Gif, b), Some((32, 16)));
    }

    #[test]
    fn reads_webp_lossy_dimensions() {
        let mut b = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
        b.extend_from_slice(&[0, 0, 0, 0]); // chunk size
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // frame tag + sync code
        b.extend_from_slice(&[0x20, 0x00, 0x10, 0x00]); // 32 x 16
        assert_eq!(dimensions(Sniffed::Webp, &b), Some((32, 16)));
    }

    /// Truncated or odd headers must return `None`, never a wrong size and
    /// never a panic — these bytes arrive from the network.
    #[test]
    fn unreadable_headers_yield_none_rather_than_a_guess() {
        assert_eq!(dimensions(Sniffed::Png, b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(dimensions(Sniffed::Jpeg, &[0xFF, 0xD8]), None);
        assert_eq!(dimensions(Sniffed::Gif, b"GIF89a"), None);
        assert_eq!(dimensions(Sniffed::Webp, b"RIFF\0\0\0\0WEBP"), None);
        // A JPEG whose segment chain runs off the end rather than reaching SOF.
        assert_eq!(dimensions(Sniffed::Jpeg, &[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF]), None);
    }

    #[test]
    fn a_pdf_has_no_dimensions() {
        assert_eq!(dimensions(Sniffed::Pdf, b"%PDF-1.7\n"), None);
    }
}
