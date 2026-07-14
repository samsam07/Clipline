//! Pure conversions between wire formats (SPEC.md §9) and Windows clipboard byte
//! layouts. No Win32 calls here — these are deterministic and unit-tested without a
//! real clipboard, so the format fidelity is proven independently of the flaky,
//! global, one-at-a-time OS clipboard.
//!
//! * Text: UTF-8 on the wire  <->  UTF-16LE (`CF_UNICODETEXT`).
//! * Image: PNG on the wire   <->  `CF_DIB` (packed `BITMAPINFOHEADER` + pixels).

use std::io::Cursor;

use image::{ImageError, RgbaImage};

/// `BITMAPINFOHEADER` is 40 bytes. We emit exactly this (no V4/V5 extension); we can
/// *read* larger headers (V4/V5) by honoring their declared size.
const BITMAPINFOHEADER_SIZE: u32 = 40;
const BI_RGB: u32 = 0;

/// UTF-8 text -> a NUL-terminated UTF-16LE code-unit buffer for `CF_UNICODETEXT`.
pub fn text_to_utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A `CF_UNICODETEXT` code-unit buffer -> `String`, stopping at the first NUL.
///
/// Inbound half (OS format -> wire): production callers are the Send/copy path (M3)
/// and file materialize; for M1 it is exercised by the round-trip tests.
#[allow(dead_code)]
pub fn utf16_to_string(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// Errors from the image codec path.
#[derive(Debug)]
pub enum CodecError {
    Image(ImageError),
    /// The DIB was malformed or uses a layout we don't decode (see `dib_to_png`).
    #[allow(dead_code)] // constructed by the inbound `dib_to_png` (see below)
    Dib(&'static str),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::Image(e) => write!(f, "image codec: {e}"),
            CodecError::Dib(m) => write!(f, "DIB layout: {m}"),
        }
    }
}

impl std::error::Error for CodecError {}

impl From<ImageError> for CodecError {
    fn from(e: ImageError) -> Self {
        CodecError::Image(e)
    }
}

/// PNG bytes (wire) -> a packed `CF_DIB`: a 40-byte `BITMAPINFOHEADER` followed by
/// 32bpp BGRA pixels, **bottom-up** (positive height — the traditional, most-
/// compatible layout). Alpha is carried in the 4th byte, so our own round-trip is
/// lossless.
pub fn png_to_dib(png: &[u8]) -> Result<Vec<u8>, CodecError> {
    let rgba = image::load_from_memory(png)?.to_rgba8();
    let (w, h) = rgba.dimensions();

    let mut out = Vec::with_capacity(BITMAPINFOHEADER_SIZE as usize + (w * h * 4) as usize);
    // BITMAPINFOHEADER (little-endian).
    out.extend_from_slice(&BITMAPINFOHEADER_SIZE.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(h as i32).to_le_bytes()); // biHeight (+ = bottom-up)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&BI_RGB.to_le_bytes()); // biCompression
    out.extend_from_slice(&(w * h * 4).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // Pixels, bottom row first, BGRA per pixel.
    for y in (0..h).rev() {
        for x in 0..w {
            let p = rgba.get_pixel(x, y).0;
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]); // B, G, R, A
        }
    }
    Ok(out)
}

/// A packed `CF_DIB` -> PNG bytes (wire). Decodes 24/32bpp uncompressed (`BI_RGB`)
/// DIBs in either row direction — enough for what we emit and for common producers.
/// Palette-indexed and `BI_BITFIELDS`/compressed DIBs are out of scope for M1.
///
/// Inbound half (OS format -> wire): production callers are the Send/copy path (M3);
/// for M1 it is exercised by the round-trip tests.
#[allow(dead_code)]
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, CodecError> {
    if dib.len() < BITMAPINFOHEADER_SIZE as usize {
        return Err(CodecError::Dib("shorter than BITMAPINFOHEADER"));
    }
    let rd_u32 = |off: usize| u32::from_le_bytes(dib[off..off + 4].try_into().unwrap());
    let rd_i32 = |off: usize| i32::from_le_bytes(dib[off..off + 4].try_into().unwrap());
    let rd_u16 = |off: usize| u16::from_le_bytes(dib[off..off + 2].try_into().unwrap());

    let header_size = rd_u32(0);
    let width = rd_i32(4);
    let height_raw = rd_i32(8);
    let bit_count = rd_u16(14);
    let compression = rd_u32(16);

    if compression != BI_RGB {
        return Err(CodecError::Dib("only BI_RGB is decoded"));
    }
    if bit_count != 24 && bit_count != 32 {
        return Err(CodecError::Dib("only 24/32 bpp is decoded"));
    }
    if width <= 0 || height_raw == 0 {
        return Err(CodecError::Dib("degenerate dimensions"));
    }
    let top_down = height_raw < 0;
    let w = width as u32;
    let h = height_raw.unsigned_abs();

    let bytes_pp = (bit_count / 8) as usize;
    let row_stride = (w as usize * bit_count as usize).div_ceil(32) * 4; // 4-byte aligned rows
    let pixels_off = header_size as usize; // no palette for 24/32bpp BI_RGB
    let needed = pixels_off
        .checked_add(row_stride * h as usize)
        .ok_or(CodecError::Dib("size overflow"))?;
    if dib.len() < needed {
        return Err(CodecError::Dib("pixel data truncated"));
    }

    let mut img = RgbaImage::new(w, h);
    for y in 0..h {
        // Row `y` (top-to-bottom) maps to a source row per the DIB's direction.
        let src_row = if top_down { y } else { h - 1 - y };
        let row_start = pixels_off + src_row as usize * row_stride;
        for x in 0..w {
            let px = row_start + x as usize * bytes_pp;
            let b = dib[px];
            let g = dib[px + 1];
            let r = dib[px + 2];
            let a = if bytes_pp == 4 { dib[px + 3] } else { 255 };
            img.put_pixel(x, y, image::Rgba([r, g, b, a]));
        }
    }

    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips_through_utf16() {
        let s = "héllo — clipboard 🌐";
        let units = text_to_utf16(s);
        assert_eq!(*units.last().unwrap(), 0, "NUL-terminated");
        assert_eq!(utf16_to_string(&units), s);
    }

    #[test]
    fn image_round_trips_png_dib_png_preserving_alpha() {
        // A tiny image with varied colors AND alpha, to prove BGRA + alpha fidelity.
        let mut src = RgbaImage::new(3, 2);
        src.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        src.put_pixel(1, 0, image::Rgba([0, 255, 0, 128]));
        src.put_pixel(2, 0, image::Rgba([0, 0, 255, 0]));
        src.put_pixel(0, 1, image::Rgba([10, 20, 30, 40]));
        src.put_pixel(1, 1, image::Rgba([200, 150, 100, 255]));
        src.put_pixel(2, 1, image::Rgba([1, 2, 3, 4]));

        let mut png = Vec::new();
        src.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let dib = png_to_dib(&png).expect("png -> dib");
        let png2 = dib_to_png(&dib).expect("dib -> png");
        let back = image::load_from_memory(&png2).unwrap().to_rgba8();

        assert_eq!(back.dimensions(), (3, 2));
        assert_eq!(back, src, "pixels (incl. alpha) survive PNG->DIB->PNG");
    }

    #[test]
    fn dib_rejects_compressed() {
        let mut dib = png_to_dib(&{
            let mut png = Vec::new();
            RgbaImage::new(1, 1)
                .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            png
        })
        .unwrap();
        dib[16] = 1; // biCompression = BI_RLE8
        assert!(matches!(dib_to_png(&dib), Err(CodecError::Dib(_))));
    }
}
