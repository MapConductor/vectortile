//! Laying out a label from SDF glyphs, and drawing it.
//!
//! Two jobs that are easier to keep apart than together: [`shape`] turns a
//! string into glyph positions and knows nothing about pixels, and [`draw`]
//! turns those positions into coverage and knows nothing about text.
//!
//! What this is not is a shaping engine. Each codepoint maps to one glyph and
//! advances the pen, which is right for Latin, Cyrillic, Greek and CJK — the
//! scripts a basemap is mostly made of — and wrong for Arabic, Devanagari and
//! the rest, where glyphs join and reorder. Those need a real shaper; until
//! there is one they come out as unjoined letters, which is worth knowing
//! rather than discovering.

use std::collections::HashMap;

use tiny_skia::Pixmap;

use crate::color::Color;
use crate::glyphs::{FontStack, Glyph, EDGE, GLYPH_SIZE, RANGE};

/// Every glyph the renderer has been given, by fontstack name.
///
/// A style names a stack per layer (`text-font`), and a stack is a list of
/// faces tried in order — the first one holding a codepoint wins, which is how
/// a Latin face and a CJK face cover a map between them.
#[derive(Debug, Default, Clone)]
pub struct GlyphStore {
    faces: HashMap<String, HashMap<u32, Glyph>>,
}

impl GlyphStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds everything in one decoded `.pbf` response.
    pub fn insert(&mut self, stack: FontStack) {
        self.faces
            .entry(stack.name)
            .or_default()
            .extend(stack.glyphs);
    }

    pub fn is_empty(&self) -> bool {
        self.faces.values().all(|f| f.is_empty())
    }

    /// The first face in `fonts` that has this codepoint.
    pub fn glyph(&self, fonts: &[String], code: u32) -> Option<&Glyph> {
        fonts
            .iter()
            .find_map(|name| self.faces.get(name).and_then(|face| face.get(&code)))
    }

    /// Which 256-codepoint ranges a string needs, per the `{range}` in a
    /// glyphs URL. Callers fetch these before shaping.
    pub fn ranges_for(text: &str) -> Vec<u32> {
        let mut ranges: Vec<u32> = text
            .chars()
            .map(|c| (c as u32) / 256)
            .filter(|r| *r <= 0xFF)
            .collect();
        ranges.sort_unstable();
        ranges.dedup();
        ranges
    }
}

/// Where one glyph sits, in SDF pixels relative to the label's own origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedGlyph {
    pub code: u32,
    /// Pen position. The glyph's own `left`/`top` offset it from here.
    pub x: f32,
    pub y: f32,
}

/// A laid-out label, in SDF pixels at [`GLYPH_SIZE`].
#[derive(Debug, Clone, Default)]
pub struct Shaped {
    pub glyphs: Vec<PlacedGlyph>,
    pub width: f32,
    pub height: f32,
    pub lines: usize,
}

impl Shaped {
    pub fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }
}

/// Line height as a multiple of the em, matching the style spec's default
/// `text-line-height`.
const LINE_HEIGHT: f32 = 1.2;

