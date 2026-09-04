//! PNG encoding for raw RGBA buffers, tuned for map tiles.
//!
//! Its own crate, and a deliberately small one: the SDK cores encode tiles too,
//! and making them depend on a rasteriser to reach an encoder would put
//! tiny-skia in every app that draws a marker. Nothing here pulls in more than
//! the `png` crate.
//!
//! The platform encoders are the reason it exists. On a Pixel 5a
//! `Bitmap.compress` spends 133-249 ms on a tile this encodes in 28-40 ms,
//! which on the GPU vector tile path was the entire remaining cost — the draw
//! and readback together take under 4 ms.
//!
//! Compression is tuned for tile serving, not archival, and the levels were
//! compared on tiles the renderer actually produced. On a dense marker tile:
//!
//! ```text
//!   Fast      15.8 ms   1093 KB     <- what this uses
//!   Default  107.0 ms    450 KB
//!   Best     271.5 ms    451 KB
//!   Bitmap.compress  165.0 ms  377 KB
//! ```
//!
//! Default buys 2.4x on size for 6.8x on time, which is the wrong way round for
//! a tile that is about to be handed to a local server; Best is slower than
//! Default for the same bytes and is never the answer. The cost of Fast is
//! real, though: files run 2.3-3.3x larger than the platform encoder's, and the
//! tile caches that hold them are budgeted in bytes, so they hold fewer.

use png::{BitDepth, ColorType, Compression, Encoder, FilterType};

#[derive(Debug)]
pub struct EncodeError(pub String);

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "png encode failed: {}", self.0)
    }
}

impl std::error::Error for EncodeError {}

/// Encodes a straight-alpha RGBA8 buffer as PNG.
///
/// `rgba` must hold exactly `width * height * 4` bytes, rows top-down.
pub fn encode_png_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, EncodeError> {
    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(EncodeError(format!(
            "buffer is {} bytes, {width}x{height} RGBA needs {expected}",
            rgba.len()
        )));
    }
    if width == 0 || height == 0 {
        return Err(EncodeError("zero-sized image".into()));
    }

    let mut out = Vec::with_capacity(expected / 4);
    {
        let mut encoder = Encoder::new(&mut out, width, height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        encoder.set_compression(Compression::Fast);
        // Sub filtering: nearly free to compute, and map tiles are dominated
        // by horizontal runs of flat colour where it helps deflate most.
        //
        // Paeth looks far better on synthetic tiles — hard-edged icons on
        // transparency compress 7x smaller with it — and is worse on real ones.
        // Real icons have anti-aliased edges with alpha gradients, which Paeth
        // predicts no better than Sub while costing more per pixel: on a
        // Pixel 5a it made a marker tile 50.9 ms -> 77.0 ms for no change in
        // size at all. Do not switch filters on generated content.
        encoder.set_filter(FilterType::Sub);
        let mut writer = encoder
            .write_header()
            .map_err(|e| EncodeError(e.to_string()))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| EncodeError(e.to_string()))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_decodable_png() {
        // A 4x2 image with distinct pixels, so a bytes-round-trip means the
        // geometry and channel order survived, not just the header.
        let mut rgba = Vec::new();
        for i in 0..8u8 {
            rgba.extend_from_slice(&[i * 30, 255 - i * 30, i, 255]);
        }
        let png = encode_png_rgba(4, 2, &rgba).expect("encode");
        assert_eq!(&png[1..4], b"PNG");

        let decoded = tiny_skia::Pixmap::decode_png(&png).expect("decode");
        assert_eq!((decoded.width(), decoded.height()), (4, 2));
        let first = decoded.pixel(0, 0).unwrap().demultiply();
        assert_eq!((first.red(), first.green(), first.blue()), (0, 255, 0));
        let last = decoded.pixel(3, 1).unwrap().demultiply();
        assert_eq!((last.red(), last.green(), last.blue()), (210, 45, 7));
    }

    #[test]
    fn rejects_a_wrong_sized_buffer() {
        assert!(encode_png_rgba(4, 2, &[0u8; 10]).is_err());
        assert!(encode_png_rgba(0, 0, &[]).is_err());
    }
}

/// Undoes alpha premultiplication in place, then encodes as PNG.
///
/// Android hands out premultiplied pixels — `Bitmap.copyPixelsToBuffer` on an
/// `ARGB_8888` bitmap, and the same is true of a CoreGraphics context — while
/// PNG stores straight alpha. Encoding premultiplied bytes as if they were
/// straight darkens every semi-transparent pixel, which on marker tiles is
/// every icon edge.
pub fn encode_png_premultiplied(
    width: u32,
    height: u32,
    rgba: &mut [u8],
) -> Result<Vec<u8>, EncodeError> {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3];
        if alpha == 0 {
            // Fully transparent pixels carry no colour to recover, and
            // dividing by zero would invent one.
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
            continue;
        }
        if alpha == 255 {
            continue;
        }
        let alpha = alpha as u32;
        for channel in 0..3 {
            // Rounded division: truncating loses a level on almost every
            // partly transparent pixel, which is visible as a dark fringe.
            pixel[channel] = (((pixel[channel] as u32 * 255) + alpha / 2) / alpha).min(255) as u8;
        }
    }
    encode_png_rgba(width, height, rgba)
}

#[cfg(test)]
mod premultiply_tests {
    use super::*;

    #[test]
    fn undoes_premultiplication() {
        // Half-alpha mid grey, stored premultiplied: 128 * 0.5 = 64.
        let mut rgba = vec![64, 64, 64, 128, 0, 0, 0, 0, 200, 100, 50, 255];
        let png = encode_png_premultiplied(3, 1, &mut rgba).expect("encode");

        let decoded = tiny_skia::Pixmap::decode_png(&png).expect("decode");
        let recovered = decoded.pixel(0, 0).unwrap().demultiply();
        // 64 * 255 / 128 = 127.5, rounded to 128; tiny-skia's own round trip
        // through premultiplied storage costs another level.
        assert!(
            (recovered.red() as i32 - 128).abs() <= 2,
            "expected ~128, got {}",
            recovered.red()
        );
        assert_eq!(decoded.pixel(1, 0).unwrap().alpha(), 0);
        let opaque = decoded.pixel(2, 0).unwrap().demultiply();
        assert_eq!((opaque.red(), opaque.green(), opaque.blue()), (200, 100, 50));
    }
}
