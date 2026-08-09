//! Port of `packages/coding-agent/src/utils/mime.ts` @ pi 0.82.1 (2efa728).
//!
//! Image magic-number sniffing: detect supported image MIME types from a
//! leading byte buffer. Supports JPEG, PNG (excluding APNG), GIF, WebP, BMP.

/// Number of bytes to read from the file header for MIME-type sniffing
/// (mime.ts:3).
pub const IMAGE_TYPE_SNIFF_BYTES: usize = 4100;

/// PNG 8-byte signature (mime.ts:4).
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

// ---------------------------------------------------------------------------
// detectSupportedImageMimeType (mime.ts:6-23)
// ---------------------------------------------------------------------------

/// Detect a supported image MIME type from a byte buffer.
///
/// Checks magic numbers in order: JPEG → PNG → GIF → WebP → BMP.
/// Returns the MIME type string (`"image/jpeg"`, `"image/png"`, etc.) or
/// `None` if the buffer is not a recognised (or supported) image.
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<&'static str> {
    // JPEG (mime.ts:7-9): FF D8 FF, reject SOF7 (buffer[3] == 0xF7).
    if starts_with(buffer, &[0xff, 0xd8, 0xff]) {
        return if buffer.get(3).copied() == Some(0xf7) {
            None
        } else {
            Some("image/jpeg")
        };
    }

    // PNG (mime.ts:10-11): 8-byte signature + valid IHDR + not animated.
    if starts_with(buffer, &PNG_SIGNATURE) {
        return if is_png(buffer) && !is_animated_png(buffer) {
            Some("image/png")
        } else {
            None
        };
    }

    // GIF (mime.ts:12-14): ASCII "GIF".
    if starts_with_ascii(buffer, 0, "GIF") {
        return Some("image/gif");
    }

    // WebP (mime.ts:15-17): "RIFF" at 0 + "WEBP" at 8.
    if starts_with_ascii(buffer, 0, "RIFF") && starts_with_ascii(buffer, 8, "WEBP") {
        return Some("image/webp");
    }

    // BMP (mime.ts:18-20): "BM" + DIB validation.
    if starts_with_ascii(buffer, 0, "BM") && is_bmp(buffer) {
        return Some("image/bmp");
    }

    None
}

// ---------------------------------------------------------------------------
// isPng (mime.ts:36-39)
// ---------------------------------------------------------------------------

/// Validate a PNG buffer: must have IHDR chunk of length 13 right after the
/// 8-byte signature.
fn is_png(buffer: &[u8]) -> bool {
    buffer.len() >= 16
        && read_u32_be(buffer, PNG_SIGNATURE.len()) == 13
        && starts_with_ascii(buffer, 12, "IHDR")
}

// ---------------------------------------------------------------------------
// isAnimatedPng (mime.ts:42-55)
// ---------------------------------------------------------------------------

/// Detect APNG: return `true` if an `acTL` chunk appears before the first
/// `IDAT` chunk.
fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buffer.len() {
        let chunk_length = read_u32_be(buffer, offset);
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(buffer, chunk_type_offset, "acTL") {
            return true;
        }
        if starts_with_ascii(buffer, chunk_type_offset, "IDAT") {
            return false;
        }

        // Advance past length(4) + type(4) + data(chunkLength) + CRC(4).
        let next_offset = offset + 8 + chunk_length as usize + 4;
        if next_offset <= offset || next_offset > buffer.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

// ---------------------------------------------------------------------------
// isBmp (mime.ts:57-81)
// ---------------------------------------------------------------------------

/// Validate a BMP buffer via DIB header checks (mime.ts:57-81).
fn is_bmp(buffer: &[u8]) -> bool {
    if buffer.len() < 26 {
        return false;
    }

    let declared_file_size = read_u32_le(buffer, 2);
    let pixel_data_offset = read_u32_le(buffer, 10);
    let dib_header_size = read_u32_le(buffer, 14);

    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    // Compare in u64 to avoid overflow on large dib_header_size.
    if (pixel_data_offset as u64) < 14 + (dib_header_size as u64) {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }

    let color_planes: u16;
    let bits_per_pixel: u16;
    if dib_header_size == 12 {
        // BITMAPCOREHEADER / OS/2 1.x (mime.ts:69-71)
        color_planes = read_u16_le(buffer, 22);
        bits_per_pixel = read_u16_le(buffer, 24);
    } else if (40..=124).contains(&dib_header_size) {
        // BITMAPINFOHEADER and later versions (mime.ts:72-75)
        if buffer.len() < 30 {
            return false;
        }
        color_planes = read_u16_le(buffer, 26);
        bits_per_pixel = read_u16_le(buffer, 28);
    } else {
        return false;
    }

    color_planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits_per_pixel)
}