/// Lays `text` out on one or more lines, breaking so no line exceeds
/// `max_width` ems where a break is possible.
///
/// Breaks go at spaces, and between two ideographs — CJK has no spaces, so
/// without the second rule a place name would never wrap and `text-max-width`
/// would do nothing on half the world's labels.
pub fn shape(text: &str, fonts: &[String], store: &GlyphStore, max_width_ems: f32) -> Shaped {
    let max_width = max_width_ems * GLYPH_SIZE;
    let mut lines: Vec<Vec<(u32, f32)>> = Vec::new();
    let mut line: Vec<(u32, f32)> = Vec::new();
    let mut line_width = 0.0f32;
    let mut break_at: Option<usize> = None;

    for ch in text.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut line));
            line_width = 0.0;
            break_at = None;
            continue;
        }
        let code = ch as u32;
        let advance = store
            .glyph(fonts, code)
            .map(|g| g.advance as f32)
            .unwrap_or(0.0);

        line.push((code, advance));
        line_width += advance;

        if ch == ' ' {
            // After a space: the space ends the line it is on.
            break_at = Some(line.len());
        } else if is_ideograph(ch) && line.len() >= 2 {
            // Before an ideograph: CJK breaks between characters, and the one
            // that overflows is the one that moves down. Recording the break
            // after it instead made the test line run to 144 px unbroken —
            // `at < line.len()` was never true.
            break_at = Some(line.len() - 1);
        }

        if line_width > max_width && max_width > 0.0 {
            if let Some(at) = break_at.filter(|at| *at < line.len()) {
                let rest = line.split_off(at);
                // A trailing space belongs to the break, not to the next line.
                while line.last().map(|(c, _)| *c == 32).unwrap_or(false) {
                    line.pop();
                }
                lines.push(std::mem::take(&mut line));
                line = rest;
                line_width = line.iter().map(|(_, a)| a).sum();
                break_at = None;
            }
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }

    let line_step = GLYPH_SIZE * LINE_HEIGHT;
    let widths: Vec<f32> = lines
        .iter()
        .map(|l| l.iter().map(|(_, a)| a).sum())
        .collect();
    let width = widths.iter().cloned().fold(0.0f32, f32::max);
    let height = if lines.is_empty() {
        0.0
    } else {
        line_step * lines.len() as f32
    };

    // Lines are centred on each other, which is what `text-justify: center`
    // means and what every basemap style asks for.
    let mut glyphs = Vec::new();
    for (index, l) in lines.iter().enumerate() {
        let mut pen = (width - widths[index]) / 2.0;
        let baseline = line_step * index as f32 + GLYPH_SIZE;
        for (code, advance) in l {
            glyphs.push(PlacedGlyph {
                code: *code,
                x: pen,
                y: baseline,
            });
            pen += advance;
        }
    }

    Shaped {
        glyphs,
        width,
        height,
        lines: lines.len(),
    }
}

/// CJK ideographs, kana and full-width forms: scripts that wrap between any
/// two characters rather than at spaces.
fn is_ideograph(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF     // hiragana, katakana
        | 0x3400..=0x4DBF   // CJK ext A
        | 0x4E00..=0x9FFF   // CJK unified
        | 0xF900..=0xFAFF   // compatibility ideographs
        | 0xFF00..=0xFF60   // full-width forms
    )
}

/// Where the label sits relative to its anchor point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Center,
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Anchor {
    pub fn from_str(s: &str) -> Anchor {
        match s {
            "left" => Anchor::Left,
            "right" => Anchor::Right,
            "top" => Anchor::Top,
            "bottom" => Anchor::Bottom,
            "top-left" => Anchor::TopLeft,
            "top-right" => Anchor::TopRight,
            "bottom-left" => Anchor::BottomLeft,
            "bottom-right" => Anchor::BottomRight,
            _ => Anchor::Center,
        }
    }

    /// Fraction of the label's box to shift by: (0,0) puts its top-left on the
    /// anchor point.
    pub(crate) fn offset(self) -> (f32, f32) {
        match self {
            Anchor::Center => (-0.5, -0.5),
            Anchor::Left => (0.0, -0.5),
            Anchor::Right => (-1.0, -0.5),
            Anchor::Top => (-0.5, 0.0),
            Anchor::Bottom => (-0.5, -1.0),
            Anchor::TopLeft => (0.0, 0.0),
            Anchor::TopRight => (-1.0, 0.0),
            Anchor::BottomLeft => (0.0, -1.0),
            Anchor::BottomRight => (-1.0, -1.0),
        }
    }
}

/// Everything drawing a label needs beyond its shape.
#[derive(Debug, Clone, Copy)]
pub struct TextPaint {
    /// Em size on the target, in target pixels.
    pub size: f32,
    pub color: Color,
    pub halo_color: Color,
    /// Halo width in target pixels.
    pub halo_width: f32,
    /// `text-halo-blur`: how far the halo fades out past its own edge, in the
    /// style's pixels. A hard-edged halo reads as an outline drawn around the
    /// letter; a blurred one reads as the letter sitting on something.
    pub halo_blur: f32,
    pub opacity: f32,
}

/// The box a label will occupy on the target, for collision testing.
///
/// Returned separately from drawing so placement can reject a label before
/// anything is committed to the pixmap.
pub fn bounds(shaped: &Shaped, paint: &TextPaint, anchor: Anchor, at: (f32, f32)) -> [f32; 4] {
    let scale = paint.size / GLYPH_SIZE;
    let w = shaped.width * scale;
    let h = shaped.height * scale;
    let (fx, fy) = anchor.offset();
    let left = at.0 + fx * w;
    let top = at.1 + fy * h;
    [left, top, left + w, top + h]
}

