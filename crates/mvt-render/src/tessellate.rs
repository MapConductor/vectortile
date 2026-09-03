//! Turns a styled tile into triangles, for a GPU to draw.
//!
//! This is the cost a GPU renderer adds that the CPU one does not have:
//! `tiny-skia` fills and strokes paths directly, whereas OpenGL ES only draws
//! triangles, so fills need ear-clipping and lines need expanding into quads.
//! Whether GPU rasterisation is worth it comes down to what this module costs —
//! everything else was measured as cheap.
//!
//! Output is deliberately flat: one vertex buffer plus a table of coloured
//! ranges, which is what a renderer uploads once and draws with one call per
//! range.

use crate::color::Color;
use crate::expr::Ctx;
use crate::filter::CompiledFilter;
use crate::model::{self, GeomType};
use crate::style::{LayerKind, Style};

/// The reference tile size the style spec's pixel units are defined against.
const REFERENCE_TILE_SIZE: f32 = 512.0;

/// One draw call: a colour and a run of vertices in [`Tessellation::vertices`].
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub color: Color,
    /// Index of the first vertex, not the first float.
    pub first_vertex: u32,
    pub vertex_count: u32,
    /// Style layer this came from, for debugging and for merging decisions.
    pub layer_id: String,
}

/// What triangulation had to give up on.
///
/// Silent failures here show up as holes in the map, which is exactly the kind
/// of defect a benchmark cannot see — so they are counted.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TessellationStats {
    /// Polygons `earcut` refused to triangulate.
    pub earcut_failures: usize,
    /// Vertices in those polygons, to show whether the loss is significant.
    pub earcut_failed_vertices: usize,
    /// Rings with fewer than three points, or zero area.
    pub degenerate_rings: usize,
    /// Time compiling filters, in ms. On-device profiling showed tessellation
    /// running 6x slower relative to decode than on desktop; these phase
    /// clocks exist to say why without guessing.
    pub filter_compile_ms: f32,
    /// Time in fill triangulation, in ms.
    pub fill_ms: f32,
    /// Time in line expansion, in ms.
    pub line_ms: f32,
}

#[derive(Debug, Clone, Default)]
pub struct Tessellation {
    /// Interleaved x, y pairs in tile-local units (0..extent).
    pub vertices: Vec<f32>,
    pub batches: Vec<Batch>,
    /// Extent the coordinates are expressed in, for the vertex shader.
    pub extent: u32,
    /// Background colour, if the style has a background layer.
    pub background: Option<Color>,
    pub stats: TessellationStats,
}

impl Tessellation {
    pub fn triangle_count(&self) -> usize {
        self.vertices.len() / 6
    }
}

/// A tile that has been decoded and is ready to triangulate.
pub struct DecodedTile<'a> {
    pub source_id: &'a str,
    pub tile: &'a model::Tile,
    pub scale: f32,
    pub offset_x: f32,
    pub offset_y: f32,
}

