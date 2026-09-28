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
#[derive(Debug, Default)]
pub struct GlyphStore {
    faces: HashMap<String, HashMap<u32, Glyph>>,
    /// Rendered coverage masks, so a glyph that appears on every tile is
    /// thresholded from its distance field once rather than once per tile.
    ///
    /// Behind a mutex because tiles render in parallel over a shared,
    /// otherwise read-only store; the lock is held for a lookup or an insert,
    /// never while rendering.
    masks: std::sync::Mutex<HashMap<MaskKey, std::sync::Arc<Mask>>>,
}

/// What makes two uses of a glyph the same drawing.
///
/// The floats are keyed by their bit patterns: sizes come from a style's zoom
/// curve evaluated at whole zoom levels, so a handful of distinct values recur
/// endlessly.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
struct MaskKey {
    code: u32,
    /// Which face resolved the code, so two fonts sharing a codepoint do not
    /// share a shape.
    face: u64,
    scale: u32,
    threshold: u32,
    gamma: u32,
}

/// A glyph's coverage at one drawn size: 0..255 per pixel. Colour is applied
/// at blit time, so one mask serves every tint, and one more the halo.
#[derive(Debug)]
struct Mask {
    width: i32,
    height: i32,
    alpha: Vec<u8>,
}

/// Masks kept before the cache empties itself. A style uses a handful of
/// sizes per zoom and a few hundred distinct characters per city; the cap
/// only stops a pathological style from growing without bound, and clearing
/// everything beats an LRU -- the working set refills in one tile.
const MASK_CACHE_CAP: usize = 8192;

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

    /// The face that would resolve this code, as a stable identity for the
    /// mask cache.
    fn face_id(&self, fonts: &[String], code: u32) -> u64 {
        use std::hash::{Hash, Hasher};
        let name = fonts
            .iter()
            .find(|name| {
                self.faces
                    .get(name.as_str())
                    .is_some_and(|face| face.contains_key(&code))
            })
            .map(String::as_str)
            .unwrap_or("");
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        hasher.finish()
    }

    /// The first face in `fonts` that has this codepoint.
    ///
    /// A range that arrived without a name is tried last. The name inside a
    /// `.pbf` is redundant -- a range is fetched from `{fontstack}/{range}.pbf`
    /// and is by construction the stack that was asked for -- and servers in
    /// the wild leave it out: every range versatiles serves carries its 256
    /// glyphs and nothing else, so every one of them was filed under the empty
    /// name and no lookup ever found it. The whole map drew its labels in
    /// invisible ink, placed correctly and coloured with nothing.
    ///
    /// A named face always wins, so this changes nothing for a server that
    /// fills the field in. What it cannot do is tell two nameless stacks
    /// apart: a style asking for both a regular and a bold face gets whichever
    /// of them arrived last wherever they hold the same character. Telling
    /// them apart takes the name from the side that knows it -- the caller
    /// that chose the URL -- which is an argument `add_glyphs` does not have
    /// yet.
    pub fn glyph(&self, fonts: &[String], code: u32) -> Option<&Glyph> {
        fonts
            .iter()
            .chain(std::iter::once(&String::new()))
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

    let fill = Ink {
        origin: (left, top),
        scale,
        color: paint.color,
        opacity: paint.opacity,
        halo: None,
        rotation: 0.0,
        face: 0,
    };
    if paint.halo_width > 0.0 {
        let halo = Ink {
            color: paint.halo_color,
            halo: Some(Halo {
                width: paint.halo_width,
                blur: paint.halo_blur,
            }),
            ..fill
        };
        for placed in &shaped.glyphs {
            if let Some(glyph) = store.glyph(fonts, placed.code) {
                let ink = Ink {
                    face: store.face_id(fonts, placed.code),
                    ..halo
                };
                blit_cached(pixmap, store, glyph, placed, &ink);
            }
        }
    }
    for placed in &shaped.glyphs {
        if let Some(glyph) = store.glyph(fonts, placed.code) {
            let ink = Ink {
                face: store.face_id(fonts, placed.code),
                ..fill
            };
            blit_cached(pixmap, store, glyph, placed, &ink);
        }
    }
}

/// One glyph laid on a line: where its pen sits and which way it faces.
#[derive(Debug, Clone, Copy)]
pub struct GlyphOnLine {
    pub code: u32,
    pub at: (f32, f32),
    /// Clockwise degrees, from the direction of the line under it.
    pub angle: f32,
}

