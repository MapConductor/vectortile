//! Rasterises the tessellator's triangles with tiny-skia and diffs the result
//! against the normal CPU render of the same tile.
//!
//! This removes OpenGL from the picture entirely: the device app showed an
//! 11.82% pixel diff, and this decides whether the triangles themselves are
//! wrong or the GL app drawing them is.
//!
//!   cargo run --release --features tessellate --example tessellate_compare -- \
//!       style.json 14 14552 6451 512 tile.mvt /tmp/out-prefix

use std::fs;

use mvt_render::tessellate::{tessellate, DecodedTile};
use mvt_render::{mvt, Renderer, Style};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 7 {
        eprintln!(
            "usage: tessellate_compare <style.json> <z> <x> <y> <size> <tile.pbf> <out-prefix>"
        );
        std::process::exit(2);
    }

    let style_text = fs::read_to_string(&args[0]).expect("style");
    let style = Style::parse(&style_text).expect("parse");
    let z: u8 = args[1].parse().unwrap();
    let x: u32 = args[2].parse().unwrap();
    let y: u32 = args[3].parse().unwrap();
    let size: u32 = args[4].parse().unwrap();
    let bytes = fs::read(&args[5]).expect("tile");
    let prefix = &args[6];

    // --- reference: the shipping CPU rasteriser -----------------------------
    let renderer = Renderer::new(&style_text).expect("renderer");
    let (reference, _) = renderer
        .render_pixmap(z, x, y, size, &[Some(bytes.as_slice())])
        .expect("cpu render");

    // --- candidate: tessellate, then fill the triangles with tiny-skia ------
    let tile = mvt::decode(&bytes).expect("decode");
    let decoded = vec![DecodedTile {
        // The first *vector* source the style actually draws from. Taking
        // `sources.keys().next()` picked whichever sorted first — a geojson
        // source in the MapLibre demo style — and silently tessellated nothing.
        source_id: style
            .active_vector_sources(z as f64)
            .first()
            .map(|s| s.id.as_str())
            .unwrap_or(""),
        tile: &tile,
        scale: 1.0,
        offset_x: 0.0,
        offset_y: 0.0,
    }];
    let tess = tessellate(&style, z as f64, size, &decoded);

    let mut candidate = Pixmap::new(size, size).unwrap();
    if let Some(bg) = tess.background {
        candidate.fill(bg.to_tiny_skia());
    }
    let scale = size as f32 / tess.extent as f32;
    let stride = mvt_render::tessellate::VERTEX_STRIDE;
    for batch in &tess.batches {
        let start = batch.first_vertex as usize * stride;
        let end = start + batch.vertex_count as usize * stride;
        let coords = &tess.vertices[start..end];

        // Colour is per vertex, and every vertex of a triangle carries the
        // same one, so triangles are grouped by colour before filling.
        let mut run_start = 0usize;
        let triangles = coords.len() / (stride * 3);
        while run_start < triangles {
            let colour = colour_of(coords, run_start, stride);
            let mut run_end = run_start + 1;
            while run_end < triangles && colour_of(coords, run_end, stride) == colour {
                run_end += 1;
            }
            let mut pb = PathBuilder::new();
            for t in run_start..run_end {
                let base = t * stride * 3;
                pb.move_to(coords[base] * scale, coords[base + 1] * scale);
                pb.line_to(
                    coords[base + stride] * scale,
                    coords[base + stride + 1] * scale,
                );
                pb.line_to(
                    coords[base + stride * 2] * scale,
                    coords[base + stride * 2 + 1] * scale,
                );
                pb.close();
            }
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color(
                    tiny_skia::Color::from_rgba(colour[0], colour[1], colour[2], colour[3])
                        .unwrap_or(tiny_skia::Color::TRANSPARENT),
                );
                paint.anti_alias = true;
                candidate.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            run_start = run_end;
        }
    }

    // --- diff ---------------------------------------------------------------
    let mut differing = 0usize;
    let mut worst = 0i32;
    let mut worst_at = (0u32, 0u32);
    let mut diff_map = Pixmap::new(size, size).unwrap();
    for py in 0..size {
        for px in 0..size {
            let a = reference.pixel(px, py).unwrap().demultiply();
            let b = candidate.pixel(px, py).unwrap().demultiply();
            let delta = (a.red() as i32 - b.red() as i32)
                .abs()
                .max((a.green() as i32 - b.green() as i32).abs())
                .max((a.blue() as i32 - b.blue() as i32).abs());
            if delta > 24 {
                differing += 1;
                let strength = 255u8.min((delta * 2) as u8);
                let map = diff_map.pixels_mut();
                map[(py * size + px) as usize] =
                    tiny_skia::PremultipliedColorU8::from_rgba(strength, 0, 0, 255).unwrap();
                if delta > worst {
                    worst = delta;
                    worst_at = (px, py);
                }
            }
        }
    }

    let total = (size * size) as f64;
    println!(
        "diff: {differing}/{} px ({:.2}%) differ by >24",
        size * size,
        differing as f64 / total * 100.0
    );
    if worst > 0 {
        let (wx, wy) = worst_at;
        let a = reference.pixel(wx, wy).unwrap().demultiply();
        let b = candidate.pixel(wx, wy).unwrap().demultiply();
        println!(
            "worst: {worst} at {wx},{wy}  cpu #{:02X}{:02X}{:02X}  tri #{:02X}{:02X}{:02X}",
            a.red(),
            a.green(),
            a.blue(),
            b.red(),
            b.green(),
            b.blue()
        );

        // Which batches put triangles over the worst pixel, in draw order —
        // this is the attribution the device app could not give.
        let tx = wx as f32 / scale;
        let ty = wy as f32 / scale;
    }

    reference.save_png(format!("{prefix}-cpu.png")).unwrap();
    candidate.save_png(format!("{prefix}-tri.png")).unwrap();
    diff_map.save_png(format!("{prefix}-diff.png")).unwrap();
    println!("wrote {prefix}-cpu.png / -tri.png / -diff.png");
}

/// Colour of triangle `index` within a batch's vertex slice.
fn colour_of(coords: &[f32], index: usize, stride: usize) -> [f32; 4] {
    let base = index * stride * 3;
    [
        coords[base + 2],
        coords[base + 3],
        coords[base + 4],
        coords[base + 5],
    ]
}

fn point_in_triangle(px: f32, py: f32, t: &[f32]) -> bool {
    let sign = |ax: f32, ay: f32, bx: f32, by: f32| (px - bx) * (ay - by) - (ax - bx) * (py - by);
    let d1 = sign(t[0], t[1], t[2], t[3]);
    let d2 = sign(t[2], t[3], t[4], t[5]);
    let d3 = sign(t[4], t[5], t[0], t[1]);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}
