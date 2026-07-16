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
/// Channel positions are given by explicit masks rather than implied by `bit_count`.
///
/// Not exotic: this is what a **screenshot** looks like. Win+Shift+S carries alpha, so it
/// produces a `BI_BITFIELDS` DIB (usually with a `BITMAPV5HEADER`), where Paint produces
/// plain `BI_RGB`. Rejecting it meant screenshots silently never synced — found by the M3
/// manual gate.
const BI_BITFIELDS: u32 = 3;
/// `BITMAPV4HEADER` — masks live in the header, not after it.
const BITMAPV4HEADER_SIZE: u32 = 108;

/// Pull one channel out of a packed pixel word and scale it to 8 bits.
///
/// A mask is a contiguous run of bits (`0x00FF0000` for red in BGRA, say). Shift it down,
/// then scale from the mask's own width to 0–255 — a 5-bit channel in a 16-bit format must
/// become 8-bit, not stay dark.
fn extract_channel(word: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let value = (word & mask) >> shift;
    let max = mask >> shift;
    if max == 0 {
        return 0;
    }
    // Round rather than truncate: 255/255 must stay 255.
    ((value as u64 * 255 + (max as u64 / 2)) / max as u64) as u8
}

/// UTF-8 text -> a NUL-terminated UTF-16LE code-unit buffer for `CF_UNICODETEXT`.
pub fn text_to_utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A `CF_UNICODETEXT` code-unit buffer -> `String`, stopping at the first NUL.
///
/// Inbound half (OS format -> wire): production callers are the Send/copy path (M2)
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

