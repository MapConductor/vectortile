//! Turning `symbol` layers into placed labels.
//!
//! Labels cannot be drawn where they are met, the way a fill or a line is.
//! Two of them landing on the same pixels is not a blend, it is unreadable, so
//! every label in the tile is collected first, tested against the ones already
//! kept, and only then drawn. That is why this is a pass of its own rather
//! than another arm of the feature loop.
//!
//! # What is drawn
//!
//! Point-placed text: `text-field`, `text-font`, `text-size`, `text-anchor`,
//! `text-offset`, `text-max-width`, `text-transform`, `text-color`,
//! `text-opacity`, `text-halo-color`, `text-halo-width`, `text-allow-overlap`.
//!
//! # What is not
//!
//! Text along a line (`symbol-placement: line`) and `text-letter-spacing`.
//! Icons are drawn -- see [`crate::sprite`] -- but only where the sheet has
//! them and only at point placement, so the arrows a style repeats along a
//! road are still missing.
//! [`unsupported`] names what a given style loses, the same way the renderer
//! reports undrawn layer types.
//!
//! Placement is per tile. A label near an edge is placed without knowing what
//! its neighbour placed, so the same name can appear twice across a seam or
//! not at all. Fixing that needs placement to run over a viewport rather than
//! a tile, which is a different shape of renderer.

use tiny_skia::Pixmap;

use crate::color::Color;
use crate::expr::Ctx;
use crate::filter::CompiledFilter;
use crate::model::{self, GeomType};
use crate::sprite::{self, IconPaint, SpriteSheet};
use crate::style::{Layer, LayerKind, Style};
use crate::text::{self, Anchor, GlyphOnLine, GlyphStore, Shaped, TextPaint};

/// How much the road may turn between two adjacent letters, in degrees,
/// before the label is dropped rather than drawn. MapLibre's
/// `text-max-angle` default.
const MAX_ANGLE_BETWEEN_GLYPHS: f32 = 45.0;

/// How much the road may turn in total under one label, in degrees --
/// MapLibre's `text-max-angle` default. A gentle arc is fine; past this the
/// run starts folding over itself.
const MAX_TURN_ACROSS_LABEL: f32 = 45.0;

/// Below this, in the style's own pixels, a label is not read -- it is a
/// smudge that still takes room from its neighbours and still costs a font
/// range to fetch.
///
/// The size is measured on screen rather than in the tile's pixels on purpose.
/// A tile rasterised at half size draws the same label at half the pixels, but
/// the map then draws that tile at twice the size, so the label is exactly as
/// big to the reader and only softer. Dropping it there would take away
/// something legible.
pub const MIN_TEXT_SIZE: f32 = 6.0;

/// Same idea for an icon, measured on its longer side. A 32-pixel glyph shrunk
/// to two or three pixels is a speck.
pub const MIN_ICON_SIZE: f32 = 3.0;

/// Where a symbol goes and which way it faces.
///
/// A point symbol has one of these; a line-placed one has a run of them, each
/// carrying the direction of the line beneath it.
#[derive(Debug, Clone, Copy)]
struct Spot {
    at: (f32, f32),
    angle: f32,
    along_a_line: bool,
    /// Which of the feature's lines this sits on, and how far along it, for a
    /// label that has to follow the line rather than sit at a point.
    line: usize,
    distance: f32,
}

/// An icon that has been positioned but not yet drawn.
pub struct IconPlacement {
    /// Name in the sheet. Kept rather than resolved so the sheet stays
    /// borrowed for as little as possible.
    pub name: String,
    pub paint: IconPaint,
    pub anchor: Anchor,
    pub at: (f32, f32),
}

/// A label that has been shaped and positioned but not yet drawn.
///
/// Text and icon travel together because a style that gives a POI both means
/// them as one thing: dropping the pin and keeping its name, or the other way
/// round, is worse than dropping both.
pub struct Label {
    pub shaped: Shaped,
    pub fonts: Vec<String>,
    pub paint: TextPaint,
    pub anchor: Anchor,
    /// Where the anchor sits on the target tile, in pixels.
    pub at: (f32, f32),
    pub icon: Option<IconPlacement>,
    /// Set when the label follows a line rather than sitting at a point: each
    /// glyph then has its own place and angle, and [`Label::shaped`] is only
    /// what it was laid out from.
    pub curve: Option<Vec<GlyphOnLine>>,
    /// Axis-aligned box the label will cover, for collision. Covers both the
    /// text and the icon when there are both.
    pub bounds: [f32; 4],
    /// The halves on their own, so a style that marks one of them optional can
    /// be given the other when the pair will not fit.
    pub text_bounds: Option<[f32; 4]>,
    pub icon_bounds: Option<[f32; 4]>,
    /// `icon-optional` / `text-optional`: which half may be dropped to save
    /// the other. A POI layer routinely says the pin is expendable and the
    /// name is not.
    pub icon_optional: bool,
    pub text_optional: bool,
    /// What the label says, and where it is in the world rather than in this
    /// tile. Together they identify the same name arriving twice.
    pub content: String,
    pub world_at: (i64, i64),
    /// Which style layer produced this, and what `symbol-sort-key` said about
    /// it. Placement runs in this order, so the style decides which label
    /// wins a collision rather than whichever the tile happened to list first.
    pub layer_index: usize,
    pub sort_key: f64,
    /// Room to keep clear around it, from `text-padding` or `icon-padding`.
    /// Per label rather than per pass: arrows repeated along a road are meant
    /// to sit closer together than two place names ever should.
    pub padding: f32,
    pub allow_overlap: bool,
}

/// Symbol features this renderer cannot place, by style layer id.
pub fn unsupported(style: &Style, zoom: f64) -> Vec<String> {
    let mut reasons = Vec::new();
    for layer in &style.layers {
        if layer.kind != LayerKind::Symbol || !layer.is_visible() || !layer.applies_at(zoom) {
            continue;
        }
        // Shaping is one glyph after another at its own advance. Letter
        // spacing changes that advance, and a script whose letters join or
        // reorder changes it per neighbour; neither is done here.
        if layer.layout.contains_key("text-letter-spacing") {
            reasons.push(format!("{}: text-letter-spacing", layer.id));
        }
    }
    reasons
}

