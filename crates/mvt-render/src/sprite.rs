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
}

impl Default for IconPaint {
    fn default() -> Self {
        IconPaint {
            scale: 1.0,
            opacity: 1.0,
            color: Color::BLACK,
            halo_color: Color::TRANSPARENT,
            halo_width: 0.0,
        }
    }
}

/// The box an icon will cover, for collision.
pub fn bounds(icon: &Icon, paint: &IconPaint, anchor: Anchor, at: (f32, f32)) -> [f32; 4] {
    let (w, h) = icon.display_size();
    let (w, h) = (w * paint.scale, h * paint.scale);
    let (left, top) = origin(w, h, anchor, at);
    [left, top, left + w, top + h]
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
    let (w, h) = icon.display_size();
    let (w, h) = (w * paint.scale, h * paint.scale);
    if w < 0.5 || h < 0.5 {
        return;
    }
    let (left, top) = origin(w, h, anchor, at);

    let x0 = left.floor().max(0.0) as i32;
    let y0 = top.floor().max(0.0) as i32;
    let x1 = (left + w).ceil().min(pixmap.width() as f32) as i32;
    let y1 = (top + h).ceil().min(pixmap.height() as f32) as i32;

    let sheet_width = sheet.image.width() as i32;
    let data = sheet.image.data().to_vec();

    for py in y0..y1 {
        for px in x0..x1 {
            // Centre of the destination pixel, in the icon's own pixels.
            let u = (px as f32 + 0.5 - left) / w * icon.width as f32;
            let v = (py as f32 + 0.5 - top) / h * icon.height as f32;
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