/// Triangulates every drawable layer of `style` against `decoded`.
pub fn tessellate(
    style: &Style,
    zoom: f64,
    tile_size: u32,
    decoded: &[DecodedTile<'_>],
) -> Tessellation {
    let paint_scale = tile_size as f32 / REFERENCE_TILE_SIZE;
    let mut out = Tessellation::default();

    for layer in &style.layers {
        if !layer.kind.is_drawable() || !layer.is_visible() || !layer.applies_at(zoom) {
            continue;
        }

        if layer.kind == LayerKind::Background {
            let ctx = Ctx::zoom_only(zoom);
            let opacity = layer.paint_number("background-opacity", &ctx, 1.0) as f32;
            out.background = Some(
                layer
                    .paint_color("background-color", &ctx, Color::BLACK)
                    .with_opacity(opacity),
            );
            continue;
        }

        let (Some(source_id), Some(source_layer)) =
            (layer.source.as_deref(), layer.source_layer.as_deref())
        else {
            continue;
        };
        let Some(tile) = decoded.iter().find(|d| d.source_id == source_id) else {
            continue;
        };
        let Some(data) = tile.tile.layer(source_layer) else {
            continue;
        };

        // Every layer works in its own extent; normalise to the first one seen
        // so a single vertex buffer can serve the whole tile.
        if out.extent == 0 {
            out.extent = data.extent;
        }
        let unify = out.extent as f32 / data.extent as f32;

        let start_len = out.vertices.len();
        let mut batch_color: Option<Color> = None;

        // Same compiled-filter fast path the CPU rasteriser uses.
        let compile_started = std::time::Instant::now();
        let filter = CompiledFilter::compile(layer.filter.as_ref(), data, zoom);
        out.stats.filter_compile_ms += compile_started.elapsed().as_secs_f32() * 1000.0;
        if filter.never_matches() {
            continue;
        }

        // Paint, evaluated once per layer on the first matching feature.
        //
        // Not an approximation this module newly introduces: a batch is one
        // draw call in one colour, so the tessellator has always painted the
        // whole layer with the first feature's paint. Evaluating the JSON per
        // feature only to discard every result after the first was measured at
        // ~14 ms per tile on a Pixel 5a — half the tessellation time.
        struct LinePaint {
            half_width: f32,
        }
        struct CirclePaint {
            radius: f32,
        }
        let mut line_paint: Option<LinePaint> = None;
        let mut circle_paint: Option<CirclePaint> = None;
        let mut layer_invisible = false;

        for feature in &data.features {
            if layer_invisible {
                break;
            }
            if !filter.matches(data, feature) {
                continue;
            }

            match layer.kind {
                LayerKind::Fill => {
                    if feature.geom_type != GeomType::Polygon {
                        continue;
                    }
                    if batch_color.is_none() {
                        let ctx = Ctx {
                            zoom,
                            layer: Some(data),
                            feature: Some(feature),
                        };
                        let opacity = layer.paint_number("fill-opacity", &ctx, 1.0) as f32;
                        let color = layer
                            .paint_color("fill-color", &ctx, Color::BLACK)
                            .with_opacity(opacity);
                        if color.a <= 0.0 {
                            layer_invisible = true;
                            continue;
                        }
                        batch_color = Some(color);
                    }
                    let started = std::time::Instant::now();
                    fill_feature(feature, tile, unify, &mut out.vertices, &mut out.stats);
                    out.stats.fill_ms += started.elapsed().as_secs_f32() * 1000.0;
                }
                LayerKind::Line => {
                    if feature.geom_type == GeomType::Point {
                        continue;
                    }
                    if batch_color.is_none() {
                        let ctx = Ctx {
                            zoom,
                            layer: Some(data),
                            feature: Some(feature),
                        };
                        let opacity = layer.paint_number("line-opacity", &ctx, 1.0) as f32;
                        let color = layer
                            .paint_color("line-color", &ctx, Color::BLACK)
                            .with_opacity(opacity);
                        let width =
                            layer.paint_number("line-width", &ctx, 1.0) as f32 * paint_scale;
                        if color.a <= 0.0 || width <= 0.0 {
                            layer_invisible = true;
                            continue;
                        }
                        batch_color = Some(color);
                        line_paint = Some(LinePaint {
                            half_width: width * 0.5 * out.extent as f32 / tile_size as f32,
                        });
                    }
                    let half = line_paint.as_ref().map(|p| p.half_width).unwrap_or(0.0);
                    let started = std::time::Instant::now();
                    line_feature(feature, tile, unify, half, &mut out.vertices);
                    out.stats.line_ms += started.elapsed().as_secs_f32() * 1000.0;
                }
                LayerKind::Circle => {
                    if batch_color.is_none() {
                        let ctx = Ctx {
                            zoom,
                            layer: Some(data),
                            feature: Some(feature),
                        };
                        let radius =
                            layer.paint_number("circle-radius", &ctx, 5.0) as f32 * paint_scale;
                        let opacity = layer.paint_number("circle-opacity", &ctx, 1.0) as f32;
                        let color = layer
                            .paint_color("circle-color", &ctx, Color::BLACK)
                            .with_opacity(opacity);
                        if radius <= 0.0 || color.a <= 0.0 {
                            layer_invisible = true;
                            continue;
                        }
                        batch_color = Some(color);
                        circle_paint = Some(CirclePaint {
                            radius: radius * out.extent as f32 / tile_size as f32,
                        });
                    }
                    let radius = circle_paint.as_ref().map(|p| p.radius).unwrap_or(0.0);
                    circle_feature(feature, tile, unify, radius, &mut out.vertices);
                }
                _ => {}
            }
        }

        let added = out.vertices.len() - start_len;
        if added == 0 {
            continue;
        }
        // One batch per layer: paint varies per feature in principle, but a
        // single colour per layer is what the overwhelming majority of styles
        // do, and mixing colours into one draw call needs per-vertex colour.
        out.batches.push(Batch {
            color: batch_color.unwrap_or(Color::BLACK),
            first_vertex: (start_len / 2) as u32,
            vertex_count: (added / 2) as u32,
            layer_id: layer.id.clone(),
        });
    }

    if out.extent == 0 {
        out.extent = 4096;
    }
    out
}

/// Maps a tile-local point through overzoom and extent unification.
#[inline]
fn project(point: [f32; 2], tile: &DecodedTile<'_>, unify: f32, extent: f32) -> [f32; 2] {
    // Same mapping the CPU rasteriser applies, expressed in tile units rather
    // than pixels so the vertex shader can stay trivial.
    let x = (point[0] * unify / extent - tile.offset_x) * tile.scale * extent;
    let y = (point[1] * unify / extent - tile.offset_y) * tile.scale * extent;
    [x, y]
}

fn signed_area(ring: &[[f32; 2]]) -> f32 {
    let mut sum = 0.0;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        sum += a[0] * b[1] - b[0] * a[1];
    }
    sum * 0.5
}

