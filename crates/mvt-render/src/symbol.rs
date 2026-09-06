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
use crate::text::{self, Anchor, GlyphStore, Shaped, TextPaint};

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
    /// Axis-aligned box the label will cover, for collision. Covers both the
    /// text and the icon when there are both.
    pub bounds: [f32; 4],
    pub allow_overlap: bool,
}

/// Symbol features this renderer cannot place, by style layer id.
pub fn unsupported(style: &Style, zoom: f64) -> Vec<String> {
    let mut reasons = Vec::new();
    for layer in &style.layers {
        if layer.kind != LayerKind::Symbol || !layer.is_visible() || !layer.applies_at(zoom) {
            continue;
        }
        let ctx = Ctx::zoom_only(zoom);
        if let Some(placement) = layer.layout_string("symbol-placement", &ctx) {
            if placement != "point" {
                reasons.push(format!("{}: symbol-placement {}", layer.id, placement));
            }
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
        let (Some(source_id), Some(source_layer_name)) =
            (layer.source.as_deref(), layer.source_layer.as_deref())
        else {
            continue;
        };
        let Some(decoded_tile) = decoded.iter().find(|d| d.source_id == source_id) else {
            continue;
        };
        let Some(data_layer) = decoded_tile.tile.layer(source_layer_name) else {
            continue;
        };
        let filter = CompiledFilter::compile(layer.filter.as_ref(), data_layer, zoom);
        if filter.never_matches() {
            continue;
        }

        for feature in &data_layer.features {
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
    wanted
        .into_iter()
        .map(|(fontstack, range)| crate::GlyphRequest {
            fontstack,
            range_start: range * 256,
        })
        .collect()
}

/// Shapes every label the style asks for in this tile, in layer order.
pub fn collect(
    style: &Style,
    zoom: f64,
    tile_size: u32,
    paint_scale: f32,
    decoded: &[crate::raster::DecodedTile],
    store: &GlyphStore,
    sprites: Option<&SpriteSheet>,
) -> Vec<Label> {
    let mut labels = Vec::new();
    // Nothing to draw labels *with*. A style whose symbols are all icons can
    // still draw them, which is why this is not a test of the glyph store
    // alone.
    if store.is_empty() && sprites.map_or(true, SpriteSheet::is_empty) {
        return labels;
    }

    for layer in &style.layers {
        if layer.kind != LayerKind::Symbol || !layer.is_visible() || !layer.applies_at(zoom) {
            continue;
        }
        let zoom_ctx = Ctx::zoom_only(zoom);
        // Only point placement for now; a line label needs the geometry walked
        // and the glyphs rotated per segment.
        if layer
            .layout_string("symbol-placement", &zoom_ctx)
            .map(|p| p != "point")
            .unwrap_or(false)
        {
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

        let filter = CompiledFilter::compile(layer.filter.as_ref(), data_layer, zoom);
        if filter.never_matches() {
            continue;
        }

        let fonts = font_stack(layer, &zoom_ctx);
        for feature in &data_layer.features {
            if !filter.matches(data_layer, feature) {
                continue;
            }
            let ctx = Ctx {
                zoom,
                layer: Some(data_layer),
                feature: Some(feature),
            };
            let Some(anchor_point) =
                anchor_point(feature, decoded_tile, data_layer.extent, tile_size)
            else {
                continue;
            };

            let icon = icon_placement(layer, &ctx, sprites, paint_scale, anchor_point);

            let content = text_field(layer, &ctx)
                .map(|raw| transform_case(&raw, layer.layout_string("text-transform", &ctx)))
                .filter(|content| !content.trim().is_empty());

            let size = layer.layout_number("text-size", &ctx, 16.0) as f32 * paint_scale;
            let shaped = match &content {
                Some(content) if size > 0.0 => {
                    let max_width = layer.layout_number("text-max-width", &ctx, 10.0) as f32;
                    text::shape(content, &fonts, store, max_width)
                }
                _ => Shaped::default(),
            };
            // A symbol with neither its glyphs nor its icon yet is not a
            // label; it would reserve space for nothing and push its
            // neighbours out of the way.
            if shaped.is_empty() && icon.is_none() {
                continue;
            }

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
                halo_width: layer.paint_number("text-halo-width", &ctx, 0.0) as f32 * paint_scale,
                opacity,
            };
            if opacity <= 0.0 && icon.is_none() {
                continue;
            }

            let text_bounds = if shaped.is_empty() {
                None
            } else {
                Some(text::bounds(&shaped, &paint, anchor, at))
            };
            let icon_bounds = icon.as_ref().and_then(|placement| {
                sprites
                    .and_then(|sheet| sheet.icon(&placement.name))
                    .map(|icon| {
                        sprite::bounds(icon, &placement.paint, placement.anchor, placement.at)
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

            labels.push(Label {
                shaped,
                fonts: fonts.clone(),
                paint,
                anchor,
                at,
                icon,
                bounds,
                allow_overlap,
            });
        }
    }
    labels
}

/// Drops labels that would cover one already kept.
///
/// First come, first served, in the order [`collect`] produced — which is
/// style layer order, so a style that wants cities to win over villages says
/// so by putting them first. Two labels touching at a corner is still a
/// collision: the boxes include the halo's bleed, and text that nearly
/// overlaps reads worse than text that is missing.
pub fn place(labels: Vec<Label>, padding: f32) -> Vec<Label> {
    let mut kept: Vec<Label> = Vec::new();
    for label in labels {
        if label.allow_overlap {
            kept.push(label);
            continue;
        }
        let b = grow(label.bounds, padding);
        if kept
            .iter()
            .any(|other| !other.allow_overlap && overlaps(b, grow(other.bounds, padding)))
        {
            continue;
        }
        kept.push(label);
    }
    kept
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
    anchor_point: (f32, f32),
) -> Option<IconPlacement> {
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
    let opacity = layer.paint_number("icon-opacity", ctx, 1.0) as f32;
    if opacity <= 0.0 {
        return None;
    }

    // `icon-offset` is in the icon's own pixels, so it grows with icon-size
    // and with the tile's scale, exactly as the icon does.
    let (ox, oy) = offset_of(layer, "icon-offset");
    let scale = size * paint_scale;
    Some(IconPlacement {
        name,
        paint: IconPaint {
            scale,
            opacity,
            color: layer.paint_color("icon-color", ctx, Color::BLACK),
            halo_color: layer.paint_color("icon-halo-color", ctx, Color::TRANSPARENT),
            halo_width: layer.paint_number("icon-halo-width", ctx, 0.0) as f32,
        },
        anchor: layer
            .layout_string("icon-anchor", ctx)
            .map(|a| Anchor::from_str(&a))
            .unwrap_or(Anchor::Center),
        at: (anchor_point.0 + ox * scale, anchor_point.1 + oy * scale),
    })
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
    let k = size * decoded.scale / extent as f32;
    let px = x * k - decoded.offset_x * decoded.scale * size;
    let py = y * k - decoded.offset_y * decoded.scale * size;
    // A label whose anchor is well outside the tile belongs to a neighbour.
    let margin = size * 0.25;
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
    fn point_tile(layer: &str, name: &str, x: i32, y: i32) -> model::Tile {
        let bytes = encode_tile(&[LayerSpec {
            name: layer.into(),
            extent: 4096,
            features: vec![FeatureSpec {
                geom: Geom::Point,
                points: vec![(x, y)],
                props: vec![("name".into(), name.into())],
            }],
        }]);
        crate::mvt::decode(&bytes).expect("fixture decodes")
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
            tile: crate::mvt::decode(&bytes).expect("fixture decodes"),
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }];

        let labels = collect(&style, 12.0, 512, 1.0, &decoded, &store(&[0x41]), Some(&sheet));
        assert_eq!(labels.len(), 2, "both were collected");
        assert!(labels[0].icon.is_some(), "the icon travels with the label");
        // The box has to cover the icon, not just the text, or two pins would
        // be allowed to sit on top of each other.
        assert!(
            labels[0].bounds[2] - labels[0].bounds[0] >= 8.0,
            "expected the icon in the collision box, got {:?}",
            labels[0].bounds
        );

        let placed = place(labels, 2.0);
        assert_eq!(placed.len(), 1, "the second symbol collided and was dropped");
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
                opacity: 1.0,
            },
            anchor: Anchor::Center,
            at: (x, y),
            icon: None,
            bounds: [x, y, x + w, y + h],
            allow_overlap,
        }
    }

    #[test]
    fn drops_a_label_that_covers_one_already_kept() {
        let kept = place(
            vec![
                label(0.0, 0.0, 50.0, 20.0, false),
                label(10.0, 5.0, 50.0, 20.0, false),
                label(200.0, 200.0, 50.0, 20.0, false),
            ],
            0.0,
        );
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].at, (0.0, 0.0));
        assert_eq!(kept[1].at, (200.0, 200.0));
    }

    /// `text-allow-overlap` is how a style says a label matters more than
    /// tidiness — a one-way arrow, a shield.
    #[test]
    fn keeps_labels_that_allow_overlap() {
        let kept = place(
            vec![
                label(0.0, 0.0, 50.0, 20.0, false),
                label(0.0, 0.0, 50.0, 20.0, true),
            ],
            0.0,
        );
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn padding_separates_labels_that_merely_touch() {
        let touching = vec![
            label(0.0, 0.0, 50.0, 20.0, false),
            label(50.0, 0.0, 50.0, 20.0, false),
        ];
        assert_eq!(place(touching, 0.0).len(), 2, "no padding, no collision");

        let touching = vec![
            label(0.0, 0.0, 50.0, 20.0, false),
            label(50.0, 0.0, 50.0, 20.0, false),
        ];
        assert_eq!(place(touching, 2.0).len(), 1, "padding pushes them apart");
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
        }];

        let labels = collect(&style, 12.0, 512, 1.0, &decoded, &store, None);
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
        let labels = collect(&style, 12.0, 512, 1.0, &[], &GlyphStore::new(), None);
        assert!(labels.is_empty());
    }
}
