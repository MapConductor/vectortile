//! The sprite sheet a style points at, and drawing icons out of it.
//!
//! A style carries a `sprite` URL, and the host fetches two files from it: a
//! `.json` index and a `.png` sheet. The index names every icon and where it
//! sits in the sheet, so one image and one round trip cover a whole style's
//! shields, POI pins and arrows.
//!
//! ```json
//! { "airport": { "x": 0, "y": 0, "width": 17, "height": 17, "pixelRatio": 1, "sdf": false } }
//! ```
//!
//! `pixelRatio` is what the icon was drawn for, not what it must be drawn at:
//! a `pixelRatio: 2` icon 34 pixels wide occupies 17 on screen. `sdf` marks an
//! icon stored as a distance field rather than as colour, which is how a style
//! recolours one shield per road class instead of shipping twenty images.

use std::collections::HashMap;

use serde_json::Value as Json;
use tiny_skia::Pixmap;

use crate::color::Color;
use crate::text::{blend, sample_alpha, smoothstep, Anchor};

/// Distance value that marks the outline of an SDF icon, matching the glyph
/// convention MapLibre uses for both.
const SDF_EDGE: f32 = 192.0;

/// Distance units per pixel in an SDF icon.
const SDF_RANGE: f32 = 8.0;

#[derive(Debug)]
pub struct SpriteError(pub &'static str);

impl std::fmt::Display for SpriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for SpriteError {}

/// One icon's place in the sheet.
#[derive(Debug, Clone)]
pub struct Icon {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// What the icon was drawn for. Its size on screen is `width / pixel_ratio`.
    pub pixel_ratio: f32,
    /// Whether the alpha channel is a distance field rather than coverage.
    pub sdf: bool,
    /// Where text goes inside the icon, in icon pixels: `[x1, y1, x2, y2]`.
    /// A shield says its border is here and its number goes there.
    pub content: Option<[f32; 4]>,
    /// Ranges along each axis that may be stretched, in icon pixels. The rest
    /// keeps its size, which is what stops a shield's rounded corners from
    /// smearing when the number inside it gets longer.
    pub stretch_x: Vec<[f32; 2]>,
    pub stretch_y: Vec<[f32; 2]>,
}

impl Icon {
    /// Size on screen before `icon-size`, in pixels.
    pub fn display_size(&self) -> (f32, f32) {
        let ratio = if self.pixel_ratio > 0.0 {
            self.pixel_ratio
        } else {
            1.0
        };
        (self.width as f32 / ratio, self.height as f32 / ratio)
    }
}

pub struct SpriteSheet {
    image: Pixmap,
    icons: HashMap<String, Icon>,
    /// The style's id for this sheet, when it listed one.
    id: Option<String>,
}

impl SpriteSheet {
    /// Reads an index and its sheet.
    ///
    /// An icon whose rectangle leaves the sheet is rejected rather than
    /// clamped: it would draw whatever happens to be next to it, which looks
    /// like a rendering bug rather than a broken sprite.
    pub fn parse(json: &str, png: &[u8]) -> Result<SpriteSheet, SpriteError> {
        let image =
            Pixmap::decode_png(png).map_err(|_| SpriteError("sprite sheet is not a PNG"))?;
        let root: Json =
            serde_json::from_str(json).map_err(|_| SpriteError("sprite index is not JSON"))?;
        let entries = root
            .as_object()
            .ok_or(SpriteError("sprite index is not an object"))?;

        let mut icons = HashMap::new();
        for (name, value) in entries {
            let Some(entry) = value.as_object() else {
                continue;
            };
            let number = |key: &str| entry.get(key).and_then(Json::as_f64);
            let (Some(x), Some(y), Some(width), Some(height)) = (
                number("x"),
                number("y"),
                number("width"),
                number("height"),
            ) else {
                continue;
            };
            if width <= 0.0 || height <= 0.0 || x < 0.0 || y < 0.0 {
                continue;
            }
            let icon = Icon {
                x: x as u32,
                y: y as u32,
                width: width as u32,
                height: height as u32,
                pixel_ratio: number("pixelRatio").unwrap_or(1.0) as f32,
                sdf: entry
                    .get("sdf")
                    .and_then(Json::as_bool)
                    .unwrap_or(false),
                content: entry.get("content").and_then(read_box),
                stretch_x: read_bands(entry.get("stretchX")),
                stretch_y: read_bands(entry.get("stretchY")),
            };
            if icon.x + icon.width > image.width() || icon.y + icon.height > image.height() {
                return Err(SpriteError("an icon's rectangle leaves the sheet"));
            }
            icons.insert(name.clone(), icon);
        }

        Ok(SpriteSheet {
            image,
            icons,
            id: None,
        })
    }

    /// Names the sheet, so `id:name` references in the style resolve.
    pub fn set_id(&mut self, id: Option<String>) {
        self.id = id;
    }

    /// Looks an icon up, allowing for the sheet's own prefix.
    ///
    /// A style that lists its sheets as `[{id, url}]` writes every icon as
    /// `id:name` while the sheet indexes them bare, so `basics:icon-atm` and
    /// `icon-atm` have to find the same icon.
    pub fn icon(&self, name: &str) -> Option<&Icon> {
        if let Some(icon) = self.icons.get(name) {
            return Some(icon);
        }
        let (prefix, bare) = name.split_once(':')?;
        if self.id.as_deref() == Some(prefix) {
            self.icons.get(bare)
        } else {
            None
        }
    }

    pub fn is_empty(&self) -> bool {
        self.icons.is_empty()
    }

    pub fn len(&self) -> usize {
        self.icons.len()
    }
}

/// Reads `[x1, y1, x2, y2]`, ignoring anything that is not four numbers.
fn read_box(value: &Json) -> Option<[f32; 4]> {
    let items = value.as_array()?;
    if items.len() != 4 {
        return None;
    }
    let mut out = [0.0f32; 4];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item.as_f64()? as f32;
    }
    Some(out)
}