fn fill_feature(
    feature: &model::Feature,
    tile: &DecodedTile<'_>,
    unify: f32,
    out: &mut Vec<f32>,
    stats: &mut TessellationStats,
) {
    let extent = 4096.0_f32.max(1.0);
    // MVT packs several polygons into one feature. The first ring is always
    // exterior; a later ring with the same winding starts a new polygon, and
    // the opposite winding is a hole. Comparing signs rather than assuming a
    // direction keeps this independent of the writer's convention.
    let mut polygon: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut exterior_sign = 0.0_f32;

    let flush = |polygon: &mut Vec<Vec<[f32; 2]>>,
                 out: &mut Vec<f32>,
                 stats: &mut TessellationStats| {
        if polygon.is_empty() {
            return;
        }
        let mut flat: Vec<f64> = Vec::new();
        let mut holes: Vec<usize> = Vec::new();
        for (index, ring) in polygon.iter().enumerate() {
            if index > 0 {
                holes.push(flat.len() / 2);
            }
            for p in ring {
                flat.push(p[0] as f64);
                flat.push(p[1] as f64);
            }
        }
        match earcutr::earcut(&flat, &holes, 2) {
            Ok(indices) => {
                for i in indices {
                    out.push(flat[i * 2] as f32);
                    out.push(flat[i * 2 + 1] as f32);
                }
            }
            Err(_) => {
                stats.earcut_failures += 1;
                stats.earcut_failed_vertices += flat.len() / 2;
            }
        }
        polygon.clear();
    };

    for part in &feature.parts {
        if part.len() < 3 {
            stats.degenerate_rings += 1;
            continue;
        }
        let ring: Vec<[f32; 2]> = part
            .iter()
            .map(|p| project(*p, tile, unify, extent))
            .collect();
        let area = signed_area(&ring);
        if area == 0.0 {
            stats.degenerate_rings += 1;
            continue;
        }
        if polygon.is_empty() {
            exterior_sign = area.signum();
            polygon.push(ring);
        } else if area.signum() == exterior_sign {
            flush(&mut polygon, out, stats);
            polygon.push(ring);
        } else {
            polygon.push(ring);
        }
    }
    flush(&mut polygon, out, stats);
}

