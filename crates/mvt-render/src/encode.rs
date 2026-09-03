//! PNG encoding for raw RGBA buffers produced outside the renderer.
//!
//! The CPU path encodes through tiny-skia and never needs this. It exists for
//! the GPU path, whose `glReadPixels` output must become a PNG before the
//! local tile server can serve it — and where the platform encoder turned out
//! to be the entire remaining cost: on a Pixel 5a, `Bitmap.compress` spends
//! 77 ms on a tile whose draw and readback together take under 4 ms.
//!
//! Compression is tuned for tile serving, not archival: tiles are ephemeral
//! network payloads, so fast compression with sub filtering wins over ratio.

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