/// Reads `[[start, end], ...]`, dropping ranges that are empty or backwards.
fn read_bands(value: Option<&Json>) -> Vec<[f32; 2]> {
    let Some(items) = value.and_then(Json::as_array) else {
        return Vec::new();
    };
    let mut bands: Vec<[f32; 2]> = items
        .iter()
        .filter_map(|item| {
            let pair = item.as_array()?;
            let start = pair.first()?.as_f64()? as f32;
            let end = pair.get(1)?.as_f64()? as f32;
            (end > start).then_some([start, end])
        })
        .collect();
    bands.sort_by(|a, b| a[0].total_cmp(&b[0]));
    bands
}

/// How an icon is painted, with `icon-size` already folded into [`scale`].
#[derive(Debug, Clone)]
pub struct IconPaint {
    /// `icon-size` times the render's paint scale.
    pub scale: f32,
    pub opacity: f32,
    /// Only used by an SDF icon; a colour one carries its own.
    pub color: Color,
    pub halo_color: Color,
    pub halo_width: f32,
    /// Clockwise, in degrees. `icon-rotate` plus, for a map-aligned symbol on
    /// a line, the direction of the line under it -- which is what turns one
    /// arrow in a sheet into every one-way marking on the map.
    pub rotation: f32,
    /// Size to draw at, in target pixels, when `icon-text-fit` has resized the
    /// icon around its label. The icon's stretchable ranges absorb the
    /// difference; everything else keeps its own size.
    pub stretch_to: Option<(f32, f32)>,
}

impl Default for IconPaint {
    fn default() -> Self {
        IconPaint {
            scale: 1.0,
            opacity: 1.0,
            color: Color::BLACK,
            halo_color: Color::TRANSPARENT,
            halo_width: 0.0,
            rotation: 0.0,
            stretch_to: None,
        }
    }
}

/// One run of the icon along an axis: a stretch of source pixels and the room
/// they are drawn into.
#[derive(Debug, Clone, Copy)]
struct Piece {
    src: f32,
    src_len: f32,
    dst: f32,
    dst_len: f32,
}

