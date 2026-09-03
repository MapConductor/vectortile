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

/// One draw call: a run of vertices in [`Tessellation::vertices`].
///
/// Colour lives on the vertices, not here. A style layer may paint each
/// feature differently — the MapLibre demo style gives every country its own
/// fill through a `match` — and collapsing that to one colour per draw call
/// disagreed with the CPU renderer on 37% of a tile's pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    /// Index of the first vertex, not the first float.
    pub first_vertex: u32,
    pub vertex_count: u32,
    /// Style layer this came from, for debugging and draw order.
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

/// Floats per vertex: x, y, r, g, b, a.
pub const VERTEX_STRIDE: usize = 6;

#[derive(Debug, Clone, Default)]
pub struct Tessellation {
    /// Interleaved x, y, r, g, b, a — see [`VERTEX_STRIDE`]. Positions are in
    /// tile-local units (0..extent), colours straight alpha in 0..1.
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
        self.vertices.len() / (VERTEX_STRIDE * 3)
    }

    pub fn vertex_count(&self) -> usize {
        self.vertices.len() / VERTEX_STRIDE
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

        // Same compiled-filter fast path the CPU rasteriser uses.
        let compile_started = std::time::Instant::now();
        let filter = CompiledFilter::compile(layer.filter.as_ref(), data, zoom);
        out.stats.filter_compile_ms += compile_started.elapsed().as_secs_f32() * 1000.0;
        if filter.never_matches() {
            continue;
        }

        // Paint is evaluated once per layer unless it actually varies per
        // feature. Walking the paint JSON for every feature costs ~14 ms a
        // tile on a mid-range phone and almost always yields the same answer;
        // collapsing it when it does *not* is what painted every country the
        // same colour.
        let (color_key, size_key) = match layer.kind {
            LayerKind::Fill => ("fill-color", "fill-opacity"),
            LayerKind::Line => ("line-color", "line-opacity"),
            _ => ("circle-color", "circle-opacity"),
        };
        let varies = layer.paint_varies_per_feature(color_key)
            || layer.paint_varies_per_feature(size_key)
            || layer.paint_varies_per_feature("line-width")
            || layer.paint_varies_per_feature("circle-radius");

        let mut cached: Option<(Color, f32)> = None;

        for feature in &data.features {
            if !filter.matches(data, feature) {
                continue;
            }
            if layer.kind == LayerKind::Fill && feature.geom_type != GeomType::Polygon {
                continue;
            }
            if layer.kind == LayerKind::Line && feature.geom_type == GeomType::Point {
                continue;
            }

            // (colour, size) where size is a half-width for lines, a radius
            // for circles, and unused for fills.
            let paint = if varies || cached.is_none() {
                let ctx = Ctx {
                    zoom,
                    layer: Some(data),
                    feature: Some(feature),
                };
                let resolved = match layer.kind {
                    LayerKind::Fill => {
                        let opacity = layer.paint_number("fill-opacity", &ctx, 1.0) as f32;
                        (
                            layer
                                .paint_color("fill-color", &ctx, Color::BLACK)
                                .with_opacity(opacity),
                            0.0,
                        )
                    }
                    LayerKind::Line => {
                        let opacity = layer.paint_number("line-opacity", &ctx, 1.0) as f32;
                        let width =
                            layer.paint_number("line-width", &ctx, 1.0) as f32 * paint_scale;
                        (
                            layer
                                .paint_color("line-color", &ctx, Color::BLACK)
                                .with_opacity(opacity),
                            width * 0.5 * out.extent as f32 / tile_size as f32,
                        )
                    }
                    _ => {
                        let opacity = layer.paint_number("circle-opacity", &ctx, 1.0) as f32;
                        let radius =
                            layer.paint_number("circle-radius", &ctx, 5.0) as f32 * paint_scale;
                        (
                            layer
                                .paint_color("circle-color", &ctx, Color::BLACK)
                                .with_opacity(opacity),
                            radius * out.extent as f32 / tile_size as f32,
                        )
                    }
                };
                if !varies {
                    cached = Some(resolved);
                }
                resolved
            } else {
                cached.unwrap()
            };

            let (color, size) = paint;
            if color.a <= 0.0 {
                // Invisible for this feature; with constant paint that means
                // the whole layer.
                if !varies {
                    break;
                }
                continue;
            }

            match layer.kind {
                LayerKind::Fill => {
                    let started = std::time::Instant::now();
                    fill_feature(feature, tile, unify, color, &mut out.vertices, &mut out.stats);
                    out.stats.fill_ms += started.elapsed().as_secs_f32() * 1000.0;
                }
                LayerKind::Line => {
                    if size <= 0.0 {
                        continue;
                    }
                    let started = std::time::Instant::now();
                    line_feature(feature, tile, unify, size, color, &mut out.vertices);
                    out.stats.line_ms += started.elapsed().as_secs_f32() * 1000.0;
                }
                LayerKind::Circle => {
                    if size <= 0.0 {
                        continue;
                    }
                    circle_feature(feature, tile, unify, size, color, &mut out.vertices);
                }
                _ => {}
            }
        }

        let added = out.vertices.len() - start_len;
        if added == 0 {
            continue;
        }
        // One batch per layer, preserving painter's order. Colour rides on the
        // vertices, so a batch is purely a range.
        out.batches.push(Batch {
            first_vertex: (start_len / VERTEX_STRIDE) as u32,
            vertex_count: (added / VERTEX_STRIDE) as u32,
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

/// Appends one vertex: position then colour.
#[inline]
fn push_vertex(out: &mut Vec<f32>, x: f32, y: f32, color: Color) {
    out.extend_from_slice(&[x, y, color.r, color.g, color.b, color.a]);
}

fn fill_feature(
    feature: &model::Feature,
    tile: &DecodedTile<'_>,
    unify: f32,
    color: Color,
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
                    push_vertex(out, flat[i * 2] as f32, flat[i * 2 + 1] as f32, color);
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
    color: Color,
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
            for point in [p0, p1, p2, p1, p3, p2] {
                push_vertex(out, point[0], point[1], color);
            }
        }

        // Bevel joins: one triangle per interior vertex. Without them a thick
        // polyline shows a notch at every bend.
        for i in 1..points.len().saturating_sub(1) {
            let (prev, here, next) = (points[i - 1], points[i], points[i + 1]);
            let d0 = normalise([here[0] - prev[0], here[1] - prev[1]]);
            let d1 = normalise([next[0] - here[0], next[1] - here[1]]);
            let n0 = [-d0[1] * half_width, d0[0] * half_width];
            let n1 = [-d1[1] * half_width, d1[0] * half_width];
            for point in [
                here,
                [here[0] + n0[0], here[1] + n0[1]],
                [here[0] + n1[0], here[1] + n1[1]],
                here,
                [here[0] - n0[0], here[1] - n0[1]],
                [here[0] - n1[0], here[1] - n1[1]],
            ] {
                push_vertex(out, point[0], point[1], color);
            }
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
    color: Color,
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
                for point in [
                    [c[0], c[1]],
                    [c[0] + a0.cos() * radius, c[1] + a0.sin() * radius],
                    [c[0] + a1.cos() * radius, c[1] + a1.sin() * radius],
                ] {
                    push_vertex(out, point[0], point[1], color);
                }
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
        // Colour rides on every vertex now.
        assert!((result.vertices[2] - 1.0).abs() < 0.01, "red channel");
        assert!(result.vertices[3].abs() < 0.01, "green channel");
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
    fn per_feature_colours_are_kept_apart() {
        // The MapLibre demo style paints each country through a `match`, and
        // collapsing that to one colour per layer disagreed with the CPU
        // renderer on 37% of a tile.
        let style = style_with(json!({
            "id": "f", "type": "fill", "source": "src", "source-layer": "poly",
            "paint": {
                "fill-color": ["match", ["get", "kind"], "a", "#ff0000", "#0000ff"]
            }
        }));
        let layer = model::Layer::from_tables(
            "poly".into(),
            4096,
            vec!["kind".into()],
            vec![model::Value::String("a".into()), model::Value::String("b".into())],
            vec![
                model::Feature::new(
                    Some(1), GeomType::Polygon,
                    vec![vec![[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]]],
                    vec![0, 0],
                ),
                model::Feature::new(
                    Some(2), GeomType::Polygon,
                    vec![vec![[200.0, 200.0], [300.0, 200.0], [300.0, 300.0], [200.0, 300.0]]],
                    vec![0, 1],
                ),
            ],
        );
        let tile = model::Tile { layers: vec![layer] };
        let result = tessellate(&style, 10.0, 512, &decoded(&tile));

        let stride = VERTEX_STRIDE;
        let reds: Vec<f32> = (0..result.vertex_count())
            .map(|i| result.vertices[i * stride + 2])
            .collect();
        let blues: Vec<f32> = (0..result.vertex_count())
            .map(|i| result.vertices[i * stride + 4])
            .collect();
        assert!(reds.iter().any(|r| *r > 0.9), "the first feature should be red");
        assert!(blues.iter().any(|b| *b > 0.9), "the second should be blue");
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
