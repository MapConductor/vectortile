//! Exports the Shortbread 8/226/100 forest for the Android stencil experiment.
//! cargo run --release -p mvt-render --features tessellate
//!   --example export_stencil_experiment -- /path/to/fixture
//! Input: style.json and 8-226-100.mvt; output: float32-LE rings and reference triangles.
use mvt_render::tessellate::{tessellate, DecodedTile};
use mvt_render::{mvt, Style};
use std::{fs, time::Instant};
fn write(path: &str, data: &[f32]) {
    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
    fs::write(path, bytes).unwrap();
}
fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("fixture directory containing style.json and 8-226-100.mvt");
    let mut style =
        Style::parse(&fs::read_to_string(format!("{dir}/style.json")).unwrap()).unwrap();
    style.layers.retain(|l| l.id == "land-forest");
    let bytes = fs::read(format!("{dir}/8-226-100.mvt")).unwrap();
    let start = Instant::now();
    let tile = mvt::decode(&bytes).unwrap();
    println!("decode_ms={:.3}", start.elapsed().as_secs_f64() * 1000.);
    let source = style.active_vector_sources(8.)[0].id.clone();
    let decoded = [DecodedTile {
        source_id: &source,
        tile: &tile,
        scale: 1.,
        offset_x: 0.,
        offset_y: 0.,
    }];
    let start = Instant::now();
    let reference = tessellate(&style, 8., 512, 512., &decoded);
    println!(
        "earcut_ms={:.3} triangles={} extent={}",
        start.elapsed().as_secs_f64() * 1000.,
        reference.triangle_count(),
        reference.extent
    );
    write(&format!("{dir}/forest-earcut.bin"), &reference.vertices);
    let start = Instant::now();
    let mut fans = Vec::new();
    let land = tile.layer("land").unwrap();
    let k = reference.extent as f32 / land.extent as f32;
    let color = &reference.vertices[2..6];
    let mut rings = Vec::new();
    rings.extend_from_slice(&reference.extent.to_le_bytes());
    for c in color {
        rings.extend_from_slice(&c.to_le_bytes());
    }
    let count: usize = land.features.iter().map(|f| f.parts.len()).sum();
    rings.extend_from_slice(&(count as u32).to_le_bytes());
    for feature in &land.features {
        for ring in &feature.parts {
            rings.extend_from_slice(&(ring.len() as u32).to_le_bytes());
            for p in ring {
                rings.extend_from_slice(&(p[0] * k).to_le_bytes());
                rings.extend_from_slice(&(p[1] * k).to_le_bytes());
            }
        }
    }
    fs::write(format!("{dir}/forest-rings.bin"), rings).unwrap();
    for feature in &land.features {
        for ring in &feature.parts {
            if ring.len() < 3 {
                continue;
            }
            for i in 1..ring.len() - 1 {
                for p in [ring[0], ring[i], ring[i + 1]] {
                    fans.extend_from_slice(&[p[0] * k, p[1] * k]);
                    fans.extend_from_slice(color);
                }
            }
        }
    }
    println!(
        "fan_pack_ms={:.3} triangles={}",
        start.elapsed().as_secs_f64() * 1000.,
        fans.len() / 18
    );
    write(&format!("{dir}/forest-fans.bin"), &fans);
}