/// Splits an axis into the parts that keep their size and the parts that
/// absorb the difference.
///
/// With no stretchable ranges, or with nothing to absorb, this is one piece
/// and the mapping is the plain scaling every unstretched icon gets.
fn axis_pieces(natural: f32, bands: &[[f32; 2]], target: f32) -> Vec<Piece> {
    let stretchable: f32 = bands
        .iter()
        .map(|b| (b[1].min(natural) - b[0].max(0.0)).max(0.0))
        .sum();
    let extra = target - natural;
    if bands.is_empty() || stretchable <= 0.0 || extra <= 0.0 {
        return vec![Piece {
            src: 0.0,
            src_len: natural,
            dst: 0.0,
            dst_len: target,
        }];
    }

    let mut pieces = Vec::with_capacity(bands.len() * 2 + 1);
    let mut src = 0.0f32;
    let mut dst = 0.0f32;
    let mut push = |src_from: f32, src_to: f32, dst_len: f32, src: &mut f32, dst: &mut f32| {
        if src_to <= src_from {
            return;
        }
        pieces.push(Piece {
            src: src_from,
            src_len: src_to - src_from,
            dst: *dst,
            dst_len,
        });
        *src = src_to;
        *dst += dst_len;
    };

    for band in bands {
        let start = band[0].clamp(0.0, natural);
        let end = band[1].clamp(0.0, natural);
        if end <= start {
            continue;
        }
        // Everything before the band keeps its size.
        push(src, start, start - src, &mut src, &mut dst);
        let share = (end - start) / stretchable;
        push(src, end, (end - start) + extra * share, &mut src, &mut dst);
    }
    push(src, natural, natural - src, &mut src, &mut dst);
    pieces
}

/// Turns a drawn coordinate back into the icon's own.
fn map_axis(pieces: &[Piece], d: f32) -> f32 {
    for piece in pieces {
        if d < piece.dst + piece.dst_len {
            if piece.dst_len <= 0.0 {
                return piece.src;
            }
            let t = ((d - piece.dst) / piece.dst_len).clamp(0.0, 1.0);
            return piece.src + t * piece.src_len;
        }
    }
    pieces
        .last()
        .map(|p| p.src + p.src_len)
        .unwrap_or(0.0)
}

/// Target pixels per icon pixel, before any stretching.
///
/// The parts of a stretched icon that keep their size keep it at this rate;
/// only the stretchable ranges depart from it.
fn unit(icon: &Icon, paint: &IconPaint) -> f32 {
    let ratio = if icon.pixel_ratio > 0.0 {
        icon.pixel_ratio
    } else {
        1.0
    };
    (paint.scale / ratio).max(f32::MIN_POSITIVE)
}

/// The size an icon is actually drawn at, in target pixels.
fn drawn_size(icon: &Icon, paint: &IconPaint) -> (f32, f32) {
    if let Some((w, h)) = paint.stretch_to {
        return (w.max(0.0), h.max(0.0));
    }
    let (w, h) = icon.display_size();
    (w * paint.scale, h * paint.scale)
}

/// The box an icon will cover, for collision.
///
/// A rotated icon is measured by the box around its corners rather than by its
/// own width and height: a 45-degree arrow reserves more room than it does
/// lying flat, and collision has no way to say anything but a rectangle.
pub fn bounds(icon: &Icon, paint: &IconPaint, anchor: Anchor, at: (f32, f32)) -> [f32; 4] {
    let (w, h) = drawn_size(icon, paint);
    let (left, top) = origin(w, h, anchor, at);
    if paint.rotation == 0.0 {
        return [left, top, left + w, top + h];
    }

    let (cx, cy) = (left + w / 2.0, top + h / 2.0);
    let (sin, cos) = paint.rotation.to_radians().sin_cos();
    let mut min = (f32::MAX, f32::MAX);
    let mut max = (f32::MIN, f32::MIN);
    for (x, y) in [
        (left, top),
        (left + w, top),
        (left, top + h),
        (left + w, top + h),
    ] {
        let (dx, dy) = (x - cx, y - cy);
        let rx = cx + dx * cos - dy * sin;
        let ry = cy + dx * sin + dy * cos;
        min = (min.0.min(rx), min.1.min(ry));
        max = (max.0.max(rx), max.1.max(ry));
    }
    [min.0, min.1, max.0, max.1]
}