// ---------------------------------------------------------------------------
// Byte-reading helpers (mime.ts:83-103)
// ---------------------------------------------------------------------------

/// Read a little-endian u16 at `offset`, returning 0 for out-of-bounds bytes
/// (mime.ts:83-85).
fn read_u16_le(buffer: &[u8], offset: usize) -> u16 {
    let b0 = *buffer.get(offset).unwrap_or(&0) as u16;
    let b1 = *buffer.get(offset + 1).unwrap_or(&0) as u16;
    b0 + (b1 << 8)
}

/// Read a big-endian u32 at `offset`, returning 0 for out-of-bounds bytes
/// (mime.ts:87-94). Uses multiplication like the JS original to match
/// unsigned semantics.
fn read_u32_be(buffer: &[u8], offset: usize) -> u32 {
    let b0 = *buffer.get(offset).unwrap_or(&0) as u32;
    let b1 = *buffer.get(offset + 1).unwrap_or(&0) as u32;
    let b2 = *buffer.get(offset + 2).unwrap_or(&0) as u32;
    let b3 = *buffer.get(offset + 3).unwrap_or(&0) as u32;
    b0 * 0x1000000 + (b1 << 16) + (b2 << 8) + b3
}

/// Read a little-endian u32 at `offset`, returning 0 for out-of-bounds bytes
/// (mime.ts:96-103).
fn read_u32_le(buffer: &[u8], offset: usize) -> u32 {
    let b0 = *buffer.get(offset).unwrap_or(&0) as u32;
    let b1 = *buffer.get(offset + 1).unwrap_or(&0) as u32;
    let b2 = *buffer.get(offset + 2).unwrap_or(&0) as u32;
    let b3 = *buffer.get(offset + 3).unwrap_or(&0) as u32;
    b0 + (b1 << 8) + (b2 << 16) + b3 * 0x1000000
}

// ---------------------------------------------------------------------------
// startsWith / startsWithAscii (mime.ts:105-116)
// ---------------------------------------------------------------------------

/// Check whether `buffer` starts with the given byte sequence (mime.ts:105-108).
fn starts_with(buffer: &[u8], bytes: &[u8]) -> bool {
    if buffer.len() < bytes.len() {
        return false;
    }
    bytes.iter().enumerate().all(|(i, &b)| buffer[i] == b)
}

