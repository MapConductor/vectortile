//! Draws decoded vector tiles into a pixmap with tiny-skia.
//!
//! tiny-skia is pure Rust, so the same rasteriser compiles for wasm, Android
//! and iOS with no platform 2D API and no C toolchain involved. That is what
//! makes "identical output on every backend" achievable.

use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform,
};

use crate::color::Color;
use crate::expr::Ctx;
use crate::filter::CompiledFilter;
use crate::model::{self, GeomType};
use crate::style::{Layer, LayerKind, Style};
use crate::symbol;
use crate::text::GlyphStore;

/// The reference tile size the style spec's pixel units are defined against.
/// dp of screen one tile covers, when the caller has not said otherwise.
///
/// Style dimensions are screen units, so turning them into pixels needs both
/// the pixel count asked for and the size the tile will be shown at. 512 is
/// the usual answer -- see `Renderer::display_tile_size`.
pub const REFERENCE_TILE_SIZE: f32 = 512.0;

/// A source tile that has been fetched and decoded, ready to draw.
pub struct DecodedTile {
    pub source_id: String,
    /// Shared, because a neighbour's labels are wanted by every tile around
    /// it and decoding the same bytes nine times is most of what the ring
    /// costs.
    pub tile: std::sync::Arc<model::Tile>,
    /// Magnification when this tile stands in for a deeper zoom level.
    pub scale: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    /// A neighbour, here so that a label straddling the edge can be drawn
    /// whole. Its geometry is skipped: this tile's own copy already has it,
    /// and drawing it twice would only cost time.
    pub labels_only: bool,
}

pub struct RenderStats {
    pub layers_drawn: usize,
    pub features_drawn: usize,
}

/// Renders one target tile. `decoded` may be in any order and may omit sources.
pub fn render(
    style: &Style,
    zoom: f64,
    tile_size: u32,
    display_tile_size: f32,
    decoded: &[DecodedTile],
) -> Option<(Pixmap, RenderStats)> {
    render_with_glyphs(
        style,
        zoom,
        tile_size,
        display_tile_size,
        decoded,
        &GlyphStore::new(),
        None,
        (0, 0),
    )
}

/// Renders one target tile, with labels if glyphs were supplied.
///
/// Labels come last and in one pass of their own: two of them on the same
/// pixels is unreadable rather than blended, so every label is collected,
/// tested against the ones already kept, and only then drawn. See
/// [`crate::symbol`].
/// Icon images cut out of the sheet, kept for the length of one tile.
///
/// A patterned layer asks for the same icon on every feature it draws, and
/// cutting it out of the sheet each time is pure repetition.
struct PatternCache<'a> {
    sheet: Option<&'a crate::sprite::SpriteSheet>,
    cut: std::collections::HashMap<String, Option<(Pixmap, f32)>>,
}

impl<'a> PatternCache<'a> {
    fn new(sheet: Option<&'a crate::sprite::SpriteSheet>) -> Self {
        PatternCache {
            sheet,
            cut: std::collections::HashMap::new(),
        }
    }

    /// The icon's pixels and the pixel ratio they were drawn for.
    fn get(&mut self, name: &str) -> Option<&(Pixmap, f32)> {
        let sheet = self.sheet?;
        self.cut
            .entry(name.to_string())
            .or_insert_with(|| {
                let ratio = sheet.icon(name).map(|i| i.pixel_ratio).unwrap_or(1.0);
                sheet
                    .icon_image(name)
                    .map(|image| (image, if ratio > 0.0 { ratio } else { 1.0 }))
            })
            .as_ref()
    }
}