/// Where the icon's top-left corner lands once the anchor is applied.
fn origin(w: f32, h: f32, anchor: Anchor, at: (f32, f32)) -> (f32, f32) {
    let (ax, ay) = anchor.offset();
    (at.0 + w * ax, at.1 + h * ay)
}

pub fn draw(
    pixmap: &mut Pixmap,
    sheet: &SpriteSheet,
    icon: &Icon,
    paint: &IconPaint,
    anchor: Anchor,
    at: (f32, f32),
) {
    if paint.opacity <= 0.0 || paint.scale <= 0.0 {
        return;
    }
    let (w, h) = drawn_size(icon, paint);
    if w < 0.5 || h < 0.5 {
        return;
    }
    let (left, top) = origin(w, h, anchor, at);

    // The area to walk is the rotated box; each pixel in it is turned back to
    // ask which part of the icon, if any, lands there.
    let covered = bounds(icon, paint, anchor, at);
    let x0 = covered[0].floor().max(0.0) as i32;
    let y0 = covered[1].floor().max(0.0) as i32;
    let x1 = covered[2].ceil().min(pixmap.width() as f32) as i32;
    let y1 = covered[3].ceil().min(pixmap.height() as f32) as i32;

    let sheet_width = sheet.image.width() as i32;
    let data = sheet.image.data().to_vec();
    let (cx, cy) = (left + w / 2.0, top + h / 2.0);
    let (sin, cos) = (-paint.rotation).to_radians().sin_cos();
    // Built once per icon rather than per pixel: the same two maps answer
    // every sample, and for an unstretched icon they are one piece each.
    let k = unit(icon, paint);
    let across = axis_pieces(icon.width as f32, &icon.stretch_x, w / k);
    let down = axis_pieces(icon.height as f32, &icon.stretch_y, h / k);

    for py in y0..y1 {
        for px in x0..x1 {
            // Centre of the destination pixel, turned back into the icon's
            // unrotated frame and then into the icon's own pixels.
            let (dx, dy) = (px as f32 + 0.5 - cx, py as f32 + 0.5 - cy);
            let (fx, fy) = if paint.rotation == 0.0 {
                (px as f32 + 0.5, py as f32 + 0.5)
            } else {
                (cx + dx * cos - dy * sin, cy + dx * sin + dy * cos)
            };
            let u = map_axis(&across, (fx - left) / k);
            let v = map_axis(&down, (fy - top) / k);
            if u < 0.0 || v < 0.0 || u >= icon.width as f32 || v >= icon.height as f32 {
                continue;
            }

            if icon.sdf {
                // Sampled from the alpha channel, which holds the distance --
                // the same arrangement as a glyph, so it is thresholded the
                // same way and takes its colour from the style.
                let distance = sample_alpha(
                    &data,
                    sheet_width,
                    sheet.image.height() as i32,
                    icon.x as f32 + u,
                    icon.y as f32 + v,
                );
                // One screen pixel is this many distance units.
                let per_pixel = SDF_RANGE / (paint.scale / icon.pixel_ratio.max(0.001)).max(0.001);
                if paint.halo_width > 0.0 && paint.halo_color.a > 0.0 {
                    let halo_edge = SDF_EDGE - paint.halo_width * SDF_RANGE;
                    let alpha = smoothstep(halo_edge - per_pixel, halo_edge + per_pixel, distance)
                        * paint.halo_color.a
                        * paint.opacity;
                    if alpha > 0.0 {
                        blend(pixmap, px, py, paint.halo_color, alpha);
                    }
                }
                let alpha = smoothstep(SDF_EDGE - per_pixel, SDF_EDGE + per_pixel, distance)
                    * paint.color.a
                    * paint.opacity;
                if alpha > 0.0 {
                    blend(pixmap, px, py, paint.color, alpha);
                }
                continue;
            }

            // A colour icon carries its own pixels; nearest neighbour, because
            // an icon drawn at its own size must come out exactly as drawn and
            // interpolation would soften every edge in the sheet.
            let sx = (icon.x as f32 + u) as i32;
            let sy = (icon.y as f32 + v) as i32;
            let index = (sy * sheet_width + sx) as usize * 4;
            if index + 3 >= data.len() {
                continue;
            }
            // tiny-skia stores premultiplied pixels; undo it so the blend
            // below, which multiplies by alpha itself, does not do it twice.
            let a = data[index + 3] as f32 / 255.0;
            if a <= 0.0 {
                continue;
            }
            let color = Color {
                r: data[index] as f32 / 255.0 / a,
                g: data[index + 1] as f32 / 255.0 / a,
                b: data[index + 2] as f32 / 255.0 / a,
                a: 1.0,
            };
            blend(pixmap, px, py, color, a * paint.opacity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_skia::{ColorU8, PremultipliedColorU8};

    /// Builds a sheet with a red square at (0,0) and a green one at (4,0).
    fn sheet_png() -> Vec<u8> {
        let mut pixmap = Pixmap::new(8, 4).expect("pixmap");
        for y in 0..4 {
            for x in 0..8 {
                let color = if x < 4 {
                    ColorU8::from_rgba(255, 0, 0, 255)
                } else {
                    ColorU8::from_rgba(0, 255, 0, 255)
                };
                pixmap.pixels_mut()[(y * 8 + x) as usize] = color.premultiply();
            }
        }
        pixmap.encode_png().expect("encode")
    }

    fn index() -> &'static str {
        r#"{
            "red":   {"x": 0, "y": 0, "width": 4, "height": 4, "pixelRatio": 1},
            "green": {"x": 4, "y": 0, "width": 4, "height": 4, "pixelRatio": 2, "sdf": true}
        }"#
    }

    #[test]
    fn reads_an_index_and_its_sheet() {
        let sheet = SpriteSheet::parse(index(), &sheet_png()).expect("parses");
        assert_eq!(sheet.len(), 2);

        let red = sheet.icon("red").expect("red");
        assert_eq!((red.x, red.y, red.width, red.height), (0, 0, 4, 4));
        assert!(!red.sdf);
        assert_eq!(red.display_size(), (4.0, 4.0));

        let green = sheet.icon("green").expect("green");
        assert!(green.sdf);
        // pixelRatio 2 means it occupies half as much room as it stores.
        assert_eq!(green.display_size(), (2.0, 2.0));
        assert!(sheet.icon("missing").is_none());
    }

    /// An icon that runs off the sheet would draw whatever is next to it,
    /// which reads as a rendering bug rather than a broken sprite file.
    /// A style that lists its sheets writes `basics:icon-atm` where the sheet
    /// itself indexes `icon-atm`. Miss that and every icon in such a style --
    /// which is every recent one -- silently fails to resolve.
    #[test]
    fn resolves_names_that_carry_the_sheets_own_id() {
        let mut sheet = SpriteSheet::parse(index(), &sheet_png()).expect("parses");
        sheet.set_id(Some("basics".into()));

        assert!(sheet.icon("basics:red").is_some(), "prefixed by its own id");
        assert!(sheet.icon("red").is_some(), "still findable unprefixed");
        assert!(
            sheet.icon("other:red").is_none(),
            "another sheet's icon is not this sheet's"
        );
    }

    /// Without an id there is nothing to strip, and a prefixed name is a
    /// reference to a sheet this renderer was never given.
    #[test]
    fn leaves_prefixed_names_alone_when_the_sheet_is_unnamed() {
        let sheet = SpriteSheet::parse(index(), &sheet_png()).expect("parses");
        assert!(sheet.icon("red").is_some());
        assert!(sheet.icon("basics:red").is_none());
    }

    #[test]
    fn rejects_an_icon_that_leaves_the_sheet() {
        let bad = r#"{"wide": {"x": 6, "y": 0, "width": 4, "height": 4}}"#;
        assert!(SpriteSheet::parse(bad, &sheet_png()).is_err());
    }

    #[test]
    fn skips_entries_that_are_not_icons() {
        let mixed = r#"{
            "ok": {"x": 0, "y": 0, "width": 4, "height": 4},
            "no-size": {"x": 0, "y": 0},
            "not-an-object": 12
        }"#;
        let sheet = SpriteSheet::parse(mixed, &sheet_png()).expect("parses");
        assert_eq!(sheet.len(), 1);
        assert!(sheet.icon("ok").is_some());
    }

    fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> PremultipliedColorU8 {
        pixmap.pixels()[(y * pixmap.width() + x) as usize]
    }

    #[test]
    fn draws_a_colour_icon_where_it_was_anchored() {
        let sheet = SpriteSheet::parse(index(), &sheet_png()).expect("parses");
        let mut target = Pixmap::new(16, 16).expect("pixmap");
        let icon = sheet.icon("red").expect("red");

        draw(
            &mut target,
            &sheet,
            icon,
            &IconPaint::default(),
            Anchor::Center,
            (8.0, 8.0),
        );

        // 4x4 centred on (8,8) covers 6..10 in both axes.
        assert_eq!(pixel(&target, 8, 8).red(), 255);
        assert_eq!(pixel(&target, 8, 8).green(), 0);
        assert_eq!(pixel(&target, 6, 6).alpha(), 255);
        // Just outside stays untouched.
        assert_eq!(pixel(&target, 5, 8).alpha(), 0);
        assert_eq!(pixel(&target, 10, 8).alpha(), 0);
    }

    /// `icon-size` and `pixelRatio` both scale the icon, and a retina icon
    /// drawn at size 1 must not come out twice as big as its neighbours.
    #[test]
    fn honours_icon_size_and_pixel_ratio() {
        let sheet = SpriteSheet::parse(index(), &sheet_png()).expect("parses");
        let red = sheet.icon("red").expect("red");
        let green = sheet.icon("green").expect("green");
        let paint = IconPaint::default();

        assert_eq!(
            bounds(red, &paint, Anchor::Center, (8.0, 8.0)),
            [6.0, 6.0, 10.0, 10.0]
        );
        // Same stored size, pixelRatio 2, so half the box.
        assert_eq!(
            bounds(green, &paint, Anchor::Center, (8.0, 8.0)),
            [7.0, 7.0, 9.0, 9.0]
        );

        let doubled = IconPaint {
            scale: 2.0,
            ..IconPaint::default()
        };
        assert_eq!(
            bounds(red, &doubled, Anchor::Center, (8.0, 8.0)),
            [4.0, 4.0, 12.0, 12.0]
        );
    }

    #[test]
    fn anchors_move_the_box_not_its_size() {
        let sheet = SpriteSheet::parse(index(), &sheet_png()).expect("parses");
        let red = sheet.icon("red").expect("red");
        let paint = IconPaint::default();

        assert_eq!(
            bounds(red, &paint, Anchor::TopLeft, (8.0, 8.0)),
            [8.0, 8.0, 12.0, 12.0]
        );
        assert_eq!(
            bounds(red, &paint, Anchor::Bottom, (8.0, 8.0)),
            [6.0, 4.0, 10.0, 8.0]
        );
    }

    /// An SDF icon has no colour of its own -- the style supplies it, which is
    /// the whole reason a style ships one shield instead of twenty.
    #[test]
    fn draws_an_sdf_icon_in_the_styles_colour() {
        // A field that is solidly "inside" everywhere, so every covered pixel
        // takes the paint colour.
        let mut pixmap = Pixmap::new(4, 4).expect("pixmap");
        for slot in pixmap.pixels_mut().iter_mut() {
            *slot = ColorU8::from_rgba(255, 255, 255, 255).premultiply();
        }
        let png = pixmap.encode_png().expect("encode");
        let sheet = SpriteSheet::parse(
            r#"{"shield": {"x": 0, "y": 0, "width": 4, "height": 4, "sdf": true}}"#,
            &png,
        )
        .expect("parses");

        let mut target = Pixmap::new(8, 8).expect("pixmap");
        draw(
            &mut target,
            &sheet,
            sheet.icon("shield").expect("shield"),
            &IconPaint {
                color: Color {
                    r: 0.0,
                    g: 0.0,
                    b: 1.0,
                    a: 1.0,
                },
                ..IconPaint::default()
            },
            Anchor::Center,
            (4.0, 4.0),
        );

        let p = pixel(&target, 4, 4);
        assert!(p.alpha() > 200, "expected the icon to be drawn, got {p:?}");
        assert!(
            p.blue() > 200 && p.red() < 50,
            "expected the style's blue, got {p:?}"
        );
    }

    /// A one-way arrow is one icon in the sheet turned to follow its road, so
    /// the box it reserves has to be the turned one.
    #[test]
    fn a_rotated_icon_reserves_the_room_it_actually_covers() {
        let mut tall = Pixmap::new(4, 8).expect("pixmap");
        for slot in tall.pixels_mut().iter_mut() {
            *slot = ColorU8::from_rgba(0, 0, 0, 255).premultiply();
        }
        let sheet = SpriteSheet::parse(
            r#"{"arrow": {"x": 0, "y": 0, "width": 4, "height": 8, "pixelRatio": 1}}"#,
            &tall.encode_png().expect("encode"),
        )
        .expect("parses");
        let arrow = sheet.icon("arrow").expect("arrow");

        let flat = bounds(arrow, &IconPaint::default(), Anchor::Center, (16.0, 16.0));
        assert_eq!(flat, [14.0, 12.0, 18.0, 20.0]);

        let turned = bounds(
            arrow,
            &IconPaint {
                rotation: 90.0,
                ..IconPaint::default()
            },
            Anchor::Center,
            (16.0, 16.0),
        );
        // Four wide and eight tall becomes eight wide and four tall.
        assert!((turned[2] - turned[0] - 8.0).abs() < 0.01, "got {turned:?}");
        assert!((turned[3] - turned[1] - 4.0).abs() < 0.01, "got {turned:?}");
    }

    #[test]
    fn draws_a_rotated_icon_along_its_new_axis() {
        let mut tall = Pixmap::new(2, 8).expect("pixmap");
        for slot in tall.pixels_mut().iter_mut() {
            *slot = ColorU8::from_rgba(0, 0, 0, 255).premultiply();
        }
        let sheet = SpriteSheet::parse(
            r#"{"bar": {"x": 0, "y": 0, "width": 2, "height": 8, "pixelRatio": 1}}"#,
            &tall.encode_png().expect("encode"),
        )
        .expect("parses");

        let mut target = Pixmap::new(24, 24).expect("pixmap");
        draw(
            &mut target,
            &sheet,
            sheet.icon("bar").expect("bar"),
            &IconPaint {
                rotation: 90.0,
                ..IconPaint::default()
            },
            Anchor::Center,
            (12.0, 12.0),
        );

        // Turned a quarter, the tall bar lies across the middle: ink to the
        // left and right of centre, none above and below it.
        assert!(pixel(&target, 9, 12).alpha() > 200, "expected ink across");
        assert!(pixel(&target, 15, 12).alpha() > 200, "expected ink across");
        assert_eq!(pixel(&target, 12, 9).alpha(), 0, "expected none above");
        assert_eq!(pixel(&target, 12, 15).alpha(), 0, "expected none below");
    }

    /// A shield stretched around a longer number must grow only in the middle:
    /// stretch its rounded corners and it stops looking like a shield.
    #[test]
    fn stretches_only_where_the_sheet_allows_it() {
        // Six pixels wide, stretchable between 2 and 4.
        let pieces = axis_pieces(6.0, &[[2.0, 4.0]], 10.0);
        assert_eq!(pieces.len(), 3);

        // The fixed ends keep their size; the band absorbs all four.
        assert_eq!((pieces[0].src_len, pieces[0].dst_len), (2.0, 2.0));
        assert_eq!((pieces[1].src_len, pieces[1].dst_len), (2.0, 6.0));
        assert_eq!((pieces[2].src_len, pieces[2].dst_len), (2.0, 2.0));

        // A point in the left edge maps one to one, and one in the last two
        // pixels maps into the right edge rather than the middle.
        assert!((map_axis(&pieces, 1.0) - 1.0).abs() < 0.01);
        assert!((map_axis(&pieces, 9.0) - 5.0).abs() < 0.01);
        // The middle of the drawn icon is the middle of the band.
        assert!((map_axis(&pieces, 5.0) - 3.0).abs() < 0.01);
    }

    /// Two bands share the extra room in proportion to their own size.
    #[test]
    fn shares_the_extra_room_between_bands() {
        let pieces = axis_pieces(10.0, &[[2.0, 4.0], [6.0, 10.0]], 16.0);
        let stretched: Vec<f32> = pieces
            .iter()
            .filter(|p| p.dst_len > p.src_len)
            .map(|p| p.dst_len - p.src_len)
            .collect();
        assert_eq!(stretched.len(), 2);
        // Six extra pixels, split 2:4.
        assert!((stretched[0] - 2.0).abs() < 0.01, "got {stretched:?}");
        assert!((stretched[1] - 4.0).abs() < 0.01, "got {stretched:?}");
    }

    /// Nothing to stretch, or nothing to absorb, is the ordinary icon: one
    /// piece, scaled.
    #[test]
    fn falls_back_to_plain_scaling() {
        let plain = axis_pieces(8.0, &[], 16.0);
        assert_eq!(plain.len(), 1);
        assert!((map_axis(&plain, 8.0) - 4.0).abs() < 0.01);

        // Asked for less room than it naturally takes, it shrinks as a whole
        // rather than eating into the fixed parts.
        let shrunk = axis_pieces(8.0, &[[2.0, 6.0]], 4.0);
        assert_eq!(shrunk.len(), 1);
        assert!((map_axis(&shrunk, 2.0) - 4.0).abs() < 0.01);
    }

    /// The drawn result: a bar with a stretchable middle keeps its ends sharp.
    #[test]
    fn draws_a_stretched_icon_with_its_ends_intact() {
        // 5x1: black, black, white, black, black.
        let mut strip = Pixmap::new(5, 1).expect("pixmap");
        for (x, slot) in strip.pixels_mut().iter_mut().enumerate() {
            *slot = if x == 2 {
                ColorU8::from_rgba(255, 255, 255, 255).premultiply()
            } else {
                ColorU8::from_rgba(0, 0, 0, 255).premultiply()
            };
        }
        let sheet = SpriteSheet::parse(
            r#"{"bar": {"x": 0, "y": 0, "width": 5, "height": 1, "pixelRatio": 1,
                        "stretchX": [[2, 3]], "content": [2, 0, 3, 1]}}"#,
            &strip.encode_png().expect("encode"),
        )
        .expect("parses");

        let mut target = Pixmap::new(20, 4).expect("pixmap");
        draw(
            &mut target,
            &sheet,
            sheet.icon("bar").expect("bar"),
            &IconPaint {
                // Ten wide instead of five: the white middle takes the extra.
                stretch_to: Some((10.0, 1.0)),
                ..IconPaint::default()
            },
            Anchor::TopLeft,
            (0.0, 0.0),
        );

        // Two black pixels at each end, six white in the middle.
        assert_eq!(pixel(&target, 0, 0).red(), 0, "left edge stayed black");
        assert_eq!(pixel(&target, 1, 0).red(), 0, "left edge stayed black");
        assert_eq!(pixel(&target, 5, 0).red(), 255, "the middle stretched");
        assert_eq!(pixel(&target, 8, 0).red(), 0, "right edge stayed black");
        assert_eq!(pixel(&target, 9, 0).red(), 0, "right edge stayed black");
    }

    #[test]
    fn draws_nothing_at_zero_opacity() {
        let sheet = SpriteSheet::parse(index(), &sheet_png()).expect("parses");
        let mut target = Pixmap::new(16, 16).expect("pixmap");
        draw(
            &mut target,
            &sheet,
            sheet.icon("red").expect("red"),
            &IconPaint {
                opacity: 0.0,
                ..IconPaint::default()
            },
            Anchor::Center,
            (8.0, 8.0),
        );
        assert!(target.pixels().iter().all(|p| p.alpha() == 0));
    }
}
