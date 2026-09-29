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
    /// Time in the per-feature loop, in ms, including the triangulation the
    /// two clocks above already count. What is left over is deciding which
    /// features belong, working out their paint, and writing the vertices.
    pub feature_loop_ms: f32,
    /// Features looked at, and features drawn. A style draws one source layer
    /// through a dozen style layers, so every feature is asked a dozen times
    /// whether it belongs -- which is invisible unless it is counted.
    pub features_considered: usize,
    pub features_drawn: usize,
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
    // dp the tile covers on screen; `raster::REFERENCE_TILE_SIZE` when there is nothing
    // better to say.
    display_tile_size: f32,
    decoded: &[DecodedTile<'_>],
) -> Tessellation {
    tessellate_with(style, zoom, tile_size, display_tile_size, decoded, true)
}

/// [`tessellate`], with the overzoom window optional.
///
/// `windowed` is the ordinary case: a magnified tile only looks at the
/// features that can land in it. Off, the whole source tile is triangulated
/// at the magnified scale, which is what [`translated_clip`] cuts children
/// out of -- one triangulation serving every child instead of one each.
pub fn tessellate_with(
    style: &Style,
    zoom: f64,
    tile_size: u32,
    display_tile_size: f32,
    decoded: &[DecodedTile<'_>],
    windowed: bool,
) -> Tessellation {
    let paint_scale = tile_size as f32 / display_tile_size.max(1.0);
    let mut out = Tessellation::default();
    // Which features of each data layer fall inside the overzoom window,
    // worked out once per data layer rather than once per style layer. A
    // style reads one source layer through dozens of its own layers, and
    // walking twelve thousand features dozens of times -- even to skip them
    // -- was a quarter of a second per street-zoom tile on a phone.
    let mut in_window: std::collections::HashMap<usize, std::rc::Rc<Vec<u32>>> =
        std::collections::HashMap::new();

    for layer in &style.layers {
        if !layer.kind.is_drawable() || !layer.is_visible() || !layer.applies_at(zoom) {
            continue;
        }

        if layer.kind == LayerKind::Background {
            let ctx = Ctx::zoom_only(zoom);
            let opacity = layer.paint_number("background-opacity", &ctx, 1.0) as f32;
            let color = layer
                .paint_color("background-color", &ctx, Color::BLACK)
                .with_opacity(opacity);
            // Stacked, not replaced. A style may have several background
            // layers, and keeping the last one throws the others away: the
            // versatiles colorful style paints its cream background and then
            // four `background-opacity: 0` layers, which exist only as slots
            // for other layers to be inserted into. The last of those was
            // being reported as the background, so the host cleared every
            // tile to nothing and the map underneath showed through
            // everywhere the data did not paint -- most of the sea, and the
            // land between the roads.
            out.background = Some(match out.background {
                Some(under) => color.over(under),
                None => color,
            });
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
        // Read out once: the feature writers need it and `out` is borrowed
        // mutably for the vertex buffer while they run.
        let out_extent = out.extent as f32;

        // The window this overzoomed tile actually shows, in the parent
        // tile's own units. A z16 tile drawn from a z14 parent covers one
        // sixteenth of it, but every one of the sixteen used to walk all
        // twelve thousand features through every layer's filter -- most of
        // the cost of a street-zoom tile, spent on features that could not
        // possibly appear. The margin is generous (an eighth of the window)
        // so a wide road casing or a label anchor just outside still draws.
        let window = if windowed && tile.scale > 1.0 {
            let extent = data.extent as f32;
            let span = extent / tile.scale;
            let margin = span / 8.0;
            Some([
                tile.offset_x * extent - margin,
                tile.offset_y * extent - margin,
                tile.offset_x * extent + span + margin,
                tile.offset_y * extent + span + margin,
            ])
        } else {
            None
        };

        let start_len = out.vertices.len();

        // Same compiled-filter fast path the CPU rasteriser uses.
        let compile_started = crate::clock::Stopwatch::start();
        let filter = CompiledFilter::compile(layer.filter.as_ref(), data, zoom);
        out.stats.filter_compile_ms += compile_started.ms();
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
        // The pattern is in line widths, and the widths here are in extent
        // units, so the scaling falls out of the width the feature resolves
        // to. Read once per layer: a per-feature dash array is legal and
        // nobody writes one.
        let dash_pattern = if layer.kind == LayerKind::Line {
            layer
                .paint_numbers("line-dasharray", &Ctx::zoom_only(zoom))
                .map(|pattern| crate::dash::normalise(&pattern))
                .filter(|pattern| !pattern.is_empty())
        } else {
            None
        };

        let loop_started = crate::clock::Stopwatch::start();
        let candidates = in_window
            .entry(data as *const model::Layer as usize)
            .or_insert_with(|| {
                std::rc::Rc::new(match window {
                    Some(window) => (0..data.features.len() as u32)
                        .filter(|&i| data.features[i as usize].touches_window(window))
                        .collect(),
                    None => (0..data.features.len() as u32).collect(),
                })
            })
            .clone();
        out.stats.features_considered += candidates.len();
        for &index in candidates.iter() {
            let feature = &data.features[index as usize];
            if !filter.matches(data, feature) {
                continue;
            }
            out.stats.features_drawn += 1;
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
                    let started = crate::clock::Stopwatch::start();
                    fill_feature(
                        feature,
                        tile,
                        unify,
                        out_extent,
                        color,
                        &mut out.vertices,
                        &mut out.stats,
                    );
                    out.stats.fill_ms += started.ms();
                }
                LayerKind::Line => {
                    if size <= 0.0 {
                        continue;
                    }
                    let started = crate::clock::Stopwatch::start();
                    line_feature(
                        feature,
                        tile,
                        unify,
                        out_extent,
                        size,
                        color,
                        dash_pattern.as_deref(),
                        &mut out.vertices,
                    );
                    out.stats.line_ms += started.ms();
                }
                LayerKind::Circle => {
                    if size <= 0.0 {
                        continue;
                    }
                    circle_feature(feature, tile, unify, out_extent, size, color, &mut out.vertices);
                }
                _ => {}
            }
        }

        out.stats.feature_loop_ms += loop_started.ms();

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

/// One child of a whole-source-tile triangulation.
///
/// Every vertex is taken into `extent` units (`unify`), moved by `dx`/`dy`,
/// and a triangle is kept when its bounds touch the tile plus `margin`. The
/// GPU clips exactly, so the margin only guards against float error at the
/// edge; what matters is that no triangle crossing the tile is dropped.
/// Batches keep their order and their layer, minus the ones left empty.
pub fn translated_clip(
    src: &Tessellation,
    unify: f32,
    dx: f32,
    dy: f32,
    extent: f32,
    margin: f32,
) -> Tessellation {
    let (lo, hi) = (-margin, extent + margin);
    let mut out = Tessellation {
        vertices: Vec::with_capacity(src.vertices.len() / 4),
        batches: Vec::with_capacity(src.batches.len()),
        extent: extent as u32,
        background: src.background,
        stats: TessellationStats::default(),
    };
    const TRI: usize = VERTEX_STRIDE * 3;
    for batch in &src.batches {
        let start = out.vertices.len();
        let first = batch.first_vertex as usize * VERTEX_STRIDE;
        let end = first + batch.vertex_count as usize * VERTEX_STRIDE;
        for tri in src.vertices[first..end].chunks_exact(TRI) {
            let mut moved = [0f32; TRI];
            moved.copy_from_slice(tri);
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for v in moved.chunks_exact_mut(VERTEX_STRIDE) {
                v[0] = v[0] * unify + dx;
                v[1] = v[1] * unify + dy;
                min_x = min_x.min(v[0]);
                max_x = max_x.max(v[0]);
                min_y = min_y.min(v[1]);
                max_y = max_y.max(v[1]);
            }
            if max_x < lo || min_x > hi || max_y < lo || min_y > hi {
                continue;
            }
            out.vertices.extend_from_slice(&moved);
        }
        let added = out.vertices.len() - start;
        if added > 0 {
            out.batches.push(Batch {
                first_vertex: (start / VERTEX_STRIDE) as u32,
                vertex_count: (added / VERTEX_STRIDE) as u32,
                layer_id: batch.layer_id.clone(),
            });
        }
    }
    out
}

/// Maps a tile-local point through extent unification and overzoom.
///
/// `unify` takes the point into the output's extent; the offset is then a
/// whole number of output extents, and the scale magnifies about that origin.
/// The result is in output extent units, which is what the vertex shader
/// divides by -- so the shader can stay trivial.
///
/// `extent` is the **output** extent, not the data tile's. It used to be
/// hardcoded to 4096, which cancelled out as long as nothing was magnified and
/// silently did not when something was: a tile drawn from its parent had its
/// whole geometry pushed a tile-width off the edge and drew nothing but its
/// background. Shortbread serves some layers at extent 2048 and some at 4096,
/// so the two are not the same number even for an ordinary tile.
#[inline]
fn project(point: [f32; 2], tile: &DecodedTile<'_>, unify: f32, extent: f32) -> [f32; 2] {
    let x = (point[0] * unify - tile.offset_x * extent) * tile.scale;
    let y = (point[1] * unify - tile.offset_y * extent) * tile.scale;
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
    extent: f32,
    color: Color,
    out: &mut Vec<f32>,
    stats: &mut TessellationStats,
) {
    // MVT packs several polygons into one feature. The first ring is always
    // exterior; a later ring with the same winding starts a new polygon, and
    // the opposite winding is a hole. Comparing signs rather than assuming a
    // direction keeps this independent of the writer's convention.
    let mut polygon: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut exterior_sign = 0.0_f32;

    let flush =
        |polygon: &mut Vec<Vec<[f32; 2]>>, out: &mut Vec<f32>, stats: &mut TessellationStats| {
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
    extent: f32,
    half_width: f32,
    color: Color,
    dash_pattern: Option<&[f32]>,
    out: &mut Vec<f32>,
) {
    for part in &feature.parts {
        if part.len() < 2 {
            continue;
        }
        if dash_pattern.is_none() {
            solid_line_part(part, tile, unify, extent, half_width, color, out);
            continue;
        }
        let points: Vec<[f32; 2]> = part
            .iter()
            .map(|p| project(*p, tile, unify, extent))
            .collect();

        // Dashing here rather than in the shader: the GPU draws whatever
        // quads it is handed, so a dashed line is simply fewer of them. The
        // pattern's lengths are multiples of the line's width, and the width
        // is a half-width in extent units, hence the doubling.
        let pattern = dash_pattern.expect("solid lines returned above");
        let scaled: Vec<f32> = pattern.iter().map(|v| v * half_width * 2.0).collect();
        let runs = crate::dash::dashed(&points, &scaled);

        for points in runs {
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
}

/// Emits the common non-dashed path straight from decoded coordinates.
/// Keeping a projected Vec per feature here cost an allocation for every road
/// under every style layer. Segment vertices are still emitted before joins,
/// in exactly the same order as the dashed path.
fn solid_line_part(
    part: &[[f32; 2]],
    tile: &DecodedTile<'_>,
    unify: f32,
    extent: f32,
    half_width: f32,
    color: Color,
    out: &mut Vec<f32>,
) {
    // Keep the projected end of the previous segment. A point belongs to two
    // adjacent segments, so projecting both ends of every window did almost
    // twice the coordinate work needed for this pass.
    let mut a = project(part[0], tile, unify, extent);
    for point in &part[1..] {
        let b = project(*point, tile, unify, extent);
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let length = (dx * dx + dy * dy).sqrt();
        if length > f32::EPSILON {
            let (nx, ny) = (-dy / length * half_width, dx / length * half_width);
            let p0 = [a[0] + nx, a[1] + ny];
            let p1 = [a[0] - nx, a[1] - ny];
            let p2 = [b[0] + nx, b[1] + ny];
            let p3 = [b[0] - nx, b[1] - ny];
            for point in [p0, p1, p2, p1, p3, p2] {
                push_vertex(out, point[0], point[1], color);
            }
        }
        a = b;
    }

    // Joins must remain after all segments to preserve the packed vertex
    // order. Use a three-point sliding window so this second pass also
    // projects each point only once.
    let mut prev = project(part[0], tile, unify, extent);
    let mut here = project(part[1], tile, unify, extent);
    for point in &part[2..] {
        let next = project(*point, tile, unify, extent);
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
        prev = here;
        here = next;
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
    extent: f32,
    radius: f32,
    color: Color,
    out: &mut Vec<f32>,
) {
    const SEGMENTS: usize = 12;
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
            vec![vec![
                [0.0, 0.0],
                [1000.0, 0.0],
                [1000.0, 1000.0],
                [0.0, 1000.0],
            ]],
        );
        let result = tessellate(&style, 10.0, 512, 512.0, &decoded(&tile));

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
                vec![
                    [400.0, 400.0],
                    [400.0, 600.0],
                    [600.0, 600.0],
                    [600.0, 400.0],
                ],
            ],
        );
        let result = tessellate(&style, 10.0, 512, 512.0, &decoded(&tile));

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
                vec![
                    [500.0, 500.0],
                    [600.0, 500.0],
                    [600.0, 600.0],
                    [500.0, 600.0],
                ],
            ],
        );
        let result = tessellate(&style, 10.0, 512, 512.0, &decoded(&tile));
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
        let result = tessellate(&style, 10.0, 512, 512.0, &decoded(&tile));
        // Two segments (2 triangles each) plus one bevel join (2 triangles).
        assert_eq!(result.triangle_count(), 6);
    }

    /// The GPU path draws the quads it is handed, so a dashed line has to be
    /// dashed here rather than in a shader -- and a tunnel drawn solid reads
    /// as an ordinary road.
    #[test]
    fn a_dashed_line_becomes_several_runs_of_quads() {
        let solid = style_with(json!({
            "id": "l", "type": "line", "source": "src", "source-layer": "road",
            "paint": { "line-color": "#000000", "line-width": 4 }
        }));
        let dashed = style_with(json!({
            "id": "l", "type": "line", "source": "src", "source-layer": "road",
            "paint": {
                "line-color": "#000000", "line-width": 4,
                "line-dasharray": [1, 1]
            }
        }));
        // A straight line across the whole tile: 4096 extent units, drawn 4
        // pixels wide at 512, so a dash is 4 pixels and there is room for
        // plenty of them.
        let tile = tile_with(
            "road",
            GeomType::LineString,
            vec![vec![[0.0, 0.0], [4096.0, 0.0]]],
        );

        let whole = tessellate(&solid, 10.0, 512, 512.0, &decoded(&tile));
        let cut = tessellate(&dashed, 10.0, 512, 512.0, &decoded(&tile));

        assert_eq!(whole.triangle_count(), 2, "one segment, one quad");
        // 512 pixels of line, 4 on and 4 off: 64 dashes, two triangles each.
        assert_eq!(cut.triangle_count(), 128);
    }

    /// An overzoomed tile walks its parent's features, and most of them fall
    /// outside the part it shows -- but a polygon can surround the window with
    /// every vertex outside it, and dropping that one leaves a hole where a
    /// lake should be.
    #[test]
    /// A magnified tile's geometry lands inside the tile, whatever extent the
    /// data came at.
    ///
    /// The projection used to divide by a hardcoded 4096 and multiply by it
    /// again. The two cancelled while nothing was magnified, so the whole
    /// tessellator looked correct; with a scale and an offset they did not,
    /// and a source layer served at extent 2048 -- which Shortbread does --
    /// pushed every vertex a tile-width off the edge. On device the ground
    /// came back as nothing but its background colour while the labels, which
    /// take the CPU path, drew perfectly.
    #[test]
    fn a_magnified_tile_lands_inside_itself_at_any_data_extent() {
        let style = style_with(json!({
            "id": "f", "type": "fill", "source": "src", "source-layer": "poly",
            "paint": { "fill-color": "#0000ff" }
        }));
        for data_extent in [2048u32, 4096] {
            let span = data_extent as f32;
            let tile = model::Tile {
                layers: vec![model::Layer::from_tables(
                    "poly".into(),
                    data_extent,
                    vec![],
                    vec![],
                    vec![model::Feature::new(
                        Some(1),
                        GeomType::Polygon,
                        vec![vec![[0.0, 0.0], [span, 0.0], [span, span], [0.0, span]]],
                        vec![],
                    )],
                )],
            };
            // The parent's south-east quarter, magnified to fill this tile.
            let decoded = vec![DecodedTile {
                source_id: "src",
                tile: &tile,
                scale: 2.0,
                offset_x: 0.5,
                offset_y: 0.5,
            }];
            let result = tessellate(&style, 10.0, 512, 512.0, &decoded);
            assert!(result.triangle_count() > 0, "extent {data_extent}: nothing drawn");

            // The polygon covers the whole parent, so magnified it must cover
            // this tile: its corners land outside, and the tile is inside
            // them. The bug put the whole thing to the left of the tile.
            let extent = result.extent as f32;
            let (mut min, mut max) = ([f32::MAX; 2], [f32::MIN; 2]);
            for v in result.vertices.chunks(VERTEX_STRIDE) {
                for axis in 0..2 {
                    min[axis] = min[axis].min(v[axis]);
                    max[axis] = max[axis].max(v[axis]);
                }
            }
            for axis in 0..2 {
                assert!(
                    min[axis] <= 0.0 && max[axis] >= extent,
                    "extent {data_extent}: axis {axis} spans {:?}..{:?}, not covering 0..{extent}",
                    min[axis],
                    max[axis]
                );
            }
        }
    }

    /// Cutting children out of one whole-tile triangulation draws what
    /// triangulating each child on its own draws: nothing a child shows is
    /// missing, and nothing it does not show is there.
    #[test]
    fn children_cut_from_one_triangulation_match_their_own() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sources": { "src": { "type": "vector", "tiles": ["https://t/{z}/{x}/{y}.pbf"] } },
            "layers": [
                { "id": "f", "type": "fill", "source": "src", "source-layer": "poly",
                  "paint": { "fill-color": "#0000ff" } },
                { "id": "l", "type": "line", "source": "src", "source-layer": "road",
                  "paint": { "line-color": "#ff0000", "line-width": 3 } }
            ]
        }))
        .unwrap();
        let span = 4096.0f32;
        // Polygons and roads spread over the parent, some crossing child
        // boundaries, one far off in a corner.
        let polys = vec![
            vec![vec![[100.0, 100.0], [1500.0, 100.0], [1500.0, 1500.0], [100.0, 1500.0]]],
            vec![vec![[1800.0, 1800.0], [3000.0, 1900.0], [2600.0, 3100.0], [1700.0, 2800.0]]],
            vec![vec![[3900.0, 3900.0], [4090.0, 3900.0], [4090.0, 4090.0], [3900.0, 4090.0]]],
        ];
        let roads = vec![
            vec![vec![[0.0, 2048.0], [span, 2100.0]]],
            vec![vec![[300.0, 0.0], [700.0, 3000.0], [3500.0, 3500.0]]],
        ];
        let tile = model::Tile {
            layers: vec![
                model::Layer::from_tables(
                    "poly".into(), 4096, vec![], vec![],
                    polys.into_iter().enumerate().map(|(i, rings)| {
                        model::Feature::new(Some(i as u64 + 1), GeomType::Polygon, rings, vec![])
                    }).collect(),
                ),
                model::Layer::from_tables(
                    "road".into(), 4096, vec![], vec![],
                    roads.into_iter().enumerate().map(|(i, parts)| {
                        model::Feature::new(Some(i as u64 + 1), GeomType::LineString, parts, vec![])
                    }).collect(),
                ),
            ],
        };
        for scale in [2.0f32, 4.0] {
            let whole = tessellate_with(
                &style, 12.0, 512, 512.0,
                &[DecodedTile { source_id: "src", tile: &tile, scale, offset_x: 0.0, offset_y: 0.0 }],
                false,
            );
            let across = scale as u32;
            for cy in 0..across {
                for cx in 0..across {
                    let (ox, oy) = (cx as f32 / scale, cy as f32 / scale);
                    let direct = tessellate(
                        &style, 12.0, 512, 512.0,
                        &[DecodedTile { source_id: "src", tile: &tile, scale, offset_x: ox, offset_y: oy }],
                    );
                    let extent = direct.extent as f32;
                    assert_eq!(whole.extent, direct.extent);
                    let cut = translated_clip(
                        &whole, 1.0, -ox * extent * scale, -oy * extent * scale, extent, extent / 64.0,
                    );
                    let tri = |t: &Tessellation| -> Vec<[f32; 6]> {
                        t.vertices
                            .chunks_exact(VERTEX_STRIDE * 3)
                            .map(|v| [v[0], v[1], v[6], v[7], v[12], v[13]])
                            .collect()
                    };
                    let (a, b) = (tri(&cut), tri(&direct));
                    let same = |p: &[f32; 6], q: &[f32; 6]| p.iter().zip(q).all(|(x, y)| (x - y).abs() < 0.05);
                    let touches = |p: &[f32; 6]| {
                        let xs = [p[0], p[2], p[4]];
                        let ys = [p[1], p[3], p[5]];
                        let (lo, hi) = (0.0, extent);
                        !(xs.iter().all(|&x| x < lo) || xs.iter().all(|&x| x > hi)
                            || ys.iter().all(|&y| y < lo) || ys.iter().all(|&y| y > hi))
                    };
                    for p in &a {
                        assert!(b.iter().any(|q| same(p, q)),
                            "scale {scale} child {cx},{cy}: cut has a triangle its own triangulation lacks: {p:?}");
                    }
                    for q in b.iter().filter(|q| touches(q)) {
                        assert!(a.iter().any(|p| same(p, q)),
                            "scale {scale} child {cx},{cy}: a triangle on the tile is missing from the cut: {q:?}");
                    }
                    // Batches stay in style order.
                    let ids: Vec<&str> = cut.batches.iter().map(|b| b.layer_id.as_str()).collect();
                    assert!(ids == ["f", "l"] || ids == ["f"] || ids == ["l"] || ids.is_empty(), "{ids:?}");
                }
            }
        }
    }

    fn an_overzoomed_tile_skips_whats_outside_but_keeps_what_surrounds_it() {
        let style = style_with(json!({
            "id": "f", "type": "fill", "source": "src", "source-layer": "poly",
            "paint": { "fill-color": "#0000ff" }
        }));
        // Two polygons in the parent: a small one in the far corner, and one
        // covering the whole parent tile. Separate features, because the test
        // is about which features are skipped.
        let tile = model::Tile {
            layers: vec![model::Layer::from_tables(
                "poly".into(),
                4096,
                vec![],
                vec![],
                vec![
                    model::Feature::new(
                        Some(1),
                        GeomType::Polygon,
                        vec![vec![
                            [100.0, 100.0],
                            [200.0, 100.0],
                            [200.0, 200.0],
                            [100.0, 200.0],
                        ]],
                        vec![],
                    ),
                    model::Feature::new(
                        Some(2),
                        GeomType::Polygon,
                        vec![vec![
                            [0.0, 0.0],
                            [4096.0, 0.0],
                            [4096.0, 4096.0],
                            [0.0, 4096.0],
                        ]],
                        vec![],
                    ),
                ],
            )],
        };
        // The bottom-right quarter of the parent: the corner polygon is
        // outside it, the surrounding one covers it.
        let decoded = vec![DecodedTile {
            source_id: "src",
            tile: &tile,
            scale: 2.0,
            offset_x: 0.5,
            offset_y: 0.5,
        }];
        let result = tessellate(&style, 10.0, 512, 512.0, &decoded);
        // Only the surrounding polygon: two triangles, not four.
        assert_eq!(result.triangle_count(), 2);
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
            vec![
                model::Value::String("a".into()),
                model::Value::String("b".into()),
            ],
            vec![
                model::Feature::new(
                    Some(1),
                    GeomType::Polygon,
                    vec![vec![[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]]],
                    vec![0, 0],
                ),
                model::Feature::new(
                    Some(2),
                    GeomType::Polygon,
                    vec![vec![
                        [200.0, 200.0],
                        [300.0, 200.0],
                        [300.0, 300.0],
                        [200.0, 300.0],
                    ]],
                    vec![0, 1],
                ),
            ],
        );
        let tile = model::Tile {
            layers: vec![layer],
        };
        let result = tessellate(&style, 10.0, 512, 512.0, &decoded(&tile));

        let stride = VERTEX_STRIDE;
        let reds: Vec<f32> = (0..result.vertex_count())
            .map(|i| result.vertices[i * stride + 2])
            .collect();
        let blues: Vec<f32> = (0..result.vertex_count())
            .map(|i| result.vertices[i * stride + 4])
            .collect();
        assert!(
            reds.iter().any(|r| *r > 0.9),
            "the first feature should be red"
        );
        assert!(blues.iter().any(|b| *b > 0.9), "the second should be blue");
    }

    #[test]
    /// A style's background is the whole stack, not its last layer.
    ///
    /// versatiles' colorful paints a cream background and then four
    /// `background-opacity: 0` layers that exist only as insertion slots.
    /// Reporting the last of those made the host clear every tile to nothing,
    /// and on a map with an opaque basemap underneath -- ArcGIS -- the result
    /// was the basemap showing through wherever the data did not paint.
    #[test]
    fn transparent_background_slots_do_not_erase_the_background() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sources": {},
            "layers": [
                { "id": "bg", "type": "background",
                  "paint": { "background-color": "rgb(248,244,240)" } },
                { "id": "slot-below-fills", "type": "background",
                  "paint": { "background-opacity": 0 } },
                { "id": "slot-below-labels", "type": "background",
                  "paint": { "background-opacity": 0 } }
            ]
        }))
        .unwrap();
        let result = tessellate(&style, 10.0, 512, 512.0, &[]);
        let background = result.background.expect("a background");
        assert!((background.a - 1.0).abs() < 1e-6, "{background:?}");
        assert!((background.r - 248.0 / 255.0).abs() < 1e-3, "{background:?}");
    }

    /// Half-transparent backgrounds mix rather than replace.
    #[test]
    fn a_translucent_background_layer_mixes_with_the_one_below() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sources": {},
            "layers": [
                { "id": "bg", "type": "background",
                  "paint": { "background-color": "#000000" } },
                { "id": "tint", "type": "background",
                  "paint": { "background-color": "#ffffff", "background-opacity": 0.5 } }
            ]
        }))
        .unwrap();
        let background = tessellate(&style, 10.0, 512, 512.0, &[])
            .background
            .expect("a background");
        assert!((background.r - 0.5).abs() < 1e-3, "{background:?}");
        assert!((background.a - 1.0).abs() < 1e-6, "{background:?}");
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
        let result = tessellate(&style, 10.0, 512, 512.0, &[]);
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
            vec![vec![
                [0.0, 0.0],
                [1000.0, 0.0],
                [1000.0, 1000.0],
                [0.0, 1000.0],
            ]],
        );
        assert_eq!(
            tessellate(&style, 10.0, 512, 512.0, &decoded(&tile)).triangle_count(),
            0
        );
    }
}