/// Draws a shaped label at `at`, in target pixels.
///
/// The halo goes down first and the fill over it, so the fill's own edge is
/// never eaten by the halo's.
pub fn draw(
    pixmap: &mut Pixmap,
    shaped: &Shaped,
    store: &GlyphStore,
    fonts: &[String],
    paint: &TextPaint,
    anchor: Anchor,
    at: (f32, f32),
) {
    if shaped.is_empty() || paint.opacity <= 0.0 {
        return;
    }
    let scale = paint.size / GLYPH_SIZE;
    let [left, top, _, _] = bounds(shaped, paint, anchor, at);

    if paint.halo_width > 0.0 {
        for placed in &shaped.glyphs {
            if let Some(glyph) = store.glyph(fonts, placed.code) {
                blit(
                    pixmap,
                    glyph,
                    placed,
                    left,
                    top,
                    scale,
                    paint.halo_color,
                    paint.opacity,
                    Some(Halo {
                        width: paint.halo_width,
                        blur: paint.halo_blur,
                    }),
                );
            }
        }
    }
    for placed in &shaped.glyphs {
        if let Some(glyph) = store.glyph(fonts, placed.code) {
            blit(
                pixmap,
                glyph,
                placed,
                left,
                top,
                scale,
                paint.color,
                paint.opacity,
                None,
            );
        }
    }
}

/// Draws one glyph's signed distance field as coverage.
///
/// The ramp is the one the GL renderers use: the stored distance is compared
/// against a threshold, and a narrow band either side of it becomes the
/// antialiased edge. Widening the threshold instead of the ink is what makes a
/// halo — the same field, read at a different depth.
///
/// How wide the halo is, and how far it fades out past its own edge.
#[derive(Debug, Clone, Copy)]
struct Halo {
    width: f32,
    blur: f32,
}

#[allow(clippy::too_many_arguments)]
fn blit(
    pixmap: &mut Pixmap,
    glyph: &Glyph,
    placed: &PlacedGlyph,
    origin_x: f32,
    origin_y: f32,
    scale: f32,
    color: Color,
    opacity: f32,
    halo: Option<Halo>,
) {
    if glyph.bitmap.is_empty() {
        return;
    }
    let bw = glyph.bitmap_width() as i32;
    let bh = glyph.bitmap_height() as i32;

    // The buffer sits outside the ink on every side, so the drawn box starts
    // that much before the ink's own origin.
    let buffer = crate::glyphs::BUFFER as f32;
    let x0 = origin_x + (placed.x + glyph.left as f32 - buffer) * scale;
    let y0 = origin_y + (placed.y - glyph.top as f32 - buffer) * scale;

    let edge = EDGE / 255.0;
    let threshold = match halo {
        // Reading the field further out from the outline thickens the ink;
        // how much further is the halo width, in the field's own units.
        Some(halo) => (RANGE - halo.width / scale) / (2.0 * RANGE) + edge - 0.5,
        None => edge,
    };
    // One target pixel spans this much of the field, which is the width the
    // edge is allowed to be soft over. `text-halo-blur` asks for more than
    // that: a halo is meant to fade rather than end, and the style says over
    // how many pixels.
    let blur = halo.map(|h| h.blur).unwrap_or(0.0).max(0.0);
    let gamma = (0.105 / scale).max(0.001) + blur / (2.0 * RANGE * scale);

    let dest_w = (bw as f32 * scale).ceil() as i32;
    let dest_h = (bh as f32 * scale).ceil() as i32;
    let pix_w = pixmap.width() as i32;
    let pix_h = pixmap.height() as i32;

    for dy in 0..dest_h {
        let py = y0.floor() as i32 + dy;
        if py < 0 || py >= pix_h {
            continue;
        }
        for dx in 0..dest_w {
            let px = x0.floor() as i32 + dx;
            if px < 0 || px >= pix_w {
                continue;
            }
            // Sample the field at this target pixel's centre.
            let sx = (px as f32 + 0.5 - x0) / scale;
            let sy = (py as f32 + 0.5 - y0) / scale;
            let distance = sample(&glyph.bitmap, bw, bh, sx, sy);
            let alpha =
                smoothstep(threshold - gamma, threshold + gamma, distance) * opacity * color.a;
            if alpha <= 0.002 {
                continue;
            }
            blend(pixmap, px, py, color, alpha);
        }
    }
}

