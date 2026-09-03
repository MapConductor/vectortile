//! Times triangulation of a real tile, the cost a GPU renderer adds.
//!
//!   REPEAT=15 cargo run --release --features tessellate \
//!       --example tessellate_file -- style.json 14 14552 6451 512 tile.mvt

use std::fs;
use std::time::Instant;

use mvt_render::expr::Ctx;
use mvt_render::style::LayerKind;
use mvt_render::tessellate::{tessellate, DecodedTile};
use mvt_render::{mvt, Style};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 6 {
        eprintln!("usage: tessellate_file <style.json> <z> <x> <y> <size> <tile.pbf>");
        std::process::exit(2);
    }

    let style = Style::parse(&fs::read_to_string(&args[0]).expect("style")).expect("parse");
    let z: u8 = args[1].parse().unwrap();
    let x: u32 = args[2].parse().unwrap();
    let y: u32 = args[3].parse().unwrap();
    let size: u32 = args[4].parse().unwrap();
    let bytes = fs::read(&args[5]).expect("tile");

    let decode_started = Instant::now();
    let tile = mvt::decode(&bytes).expect("decode");
    let decode_ms = decode_started.elapsed().as_secs_f64() * 1000.0;

    let repeat: usize = std::env::var("REPEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    let mut times = Vec::with_capacity(repeat);
    let mut last = None;
    for _ in 0..repeat {
        let decoded = vec![DecodedTile {
            source_id: style.sources.keys().next().map(String::as_str).unwrap_or(""),
            tile: &tile,
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }];
        let started = Instant::now();
        let result = tessellate(&style, z as f64, size, &decoded);
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        last = Some(result);
    }
    let result = last.unwrap();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());

    // Filtering alone, with no geometry work. Removing filters from the style
    // is not a valid way to measure this — it changes which features are drawn
    // (975 becomes 387,593) so the comparison measures something else entirely.
    let mut filter_times = Vec::with_capacity(repeat);
    let mut evaluations = 0usize;
    let mut matches = 0usize;
    for _ in 0..repeat {
        evaluations = 0;
        matches = 0;
        let started = Instant::now();
        for layer in &style.layers {
            if !layer.kind.is_drawable()
                || layer.kind == LayerKind::Background
                || !layer.is_visible()
                || !layer.applies_at(z as f64)
            {
                continue;
            }
            let Some(source_layer) = layer.source_layer.as_deref() else {
                continue;
            };
            let Some(data) = tile.layer(source_layer) else {
                continue;
            };
            for feature in &data.features {
                let ctx = Ctx {
                    zoom: z as f64,
                    layer: Some(data),
                    feature: Some(feature),
                };
                evaluations += 1;
                if layer.passes_filter(&ctx) {
                    matches += 1;
                }
            }
        }
        filter_times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    filter_times.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let _ = (x, y);
    eprintln!("decode {decode_ms:.1} ms");
    eprintln!(
        "tessellate median {:.1} ms (min {:.1}, max {:.1}, n={})",
        times[times.len() / 2],
        times[0],
        times[times.len() - 1],
        repeat
    );
    eprintln!(
        "{} triangles across {} batches, {:.0} KB of vertices",
        result.triangle_count(),
        result.batches.len(),
        (result.vertices.len() * 4) as f64 / 1024.0
    );
    // The largest batches are where a wrong colour would be most visible.
    let mut ranked: Vec<_> = result.batches.iter().collect();
    ranked.sort_by_key(|b| std::cmp::Reverse(b.vertex_count));
    eprintln!("near-white batches (the ones that would show as gaps):");
    for batch in &result.batches {
        if batch.color.r > 0.95 && batch.color.g > 0.95 && batch.color.b > 0.95 && batch.color.a > 0.5 {
            eprintln!(
                "  {:24} {:>7} tris  rgba({:.0},{:.0},{:.0},{:.2})",
                batch.layer_id,
                batch.vertex_count / 3,
                batch.color.r * 255.0, batch.color.g * 255.0,
                batch.color.b * 255.0, batch.color.a,
            );
        }
    }
    eprintln!("largest batches:");
    for batch in ranked.iter().take(8) {
        eprintln!(
            "  {:24} {:>7} tris  rgba({:.0},{:.0},{:.0},{:.2})",
            batch.layer_id,
            batch.vertex_count / 3,
            batch.color.r * 255.0,
            batch.color.g * 255.0,
            batch.color.b * 255.0,
            batch.color.a,
        );
    }
    eprintln!(
        "skipped: {} earcut failures ({} vertices), {} degenerate rings",
        result.stats.earcut_failures,
        result.stats.earcut_failed_vertices,
        result.stats.degenerate_rings
    );
    eprintln!(
        "filter-only median {:.1} ms — {} evaluations, {} matched ({:.1}%)",
        filter_times[filter_times.len() / 2],
        evaluations,
        matches,
        matches as f64 / evaluations.max(1) as f64 * 100.0
    );
}