pub fn render_with_glyphs(
    style: &Style,
    zoom: f64,
    tile_size: u32,
    // dp the tile covers on screen; REFERENCE_TILE_SIZE when unknown.
    display_tile_size: f32,
    decoded: &[DecodedTile],
    glyphs: &GlyphStore,
    sprites: Option<&crate::sprite::SpriteSheet>,
    tile_xy: (u32, u32),
) -> Option<(Pixmap, RenderStats)> {
    let mut pixmap = Pixmap::new(tile_size, tile_size)?;
    let paint_scale = tile_size as f32 / display_tile_size.max(1.0);
    let mut stats = RenderStats {
        layers_drawn: 0,
        features_drawn: 0,
    };
    let mut patterns = PatternCache::new(sprites);

    for layer in &style.layers {
        if !layer.kind.is_drawable() || !layer.is_visible() || !layer.applies_at(zoom) {
            continue;
        }

        if layer.kind == LayerKind::Background {
            draw_background(&mut pixmap, layer, zoom);
            stats.layers_drawn += 1;
            continue;
        }

        let Some(source_id) = layer.source.as_deref() else {
            continue;
        };
        let Some(source_layer_name) = layer.source_layer.as_deref() else {
            continue;
        };
        // The neighbours in `decoded` are here for the label pass; this tile's
        // own copy of the ground is the one that gets drawn.
        let Some(decoded_tile) = decoded
            .iter()
            .find(|d| d.source_id == source_id && !d.labels_only)
        else {
            continue;
        };
        let Some(data_layer) = decoded_tile.tile.layer(source_layer_name) else {
            continue;
        };

        let transform = tile_transform(decoded_tile, data_layer.extent, tile_size);
        let drawn = draw_data_layer(
            &mut pixmap,
            layer,
            data_layer,
            &transform,
            zoom,
            paint_scale,
            &mut patterns,
            overzoom_window(decoded_tile, data_layer.extent),
        );
        if drawn > 0 {
            stats.layers_drawn += 1;
            stats.features_drawn += drawn;
        }
    }

    // Either is enough on its own: a POI layer with only `icon-image` draws
    // without a single glyph, and a place-name layer draws without a sheet.
    if !glyphs.is_empty() || sprites.is_some_and(|sheet| !sheet.is_empty()) {
        let labels = symbol::collect(
            style,
            symbol::Target {
                zoom,
                tile_size,
                paint_scale,
                xy: tile_xy,
            },
            decoded,
            glyphs,
            sprites,
        );
        // Two pixels of padding: labels that only just clear each other read
        // as one smear at small sizes.
        // Placement ran over a tile of margin so that neighbouring tiles agree;
        // only what reaches this tile is drawn.
        let placed: Vec<_> = symbol::place(labels)
            .into_iter()
            .filter(|label| symbol::touches_tile(label.ink_bounds, tile_size))
            .collect();
        if !placed.is_empty() {
            stats.layers_drawn += 1;
            stats.features_drawn += placed.len();
            symbol::draw(&mut pixmap, &placed, glyphs, sprites);
        }
    }

    Some((pixmap, stats))
}

/// Maps tile-local geometry units onto pixels of the target tile, accounting
/// for the sub-rect an overzoomed ancestor tile stands in for.
/// The part of an overzoomed parent this tile shows, in the parent's units,
/// with an eighth of the window as margin. `None` when the tile is its own.
pub(crate) fn overzoom_window(decoded: &DecodedTile, extent: u32) -> Option<[f32; 4]> {
    window_with_margin(decoded, extent, 0.125)
}

/// As [`overzoom_window`], but a whole tile of margin on every side.
///
/// Labels need it. A tile does not decide on its own which of them to draw: it
/// runs a collision contest, and a label that straddles a seam is drawn half by
/// each neighbour, so the two must reach the same verdict. They only do that if
/// they judge it against the same rivals -- which means each tile has to see a
/// whole tile beyond its own edge, exactly what the ring of neighbours gives an
/// unmagnified tile.
///
/// An eighth of a tile was not enough and the difference was visible: 中野区
/// sits 19 px east of the seam and 高円寺 40 px west of it. The east tile could
/// not see 高円寺, so it drew 中野区; the west tile saw both, gave the spot to
/// 高円寺, and drew nothing -- leaving 中野区 sliced down the middle at the
/// seam. Nothing was wrong at 512 dp because nothing was magnified there and
/// the window did not exist at all.
pub(crate) fn label_window(decoded: &DecodedTile, extent: u32) -> Option<[f32; 4]> {
    window_with_margin(decoded, extent, 1.0)
}