/// Bilinear, because nearest sampling makes the edge crawl when a label is
/// drawn at a size the field was not rasterised for — which is every size.
fn sample(bitmap: &[u8], width: i32, height: i32, x: f32, y: f32) -> f32 {
    let fx = (x - 0.5).clamp(0.0, (width - 1) as f32);
    let fy = (y - 0.5).clamp(0.0, (height - 1) as f32);
    let x0 = fx.floor() as i32;
    let y0 = fy.floor() as i32;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;

    let at = |x: i32, y: i32| bitmap[(y * width + x) as usize] as f32 / 255.0;
    let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
    let bottom = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
    top * (1.0 - ty) + bottom * ty
}

/// Bilinear sample of the alpha channel of an RGBA buffer, on the 0..255
/// scale the distance fields use.
///
/// An SDF sprite keeps its distance where a glyph keeps its whole bitmap, so
/// the sampling is the same and only the stride differs.
pub(crate) fn sample_alpha(rgba: &[u8], width: i32, height: i32, x: f32, y: f32) -> f32 {
    let fx = (x - 0.5).clamp(0.0, (width - 1) as f32);
    let fy = (y - 0.5).clamp(0.0, (height - 1) as f32);
    let x0 = fx.floor() as i32;
    let y0 = fy.floor() as i32;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;

    let at = |x: i32, y: i32| {
        let index = (y * width + x) as usize * 4 + 3;
        rgba.get(index).copied().unwrap_or(0) as f32
    };
    let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
    let bottom = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
    top * (1.0 - ty) + bottom * ty
}