/// Draws a label that follows a line, glyph by glyph.
///
/// A road name cannot be laid out once and stamped down: each letter sits at
/// its own place along the curve and faces its own way, so the run is walked
/// rather than positioned. The halo still goes down first, and for the same
/// reason -- but for the whole run, not per letter, or each letter's halo
/// would eat the one before it.
pub fn draw_along_line(
    pixmap: &mut Pixmap,
    glyphs: &[GlyphOnLine],
    store: &GlyphStore,
    fonts: &[String],
    paint: &TextPaint,
) {
    if glyphs.is_empty() || paint.opacity <= 0.0 {
        return;
    }
    let scale = paint.size / GLYPH_SIZE;

    let ink_for = |glyph: &GlyphOnLine, halo: Option<Halo>| Ink {
        face: 0,
        // Each glyph carries its own origin, so the pen is at zero.
        origin: glyph.at,
        scale,
        color: if halo.is_some() {
            paint.halo_color
        } else {
            paint.color
        },
        opacity: paint.opacity,
        halo,
        rotation: glyph.angle,
    };
    let pen = PlacedGlyph {
        code: 0,
        x: 0.0,
        y: 0.0,
    };

    if paint.halo_width > 0.0 {
        let halo = Some(Halo {
            width: paint.halo_width,
            blur: paint.halo_blur,
        });
        for glyph in glyphs {
            if let Some(face) = store.glyph(fonts, glyph.code) {
                blit(pixmap, face, &pen, &ink_for(glyph, halo));
            }
        }
    }
    for glyph in glyphs {
        if let Some(face) = store.glyph(fonts, glyph.code) {
            blit(pixmap, face, &pen, &ink_for(glyph, None));
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
/// Everything a single glyph's blit needs beyond the glyph itself.
#[derive(Debug, Clone, Copy)]
struct Ink {
    /// Where the run's own origin sits on the target.
    origin: (f32, f32),
    /// Target pixels per SDF pixel.
    scale: f32,
    color: Color,
    opacity: f32,
    halo: Option<Halo>,
    /// Clockwise, about the pen position. Zero for a label laid out flat,
    /// which is every label that is not following a road.
    rotation: f32,
    /// Which face resolved this glyph, for the mask cache.
    face: u64,
}

/// The field threshold and softness an ink draws at.
///
/// Shared by the direct and the cached path, so the two cannot disagree about
/// what a glyph looks like. Reading the field further out from the outline
/// thickens the ink into a halo; `text-halo-blur` widens the band the edge
/// fades over -- a halo is meant to fade rather than end.
fn threshold_and_gamma(scale: f32, halo: Option<Halo>) -> (f32, f32) {
    let edge = EDGE / 255.0;
    let threshold = match halo {
        Some(halo) => (RANGE - halo.width / scale) / (2.0 * RANGE) + edge - 0.5,
        None => edge,
    };
    let blur = halo.map(|h| h.blur).unwrap_or(0.0).max(0.0);
    let gamma = (0.105 / scale).max(0.001) + blur / (2.0 * RANGE * scale);
    (threshold, gamma)
}

/// Thresholds a glyph's field into coverage at one drawn size.
fn build_mask(glyph: &Glyph, scale: f32, threshold: f32, gamma: f32) -> Mask {
    let bw = glyph.bitmap_width() as i32;
    let bh = glyph.bitmap_height() as i32;
    let width = (bw as f32 * scale).ceil() as i32 + 1;
    let height = (bh as f32 * scale).ceil() as i32 + 1;
    let mut alpha = vec![0u8; (width * height) as usize];
    for my in 0..height {
        for mx in 0..width {
            let sx = (mx as f32 + 0.5) / scale;
            let sy = (my as f32 + 0.5) / scale;
            if sx >= bw as f32 || sy >= bh as f32 {
                continue;
            }
            let distance = sample(&glyph.bitmap, bw, bh, sx, sy);
            let coverage = smoothstep(threshold - gamma, threshold + gamma, distance);
            alpha[(my * width + mx) as usize] = (coverage * 255.0 + 0.5) as u8;
        }
    }
    Mask {
        width,
        height,
        alpha,
    }
}

/// As [`blit`], but through the store's mask cache when the glyph is drawn
/// unrotated -- which is every glyph of every point label.
///
/// A glyph's coverage at a given size never changes, and thresholding it out
/// of the distance field -- a bilinear sample and a smoothstep per pixel --
/// was repeated for the same glyph on every tile it appeared on.
fn blit_cached(
    pixmap: &mut Pixmap,
    store: &GlyphStore,
    glyph: &Glyph,
    placed: &PlacedGlyph,
    ink: &Ink,
) {
    if ink.rotation != 0.0 {
        blit(pixmap, glyph, placed, ink);
        return;
    }
    if glyph.bitmap.is_empty() {
        return;
    }
    let (threshold, gamma) = threshold_and_gamma(ink.scale, ink.halo);
    let key = MaskKey {
        code: glyph.id,
        face: ink.face,
        scale: ink.scale.to_bits(),
        threshold: threshold.to_bits(),
        gamma: gamma.to_bits(),
    };
    let cached = store.masks.lock().ok().and_then(|m| m.get(&key).cloned());
    let mask = match cached {
        Some(mask) => mask,
        None => {
            let mask = std::sync::Arc::new(build_mask(glyph, ink.scale, threshold, gamma));
            if let Ok(mut masks) = store.masks.lock() {
                if masks.len() >= MASK_CACHE_CAP {
                    masks.clear();
                }
                masks.insert(key, mask.clone());
            }
            mask
        }
    };

    // The mask's origin sits on a whole pixel; drawing it at a fractional one
    // would need the resample the mask is here to remove. Rounding moves the
    // glyph under half a pixel -- a quarter of a style pixel on the doubled
    // overlay.
    let buffer = crate::glyphs::BUFFER as f32;
    let x0 = (ink.origin.0 + (placed.x + glyph.left as f32 - buffer) * ink.scale).round() as i32;
    let y0 = (ink.origin.1 + (placed.y - glyph.top as f32 - buffer) * ink.scale).round() as i32;
    let pix_w = pixmap.width() as i32;
    let pix_h = pixmap.height() as i32;
    let base_alpha = ink.opacity * ink.color.a;

    for my in 0..mask.height {
        let py = y0 + my;
        if py < 0 || py >= pix_h {
            continue;
        }
        let row = (my * mask.width) as usize;
        for mx in 0..mask.width {
            let coverage = mask.alpha[row + mx as usize];
            if coverage == 0 {
                continue;
            }
            let px = x0 + mx;
            if px < 0 || px >= pix_w {
                continue;
            }
            let alpha = coverage as f32 / 255.0 * base_alpha;
            if alpha <= 0.002 {
                continue;
            }
            blend(pixmap, px, py, ink.color, alpha);
        }
    }
}

fn blit(pixmap: &mut Pixmap, glyph: &Glyph, placed: &PlacedGlyph, ink: &Ink) {
    let Ink {
        origin: (origin_x, origin_y),
        scale,
        color,
        opacity,
        halo,
        rotation,
        ..
    } = *ink;
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

    let (threshold, gamma) = threshold_and_gamma(scale, halo);

    let dest_w = bw as f32 * scale;
    let dest_h = bh as f32 * scale;
    let pix_w = pixmap.width() as i32;
    let pix_h = pixmap.height() as i32;

    // A glyph following a road is turned about the pen, so the area to walk
    // is the box around the turned corners and each pixel in it is turned
    // back to ask which part of the glyph lands there.
    let (sin, cos) = rotation.to_radians().sin_cos();
    let (pen_x, pen_y) = (origin_x + placed.x * scale, origin_y + placed.y * scale);
    let corners = [
        (x0, y0),
        (x0 + dest_w, y0),
        (x0, y0 + dest_h),
        (x0 + dest_w, y0 + dest_h),
    ];
    let mut min = (f32::MAX, f32::MAX);
    let mut max = (f32::MIN, f32::MIN);
    for (cx, cy) in corners {
        let (dx, dy) = (cx - pen_x, cy - pen_y);
        let rx = pen_x + dx * cos - dy * sin;
        let ry = pen_y + dx * sin + dy * cos;
        min = (min.0.min(rx), min.1.min(ry));
        max = (max.0.max(rx), max.1.max(ry));
    }

    let (back_sin, back_cos) = (-rotation).to_radians().sin_cos();
    for py in min.1.floor() as i32..=max.1.ceil() as i32 {
        if py < 0 || py >= pix_h {
            continue;
        }
        for px in min.0.floor() as i32..=max.0.ceil() as i32 {
            if px < 0 || px >= pix_w {
                continue;
            }
            // Sample the field at this target pixel's centre, turned back
            // into the glyph's own frame first.
            let (fx, fy) = if rotation == 0.0 {
                (px as f32 + 0.5, py as f32 + 0.5)
            } else {
                let (dx, dy) = (px as f32 + 0.5 - pen_x, py as f32 + 0.5 - pen_y);
                (
                    pen_x + dx * back_cos - dy * back_sin,
                    pen_y + dx * back_sin + dy * back_cos,
                )
            };
            let sx = (fx - x0) / scale;
            let sy = (fy - y0) / scale;
            if sx < 0.0 || sy < 0.0 || sx >= bw as f32 || sy >= bh as f32 {
                continue;
            }
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

    /// A range that arrived without a name still draws.
    ///
    /// Not a hypothetical: versatiles serves every range as its 256 glyphs and
    /// nothing else, and the whole map's labels went invisible because of it.
    #[test]
    fn a_nameless_range_still_answers_for_the_stack_that_asked() {
        let mut store = GlyphStore::new();
        let mut glyphs = HashMap::new();
        glyphs.insert(65, solid(65, 4, 6, 5));
        store.insert(FontStack {
            name: String::new(),
            range: String::new(),
            glyphs,
        });
        let wanted = vec!["noto_sans_regular".to_string()];
        assert!(store.glyph(&wanted, 65).is_some());
        assert!(store.glyph(&wanted, 66).is_none(), "invented a glyph");
    }

    /// A named face is still preferred, so a well-formed server is unaffected.
    #[test]
    fn a_named_face_wins_over_a_nameless_one() {
        let mut store = GlyphStore::new();
        let mut nameless = HashMap::new();
        nameless.insert(65, solid(65, 4, 6, 5));
        store.insert(FontStack {
            name: String::new(),
            range: String::new(),
            glyphs: nameless,
        });
        let mut named = HashMap::new();
        named.insert(65, solid(65, 9, 6, 11));
        store.insert(FontStack {
            name: "F".into(),
            range: "0-255".into(),
            glyphs: named,
        });
        let glyph = store.glyph(&["F".to_string()], 65).expect("the named face");
        assert_eq!(glyph.advance, 11);
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

    /// Every letter of a run laid along a line has to be drawn, including the
    /// first: a road name missing its first character reads as a different
    /// road.
    #[test]
    fn draws_every_glyph_of_a_line_label() {
        let (store, fonts) = store_with(vec![solid(65, 8, 8, 10), solid(66, 8, 8, 10)]);
        let paint = TextPaint {
            size: 24.0,
            color: Color::new(0.0, 0.0, 0.0, 1.0),
            halo_color: Color::new(1.0, 1.0, 1.0, 1.0),
            halo_width: 1.0,
            halo_blur: 0.0,
            opacity: 1.0,
        };
        // Four pens along a slightly tilted line, as a road label would be.
        let glyphs: Vec<GlyphOnLine> = (0..4)
            .map(|i| GlyphOnLine {
                code: if i % 2 == 0 { 65 } else { 66 },
                at: (30.0 + i as f32 * 24.0, 60.0 + i as f32 * 1.0),
                angle: 2.0,
            })
            .collect();

        let mut pixmap = Pixmap::new(160, 120).unwrap();
        draw_along_line(&mut pixmap, &glyphs, &store, &fonts, &paint);

        for (index, glyph) in glyphs.iter().enumerate() {
            // Ink somewhere in the box the glyph occupies: it sits above its
            // pen, which is the baseline.
            let ink = (0..160)
                .flat_map(|x| (0..120).map(move |y| (x, y)))
                .filter(|(x, y)| {
                    (*x as f32 - glyph.at.0).abs() < 12.0
                        && (glyph.at.1 - *y as f32) > 0.0
                        && (glyph.at.1 - *y as f32) < 12.0
                })
                .filter(|(x, y)| pixmap.data()[((y * 160 + x) * 4 + 3) as usize] > 40)
                .count();
            assert!(ink > 0, "glyph {index} of the run was not drawn");
        }
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