/// A packed `CF_DIB` -> PNG bytes (wire). Decodes 24/32bpp DIBs in either row direction,
/// both `BI_RGB` (Paint and friends) and `BI_BITFIELDS` with a v3/V4/V5 header (screenshots
/// — see [`BI_BITFIELDS`]).
///
/// Still out of scope: palette-indexed DIBs, and genuinely compressed ones (RLE), which
/// would need a real decoder rather than a layout change.
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

    if compression != BI_RGB && compression != BI_BITFIELDS {
        return Err(CodecError::Dib("only BI_RGB / BI_BITFIELDS are decoded"));
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

    // Where the channels are, and where the pixels start. `BI_BITFIELDS` states the layout
    // explicitly, and *where* it states it depends on the header: a plain
    // `BITMAPINFOHEADER` is followed by three mask DWORDs (which also push the pixels
    // back); V4/V5 carry the masks inside the header instead.
    let (masks, pixels_off) = if compression == BI_BITFIELDS {
        if header_size >= BITMAPV4HEADER_SIZE {
            // V4/V5: masks live inside the header — R, G, B at 40/44/48 and, unlike the v3
            // form, an alpha mask at 52. That alpha is the whole reason a screenshot uses
            // this layout.
            if dib.len() < 56 {
                return Err(CodecError::Dib("V4/V5 header truncated"));
            }
            (
                Some([rd_u32(40), rd_u32(44), rd_u32(48), rd_u32(52)]),
                header_size as usize,
            )
        } else {
            // v3 + trailing masks: R, G, B only — no alpha mask exists in this form.
            let off = header_size as usize;
            if dib.len() < off + 12 {
                return Err(CodecError::Dib("bitfield masks truncated"));
            }
            (
                Some([rd_u32(off), rd_u32(off + 4), rd_u32(off + 8), 0]),
                off + 12,
            )
        }
    } else {
        (None, header_size as usize) // no palette for 24/32bpp BI_RGB
    };

    let bytes_pp = (bit_count / 8) as usize;
    let row_stride = (w as usize * bit_count as usize).div_ceil(32) * 4; // 4-byte aligned rows
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
            let rgba = match masks {
                // Implied BGR(A) order.
                None => {
                    let a = if bytes_pp == 4 { dib[px + 3] } else { 255 };
                    [dib[px + 2], dib[px + 1], dib[px], a]
                }
                // Masked: pull each channel out of the little-endian pixel word.
                Some([mr, mg, mb, ma]) => {
                    let word = match bytes_pp {
                        4 => u32::from_le_bytes([dib[px], dib[px + 1], dib[px + 2], dib[px + 3]]),
                        _ => u32::from_le_bytes([dib[px], dib[px + 1], dib[px + 2], 0]),
                    };
                    // No alpha mask means opaque — an unmasked channel is not a
                    // transparent one, and a screenshot read as fully transparent would
                    // look like a bug to the user.
                    let a = if ma == 0 {
                        255
                    } else {
                        extract_channel(word, ma)
                    };
                    [
                        extract_channel(word, mr),
                        extract_channel(word, mg),
                        extract_channel(word, mb),
                        a,
                    ]
                }
            };
            img.put_pixel(x, y, image::Rgba(rgba));
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
        dib[16] = 1; // biCompression = BI_RLE8 — genuinely undecodable, unlike BI_BITFIELDS
        assert!(matches!(dib_to_png(&dib), Err(CodecError::Dib(_))));
    }

    /// Build a `BITMAPV5HEADER` + `BI_BITFIELDS` BGRA DIB — the shape **Win+Shift+S**
    /// produces, which we used to reject outright (found by the M3 manual gate: Paint
    /// pasted, screenshots silently did not).
    fn v5_bgra_dib(pixels: &[[u8; 4]], w: i32, h: i32) -> Vec<u8> {
        let mut dib = vec![0u8; 124];
        dib[0..4].copy_from_slice(&124u32.to_le_bytes()); // bV5Size
        dib[4..8].copy_from_slice(&w.to_le_bytes());
        dib[8..12].copy_from_slice(&h.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes()); // planes
        dib[14..16].copy_from_slice(&32u16.to_le_bytes()); // bit count
        dib[16..20].copy_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
        dib[40..44].copy_from_slice(&0x00FF_0000u32.to_le_bytes()); // red
        dib[44..48].copy_from_slice(&0x0000_FF00u32.to_le_bytes()); // green
        dib[48..52].copy_from_slice(&0x0000_00FFu32.to_le_bytes()); // blue
        dib[52..56].copy_from_slice(&0xFF00_0000u32.to_le_bytes()); // alpha
                                                                    // Bottom-up rows, BGRA byte order.
        for row in (0..h as usize).rev() {
            for x in 0..w as usize {
                let [r, g, b, a] = pixels[row * w as usize + x];
                dib.extend_from_slice(&[b, g, r, a]);
            }
        }
        dib
    }

    #[test]
    fn dib_decodes_bitfields_v5_like_a_screenshot() {
        let pixels = [
            [255, 0, 0, 255],
            [0, 255, 0, 128],
            [10, 20, 30, 40],
            [1, 2, 3, 255],
        ];
        let dib = v5_bgra_dib(&pixels, 2, 2);

        let png = dib_to_png(&dib).expect("BI_BITFIELDS V5 must decode");
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (2, 2));
        for (i, expect) in pixels.iter().enumerate() {
            let (x, y) = ((i % 2) as u32, (i / 2) as u32);
            assert_eq!(img.get_pixel(x, y).0, *expect, "pixel {x},{y}");
        }
    }

    /// No alpha mask means opaque. A screenshot decoded as fully transparent would look
    /// like a bug to the user, so the absent mask must not read as `a = 0`.
    #[test]
    fn dib_bitfields_without_an_alpha_mask_is_opaque() {
        let mut dib = v5_bgra_dib(&[[7, 8, 9, 0]], 1, 1);
        dib[52..56].copy_from_slice(&0u32.to_le_bytes()); // no alpha mask

        let png = dib_to_png(&dib).expect("decode");
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [7, 8, 9, 255]);
    }

    /// Channels narrower than 8 bits scale up rather than staying dark, and a full mask
    /// round-trips to 255 rather than 254.
    #[test]
    fn channel_extraction_scales_to_eight_bits() {
        assert_eq!(extract_channel(0x00FF_0000, 0x00FF_0000), 255);
        assert_eq!(extract_channel(0x0000_0000, 0x00FF_0000), 0);
        // 5-bit channel, all ones -> full white, not 31.
        assert_eq!(extract_channel(0b11111, 0b11111), 255);
        // 5-bit channel, half -> about half.
        assert_eq!(extract_channel(0b10000, 0b11111), 132);
    }
}
