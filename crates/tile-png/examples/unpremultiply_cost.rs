//! What undoing premultiplication costs, against the encode it precedes.
//!
//! The loop cannot vectorise: it divides by alpha, and NEON has no integer
//! divide. Whether that matters depends on how many pixels take the slow path,
//! which on a marker tile is only the anti-aliased edges — so it has to be
//! measured on tile-shaped content rather than reasoned about.

use std::time::Instant;

/// A tile the way the marker renderer leaves it: mostly transparent, with
/// anti-aliased circles on it, premultiplied.
fn marker_tile(size: u32, icons: u32, icon_px: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let mut seed = 1u64;
    let mut next = || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as u32
    };
    let radius = icon_px as f32 / 2.0;
    for _ in 0..icons {
        let ox = next() % (size - icon_px);
        let oy = next() % (size - icon_px);
        for y in 0..icon_px {
            for x in 0..icon_px {
                let dx = x as f32 - radius;
                let dy = y as f32 - radius;
                let distance = (dx * dx + dy * dy).sqrt();
                // A one-pixel soft edge, which is what makes some pixels take
                // the divide and most of them not.
                let coverage = (radius - distance).clamp(0.0, 1.0);
                if coverage <= 0.0 {
                    continue;
                }
                // Opaque inside, soft only at the rim. The first attempt at
                // this used a flat 230 and made 93% of the tile translucent,
                // which forced the divide on nearly every pixel and made the
                // overhead look twice what it is.
                let alpha = (coverage * 255.0).round() as u32;
                let index = (((oy + y) * size + ox + x) * 4) as usize;
                // Premultiplied, as CoreGraphics and Bitmap hand them over.
                rgba[index] = (200 * alpha / 255) as u8;
                rgba[index + 1] = (60 * alpha / 255) as u8;
                rgba[index + 2] = (70 * alpha / 255) as u8;
                rgba[index + 3] = alpha as u8;
            }
        }
    }
    rgba
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

fn main() {
    for (size, icons, icon_px) in [(512u32, 900u32, 44u32), (1344, 900, 84)] {
        let tile = marker_tile(size, icons, icon_px);
        let translucent = tile
            .chunks_exact(4)
            .filter(|p| p[3] != 0 && p[3] != 255)
            .count();

        let straight = median(
            (0..7)
                .map(|_| {
                    let mut scratch = tile.clone();
                    let started = Instant::now();
                    let png = tile_png::encode_png_rgba(size, size, &mut scratch).unwrap();
                    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                    std::hint::black_box(png);
                    elapsed
                })
                .collect(),
        );

        let premultiplied = median(
            (0..7)
                .map(|_| {
                    let mut scratch = tile.clone();
                    let started = Instant::now();
                    let png =
                        tile_png::encode_png_premultiplied(size, size, &mut scratch).unwrap();
                    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                    std::hint::black_box(png);
                    elapsed
                })
                .collect(),
        );

        println!(
            "{size}px  encode {straight:.2} ms   +unpremultiply {premultiplied:.2} ms   \
             overhead {:.2} ms ({:.0}%)   translucent pixels {:.1}%",
            premultiplied - straight,
            (premultiplied - straight) / straight * 100.0,
            translucent as f64 / (size * size) as f64 * 100.0
        );
    }
}