fn line_feature(
    feature: &model::Feature,
    tile: &DecodedTile<'_>,
    unify: f32,
    half_width: f32,
    out: &mut Vec<f32>,
) {
    let extent = 4096.0_f32;
    for part in &feature.parts {
        if part.len() < 2 {
            continue;
        }
        let points: Vec<[f32; 2]> = part
            .iter()
            .map(|p| project(*p, tile, unify, extent))
            .collect();

        for window in points.windows(2) {
            let (a, b) = (window[0], window[1]);
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let length = (dx * dx + dy * dy).sqrt();
            if length <= f32::EPSILON {
                continue;
            }
            // Perpendicular offset; two triangles per segment.
            let (nx, ny) = (-dy / length * half_width, dx / length * half_width);
            let p0 = [a[0] + nx, a[1] + ny];
            let p1 = [a[0] - nx, a[1] - ny];
            let p2 = [b[0] + nx, b[1] + ny];
            let p3 = [b[0] - nx, b[1] - ny];
            out.extend_from_slice(&[
                p0[0], p0[1], p1[0], p1[1], p2[0], p2[1],
                p1[0], p1[1], p3[0], p3[1], p2[0], p2[1],
            ]);
        }

        // Bevel joins: one triangle per interior vertex. Without them a thick
        // polyline shows a notch at every bend.
        for i in 1..points.len().saturating_sub(1) {
            let (prev, here, next) = (points[i - 1], points[i], points[i + 1]);
            let d0 = normalise([here[0] - prev[0], here[1] - prev[1]]);
            let d1 = normalise([next[0] - here[0], next[1] - here[1]]);
            let n0 = [-d0[1] * half_width, d0[0] * half_width];
            let n1 = [-d1[1] * half_width, d1[0] * half_width];
            out.extend_from_slice(&[
                here[0], here[1],
                here[0] + n0[0], here[1] + n0[1],
                here[0] + n1[0], here[1] + n1[1],
            ]);
            out.extend_from_slice(&[
                here[0], here[1],
                here[0] - n0[0], here[1] - n0[1],
                here[0] - n1[0], here[1] - n1[1],
            ]);
        }
    }
}

#[inline]
fn normalise(v: [f32; 2]) -> [f32; 2] {
    let length = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if length <= f32::EPSILON {
        [0.0, 0.0]
    } else {
        [v[0] / length, v[1] / length]
    }
}