fn window_with_margin(decoded: &DecodedTile, extent: u32, margin_spans: f32) -> Option<[f32; 4]> {
    if decoded.scale <= 1.0 {
        return None;
    }
    let extent = extent as f32;
    let span = extent / decoded.scale;
    let margin = span * margin_spans;
    Some([
        decoded.offset_x * extent - margin,
        decoded.offset_y * extent - margin,
        decoded.offset_x * extent + span + margin,
        decoded.offset_y * extent + span + margin,
    ])
}

fn tile_transform(decoded: &DecodedTile, extent: u32, tile_size: u32) -> Transform {
    let size = tile_size as f32;
    let k = size * decoded.scale / extent as f32;
    let tx = -decoded.offset_x * decoded.scale * size;
    let ty = -decoded.offset_y * decoded.scale * size;
    Transform::from_row(k, 0.0, 0.0, k, tx, ty)
}

fn solid_paint(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color.to_tiny_skia());
    paint.anti_alias = true;
    paint
}

fn draw_background(pixmap: &mut Pixmap, layer: &Layer, zoom: f64) {
    let ctx = Ctx::zoom_only(zoom);
    let opacity = layer.paint_number("background-opacity", &ctx, 1.0) as f32;
    let color = layer
        .paint_color("background-color", &ctx, Color::BLACK)
        .with_opacity(opacity);
    if color.a <= 0.0 {
        return;
    }
    if color.a >= 1.0 {
        pixmap.fill(color.to_tiny_skia());
        return;
    }
    // Painted over what is already there rather than replacing it. A style may
    // have more than one background layer -- the tessellator's copy of this
    // says what real styles do with them -- and `fill` would drop whatever the
    // ones below it put down.
    let Some(rect) =
        tiny_skia::Rect::from_xywh(0.0, 0.0, pixmap.width() as f32, pixmap.height() as f32)
    else {
        return;
    };
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(color.to_tiny_skia());
    paint.blend_mode = tiny_skia::BlendMode::SourceOver;
    pixmap.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
}

fn draw_data_layer(
    pixmap: &mut Pixmap,
    layer: &Layer,
    data: &model::Layer,
    transform: &Transform,
    zoom: f64,
    paint_scale: f32,
    patterns: &mut PatternCache<'_>,
    window: Option<[f32; 4]>,
) -> usize {
    let mut drawn = 0;

    // Compiled once against this data layer's tables; per feature the check is
    // an index-pair scan. A filter that cannot match here at all skips the
    // whole feature loop.
    let filter = CompiledFilter::compile(layer.filter.as_ref(), data, zoom);
    if filter.never_matches() {
        return 0;
    }

    for feature in &data.features {
        // An overzoomed tile draws a fraction of its parent, and most of the
        // parent's features are nowhere near it.
        if let Some(window) = window {
            if !feature.touches_window(window) {
                continue;
            }
        }
        if !filter.matches(data, feature) {
            continue;
        }
        let ctx = Ctx {
            zoom,
            layer: Some(data),
            feature: Some(feature),
        };

        let painted = match layer.kind {
            LayerKind::Fill => draw_fill(
                pixmap,
                layer,
                feature,
                transform,
                &ctx,
                paint_scale,
                patterns,
            ),
            LayerKind::Line => draw_line(pixmap, layer, feature, transform, &ctx, paint_scale),
            LayerKind::Circle => draw_circle(pixmap, layer, feature, transform, &ctx, paint_scale),
            _ => false,
        };
        if painted {
            drawn += 1;
        }
    }

    drawn
}