/// Which `{fontstack}/{range}` files this tile's labels need and the store
/// does not have.
///
/// Answered from the tile rather than from the style, because ranges follow
/// the text: a tile of Tokyo needs a handful of CJK ranges, and fetching every
/// range a CJK font could have would be 82 files to use four of them.
pub fn needed_glyphs(
    style: &Style,
    zoom: f64,
    decoded: &[crate::raster::DecodedTile],
    store: &GlyphStore,
) -> Vec<crate::GlyphRequest> {
    let mut wanted: Vec<(String, u32)> = Vec::new();
    for layer in &style.layers {
        if layer.kind != LayerKind::Symbol || !layer.is_visible() || !layer.applies_at(zoom) {
            continue;
        }
        let zoom_ctx = Ctx::zoom_only(zoom);
        let fonts = font_stack(layer, &zoom_ctx);
        if fonts.is_empty() {
            continue;
        }
        // A range is a round trip; there is no sense in one for text this
        // pass has already decided not to draw.
        if (layer.layout_number("text-size", &zoom_ctx, 16.0) as f32) < MIN_TEXT_SIZE {
            continue;
        }
        let (Some(source_id), Some(source_layer_name)) =
            (layer.source.as_deref(), layer.source_layer.as_deref())
        else {
            continue;
        };
        // Every tile carrying this source, which is this tile and the
        // neighbours it borrowed for labels that cross an edge. Asking only
        // the first would leave a borrowed label with no font to draw it in --
        // and, worse, would let the tile call itself finished without one.
        for decoded_tile in decoded.iter().filter(|d| d.source_id == source_id) {
            let Some(data_layer) = decoded_tile.tile.layer(source_layer_name) else {
                continue;
            };
            let filter = CompiledFilter::compile(layer.filter.as_ref(), data_layer, zoom);
            if filter.never_matches() {
                continue;
            }

            let window = crate::raster::overzoom_window(decoded_tile, data_layer.extent);
            for feature in &data_layer.features {
                // Most of an overzoomed parent is nowhere near this tile.
                if let Some(window) = window {
                    if !feature.touches_window(window) {
                        continue;
                    }
                }
                if !filter.matches(data_layer, feature) {
                    continue;
                }
                let ctx = Ctx {
                    zoom,
                    layer: Some(data_layer),
                    feature: Some(feature),
                };
                let Some(text) = text_field(layer, &ctx) else {
                    continue;
                };
                for range in GlyphStore::ranges_for(&text) {
                    for ch in text.chars() {
                        if (ch as u32) / 256 != range {
                            continue;
                        }
                        // Only ask for a range if no face in the stack already has
                        // this character.
                        if store.glyph(&fonts, ch as u32).is_none() {
                            for font in &fonts {
                                let entry = (font.clone(), range);
                                if !wanted.contains(&entry) {
                                    wanted.push(entry);
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
    wanted
        .into_iter()
        .map(|(fontstack, range)| crate::GlyphRequest {
            fontstack,
            range_start: range * 256,
        })
        .collect()
}

/// Shapes every label the style asks for in this tile, in layer order.
/// Where and at what size a tile is being drawn.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub zoom: f64,
    pub tile_size: u32,
    /// Output pixels per style pixel.
    pub paint_scale: f32,
    /// Which tile this is, so a label can be identified by where it is in the
    /// world rather than by where it falls on this particular tile.
    pub xy: (u32, u32),
}

pub fn collect(
    style: &Style,
    target: Target,
    decoded: &[crate::raster::DecodedTile],
    store: &GlyphStore,
    sprites: Option<&SpriteSheet>,
) -> Vec<Label> {
    let Target {
        zoom,
        tile_size,
        paint_scale,
        xy: tile_xy,
    } = target;
    let origin = (
        tile_xy.0 as f64 * tile_size as f64,
        tile_xy.1 as f64 * tile_size as f64,
    );
    let mut labels = Vec::new();
    // Nothing to draw labels *with*. A style whose symbols are all icons can
    // still draw them, which is why this is not a test of the glyph store
    // alone.
    if store.is_empty() && sprites.is_none_or(SpriteSheet::is_empty) {
        return labels;
    }

    for (layer_index, layer) in style.layers.iter().enumerate() {
        if layer.kind != LayerKind::Symbol || !layer.is_visible() || !layer.applies_at(zoom) {
            continue;
        }
        let zoom_ctx = Ctx::zoom_only(zoom);
        // Along a line, symbols repeat: one every `symbol-spacing`, each
        // turned to follow the road under it. Text is still point-only -- the
        // glyphs would have to be laid along the curve one at a time -- so a
        // line-placed layer contributes its icons and `unsupported` reports
        // the rest.
        let placement = layer
            .layout_string("symbol-placement", &zoom_ctx)
            .unwrap_or_else(|| "point".to_string());
        let along_a_line = placement == "line" || placement == "line-center";

        let Some(source_id) = layer.source.as_deref() else {
            continue;
        };
        let Some(source_layer_name) = layer.source_layer.as_deref() else {
            continue;
        };
        let fonts = font_stack(layer, &zoom_ctx);
        // This tile and every neighbour that came with it. A label belongs to
        // whichever tile holds the point it names, so the half that reaches
        // across an edge can only be drawn from the neighbour's own copy.
        for decoded_tile in decoded.iter().filter(|d| d.source_id == source_id) {
            let Some(data_layer) = decoded_tile.tile.layer(source_layer_name) else {
                continue;
            };

            let filter = CompiledFilter::compile(layer.filter.as_ref(), data_layer, zoom);
            if filter.never_matches() {
                continue;
            }

            let window = crate::raster::overzoom_window(decoded_tile, data_layer.extent);
            for feature in &data_layer.features {
                // Most of an overzoomed parent is nowhere near this tile.
                if let Some(window) = window {
                    if !feature.touches_window(window) {
                        continue;
                    }
                }
                if !filter.matches(data_layer, feature) {
                    continue;
                }
                let ctx = Ctx {
                    zoom,
                    layer: Some(data_layer),
                    feature: Some(feature),
                };
                // One symbol at the feature's anchor, or a run of them along
                // it. The lines are kept as well as the spots, because text
                // that follows a road needs the road, not just a point on it.
                let lines: Vec<PixelLine> = if along_a_line {
                    pixel_lines(feature, decoded_tile, data_layer.extent, tile_size)
                } else {
                    Vec::new()
                };
                let spots: Vec<Spot> = if along_a_line {
                    let spacing =
                        layer.layout_number("symbol-spacing", &ctx, 250.0) as f32 * paint_scale;
                    let keep_upright = layer
                        .layout_value("icon-keep-upright", &ctx)
                        .map(|v| v.truthy())
                        .unwrap_or(false);
                    line_spots(&lines, spacing, keep_upright, placement == "line-center")
                } else {
                    anchor_point(feature, decoded_tile, data_layer.extent, tile_size)
                        .map(|at| {
                            vec![Spot {
                                at,
                                angle: 0.0,
                                along_a_line: false,
                                line: 0,
                                distance: 0.0,
                            }]
                        })
                        .unwrap_or_default()
                };

                // Shaped once for the feature, not once per place it is
                // drawn: a road carries the same name every 250 pixels, and
                // laying the glyphs out again for each is work with a known
                // answer.
                let content = text_field(layer, &ctx)
                    .map(|raw| transform_case(&raw, layer.layout_string("text-transform", &ctx)))
                    .filter(|content| !content.trim().is_empty());

                let size = layer.layout_number("text-size", &ctx, 16.0) as f32 * paint_scale;
                // Measured on screen: `size` is in this tile's pixels, and a
                // tile rasterised small is drawn back up to size.
                let readable = paint_scale > 0.0 && size / paint_scale >= MIN_TEXT_SIZE;
                let shaped = match &content {
                    Some(content) if readable => {
                        // A label following a road is one line: there is
                        // nowhere for a second one to go.
                        let max_width = if along_a_line {
                            f32::MAX
                        } else {
                            layer.layout_number("text-max-width", &ctx, 10.0) as f32
                        };
                        text::shape(content, &fonts, store, max_width)
                    }
                    _ => Shaped::default(),
                };

                for spot in spots {
                    let anchor_point = spot.at;
                    let shaped = shaped.clone();

                    // Along a road each glyph gets its own place and angle,
                    // and the whole label is dropped if the road is too short
                    // to hold it -- half a name is worse than none, and here
                    // there is no neighbouring tile to draw the rest.
                    let curve = if along_a_line && !shaped.is_empty() {
                        let upright = layer
                            .layout_value("text-keep-upright", &ctx)
                            .map(|v| v.truthy())
                            .unwrap_or(true);
                        match lines.get(spot.line).and_then(|line| {
                            lay_along_line(line, &shaped, spot.distance, size, upright)
                        }) {
                            Some(glyphs) => Some(glyphs),
                            None => continue,
                        }
                    } else {
                        None
                    };

                    let anchor = layer
                        .layout_string("text-anchor", &ctx)
                        .map(|a| Anchor::from_str(&a))
                        .unwrap_or(Anchor::Center);
                    // `text-offset` is in ems, so it scales with the text.
                    let (ox, oy) = layout_offset(layer, &ctx);
                    let at = (anchor_point.0 + ox * size, anchor_point.1 + oy * size);

                    let opacity = layer.paint_number("text-opacity", &ctx, 1.0) as f32;
                    let paint = TextPaint {
                        size,
                        color: layer.paint_color("text-color", &ctx, Color::BLACK),
                        halo_color: layer.paint_color("text-halo-color", &ctx, Color::TRANSPARENT),
                        halo_width: layer.paint_number("text-halo-width", &ctx, 0.0) as f32
                            * paint_scale,
                        halo_blur: layer.paint_number("text-halo-blur", &ctx, 0.0) as f32
                            * paint_scale,
                        opacity,
                    };

                    let text_bounds = if shaped.is_empty() || opacity <= 0.0 {
                        None
                    } else {
                        match &curve {
                            Some(glyphs) => curved_bounds(glyphs, size),
                            None => Some(text::bounds(&shaped, &paint, anchor, at)),
                        }
                    };

                    // After the text, because `icon-text-fit` sizes the icon around
                    // it: a shield is only as wide as the number it carries.
                    let icon = icon_placement(layer, &ctx, sprites, paint_scale, spot, text_bounds);

                    // A symbol with neither its glyphs nor its icon yet is not a
                    // label; it would reserve space for nothing and push its
                    // neighbours out of the way.
                    if (shaped.is_empty() || opacity <= 0.0) && icon.is_none() {
                        continue;
                    }

                    let icon_bounds = icon.as_ref().and_then(|placement| {
                        sprites
                            .and_then(|sheet| sheet.icon(&placement.name))
                            .map(|icon| {
                                sprite::bounds(
                                    icon,
                                    &placement.paint,
                                    placement.anchor,
                                    placement.at,
                                )
                            })
                    });
                    let Some(bounds) = union(text_bounds, icon_bounds) else {
                        continue;
                    };

                    let allow_overlap = layer
                        .layout_value("text-allow-overlap", &ctx)
                        .map(|v| v.truthy())
                        .unwrap_or(false)
                        || layer
                            .layout_value("icon-allow-overlap", &ctx)
                            .map(|v| v.truthy())
                            .unwrap_or(false);

                    // `symbol-avoid-edges` is how a style says a symbol must not
                    // be cut in half at a tile seam.
                    if avoid_edges(layer, &ctx) && leaves_tile(bounds, tile_size) {
                        continue;
                    }
                    // Most of a neighbour's tile is of no interest here; what is,
                    // is the label near the shared edge that reaches across it.
                    if !touches_tile(bounds, tile_size) {
                        continue;
                    }

                    let padding = if icon.is_some() && shaped.is_empty() {
                        layer.layout_number("icon-padding", &ctx, 2.0) as f32 * paint_scale
                    } else {
                        layer.layout_number("text-padding", &ctx, 2.0) as f32 * paint_scale
                    };

                    labels.push(Label {
                        curve,
                        layer_index,
                        // The style's own idea of which of its labels matters
                        // most. Lower goes first, which is why a style that
                        // wants the biggest place first negates the
                        // population.
                        sort_key: layer.layout_number("symbol-sort-key", &ctx, 0.0),
                        content: content.clone().unwrap_or_default(),
                        world_at: (
                            (origin.0 + at.0 as f64).round() as i64,
                            (origin.1 + at.1 as f64).round() as i64,
                        ),
                        shaped,
                        fonts: fonts.clone(),
                        paint,
                        anchor,
                        at,
                        icon,
                        bounds,
                        text_bounds,
                        icon_bounds,
                        icon_optional: layer
                            .layout_value("icon-optional", &ctx)
                            .map(|v| v.truthy())
                            .unwrap_or(false),
                        text_optional: layer
                            .layout_value("text-optional", &ctx)
                            .map(|v| v.truthy())
                            .unwrap_or(false),
                        padding,
                        allow_overlap,
                    });
                }
            }
        }
    }
    labels
}

fn avoid_edges(layer: &Layer, ctx: &Ctx) -> bool {
    layer
        .layout_value("symbol-avoid-edges", ctx)
        .map(|v| v.truthy())
        .unwrap_or(false)
}

/// Whether any of the label lands on this tile at all.
fn touches_tile(bounds: [f32; 4], tile_size: u32) -> bool {
    let size = tile_size as f32;
    bounds[2] > 0.0 && bounds[3] > 0.0 && bounds[0] < size && bounds[1] < size
}

fn leaves_tile(bounds: [f32; 4], tile_size: u32) -> bool {
    let size = tile_size as f32;
    bounds[0] < 0.0 || bounds[1] < 0.0 || bounds[2] > size || bounds[3] > size
}

/// A feature's line in target pixels, with enough bookkeeping to ask where a
/// given distance along it lands.
struct PixelLine {
    points: Vec<(f32, f32)>,
    /// Distance from the start to each point.
    marks: Vec<f32>,
}

impl PixelLine {
    fn new(points: Vec<(f32, f32)>) -> Option<PixelLine> {
        if points.len() < 2 {
            return None;
        }
        let mut marks = Vec::with_capacity(points.len());
        let mut travelled = 0.0;
        marks.push(0.0);
        for pair in points.windows(2) {
            travelled += (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1);
            marks.push(travelled);
        }
        Some(PixelLine { points, marks })
    }

    fn length(&self) -> f32 {
        *self.marks.last().unwrap_or(&0.0)
    }

    /// The point at this distance along the line, and the direction there.
    fn at(&self, distance: f32) -> Option<((f32, f32), f32)> {
        if distance < 0.0 || distance > self.length() {
            return None;
        }
        let index = match self.marks.binary_search_by(|m| m.total_cmp(&distance)) {
            Ok(i) => i.min(self.points.len() - 2),
            Err(i) => i.saturating_sub(1).min(self.points.len() - 2),
        };
        let (a, b) = (self.points[index], self.points[index + 1]);
        let span = self.marks[index + 1] - self.marks[index];
        let t = if span > f32::EPSILON {
            (distance - self.marks[index]) / span
        } else {
            0.0
        };
        let angle = (b.1 - a.1).atan2(b.0 - a.0).to_degrees();
        Some(((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t), angle))
    }

    fn reversed(&self) -> PixelLine {
        let mut points = self.points.clone();
        points.reverse();
        PixelLine::new(points).expect("a reversed line has the same points")
    }
}

/// Lays a shaped run along a line, centred on `centre` distance.
///
/// Returns nothing when the label would run off either end: half a road name
/// is worse than none, and unlike a tile edge there is no neighbour to draw
/// the rest.
fn lay_along_line(
    line: &PixelLine,
    shaped: &Shaped,
    centre: f32,
    size: f32,
    keep_upright: bool,
) -> Option<Vec<GlyphOnLine>> {
    let scale = size / crate::glyphs::GLYPH_SIZE;
    let width = shaped.width * scale;
    let start = centre - width / 2.0;
    if start < 0.0 || start + width > line.length() {
        return None;
    }

    // Which way the text runs. A road drawn east to west would put every
    // letter upside down, so the line is walked backwards instead.
    if keep_upright {
        if let Some((_, angle)) = line.at(centre) {
            if !(-90.0..=90.0).contains(&angle) {
                let flipped = line.reversed();
                let mirrored = line.length() - centre;
                return lay_along_line(&flipped, shaped, mirrored, size, false);
            }
        }
    }

    let mut out = Vec::with_capacity(shaped.glyphs.len());
    let mut last_angle: Option<f32> = None;
    // The label's turn so far, unwrapped, and the extremes it has reached.
    let mut turned = 0.0f32;
    let mut turn_min = 0.0f32;
    let mut turn_max = 0.0f32;
    for glyph in &shaped.glyphs {
        let along = start + glyph.x * scale;
        let ((x, y), angle) = line.at(along)?;
        // A bend under a label piles its letters onto each other, two ways. A
        // hairpin does it between two adjacent letters; a long gentle curve
        // does it by degrees, each step passing the adjacent check while the
        // run folds back over itself -- which is how 新所原停車場堺の岡線 was
        // drawn as a knot. Both are dropped rather than drawn garbled; the
        // road simply goes unnamed here.
        if let Some(last) = last_angle {
            let step = normalise_degrees(angle - last);
            if step.abs() > MAX_ANGLE_BETWEEN_GLYPHS {
                return None;
            }
            turned += step;
            turn_min = turn_min.min(turned);
            turn_max = turn_max.max(turned);
            if turn_max - turn_min > MAX_TURN_ACROSS_LABEL {
                return None;
            }
        }
        last_angle = Some(angle);
        // The bend compresses the inside of the curve: pens keep their spacing
        // measured along the road, but the straight-line distance between them
        // shrinks -- the arc is longer than the chord -- and the letters pile
        // up even where every angle check passes.
        if let Some(prev) = out.last() {
            let prev: &GlyphOnLine = prev;
            let dx = x - prev.at.0;
            let dy = y - prev.at.1;
            let spacing = along - (start + shaped.glyphs[out.len() - 1].x * scale);
            if spacing > 0.0 && dx.hypot(dy) < spacing * 0.8 {
                return None;
            }
        }
        // The baseline sits below the line's own path, so the text reads as
        // sitting on the road rather than through it.
        let (sin, cos) = angle.to_radians().sin_cos();
        let drop = 0.35 * size;
        out.push(GlyphOnLine {
            code: glyph.code,
            at: (x - sin * drop, y + cos * drop),
            angle,
        });
    }
    Some(out)
}

/// The box a run laid on a line covers, roughly: every glyph's pen, grown by
/// the size of a glyph. Collision has no way to say anything but a rectangle,
/// and a curved label's true shape is not one.
fn curved_bounds(glyphs: &[GlyphOnLine], size: f32) -> Option<[f32; 4]> {
    let first = glyphs.first()?;
    let mut box_ = [first.at.0, first.at.1, first.at.0, first.at.1];
    for glyph in glyphs {
        box_[0] = box_[0].min(glyph.at.0);
        box_[1] = box_[1].min(glyph.at.1);
        box_[2] = box_[2].max(glyph.at.0);
        box_[3] = box_[3].max(glyph.at.1);
    }
    Some([
        box_[0] - size * 0.5,
        box_[1] - size * 0.9,
        box_[2] + size * 0.5,
        box_[3] + size * 0.3,
    ])
}

/// A feature's lines, in target pixels.
fn pixel_lines(
    feature: &model::Feature,
    decoded: &crate::raster::DecodedTile,
    extent: u32,
    tile_size: u32,
) -> Vec<PixelLine> {
    feature
        .parts
        .iter()
        .filter_map(|part| {
            PixelLine::new(
                part.iter()
                    .map(|p| to_pixels(p[0], p[1], decoded, extent, tile_size))
                    .collect(),
            )
        })
        .collect()
}

/// Where along those lines symbols go.
///
/// One every `spacing`, starting half a spacing in so a road shorter than the
/// spacing still gets one marking rather than none -- or a single one in the
/// middle when the style asks for `line-center`.
fn line_spots(
    lines: &[PixelLine],
    spacing: f32,
    keep_upright: bool,
    centre_only: bool,
) -> Vec<Spot> {
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let length = line.length();
        if length <= 0.0 {
            continue;
        }
        let mut distance = if centre_only {
            length / 2.0
        } else {
            spacing / 2.0
        };
        while distance <= length {
            if let Some((at, mut angle)) = line.at(distance) {
                // Upside down reads as a different sign entirely, so a style
                // can ask for the symbol to be turned the right way up.
                if keep_upright && !(-90.0..=90.0).contains(&angle) {
                    angle = normalise_degrees(angle + 180.0);
                }
                out.push(Spot {
                    at,
                    angle,
                    along_a_line: true,
                    line: index,
                    distance,
                });
            }
            if centre_only || spacing < 1.0 {
                break;
            }
            distance += spacing;
        }
    }
    out
}

/// Folds an angle back into (-180, 180], so a flipped symbol reads as 0
/// rather than 360.
fn normalise_degrees(angle: f32) -> f32 {
    let wrapped = angle % 360.0;
    if wrapped > 180.0 {
        wrapped - 360.0
    } else if wrapped <= -180.0 {
        wrapped + 360.0
    } else {
        wrapped
    }
}

/// Tile coordinates to target pixels, allowing for an overzoomed parent.
fn to_pixels(
    x: f32,
    y: f32,
    decoded: &crate::raster::DecodedTile,
    extent: u32,
    tile_size: u32,
) -> (f32, f32) {
    let size = tile_size as f32;
    let k = size * decoded.scale / extent as f32;
    (
        x * k - decoded.offset_x * decoded.scale * size,
        y * k - decoded.offset_y * decoded.scale * size,
    )
}

/// Drops labels that would cover one already kept.
///
/// First come, first served, in the order [`collect`] produced — which is
/// style layer order, so a style that wants cities to win over villages says
/// so by putting them first. Two labels touching at a corner is still a
/// collision: the boxes include the halo's bleed, and text that nearly
/// overlaps reads worse than text that is missing.
pub fn place(labels: Vec<Label>) -> Vec<Label> {
    let mut labels = drop_repeats(labels);
    // Within a layer, the order the style asked for; between layers, the
    // style's own order, which is why this is stable and keyed on both. A
    // collision is decided by whoever is placed first, so this is the whole
    // of what `symbol-sort-key` does.
    labels.sort_by(|a, b| {
        a.layer_index
            .cmp(&b.layer_index)
            .then(a.sort_key.total_cmp(&b.sort_key))
    });
    let mut kept: Vec<Label> = Vec::new();
    for mut label in labels {
        if label.allow_overlap {
            kept.push(label);
            continue;
        }

        let fits = |bounds: [f32; 4], padding: f32, kept: &[Label]| {
            let grown = grow(bounds, padding);
            !kept.iter().any(|other| {
                !other.allow_overlap && overlaps(grown, grow(other.bounds, other.padding))
            })
        };

        if fits(label.bounds, label.padding, &kept) {
            kept.push(label);
            continue;
        }

        // The pair does not fit. A style that marked one half optional would
        // rather have the other half than nothing -- a POI whose pin is in the
        // way still wants its name read.
        if label.icon_optional {
            if let Some(text_only) = label.text_bounds {
                if fits(text_only, label.padding, &kept) {
                    label.icon = None;
                    label.bounds = text_only;
                    kept.push(label);
                    continue;
                }
            }
        }
        if label.text_optional {
            if let Some(icon_only) = label.icon_bounds {
                if fits(icon_only, label.padding, &kept) {
                    label.shaped = Shaped::default();
                    label.bounds = icon_only;
                    kept.push(label);
                    continue;
                }
            }
        }
    }
    kept
}

/// How far apart two copies of the same name can be and still be the same
/// place, in pixels.
///
/// A district's polygon is cut at every tile it crosses, and each piece has
/// its own middle, so the same name arrives once per piece. Anything within a
/// tile of another copy is one of those; two towns that genuinely share a name
/// are not this close.
const REPEAT_DISTANCE: f64 = 512.0;

/// Drops the extra copies of a name that tile clipping produced.
///
/// The copy kept is the one nearest the top-left of the world, which is a
/// silly-sounding rule with one essential property: every tile that can see
/// this name picks the same copy. Keeping "the one from my own tile" would
/// have each tile keep a different copy and draw it, which is the duplicate
/// this is meant to remove.
fn drop_repeats(labels: Vec<Label>) -> Vec<Label> {
    let mut winners: Vec<(usize, (i64, i64))> = Vec::new();
    let mut dropped = vec![false; labels.len()];

    for (index, label) in labels.iter().enumerate() {
        if label.content.is_empty() {
            continue;
        }
        let mut replaced = None;
        for (other, at) in winners.iter_mut() {
            if labels[*other].content != label.content {
                continue;
            }
            let dx = (at.0 - label.world_at.0) as f64;
            let dy = (at.1 - label.world_at.1) as f64;
            if dx.hypot(dy) > REPEAT_DISTANCE {
                continue;
            }
            if label.world_at < *at {
                dropped[*other] = true;
                *other = index;
                *at = label.world_at;
            } else {
                replaced = Some(());
            }
            break;
        }
        if replaced.is_some() {
            dropped[index] = true;
        } else if !dropped[index] && !winners.iter().any(|(i, _)| *i == index) {
            winners.push((index, label.world_at));
        }
    }

    labels
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !dropped[*index])
        .map(|(_, label)| label)
        .collect()
}

pub fn draw(
    pixmap: &mut Pixmap,
    labels: &[Label],
    store: &GlyphStore,
    sprites: Option<&SpriteSheet>,
) {
    for label in labels {
        // Icon first: a style that puts a name on a pin means the name to be
        // readable over it.
        if let (Some(placement), Some(sheet)) = (&label.icon, sprites) {
            if let Some(icon) = sheet.icon(&placement.name) {
                sprite::draw(
                    pixmap,
                    sheet,
                    icon,
                    &placement.paint,
                    placement.anchor,
                    placement.at,
                );
            }
        }
        if label.shaped.is_empty() {
            continue;
        }
        if let Some(curve) = &label.curve {
            text::draw_along_line(pixmap, curve, store, &label.fonts, &label.paint);
            continue;
        }
        text::draw(
            pixmap,
            &label.shaped,
            store,
            &label.fonts,
            &label.paint,
            label.anchor,
            label.at,
        );
    }
}

fn grow(b: [f32; 4], by: f32) -> [f32; 4] {
    [b[0] - by, b[1] - by, b[2] + by, b[3] + by]
}

fn overlaps(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// `text-font` is a list of faces tried in order.
fn font_stack(layer: &Layer, ctx: &Ctx) -> Vec<String> {
    layer
        .layout
        .get("text-font")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_owned))
                .collect()
        })
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| {
            layer
                .layout_string("text-font", ctx)
                .map(|s| vec![s])
                .unwrap_or_default()
        })
}

/// `text-field` in either form: an expression, or the older `{name}` tokens.
///
/// Every basemap style in the wild still uses tokens — `{name}`, `{name:latin}`
/// — so an implementation that only evaluated expressions would render a map
/// with no labels at all and no error to explain it.
/// Reads a symbol's icon, if the style names one and the sheet has it.
///
/// A name the sheet does not carry is not an error worth failing a tile over:
/// styles routinely name icons per feature class and ship a sheet that covers
/// most of them, so a miss draws nothing and the label's text still stands.
fn icon_placement(
    layer: &Layer,
    ctx: &Ctx,
    sprites: Option<&SpriteSheet>,
    paint_scale: f32,
    spot: Spot,
    text_bounds: Option<[f32; 4]>,
) -> Option<IconPlacement> {
    let Spot {
        at: anchor_point,
        angle: line_angle,
        along_a_line,
        ..
    } = spot;
    let sheet = sprites?;
    let raw = layer.layout.get("icon-image")?;
    let name = match raw.as_str() {
        Some(template) => expand_tokens(template, ctx),
        None => layer.layout_string("icon-image", ctx)?,
    };
    if name.is_empty() {
        return None;
    }
    // Checked here rather than at draw time so a name the sheet does not have
    // reserves no room in the collision pass.
    sheet.icon(&name)?;

    let size = layer.layout_number("icon-size", ctx, 1.0) as f32;
    if size <= 0.0 {
        return None;
    }
    let icon_entry = sheet.icon(&name)?;
    let (natural_w, natural_h) = icon_entry.display_size();
    if natural_w.max(natural_h) * size < MIN_ICON_SIZE {
        return None;
    }
    let opacity = layer.paint_number("icon-opacity", ctx, 1.0) as f32;
    if opacity <= 0.0 {
        return None;
    }

    // `icon-offset` is in the icon's own pixels, so it grows with icon-size
    // and with the tile's scale, exactly as the icon does.
    let (ox, oy) = offset_of(layer, "icon-offset");
    let scale = size * paint_scale;

    // `icon-text-fit` resizes the icon around its label, which is how one
    // shield in a sheet serves every route number. The icon then sits on the
    // text rather than on the feature's own point.
    let fitted = text_fit(layer, ctx, icon_entry, scale, paint_scale, text_bounds);
    Some(IconPlacement {
        name,
        paint: IconPaint {
            scale,
            opacity,
            color: layer.paint_color("icon-color", ctx, Color::BLACK),
            halo_color: layer.paint_color("icon-halo-color", ctx, Color::TRANSPARENT),
            halo_width: layer.paint_number("icon-halo-width", ctx, 0.0) as f32,
            // A map-aligned symbol turns with the ground under it; a
            // viewport-aligned one keeps its own angle whatever the map does.
            // Along a line the first is the default, and it is the whole point
            // -- an arrow that does not follow its road points nowhere.
            rotation: layer.layout_number("icon-rotate", ctx, 0.0) as f32
                + if map_aligned(layer, ctx, along_a_line) {
                    line_angle
                } else {
                    0.0
                },
            stretch_to: fitted.map(|(size, _)| size),
        },
        anchor: layer
            .layout_string("icon-anchor", ctx)
            .map(|a| Anchor::from_str(&a))
            .unwrap_or(Anchor::Center),
        at: match fitted {
            Some((_, centre)) => centre,
            None => (anchor_point.0 + ox * scale, anchor_point.1 + oy * scale),
        },
    })
}

/// The size and centre an `icon-text-fit` icon should be drawn at.
///
/// The padded text box is what the icon's *content* has to cover, so the
/// margins outside the content -- a shield's border -- are added back rather
/// than squashed. Returns `None` when the style does not ask for a fit, or
/// when there is no text to fit around.
fn text_fit(
    layer: &Layer,
    ctx: &Ctx,
    icon: &crate::sprite::Icon,
    scale: f32,
    paint_scale: f32,
    text_bounds: Option<[f32; 4]>,
) -> Option<((f32, f32), (f32, f32))> {
    let fit = layer.layout_string("icon-text-fit", ctx)?;
    if fit == "none" {
        return None;
    }
    let text = text_bounds?;

    // `icon-text-fit-padding` is [top, right, bottom, left].
    let pad = layer
        .layout
        .get("icon-text-fit-padding")
        .and_then(|v| v.as_array())
        .map(|items| {
            let n = |i: usize| items.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            [n(0), n(1), n(2), n(3)]
        })
        .unwrap_or([0.0; 4])
        .map(|v| v * paint_scale);

    let wanted_w = (text[2] - text[0]) + pad[1] + pad[3];
    let wanted_h = (text[3] - text[1]) + pad[0] + pad[2];

    // What the icon adds around its content, at the size it would have drawn.
    let (natural_w, natural_h) = icon.display_size();
    let (natural_w, natural_h) = (natural_w * scale, natural_h * scale);
    let (margin_w, margin_h) = match icon.content {
        Some(content) => {
            let k = scale / icon.pixel_ratio.max(0.001);
            (
                natural_w - (content[2] - content[0]) * k,
                natural_h - (content[3] - content[1]) * k,
            )
        }
        None => (0.0, 0.0),
    };

    let width = match fit.as_str() {
        "width" | "both" => wanted_w + margin_w,
        _ => natural_w,
    };
    let height = match fit.as_str() {
        "height" | "both" => wanted_h + margin_h,
        _ => natural_h,
    };

    let centre = (
        (text[0] + text[2]) / 2.0 + (pad[3] - pad[1]) / 2.0,
        (text[1] + text[3]) / 2.0 + (pad[0] - pad[2]) / 2.0,
    );
    Some(((width, height), centre))
}

/// Whether the icon turns with the map rather than with the screen.
///
/// The spec's default differs by placement -- map for a line, viewport for a
/// point -- and this renderer draws north-up, so a viewport-aligned icon is
/// left with only its own `icon-rotate`.
fn map_aligned(layer: &Layer, ctx: &Ctx, along_a_line: bool) -> bool {
    match layer
        .layout_string("icon-rotation-alignment", ctx)
        .as_deref()
    {
        Some("map") => true,
        Some("viewport") => false,
        // `auto` means map on a line and viewport at a point, which is also
        // what the absent value means.
        _ => along_a_line,
    }
}

/// The box covering both, or whichever one there is.
fn union(a: Option<[f32; 4]>, b: Option<[f32; 4]>) -> Option<[f32; 4]> {
    match (a, b) {
        (Some(a), Some(b)) => Some([
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ]),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

fn text_field(layer: &Layer, ctx: &Ctx) -> Option<String> {
    let raw = layer.layout.get("text-field")?;
    if let Some(template) = raw.as_str() {
        return Some(expand_tokens(template, ctx));
    }
    layer.layout_string("text-field", ctx)
}

fn expand_tokens(template: &str, ctx: &Ctx) -> String {
    if !template.contains('{') {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let key = &rest[open + 1..open + close];
        if let Some(value) = ctx.property(key).as_str() {
            out.push_str(value);
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

fn transform_case(text: &str, transform: Option<String>) -> String {
    match transform.as_deref() {
        Some("uppercase") => text.to_uppercase(),
        Some("lowercase") => text.to_lowercase(),
        _ => text.to_string(),
    }
}

fn layout_offset(layer: &Layer, _ctx: &Ctx) -> (f32, f32) {
    offset_of(layer, "text-offset")
}

fn offset_of(layer: &Layer, key: &str) -> (f32, f32) {
    layer
        .layout
        .get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            let n = |i: usize| items.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            (n(0), n(1))
        })
        .unwrap_or((0.0, 0.0))
}

/// Where a feature's label hangs.
///
/// A point uses itself. A line or polygon uses the mean of its first ring,
/// which is a centroid for compact shapes and merely somewhere-inside for long
/// ones — good enough to place a name, and honest about being an approximation
/// rather than the pole of inaccessibility a full renderer computes.
fn anchor_point(
    feature: &model::Feature,
    decoded: &crate::raster::DecodedTile,
    extent: u32,
    tile_size: u32,
) -> Option<(f32, f32)> {
    let part = feature.parts.first()?;
    if part.is_empty() {
        return None;
    }
    let (sum_x, sum_y) = part
        .iter()
        .fold((0.0f32, 0.0f32), |acc, p| (acc.0 + p[0], acc.1 + p[1]));
    let count = part.len() as f32;
    let (x, y) = match feature.geom_type {
        GeomType::Point => (part[0][0], part[0][1]),
        _ => (sum_x / count, sum_y / count),
    };

    let size = tile_size as f32;
    let (px, py) = to_pixels(x, y, decoded, extent, tile_size);
    // Half a tile: far enough to catch a neighbour's label that reaches in,
    // near enough to skip the rest of the neighbour's tile early. What
    // actually reaches in is decided later, from the label's own box.
    let margin = size * 0.5;
    if px < -margin || py < -margin || px > size + margin || py > size + margin {
        return None;
    }
    Some((px, py))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{encode_tile, FeatureSpec, Geom, LayerSpec};
    use crate::glyphs::{FontStack, Glyph, BUFFER};
    use std::collections::HashMap;

    /// Builds a one-feature tile through the real encoder and decoder, so the
    /// test exercises the same path a served tile takes.
    fn point_tile(layer: &str, name: &str, x: i32, y: i32) -> std::sync::Arc<model::Tile> {
        let bytes = encode_tile(&[LayerSpec {
            name: layer.into(),
            extent: 4096,
            features: vec![FeatureSpec {
                geom: Geom::Point,
                points: vec![(x, y)],
                props: vec![("name".into(), name.into())],
            }],
        }]);
        std::sync::Arc::new(crate::mvt::decode(&bytes).expect("fixture decodes"))
    }

    fn glyph(code: u32, advance: u32) -> Glyph {
        let (w, h) = (10u32, 12u32);
        Glyph {
            id: code,
            bitmap: vec![200u8; ((w + 2 * BUFFER) * (h + 2 * BUFFER)) as usize],
            width: w,
            height: h,
            left: 0,
            top: h as i32,
            advance,
        }
    }

    fn store(codes: &[u32]) -> GlyphStore {
        let mut stack = FontStack {
            name: "F".into(),
            range: "0-255".into(),
            glyphs: HashMap::new(),
        };
        for c in codes {
            stack.glyphs.insert(*c, glyph(*c, 12));
        }
        let mut s = GlyphStore::new();
        s.insert(stack);
        s
    }

    /// A road name follows the road: each glyph at its own place, facing its
    /// own way. Laid out flat it would cross every street it names.
    #[test]
    fn lays_a_name_along_the_line_it_names() {
        let store = store(&[0x41, 0x42]);
        let shaped = text::shape("AB", &["F".to_string()], &store, f32::MAX);

        // A straight horizontal line, 400 pixels long.
        let flat = PixelLine::new(vec![(50.0, 200.0), (450.0, 200.0)]).expect("a line");
        let glyphs = lay_along_line(&flat, &shaped, 200.0, 24.0, true).expect("it fits");
        assert_eq!(glyphs.len(), 2);
        assert!(glyphs[0].angle.abs() < 0.01, "a flat road is 0 degrees");
        assert!(
            glyphs[1].at.0 > glyphs[0].at.0,
            "the run goes left to right"
        );
        // Centred on the anchor, which is a distance along the line rather
        // than a coordinate: the line starts at x=50, the run is 24 pixels
        // wide, so it begins at 50 + 200 - 12.
        assert!(
            (glyphs[0].at.0 - 238.0).abs() < 1.0,
            "got {:?}",
            glyphs[0].at
        );

        // The same line turned: every glyph turns with it.
        let down = PixelLine::new(vec![(200.0, 50.0), (200.0, 450.0)]).expect("a line");
        let turned = lay_along_line(&down, &shaped, 200.0, 24.0, true).expect("it fits");
        assert!((turned[0].angle - 90.0).abs() < 0.01, "got {:?}", turned[0]);
        assert!(
            turned[1].at.1 > turned[0].at.1,
            "the run goes down the line"
        );
    }

    /// A hairpin under a label piles its letters onto each other; better no
    /// name here than a garbled one.
    #[test]
    fn drops_a_name_on_a_hairpin_bend() {
        let store = store(&[0x41, 0x42]);
        let shaped = text::shape("AB", &["F".to_string()], &store, f32::MAX);

        // The line doubles back on itself right where the label would sit.
        let hairpin =
            PixelLine::new(vec![(0.0, 100.0), (24.0, 100.0), (0.0, 104.0)]).expect("a line");
        assert!(
            lay_along_line(&hairpin, &shaped, 28.0, 24.0, false).is_none(),
            "a label across a 170-degree bend is unreadable"
        );

        // A gentle bend is fine.
        let gentle =
            PixelLine::new(vec![(0.0, 100.0), (24.0, 100.0), (48.0, 108.0)]).expect("a line");
        assert!(lay_along_line(&gentle, &shaped, 24.0, 24.0, false).is_some());
    }

    /// A long gentle curve garbles a label by degrees: every adjacent pair of
    /// letters clears the hairpin check while the run folds back on itself.
    #[test]
    fn drops_a_name_that_curls_around_a_long_bend() {
        let store = store(&[0x41, 0x42]);
        // Six glyphs, so the curve is sampled often enough that each step is
        // gentle.
        let shaped = text::shape("ABABAB", &["F".to_string()], &store, f32::MAX);

        // A half-circle: each step turns 30 degrees, well under the adjacent
        // limit, and the whole run turns 150.
        let mut points = Vec::new();
        for i in 0..=6 {
            let angle = (i as f32) * 30.0f32.to_radians();
            points.push((100.0 - angle.cos() * 40.0, 100.0 + angle.sin() * 40.0));
        }
        let arc = PixelLine::new(points).expect("a line");
        let centre = arc.length() / 2.0;
        assert!(
            lay_along_line(&arc, &shaped, centre, 24.0, false).is_none(),
            "a label around a half-circle overlaps itself"
        );
    }

    /// A road drawn east to west is the same road, and its name still has to
    /// read left to right.
    #[test]
    fn turns_a_backwards_road_name_the_right_way_up() {
        let store = store(&[0x41, 0x42]);
        let shaped = text::shape("AB", &["F".to_string()], &store, f32::MAX);
        let backwards = PixelLine::new(vec![(450.0, 200.0), (50.0, 200.0)]).expect("a line");

        let upright = lay_along_line(&backwards, &shaped, 200.0, 24.0, true).expect("it fits");
        assert!(upright[0].angle.abs() < 0.01, "got {:?}", upright[0]);
        assert!(upright[1].at.0 > upright[0].at.0, "it reads left to right");

        let as_drawn = lay_along_line(&backwards, &shaped, 200.0, 24.0, false).expect("it fits");
        assert!(
            (as_drawn[0].angle.abs() - 180.0).abs() < 0.01,
            "without keep-upright it follows the line as drawn, got {:?}",
            as_drawn[0]
        );
    }

    /// Half a road name is worse than none, and there is no neighbour to draw
    /// the rest -- the line simply ends.
    #[test]
    fn drops_a_name_the_road_is_too_short_for() {
        let store = store(&[0x41, 0x42]);
        let shaped = text::shape("AB", &["F".to_string()], &store, f32::MAX);
        let stub = PixelLine::new(vec![(100.0, 100.0), (120.0, 100.0)]).expect("a line");

        assert!(lay_along_line(&stub, &shaped, 10.0, 24.0, true).is_none());
    }

    /// `symbol-sort-key` is the style saying which of its own labels matters
    /// most. Without it, a collision is settled by whichever the tile happened
    /// to list first -- so a village can take the place of a city.
    #[test]
    fn places_in_the_order_the_style_asked_for() {
        let entry = |x: f32, name: &str, key: f64| {
            let mut l = label(x, 100.0, 60.0, 20.0, false);
            l.content = name.into();
            l.world_at = (x as i64, 100);
            l.sort_key = key;
            l
        };

        // Two labels on the same spot. The village is listed first and the
        // city second, so first-come would keep the village.
        let kept = place(vec![entry(100.0, "村", -10.0), entry(110.0, "市", -1000.0)]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].content, "市", "the style's own order was ignored");
    }

    /// Between layers it is still the style's layer order that decides; the
    /// sort key only orders a layer's own labels.
    #[test]
    fn a_sort_key_does_not_reorder_the_layers() {
        let entry = |x: f32, name: &str, layer: usize, key: f64| {
            let mut l = label(x, 100.0, 60.0, 20.0, false);
            l.content = name.into();
            l.world_at = (x as i64, 100);
            l.layer_index = layer;
            l.sort_key = key;
            l
        };

        // The later layer has the lower key, and still loses: a style puts
        // what matters lower down the list on purpose.
        let kept = place(vec![
            entry(100.0, "first-layer", 3, 100.0),
            entry(110.0, "later-layer", 9, -100.0),
        ]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].content, "first-layer");
    }

    /// A name anchored just inside one tile spills across the seam, and the
    /// neighbour has to draw the other half. It can only do that if the
    /// neighbour's data is offered to it, which is what a labels-only tile is.
    #[test]
    fn draws_the_half_of_a_neighbours_label_that_reaches_in() {
        let style = Style::parse(
            r##"{
                "version": 8,
                "sources": { "src": { "type": "vector" } },
                "layers": [{
                    "id": "places", "type": "symbol", "source": "src",
                    "source-layer": "place",
                    "layout": { "text-field": "{name}", "text-font": ["F"], "text-size": 20 }
                }]
            }"##,
        )
        .expect("style parses");
        let store = store(&[0x41, 0x42]);

        // The label sits five pixels inside the western neighbour's east
        // edge, so most of it belongs on this tile. The neighbour is offered
        // shifted a whole tile to the left.
        let west = crate::raster::DecodedTile {
            source_id: "src".into(),
            tile: point_tile("place", "AB", 4096 - 40, 2048),
            scale: 1.0,
            offset_x: 1.0,
            offset_y: 0.0,
            labels_only: true,
        };
        let placed = collect(
            &style,
            Target {
                zoom: 12.0,
                tile_size: 512,
                paint_scale: 1.0,
                xy: (1, 0),
            },
            &[west],
            &store,
            None,
        );
        assert_eq!(placed.len(), 1, "the neighbour's label was not picked up");
        assert!(
            placed[0].bounds[0] < 0.0 && placed[0].bounds[2] > 0.0,
            "expected it to straddle the edge, got {:?}",
            placed[0].bounds
        );

        // One in the middle of the neighbour reaches nothing here and is not
        // worth carrying.
        let far = crate::raster::DecodedTile {
            source_id: "src".into(),
            tile: point_tile("place", "AB", 2048, 2048),
            scale: 1.0,
            offset_x: 1.0,
            offset_y: 0.0,
            labels_only: true,
        };
        assert!(collect(
            &style,
            Target {
                zoom: 12.0,
                tile_size: 512,
                paint_scale: 1.0,
                xy: (1, 0)
            },
            &[far],
            &store,
            None
        )
        .is_empty());
    }

    /// A district's polygon is cut at every tile it crosses and each piece
    /// gets its own label, so the same name arrives more than once. Dropping
    /// "the ones that are not mine" would have each tile keep a different one
    /// and draw it -- the duplicate would survive. Every tile has to choose
    /// the same copy.
    #[test]
    fn every_tile_drops_the_same_duplicate() {
        let here = |x: f32, world: (i64, i64)| {
            let mut l = label(x, 100.0, 60.0, 20.0, false);
            l.content = "港区".into();
            l.world_at = world;
            l
        };

        // Tile A sees its own copy at world 5000 and the neighbour's at 5300.
        let from_a = place(vec![here(100.0, (5000, 100)), here(400.0, (5300, 100))]);
        // Tile B sees the same two, in the other order and shifted into its
        // own pixels.
        let from_b = place(vec![here(-100.0, (5300, 100)), here(-400.0, (5000, 100))]);

        assert_eq!(from_a.len(), 1);
        assert_eq!(from_b.len(), 1);
        assert_eq!(
            from_a[0].world_at, from_b[0].world_at,
            "the two tiles kept different copies, so the name is still drawn twice"
        );
    }

    /// Two places that really do share a name are not a duplicate.
    #[test]
    fn keeps_two_distant_places_with_the_same_name() {
        let far = |x: f32, world: (i64, i64)| {
            let mut l = label(x, 100.0, 60.0, 20.0, true);
            l.content = "中央区".into();
            l.world_at = world;
            l
        };
        let kept = place(vec![far(100.0, (5000, 100)), far(400.0, (9000, 100))]);
        assert_eq!(kept.len(), 2);
    }

    /// Text too small to read is worse than no text: it is a smudge that also
    /// takes room from its neighbours and costs a font range to fetch.
    #[test]
    fn drops_text_too_small_to_read() {
        let style = |size: f64| {
            Style::parse(&format!(
                r##"{{
                    "version": 8,
                    "glyphs": "https://example.com/{{fontstack}}/{{range}}.pbf",
                    "sources": {{ "src": {{ "type": "vector" }} }},
                    "layers": [{{
                        "id": "places", "type": "symbol", "source": "src",
                        "source-layer": "place",
                        "layout": {{
                            "text-field": "{{name}}", "text-font": ["F"],
                            "text-size": {size}
                        }}
                    }}]
                }}"##
            ))
            .expect("style parses")
        };
        let decoded = vec![crate::raster::DecodedTile {
            source_id: "src".into(),
            tile: point_tile("place", "AB", 2048, 2048),
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,

            labels_only: false,
        }];
        let store = store(&[0x41, 0x42]);

        let readable = style(12.0);
        assert_eq!(
            collect(
                &readable,
                Target {
                    zoom: 12.0,
                    tile_size: 512,
                    paint_scale: 1.0,
                    xy: (0, 0)
                },
                &decoded,
                &store,
                None
            )
            .len(),
            1
        );

        let tiny = style(4.0);
        assert!(
            collect(
                &tiny,
                Target {
                    zoom: 12.0,
                    tile_size: 512,
                    paint_scale: 1.0,
                    xy: (0, 0)
                },
                &decoded,
                &store,
                None
            )
            .is_empty(),
            "4 pixels of text is a smudge"
        );
        assert!(
            needed_glyphs(&tiny, 12.0, &decoded, &GlyphStore::new()).is_empty(),
            "and it must not send anyone to fetch a font for it"
        );
    }

    /// The floor is about how big the label is to the reader, not how many
    /// pixels this particular tile happens to have. A tile rasterised at half
    /// size is drawn at twice the size, so its labels are exactly as legible
    /// and only softer -- dropping them there would take away something the
    /// reader can read.
    #[test]
    fn measures_legibility_on_screen_not_in_tile_pixels() {
        let style = Style::parse(
            r##"{
                "version": 8,
                "sources": { "src": { "type": "vector" } },
                "layers": [{
                    "id": "places", "type": "symbol", "source": "src",
                    "source-layer": "place",
                    "layout": { "text-field": "{name}", "text-font": ["F"], "text-size": 8 }
                }]
            }"##,
        )
        .expect("style parses");
        let decoded = vec![crate::raster::DecodedTile {
            source_id: "src".into(),
            tile: point_tile("place", "AB", 2048, 2048),
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,

            labels_only: false,
        }];
        let store = store(&[0x41, 0x42]);

        // Half-size tile: the label is drawn at four pixels and kept.
        let half = collect(
            &style,
            Target {
                zoom: 12.0,
                tile_size: 256,
                paint_scale: 0.5,
                xy: (0, 0),
            },
            &decoded,
            &store,
            None,
        );
        assert_eq!(half.len(), 1);
        assert!((half[0].paint.size - 4.0).abs() < 0.01);
    }

    /// A 32-pixel icon shrunk to a speck is not information either.
    #[test]
    fn drops_icons_too_small_to_make_out() {
        use crate::sprite::SpriteSheet;

        let mut image = Pixmap::new(32, 32).expect("pixmap");
        for slot in image.pixels_mut().iter_mut() {
            *slot = tiny_skia::ColorU8::from_rgba(0, 0, 0, 255).premultiply();
        }
        let sheet = SpriteSheet::parse(
            r#"{"pin": {"x": 0, "y": 0, "width": 32, "height": 32, "pixelRatio": 1}}"#,
            &image.encode_png().expect("encode"),
        )
        .expect("sheet");

        let style = |icon_size: f64| {
            Style::parse(&format!(
                r##"{{
                    "version": 8,
                    "sources": {{ "src": {{ "type": "vector" }} }},
                    "layers": [{{
                        "id": "pois", "type": "symbol", "source": "src",
                        "source-layer": "place",
                        "layout": {{ "icon-image": "pin", "icon-size": {icon_size} }}
                    }}]
                }}"##
            ))
            .expect("style parses")
        };
        let decoded = vec![crate::raster::DecodedTile {
            source_id: "src".into(),
            tile: point_tile("place", "AB", 2048, 2048),
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,

            labels_only: false,
        }];

        let target = Target {
            zoom: 12.0,
            tile_size: 512,
            paint_scale: 1.0,
            xy: (0, 0),
        };
        let visible = collect(&style(0.5), target, &decoded, &store(&[]), Some(&sheet));
        assert_eq!(visible.len(), 1, "16 pixels is an icon");

        let speck = collect(&style(0.05), target, &decoded, &store(&[]), Some(&sheet));
        assert!(speck.is_empty(), "1.6 pixels is a speck");
    }

    /// `icon-optional` is a style saying the pin is expendable and the name is
    /// not. Dropping both is the wrong answer, and so is keeping both.
    #[test]
    fn gives_up_the_optional_half_rather_than_the_whole_label() {
        let blocker = label(0.0, 0.0, 40.0, 20.0, false);

        // Its icon sits inside the blocker; its text is clear of it.
        let mut crowded = label(0.0, 0.0, 100.0, 20.0, false);
        crowded.icon_bounds = Some([10.0, 5.0, 30.0, 15.0]);
        crowded.text_bounds = Some([60.0, 0.0, 100.0, 20.0]);
        crowded.icon = None; // nothing to draw in this test; the boxes decide
        crowded.icon_optional = true;

        let kept = place(vec![blocker, crowded]);
        assert_eq!(kept.len(), 2, "the name survived without its pin");
        assert_eq!(
            kept[1].bounds,
            [60.0, 0.0, 100.0, 20.0],
            "and reserves only the room it now uses"
        );
    }

    #[test]
    fn drops_both_halves_when_neither_is_optional() {
        let blocker = label(0.0, 0.0, 40.0, 20.0, false);
        let mut crowded = label(0.0, 0.0, 100.0, 20.0, false);
        crowded.icon_bounds = Some([10.0, 5.0, 30.0, 15.0]);
        crowded.text_bounds = Some([60.0, 0.0, 100.0, 20.0]);

        assert_eq!(place(vec![blocker, crowded]).len(), 1);
    }

    /// A style repeats a marking along a road with `symbol-spacing`, and each
    /// one has to point the way the road does.
    #[test]
    fn walks_a_line_and_turns_each_symbol_to_follow_it() {
        // A straight horizontal line across the tile: 4096 units of extent
        // become 512 pixels, so the whole line is 512 long.
        let bytes = encode_tile(&[LayerSpec {
            name: "roads".into(),
            extent: 4096,
            features: vec![FeatureSpec {
                geom: Geom::Line,
                points: vec![(0, 2048), (4096, 2048)],
                props: vec![],
            }],
        }]);
        let tile = std::sync::Arc::new(crate::mvt::decode(&bytes).expect("decodes"));
        let decoded = crate::raster::DecodedTile {
            source_id: "src".into(),
            tile,
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,

            labels_only: false,
        };
        let feature = &decoded.tile.layers[0].features[0];

        let spots = line_spots(
            &pixel_lines(feature, &decoded, 4096, 512),
            100.0,
            false,
            false,
        );
        // First at half a spacing, then every spacing: 50, 150, 250, 350, 450.
        assert_eq!(spots.len(), 5);
        assert!((spots[0].at.0 - 50.0).abs() < 0.5, "got {:?}", spots[0]);
        assert!((spots[1].at.0 - 150.0).abs() < 0.5, "got {:?}", spots[1]);
        for spot in &spots {
            assert!((spot.at.1 - 256.0).abs() < 0.5, "off the line: {spot:?}");
            assert!(
                spot.angle.abs() < 0.5,
                "a flat line is 0 degrees, got {spot:?}"
            );
        }
    }

    /// A road drawn right-to-left is the same road; a symbol that follows it
    /// blindly ends up upside down, which `icon-keep-upright` exists to stop.
    #[test]
    fn keeps_a_symbol_upright_when_asked() {
        let bytes = encode_tile(&[LayerSpec {
            name: "roads".into(),
            extent: 4096,
            features: vec![FeatureSpec {
                geom: Geom::Line,
                points: vec![(4096, 2048), (0, 2048)],
                props: vec![],
            }],
        }]);
        let tile = std::sync::Arc::new(crate::mvt::decode(&bytes).expect("decodes"));
        let decoded = crate::raster::DecodedTile {
            source_id: "src".into(),
            tile,
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,

            labels_only: false,
        };
        let feature = &decoded.tile.layers[0].features[0];

        let facing = line_spots(
            &pixel_lines(feature, &decoded, 4096, 512),
            200.0,
            false,
            false,
        );
        assert!(
            (facing[0].angle.abs() - 180.0).abs() < 0.5,
            "got {:?}",
            facing[0]
        );

        let upright = line_spots(
            &pixel_lines(feature, &decoded, 4096, 512),
            200.0,
            true,
            false,
        );
        assert!(upright[0].angle.abs() < 0.5, "got {:?}", upright[0]);
    }

    /// A line shorter than one spacing still carries its marking -- that is
    /// what the half-spacing start is for.
    #[test]
    fn a_short_line_still_gets_one_symbol() {
        let bytes = encode_tile(&[LayerSpec {
            name: "roads".into(),
            extent: 4096,
            features: vec![FeatureSpec {
                geom: Geom::Line,
                points: vec![(2000, 2048), (2400, 2048)],
                props: vec![],
            }],
        }]);
        let tile = std::sync::Arc::new(crate::mvt::decode(&bytes).expect("decodes"));
        let decoded = crate::raster::DecodedTile {
            source_id: "src".into(),
            tile,
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,

            labels_only: false,
        };
        let feature = &decoded.tile.layers[0].features[0];

        // The line is 50 pixels long and the spacing is 40.
        let spots = line_spots(
            &pixel_lines(feature, &decoded, 4096, 512),
            40.0,
            false,
            false,
        );
        assert_eq!(spots.len(), 1);
    }

    /// A pin and its name are one thing. Keeping the name after the pin lost
    /// its place leaves a label floating over nothing.
    #[test]
    fn an_icon_and_its_text_are_placed_as_one() {
        use crate::sprite::SpriteSheet;

        let mut sheet_image = Pixmap::new(8, 8).expect("pixmap");
        for slot in sheet_image.pixels_mut().iter_mut() {
            *slot = tiny_skia::ColorU8::from_rgba(0, 0, 0, 255).premultiply();
        }
        let sheet = SpriteSheet::parse(
            r#"{"pin": {"x": 0, "y": 0, "width": 8, "height": 8, "pixelRatio": 1}}"#,
            &sheet_image.encode_png().expect("encode"),
        )
        .expect("sheet");

        let style = Style::parse(
            r##"{
                "version": 8,
                "sources": { "src": { "type": "vector" } },
                "layers": [{
                    "id": "pois", "type": "symbol", "source": "src", "source-layer": "poi",
                    "layout": {
                        "icon-image": "pin", "text-field": "{name}",
                        "text-font": ["F"], "text-size": 10
                    }
                }]
            }"##,
        )
        .expect("style parses");

        // Two symbols close enough to collide: the second must lose both
        // halves, not keep its name after its pin was dropped.
        let bytes = encode_tile(&[LayerSpec {
            name: "poi".into(),
            extent: 4096,
            features: vec![
                FeatureSpec {
                    geom: Geom::Point,
                    points: vec![(2048, 2048)],
                    props: vec![("name".into(), "A".into())],
                },
                FeatureSpec {
                    geom: Geom::Point,
                    points: vec![(2060, 2060)],
                    props: vec![("name".into(), "A".into())],
                },
            ],
        }]);
        let decoded = vec![crate::raster::DecodedTile {
            source_id: "src".into(),
            tile: std::sync::Arc::new(crate::mvt::decode(&bytes).expect("fixture decodes")),
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,

            labels_only: false,
        }];

        let labels = collect(
            &style,
            Target {
                zoom: 12.0,
                tile_size: 512,
                paint_scale: 1.0,
                xy: (0, 0),
            },
            &decoded,
            &store(&[0x41]),
            Some(&sheet),
        );
        assert_eq!(labels.len(), 2, "both were collected");
        assert!(labels[0].icon.is_some(), "the icon travels with the label");
        // The box has to cover the icon, not just the text, or two pins would
        // be allowed to sit on top of each other.
        assert!(
            labels[0].bounds[2] - labels[0].bounds[0] >= 8.0,
            "expected the icon in the collision box, got {:?}",
            labels[0].bounds
        );

        let placed = place(labels);
        assert_eq!(
            placed.len(),
            1,
            "the second symbol collided and was dropped"
        );
    }

    fn label(x: f32, y: f32, w: f32, h: f32, allow_overlap: bool) -> Label {
        Label {
            shaped: Shaped::default(),
            fonts: vec!["F".into()],
            paint: TextPaint {
                size: 16.0,
                color: Color::BLACK,
                halo_color: Color::TRANSPARENT,
                halo_width: 0.0,
                halo_blur: 0.0,
                opacity: 1.0,
            },
            anchor: Anchor::Center,
            at: (x, y),
            icon: None,
            curve: None,
            bounds: [x, y, x + w, y + h],
            text_bounds: Some([x, y, x + w, y + h]),
            icon_bounds: None,
            icon_optional: false,
            text_optional: false,
            content: String::new(),
            world_at: (x as i64, y as i64),
            layer_index: 0,
            sort_key: 0.0,
            padding: 0.0,
            allow_overlap,
        }
    }

    #[test]
    fn drops_a_label_that_covers_one_already_kept() {
        let kept = place(vec![
            label(0.0, 0.0, 50.0, 20.0, false),
            label(10.0, 5.0, 50.0, 20.0, false),
            label(200.0, 200.0, 50.0, 20.0, false),
        ]);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].at, (0.0, 0.0));
        assert_eq!(kept[1].at, (200.0, 200.0));
    }

    /// `text-allow-overlap` is how a style says a label matters more than
    /// tidiness — a one-way arrow, a shield.
    #[test]
    fn keeps_labels_that_allow_overlap() {
        let kept = place(vec![
            label(0.0, 0.0, 50.0, 20.0, false),
            label(0.0, 0.0, 50.0, 20.0, true),
        ]);
        assert_eq!(kept.len(), 2);
    }

    /// Padding is the label's own now, because a line of arrows and a pair of
    /// place names want different amounts of room.
    #[test]
    fn padding_separates_labels_that_merely_touch() {
        let touching = vec![
            label(0.0, 0.0, 50.0, 20.0, false),
            label(50.0, 0.0, 50.0, 20.0, false),
        ];
        assert_eq!(place(touching).len(), 2, "no padding, no collision");

        let padded = |x: f32| {
            let mut l = label(x, 0.0, 50.0, 20.0, false);
            l.padding = 2.0;
            l
        };
        assert_eq!(
            place(vec![padded(0.0), padded(50.0)]).len(),
            1,
            "padding pushes them apart"
        );
    }

    /// Every basemap style still writes `text-field: "{name}"`. Evaluating
    /// that as an expression yields the literal string, and the map comes out
    /// labelled `{name}` — or, if the token is dropped, with no labels and no
    /// error.
    #[test]
    fn expands_the_legacy_token_form() {
        let tile = point_tile("place", "中央区", 10, 10);
        let layer = tile.layer("place").expect("layer");
        let ctx = Ctx {
            zoom: 12.0,
            layer: Some(layer),
            feature: Some(&layer.features[0]),
        };
        assert_eq!(expand_tokens("{name}", &ctx), "中央区");
        assert_eq!(expand_tokens("[{name}]", &ctx), "[中央区]");
        assert_eq!(expand_tokens("no tokens", &ctx), "no tokens");
        assert_eq!(expand_tokens("{missing}", &ctx), "");
        assert_eq!(expand_tokens("{unclosed", &ctx), "{unclosed");
    }

    #[test]
    fn shapes_and_places_a_point_label() {
        let store = store(&[65, 66]);
        let style = Style::parse(
            r##"{
                "version": 8,
                "sources": { "src": { "type": "vector" } },
                "layers": [{
                    "id": "labels", "type": "symbol", "source": "src", "source-layer": "place",
                    "layout": { "text-field": "{name}", "text-font": ["F"], "text-size": 16 },
                    "paint": { "text-color": "#000000" }
                }]
            }"##,
        )
        .expect("style parses");

        let decoded = vec![crate::raster::DecodedTile {
            source_id: "src".into(),
            tile: point_tile("place", "AB", 2048, 2048),
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,

            labels_only: false,
        }];

        let labels = collect(
            &style,
            Target {
                zoom: 12.0,
                tile_size: 512,
                paint_scale: 1.0,
                xy: (0, 0),
            },
            &decoded,
            &store,
            None,
        );
        assert_eq!(labels.len(), 1);
        // The point sits at half the 4096 extent, so the middle of the tile.
        assert!((labels[0].at.0 - 256.0).abs() < 1.0);
        assert!((labels[0].at.1 - 256.0).abs() < 1.0);
        assert_eq!(labels[0].shaped.glyphs.len(), 2);
    }

    /// A style with no glyphs loaded must not silently place empty boxes that
    /// then block real labels.
    #[test]
    fn places_nothing_without_glyphs() {
        let style = Style::parse(
            r##"{
                "version": 8,
                "sources": { "src": { "type": "vector" } },
                "layers": [{
                    "id": "labels", "type": "symbol", "source": "src", "source-layer": "place",
                    "layout": { "text-field": "{name}", "text-font": ["F"] }
                }]
            }"##,
        )
        .unwrap();
        let labels = collect(
            &style,
            Target {
                zoom: 12.0,
                tile_size: 512,
                paint_scale: 1.0,
                xy: (0, 0),
            },
            &[],
            &GlyphStore::new(),
            None,
        );
        assert!(labels.is_empty());
    }
}
