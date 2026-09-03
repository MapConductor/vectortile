//! Draws decoded vector tiles into a pixmap with tiny-skia.
//!
//! tiny-skia is pure Rust, so the same rasteriser compiles for wasm, Android
//! and iOS with no platform 2D API and no C toolchain involved. That is what
//! makes "identical output on every backend" achievable.

use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform,
};

use crate::color::Color;
use crate::expr::Ctx;
use crate::model::{self, GeomType};
use crate::style::{Layer, LayerKind, Style};

/// The reference tile size the style spec's pixel units are defined against.
const REFERENCE_TILE_SIZE: f32 = 512.0;

/// A source tile that has been fetched and decoded, ready to draw.
pub struct DecodedTile {
    pub source_id: String,
    pub tile: model::Tile,
    /// Magnification when this tile stands in for a deeper zoom level.
    pub scale: f32,
    pub offset_x: f32,
    pub offset_y: f32,
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
    decoded: &[DecodedTile],
) -> Option<(Pixmap, RenderStats)> {
    let mut pixmap = Pixmap::new(tile_size, tile_size)?;
    let paint_scale = tile_size as f32 / REFERENCE_TILE_SIZE;
    let mut stats = RenderStats {
        layers_drawn: 0,
        features_drawn: 0,
    };

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
        let Some(decoded_tile) = decoded.iter().find(|d| d.source_id == source_id) else {
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
        );
        if drawn > 0 {
            stats.layers_drawn += 1;
            stats.features_drawn += drawn;
        }
    }

    Some((pixmap, stats))
}

/// Maps tile-local geometry units onto pixels of the target tile, accounting
/// for the sub-rect an overzoomed ancestor tile stands in for.
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
    pixmap.fill(color.to_tiny_skia());
}

fn draw_data_layer(
    pixmap: &mut Pixmap,
    layer: &Layer,
    data: &model::Layer,
    transform: &Transform,
    zoom: f64,
    paint_scale: f32,
) -> usize {
    let mut drawn = 0;

    for feature in &data.features {
        let ctx = Ctx {
            zoom,
            layer: Some(data),
            feature: Some(feature),
        };
        if !layer.passes_filter(&ctx) {
            continue;
        }

        let painted = match layer.kind {
            LayerKind::Fill => draw_fill(pixmap, layer, feature, transform, &ctx, paint_scale),
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
    if fill.a > 0.0 {
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
        miter_limit: layer.layout_value("line-miter-limit", ctx)
            .and_then(|v| v.as_f64())
            .unwrap_or(2.0) as f32,
        ..Stroke::default()
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