/// Builds a closed path from every ring, keeping each ring's winding direction
/// so the non-zero fill rule cuts holes without us classifying rings.
fn build_polygon(feature: &model::Feature, transform: &Transform) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let mut any = false;
    for ring in &feature.parts {
        if ring.len() < 3 {
            continue;
        }
        pb.move_to(ring[0][0], ring[0][1]);
        for point in &ring[1..] {
            pb.line_to(point[0], point[1]);
        }
        pb.close();
        any = true;
    }
    if !any {
        return None;
    }
    pb.finish()?.transform(*transform)
}

fn build_polyline(feature: &model::Feature, transform: &Transform) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let mut any = false;
    for line in &feature.parts {
        if line.len() < 2 {
            continue;
        }
        pb.move_to(line[0][0], line[0][1]);
        for point in &line[1..] {
            pb.line_to(point[0], point[1]);
        }
        any = true;
    }
    if !any {
        return None;
    }
    pb.finish()?.transform(*transform)
}

fn draw_fill(
    pixmap: &mut Pixmap,
    layer: &Layer,
    feature: &model::Feature,
    transform: &Transform,
    ctx: &Ctx,
    paint_scale: f32,
    patterns: &mut PatternCache<'_>,
) -> bool {
    if feature.geom_type != GeomType::Polygon {
        return false;
    }
    let Some(path) = build_polygon(feature, transform) else {
        return false;
    };

    let opacity = layer.paint_number("fill-opacity", ctx, 1.0) as f32;
    let fill = layer
        .paint_color("fill-color", ctx, Color::BLACK)
        .with_opacity(opacity);

    let mut painted = false;
    // `fill-pattern` wins over `fill-color`: a style that names both means the
    // colour as the fallback for a sheet that has not arrived.
    let pattern = layer
        .paint_value("fill-pattern", ctx)
        .and_then(|v| match v {
            crate::expr::Val::Str(name) => Some(name),
            _ => None,
        })
        .and_then(|name| patterns.get(&name));

    if let Some(image) = pattern {
        // The pattern is drawn at the sprite's own size, scaled the way
        // everything else on this tile is.
        let k = paint_scale / image.1;
        let paint = Paint {
            shader: tiny_skia::Pattern::new(
                image.0.as_ref(),
                tiny_skia::SpreadMode::Repeat,
                // Nearest keeps a hatch crisp when it is drawn at its own size
                // or larger. Shrunk, nearest picks the same source pixel every
                // time -- a two-pixel stripe halved comes out a flat white --
                // so a downscale averages instead.
                if k < 1.0 {
                    tiny_skia::FilterQuality::Bilinear
                } else {
                    tiny_skia::FilterQuality::Nearest
                },
                opacity.clamp(0.0, 1.0),
                Transform::from_scale(k, k),
            ),
            anti_alias: true,
            ..Paint::default()
        };
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        painted = true;
    } else if fill.a > 0.0 {
        pixmap.fill_path(
            &path,
            &solid_paint(fill),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        painted = true;
    }

    // `fill-outline-color` is a hairline stroke along the polygon boundary.
    if let Some(outline) = layer
        .paint_value("fill-outline-color", ctx)
        .and_then(|v| v.as_color())
    {
        let outline = outline.with_opacity(opacity);
        if outline.a > 0.0 {
            let stroke = Stroke {
                width: paint_scale.max(0.1),
                ..Stroke::default()
            };
            pixmap.stroke_path(
                &path,
                &solid_paint(outline),
                &stroke,
                Transform::identity(),
                None,
            );
            painted = true;
        }
    }

    painted
}

fn draw_line(
    pixmap: &mut Pixmap,
    layer: &Layer,
    feature: &model::Feature,
    transform: &Transform,
    ctx: &Ctx,
    paint_scale: f32,
) -> bool {
    if feature.geom_type == GeomType::Point {
        return false;
    }
    // Polygons are legal input to a line layer: they stroke their boundary.
    let path = if feature.geom_type == GeomType::Polygon {
        build_polygon(feature, transform)
    } else {
        build_polyline(feature, transform)
    };
    let Some(path) = path else {
        return false;
    };

    let opacity = layer.paint_number("line-opacity", ctx, 1.0) as f32;
    let color = layer
        .paint_color("line-color", ctx, Color::BLACK)
        .with_opacity(opacity);
    let width = layer.paint_number("line-width", ctx, 1.0) as f32 * paint_scale;
    if color.a <= 0.0 || width <= 0.0 {
        return false;
    }

    let stroke = Stroke {
        // Hairlines still need a positive width or tiny-skia drops them.
        width: width.max(0.05),
        line_cap: match layer.layout_string("line-cap", ctx).as_deref() {
            Some("round") => LineCap::Round,
            Some("square") => LineCap::Square,
            _ => LineCap::Butt,
        },
        line_join: match layer.layout_string("line-join", ctx).as_deref() {
            Some("bevel") => LineJoin::Bevel,
            Some("miter") => LineJoin::Miter,
            _ => LineJoin::Round,
        },
        miter_limit: layer
            .layout_value("line-miter-limit", ctx)
            .and_then(|v| v.as_f64())
            .unwrap_or(2.0) as f32,
        // `line-dasharray` counts in line widths, not pixels, so a tunnel
        // casing keeps its proportions as the road thickens with zoom.
        dash: layer
            .paint_numbers("line-dasharray", ctx)
            .map(|pattern| crate::dash::normalise(&pattern))
            .filter(|pattern| !pattern.is_empty())
            .and_then(|pattern| {
                let scaled: Vec<f32> = pattern.iter().map(|v| v * width).collect();
                StrokeDash::new(scaled, 0.0)
            }),
    };

    pixmap.stroke_path(
        &path,
        &solid_paint(color),
        &stroke,
        Transform::identity(),
        None,
    );
    true
}

fn draw_circle(
    pixmap: &mut Pixmap,
    layer: &Layer,
    feature: &model::Feature,
    transform: &Transform,
    ctx: &Ctx,
    paint_scale: f32,
) -> bool {
    let radius = layer.paint_number("circle-radius", ctx, 5.0) as f32 * paint_scale;
    if radius <= 0.0 {
        return false;
    }

    let opacity = layer.paint_number("circle-opacity", ctx, 1.0) as f32;
    let fill = layer
        .paint_color("circle-color", ctx, Color::BLACK)
        .with_opacity(opacity);
    let stroke_width = layer.paint_number("circle-stroke-width", ctx, 0.0) as f32 * paint_scale;
    let stroke_color = layer
        .paint_color("circle-stroke-color", ctx, Color::BLACK)
        .with_opacity(layer.paint_number("circle-stroke-opacity", ctx, 1.0) as f32);

    if fill.a <= 0.0 && (stroke_width <= 0.0 || stroke_color.a <= 0.0) {
        return false;
    }

    // Circles keep a constant screen size, so project each centre and build the
    // circle in pixels instead of transforming a path (which would scale the
    // radius along with the geometry). The tile transform is a uniform
    // scale-plus-translate, so this is just the two axis equations.
    let mut pb = PathBuilder::new();
    let mut any = false;
    for part in &feature.parts {
        for point in part {
            let cx = point[0] * transform.sx + transform.tx;
            let cy = point[1] * transform.sy + transform.ty;
            pb.push_circle(cx, cy, radius);
            any = true;
        }
    }
    if !any {
        return false;
    }
    let Some(path) = pb.finish() else {
        return false;
    };

    if fill.a > 0.0 {
        pixmap.fill_path(
            &path,
            &solid_paint(fill),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    if stroke_width > 0.0 && stroke_color.a > 0.0 {
        let stroke = Stroke {
            width: stroke_width,
            ..Stroke::default()
        };
        pixmap.stroke_path(
            &path,
            &solid_paint(stroke_color),
            &stroke,
            Transform::identity(),
            None,
        );
    }
    true
}