fn circle_feature(
    feature: &model::Feature,
    tile: &DecodedTile<'_>,
    unify: f32,
    radius: f32,
    out: &mut Vec<f32>,
) {
    const SEGMENTS: usize = 12;
    let extent = 4096.0_f32;
    for part in &feature.parts {
        for point in part {
            let c = project(*point, tile, unify, extent);
            for i in 0..SEGMENTS {
                let a0 = (i as f32 / SEGMENTS as f32) * std::f32::consts::TAU;
                let a1 = ((i + 1) as f32 / SEGMENTS as f32) * std::f32::consts::TAU;
                out.extend_from_slice(&[
                    c[0], c[1],
                    c[0] + a0.cos() * radius, c[1] + a0.sin() * radius,
                    c[0] + a1.cos() * radius, c[1] + a1.sin() * radius,
                ]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn style_with(layer: serde_json::Value) -> Style {
        Style::from_json(&json!({
            "version": 8,
            "sources": { "src": { "type": "vector", "tiles": ["https://t/{z}/{x}/{y}.pbf"] } },
            "layers": [layer]
        }))
        .unwrap()
    }

    fn tile_with(name: &str, geom: GeomType, parts: model::Parts) -> model::Tile {
        model::Tile {
            layers: vec![model::Layer::from_tables(
                name.into(),
                4096,
                vec![],
                vec![],
                vec![model::Feature::new(Some(1), geom, parts, vec![])],
            )],
        }
    }

    fn decoded(tile: &model::Tile) -> Vec<DecodedTile<'_>> {
        vec![DecodedTile {
            source_id: "src",
            tile,
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }]
    }

    #[test]
    fn triangulates_a_square_into_two_triangles() {
        let style = style_with(json!({
            "id": "f", "type": "fill", "source": "src", "source-layer": "poly",
            "paint": { "fill-color": "#ff0000" }
        }));
        let tile = tile_with(
            "poly",
            GeomType::Polygon,
            vec![vec![[0.0, 0.0], [1000.0, 0.0], [1000.0, 1000.0], [0.0, 1000.0]]],
        );
        let result = tessellate(&style, 10.0, 512, &decoded(&tile));

        assert_eq!(result.triangle_count(), 2);
        assert_eq!(result.batches.len(), 1);
        assert_eq!(result.batches[0].vertex_count, 6);
        assert!((result.batches[0].color.r - 1.0).abs() < 0.01);
    }

    #[test]
    fn a_hole_is_cut_rather_than_filled() {
        let style = style_with(json!({
            "id": "f", "type": "fill", "source": "src", "source-layer": "poly",
            "paint": { "fill-color": "#00ff00" }
        }));
        // Outer ring counter-clockwise, inner ring the other way round.
        let tile = tile_with(
            "poly",
            GeomType::Polygon,
            vec![
                vec![[0.0, 0.0], [1000.0, 0.0], [1000.0, 1000.0], [0.0, 1000.0]],
                vec![[400.0, 400.0], [400.0, 600.0], [600.0, 600.0], [600.0, 400.0]],
            ],
        );
        let result = tessellate(&style, 10.0, 512, &decoded(&tile));

        // A square with a square hole triangulates to eight triangles, not the
        // two a solid square would give.
        assert!(
            result.triangle_count() >= 6,
            "expected a ring of triangles, got {}",
            result.triangle_count()
        );
    }

    #[test]
    fn two_separate_polygons_stay_separate() {
        let style = style_with(json!({
            "id": "f", "type": "fill", "source": "src", "source-layer": "poly",
            "paint": { "fill-color": "#0000ff" }
        }));
        let tile = tile_with(
            "poly",
            GeomType::Polygon,
            vec![
                vec![[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]],
                vec![[500.0, 500.0], [600.0, 500.0], [600.0, 600.0], [500.0, 600.0]],
            ],
        );
        let result = tessellate(&style, 10.0, 512, &decoded(&tile));
        assert_eq!(result.triangle_count(), 4);
    }

    #[test]
    fn a_polyline_becomes_quads_with_joins() {
        let style = style_with(json!({
            "id": "l", "type": "line", "source": "src", "source-layer": "road",
            "paint": { "line-color": "#000000", "line-width": 4 }
        }));
        let tile = tile_with(
            "road",
            GeomType::LineString,
            vec![vec![[0.0, 0.0], [1000.0, 0.0], [1000.0, 1000.0]]],
        );
        let result = tessellate(&style, 10.0, 512, &decoded(&tile));
        // Two segments (2 triangles each) plus one bevel join (2 triangles).
        assert_eq!(result.triangle_count(), 6);
    }

    #[test]
    fn background_is_reported_separately_from_geometry() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sources": {},
            "layers": [{ "id": "bg", "type": "background",
                         "paint": { "background-color": "#123456" } }]
        }))
        .unwrap();
        let result = tessellate(&style, 10.0, 512, &[]);
        assert!(result.background.is_some());
        assert_eq!(result.triangle_count(), 0);
    }

    #[test]
    fn invisible_paint_produces_nothing() {
        let style = style_with(json!({
            "id": "f", "type": "fill", "source": "src", "source-layer": "poly",
            "paint": { "fill-color": "#ff0000", "fill-opacity": 0 }
        }));
        let tile = tile_with(
            "poly",
            GeomType::Polygon,
            vec![vec![[0.0, 0.0], [1000.0, 0.0], [1000.0, 1000.0], [0.0, 1000.0]]],
        );
        assert_eq!(tessellate(&style, 10.0, 512, &decoded(&tile)).triangle_count(), 0);
    }
}