/// Check whether `buffer` contains the ASCII string `text` at `offset`
/// (mime.ts:110-116).
fn starts_with_ascii(buffer: &[u8], offset: usize, text: &str) -> bool {
    let text_bytes = text.as_bytes();
    if buffer.len() < offset + text_bytes.len() {
        return false;
    }
    text_bytes
        .iter()
        .enumerate()
        .all(|(i, &b)| buffer[offset + i] == b)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Positive cases ----

    #[test]
    fn test_jpeg_detected() {
        let buf = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x00];
        assert_eq!(detect_supported_image_mime_type(&buf), Some("image/jpeg"));
    }

    #[test]
    fn test_png_detected() {
        // Minimal valid PNG header: signature + IHDR chunk (length 13)
        let mut buf = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        // IHDR length (13 = 0x0000000D, big-endian)
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x0d]);
        // "IHDR"
        buf.extend_from_slice(b"IHDR");
        // Need at least 16 bytes total — we have 16 already
        assert_eq!(detect_supported_image_mime_type(&buf), Some("image/png"));
    }

    #[test]
    fn test_gif_detected() {
        let buf = b"GIF89a rest of header";
        assert_eq!(detect_supported_image_mime_type(buf), Some("image/gif"));
    }

    #[test]
    fn test_webp_detected() {
        let mut buf = b"RIFF".to_vec();
        buf.extend_from_slice(&[0x00; 4]); // file size
        buf.extend_from_slice(b"WEBP");
        buf.extend_from_slice(&[0x00; 20]);
        assert_eq!(detect_supported_image_mime_type(&buf), Some("image/webp"));
    }

    #[test]
    fn test_bmp_detected() {
        // Minimal valid BMP: "BM" + valid DIB header (40-byte BITMAPINFOHEADER)
        let mut buf = vec![0u8; 30];
        buf[0] = b'B';
        buf[1] = b'M';
        // declaredFileSize at offset 2 (LE) — 0 means "not specified" (skips
        // the declaredFileSize checks in upstream mime.ts:63-65).
        buf[2] = 0;
        // pixelDataOffset at offset 10 (LE) — must be >= 14 + dibHeaderSize
        buf[10] = 54;
        // dibHeaderSize at offset 14 (LE) — 40
        buf[14] = 40;
        // colorPlanes at offset 26 (LE) — 1
        buf[26] = 1;
        // bitsPerPixel at offset 28 (LE) — 24
        buf[28] = 24;
        assert_eq!(detect_supported_image_mime_type(&buf), Some("image/bmp"));
    }

    // ---- JPEG rejection sub-rule ----

    #[test]
    fn test_jpeg_sof7_rejected() {
        let buf = [0xff, 0xd8, 0xff, 0xf7];
        assert_eq!(detect_supported_image_mime_type(&buf), None);
    }

    // ---- PNG rejection sub-rules ----

    #[test]
    fn test_png_invalid_ihdr_rejected() {
        // PNG signature but wrong IHDR length
        let mut buf = PNG_SIGNATURE.to_vec();
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x20]); // length 32, not 13
        buf.extend_from_slice(b"IHDR");
        buf.extend_from_slice(&[0x00; 10]);
        assert_eq!(detect_supported_image_mime_type(&buf), None);
    }

    #[test]
    fn test_animated_png_rejected() {
        // PNG signature + IHDR + acTL chunk before IDAT
        let mut buf = PNG_SIGNATURE.to_vec();
        // IHDR chunk: length 13
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x0d]);
        buf.extend_from_slice(b"IHDR");
        buf.extend_from_slice(&[0x00; 13]); // IHDR data
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // IHDR CRC
                                                          // acTL chunk: length 8
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x08]);
        buf.extend_from_slice(b"acTL");
        buf.extend_from_slice(&[0x00; 12]); // acTL data + CRC
        assert_eq!(detect_supported_image_mime_type(&buf), None);
    }

    // ---- BMP rejection sub-rule ----

    #[test]
    fn test_bmp_bad_bpp_rejected() {
        let mut buf = vec![0u8; 30];
        buf[0] = b'B';
        buf[1] = b'M';
        buf[2] = 0; // declaredFileSize = 0 (unspecified)
        buf[10] = 54; // pixelDataOffset >= 14+40
        buf[14] = 40; // dibHeaderSize
        buf[26] = 1; // colorPlanes
        buf[28] = 7; // bitsPerPixel = 7 (invalid)
        assert_eq!(detect_supported_image_mime_type(&buf), None);
    }

    // ---- Non-image returns None ----

    #[test]
    fn test_non_image_returns_none() {
        let buf = b"Hello, world! This is a text file.";
        assert_eq!(detect_supported_image_mime_type(buf), None);
    }

    #[test]
    fn test_empty_buffer_returns_none() {
        assert_eq!(detect_supported_image_mime_type(&[]), None);
    }

    // ---- is_animated_png returns false when IDAT comes first ----

    #[test]
    fn test_static_png_idat_first_not_animated() {
        let mut buf = PNG_SIGNATURE.to_vec();
        // IHDR chunk: length 13
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x0d]);
        buf.extend_from_slice(b"IHDR");
        buf.extend_from_slice(&[0x00; 13]); // IHDR data
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // IHDR CRC
                                                          // IDAT chunk before any acTL → static PNG
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x10]);
        buf.extend_from_slice(b"IDAT");
        buf.extend_from_slice(&[0x00; 20]); // IDAT data + CRC
        assert_eq!(detect_supported_image_mime_type(&buf), Some("image/png"));
    }
}