pub(crate) fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge1 <= edge0 {
        return if x < edge0 { 0.0 } else { 1.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Source-over, premultiplied, straight into the pixmap.
///
/// tiny-skia has no way to run a per-pixel function, and a path per glyph
/// would lose the field's antialiasing — which is the only reason the field is
/// there.
pub(crate) fn blend(pixmap: &mut Pixmap, x: i32, y: i32, color: Color, alpha: f32) {
    let index = (y as usize * pixmap.width() as usize + x as usize) * 4;
    let data = pixmap.data_mut();
    let src = [color.r, color.g, color.b];
    for channel in 0..3 {
        let dst = data[index + channel] as f32 / 255.0;
        let s = src[channel] * alpha;
        data[index + channel] = (((s + dst * (1.0 - alpha)) * 255.0) + 0.5).clamp(0.0, 255.0) as u8;
    }
    let dst_a = data[index + 3] as f32 / 255.0;
    data[index + 3] = (((alpha + dst_a * (1.0 - alpha)) * 255.0) + 0.5).clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::BUFFER;

    /// A glyph whose field is a real signed distance to a solid rectangle of
    /// ink, not a binary mask.
    ///
    /// The first version filled the ink with 255 and left the rest 0. Every
    /// threshold between the two then produces the same one-pixel edge, so the
    /// halo test passed a widened threshold and got back an identical image —
    /// it was measuring the fixture, not the code.
    fn solid(code: u32, width: u32, height: u32, advance: u32) -> Glyph {
        let bw = width + 2 * BUFFER;
        let bh = height + 2 * BUFFER;
        let mut bitmap = vec![0u8; (bw * bh) as usize];
        let (x0, y0) = (BUFFER as f32, BUFFER as f32);
        let (x1, y1) = (x0 + width as f32, y0 + height as f32);
        for y in 0..bh {
            for x in 0..bw {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                // Signed distance to the rectangle, positive inside.
                let dx = (x0 - px).max(px - x1);
                let dy = (y0 - py).max(py - y1);
                let outside = dx.max(0.0).hypot(dy.max(0.0));
                let inside = dx.max(dy).min(0.0);
                let distance = -(outside + inside);
                let value = EDGE + distance * (255.0 - EDGE) / RANGE;
                bitmap[(y * bw + x) as usize] = value.clamp(0.0, 255.0) as u8;
            }
        }
        Glyph {
            id: code,
            bitmap,
            width,
            height,
            left: 0,
            top: height as i32,
            advance,
        }
    }

    fn store_with(glyphs: Vec<Glyph>) -> (GlyphStore, Vec<String>) {
        let mut stack = FontStack {
            name: "F".into(),
            range: "0-255".into(),
            glyphs: HashMap::new(),
        };
        for g in glyphs {
            stack.glyphs.insert(g.id, g);
        }
        let mut store = GlyphStore::new();
        store.insert(stack);
        (store, vec!["F".into()])
    }

    #[test]
    fn advances_the_pen_by_each_glyph() {
        let (store, fonts) = store_with(vec![solid(65, 10, 14, 12), solid(66, 10, 14, 8)]);
        let shaped = shape("AB", &fonts, &store, 100.0);
        assert_eq!(shaped.lines, 1);
        assert_eq!(shaped.glyphs.len(), 2);
        assert_eq!(shaped.glyphs[0].x, 0.0);
        assert_eq!(shaped.glyphs[1].x, 12.0);
        assert_eq!(shaped.width, 20.0);
    }

    /// A codepoint no face carries still has to leave the rest of the label
    /// alone; it simply contributes nothing.
    #[test]
    fn skips_codepoints_no_face_has() {
        let (store, fonts) = store_with(vec![solid(65, 10, 14, 12)]);
        let shaped = shape("A\u{4e2d}A", &fonts, &store, 100.0);
        assert_eq!(shaped.glyphs.len(), 3);
        assert_eq!(shaped.width, 24.0);
    }

    #[test]
    fn breaks_lines_at_spaces() {
        let (store, fonts) = store_with(vec![solid(65, 10, 14, 12), solid(32, 0, 0, 6)]);
        // Three As per line at most: max_width 1.5 em = 36 SDF px = three
        // advances of 12.
        let shaped = shape("AAA AAA", &fonts, &store, 1.5);
        assert_eq!(shaped.lines, 2);
        assert!(shaped.width <= 36.0, "width was {}", shaped.width);
    }

    /// CJK has no spaces. Without a rule for it, `text-max-width` would do
    /// nothing at all on a Japanese basemap.
    #[test]
    fn breaks_lines_between_ideographs() {
        let (store, fonts) = store_with(vec![
            solid(0x4e2d, 20, 20, 24),
            solid(0x592e, 20, 20, 24),
            solid(0x533a, 20, 20, 24),
        ]);
        let shaped = shape("中央区中央区", &fonts, &store, 2.0);
        assert!(shaped.lines >= 2, "one line of {} px", shaped.width);
    }

    #[test]
    fn centres_lines_on_each_other() {
        let (store, fonts) = store_with(vec![solid(65, 10, 14, 12), solid(32, 0, 0, 6)]);
        let shaped = shape("AAA A", &fonts, &store, 1.5);
        assert_eq!(shaped.lines, 2);
        let first_line_y = shaped.glyphs[0].y;
        let last = shaped.glyphs.last().unwrap();
        assert!(
            last.y > first_line_y,
            "the second line sits below the first"
        );
        assert!(last.x > 0.0, "a shorter line is centred, not left-aligned");
    }

    #[test]
    fn anchors_shift_the_box() {
        let (store, fonts) = store_with(vec![solid(65, 10, 14, 12)]);
        let shaped = shape("A", &fonts, &store, 100.0);
        let paint = TextPaint {
            size: 24.0,
            color: Color::new(0.0, 0.0, 0.0, 1.0),
            halo_color: Color::new(1.0, 1.0, 1.0, 1.0),
            halo_width: 0.0,
            halo_blur: 0.0,
            opacity: 1.0,
        };
        let centre = bounds(&shaped, &paint, Anchor::Center, (100.0, 100.0));
        let top_left = bounds(&shaped, &paint, Anchor::TopLeft, (100.0, 100.0));
        assert!(centre[0] < top_left[0] && centre[1] < top_left[1]);
        assert_eq!(top_left[0], 100.0);
        assert_eq!(top_left[1], 100.0);
    }

    #[test]
    fn draws_ink_where_the_glyph_is() {
        let (store, fonts) = store_with(vec![solid(65, 12, 12, 14)]);
        let shaped = shape("A", &fonts, &store, 100.0);
        let paint = TextPaint {
            size: 24.0,
            color: Color::new(0.0, 0.0, 0.0, 1.0),
            halo_color: Color::new(1.0, 1.0, 1.0, 1.0),
            halo_width: 0.0,
            halo_blur: 0.0,
            opacity: 1.0,
        };
        let mut pixmap = Pixmap::new(64, 64).unwrap();
        draw(
            &mut pixmap,
            &shaped,
            &store,
            &fonts,
            &paint,
            Anchor::Center,
            (32.0, 32.0),
        );

        let alpha_at = |x: u32, y: u32| pixmap.data()[((y * 64 + x) * 4 + 3) as usize];
        assert!(alpha_at(32, 32) > 200, "the centre of the glyph is inked");
        assert_eq!(alpha_at(2, 2), 0, "the far corner is untouched");
    }

    /// A blurred halo fades out instead of ending, which is what
    /// `text-halo-blur` asks for -- 30 of the sample style's layers do.
    #[test]
    fn blur_softens_the_halos_edge() {
        let (store, fonts) = store_with(vec![solid(65, 8, 8, 10)]);
        let shaped = shape("A", &fonts, &store, 100.0);
        let base = TextPaint {
            size: 24.0,
            color: Color::new(0.0, 0.0, 0.0, 0.0),
            halo_color: Color::new(1.0, 0.0, 0.0, 1.0),
            halo_width: 2.0,
            halo_blur: 0.0,
            opacity: 1.0,
        };
        let ring = |paint: &TextPaint, from: f32, to: f32| {
            let mut pixmap = Pixmap::new(64, 64).unwrap();
            draw(
                &mut pixmap,
                &shaped,
                &store,
                &fonts,
                paint,
                Anchor::Center,
                (32.0, 32.0),
            );
            (0..64)
                .flat_map(|y| (0..64).map(move |x| (x, y)))
                .filter(|(x, y)| {
                    let dx = *x as f32 - 32.0;
                    let dy = *y as f32 - 32.0;
                    let d = (dx * dx + dy * dy).sqrt();
                    (from..to).contains(&d)
                })
                .map(|(x, y)| pixmap.data()[((y * 64 + x) * 4 + 3) as usize] as u32)
                .sum::<u32>()
        };

        // A halo either ends or fades. Counting the pixels caught mid-fade
        // is what tells the two apart -- it cannot reach further than the
        // field it is read from, which stops a few pixels past the ink.
        let half_lit = |paint: &TextPaint| {
            let mut pixmap = Pixmap::new(64, 64).unwrap();
            draw(
                &mut pixmap,
                &shaped,
                &store,
                &fonts,
                paint,
                Anchor::Center,
                (32.0, 32.0),
            );
            pixmap
                .data()
                .chunks(4)
                .filter(|p| (20..235).contains(&p[3]))
                .count()
        };

        let hard = half_lit(&base);
        let soft = half_lit(&TextPaint {
            halo_blur: 3.0,
            ..base
        });
        assert!(
            soft > hard + hard / 4,
            "expected the blurred halo to fade over more of its edge: hard {hard}, blurred {soft}"
        );
    }

    /// The halo has to reach outside the ink, or it is not a halo.
    #[test]
    fn the_halo_reaches_past_the_ink() {
        let (store, fonts) = store_with(vec![solid(65, 8, 8, 10)]);
        let shaped = shape("A", &fonts, &store, 100.0);
        let base = TextPaint {
            size: 24.0,
            color: Color::new(0.0, 0.0, 0.0, 1.0),
            halo_color: Color::new(1.0, 0.0, 0.0, 1.0),
            halo_width: 0.0,
            halo_blur: 0.0,
            opacity: 1.0,
        };
        let alpha_ring = |paint: &TextPaint| {
            let mut pixmap = Pixmap::new(64, 64).unwrap();
            draw(
                &mut pixmap,
                &shaped,
                &store,
                &fonts,
                paint,
                Anchor::Center,
                (32.0, 32.0),
            );
            // A ring just outside the ink: the glyph is 8 SDF px wide drawn at
            // 1:1, so its edge is 4 px from the centre.
            (0..64)
                .flat_map(|y| (0..64).map(move |x| (x, y)))
                .filter(|(x, y)| {
                    let dx = *x as f32 - 32.0;
                    let dy = *y as f32 - 32.0;
                    let d = (dx * dx + dy * dy).sqrt();
                    (6.0..8.0).contains(&d)
                })
                .map(|(x, y)| pixmap.data()[((y * 64 + x) * 4 + 3) as usize] as u32)
                .sum::<u32>()
        };
        let without = alpha_ring(&base);
        let with = alpha_ring(&TextPaint {
            halo_width: 2.0,
            halo_blur: 0.0,
            ..base
        });
        assert!(
            with > without,
            "halo {} should cover more than none {}",
            with,
            without
        );
    }

    #[test]
    fn reports_the_ranges_a_string_needs() {
        assert_eq!(GlyphStore::ranges_for("AB"), vec![0]);
        // 中 is U+4E2D, so range 78 (0x4E00/256 = 78).
        assert_eq!(GlyphStore::ranges_for("A中"), vec![0, 78]);
    }
}
