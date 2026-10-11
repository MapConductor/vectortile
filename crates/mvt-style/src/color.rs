//! Colours, only as far as a style document needs them.
//!
//! Deliberately not shared with `mvt-render`'s colour module: this crate
//! stays free of the rasteriser so it can ship on its own, and what is
//! needed here is different anyway. The renderer turns a colour into
//! premultiplied floats to paint with; this reads one out of a style, bends
//! it, and writes it back as text the next reader -- MapLibre, Mapbox, the
//! rasteriser -- will parse the same way.

use crate::rules::ColorFilter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r, g, b, a }
    }

    /// `#rrggbb`, or `rgba(r, g, b, a)` when it is not fully opaque.
    ///
    /// Hex rather than the shortest form because every renderer parses it and
    /// a human reading the transformed style can see what happened. The
    /// `rgba()` form is used for transparency because `#rrggbbaa` is CSS
    /// Color 4 and not every style parser in the chain accepts it.
    pub fn to_style_text(self) -> String {
        if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!(
                "rgba({},{},{},{})",
                self.r,
                self.g,
                self.b,
                trim(f64::from(self.a) / 255.0)
            )
        }
    }
}

/// `0.5` rather than `0.5000000000000001`, which is what a naive format gives.
fn trim(value: f64) -> String {
    let text = format!("{:.3}", value);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() {
        "0".to_string()
    } else {
        text.to_string()
    }
}

/// Reads the colour forms a style actually contains.
///
/// `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, `rgb()`, `rgba()`, `hsl()`,
/// `hsla()` and the handful of named colours a style is likely to use.
/// Anything else is left alone rather than guessed at -- a value this cannot
/// read is a value this must not rewrite.
pub fn parse(text: &str) -> Option<Rgba> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex(hex);
    }
    if let Some(rest) = strip_call(text, "rgba").or_else(|| strip_call(text, "rgb")) {
        let parts = split_args(rest);
        if parts.len() != 3 && parts.len() != 4 {
            return None;
        }
        let r = parse_channel(parts[0])?;
        let g = parse_channel(parts[1])?;
        let b = parse_channel(parts[2])?;
        let a = match parts.get(3) {
            Some(alpha) => {
                (alpha.trim().parse::<f64>().ok()?.clamp(0.0, 1.0) * 255.0).round() as u8
            }
            None => 255,
        };
        return Some(Rgba::new(r, g, b, a));
    }
    if let Some(rest) = strip_call(text, "hsla").or_else(|| strip_call(text, "hsl")) {
        let parts = split_args(rest);
        if parts.len() != 3 && parts.len() != 4 {
            return None;
        }
        let h = parts[0]
            .trim()
            .trim_end_matches("deg")
            .parse::<f64>()
            .ok()?;
        let s = parse_percent(parts[1])?;
        let l = parse_percent(parts[2])?;
        let a = match parts.get(3) {
            Some(alpha) => {
                (alpha.trim().parse::<f64>().ok()?.clamp(0.0, 1.0) * 255.0).round() as u8
            }
            None => 255,
        };
        let (r, g, b) = hsl_to_rgb(h, s, l);
        return Some(Rgba::new(r, g, b, a));
    }
    named(&text.to_ascii_lowercase())
}

fn strip_call<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(name)?.trim_start();
    rest.strip_prefix('(')?.strip_suffix(')')
}

fn split_args(args: &str) -> Vec<&str> {
    // Both `rgb(1, 2, 3)` and the CSS Color 4 `rgb(1 2 3 / 0.5)` turn up.
    args.split([',', '/'])
        .flat_map(|part| part.split_whitespace())
        .collect()
}

fn parse_channel(text: &str) -> Option<u8> {
    let text = text.trim();
    if let Some(percent) = text.strip_suffix('%') {
        let value = percent.parse::<f64>().ok()?;
        return Some((value / 100.0 * 255.0).clamp(0.0, 255.0).round() as u8);
    }
    Some(text.parse::<f64>().ok()?.clamp(0.0, 255.0).round() as u8)
}

fn parse_percent(text: &str) -> Option<f64> {
    let text = text.trim();
    let value = text.strip_suffix('%').unwrap_or(text).parse::<f64>().ok()?;
    Some((value / 100.0).clamp(0.0, 1.0))
}

fn parse_hex(hex: &str) -> Option<Rgba> {
    let digits: Vec<u8> = hex
        .chars()
        .map(|c| c.to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()?;
    match digits.len() {
        3 | 4 => {
            let at = |i: usize| digits[i] * 17;
            Some(Rgba::new(
                at(0),
                at(1),
                at(2),
                digits.get(3).map_or(255, |d| d * 17),
            ))
        }
        6 | 8 => {
            let at = |i: usize| digits[i] * 16 + digits[i + 1];
            Some(Rgba::new(
                at(0),
                at(2),
                at(4),
                if digits.len() == 8 { at(6) } else { 255 },
            ))
        }
        _ => None,
    }
}

/// The CSS named colours, which a style document is free to use.
///
/// The whole list rather than the handful that turn up in the basemaps we
/// looked at, because a name this cannot read is a colour MapLibre on iOS
/// cannot restore -- that provider parses the colour itself, so a name it
/// never sees is a mutation that quietly will not come off. A name that is
/// still not here is left untouched, which remains the safe way to be wrong.
fn named(name: &str) -> Option<Rgba> {
    let (r, g, b) = match name {
        "transparent" => return Some(Rgba::new(0, 0, 0, 0)),
        "aliceblue" => (240, 248, 255),
        "antiquewhite" => (250, 235, 215),
        "aqua" | "cyan" => (0, 255, 255),
        "aquamarine" => (127, 255, 212),
        "azure" => (240, 255, 255),
        "beige" => (245, 245, 220),
        "bisque" => (255, 228, 196),
        "black" => (0, 0, 0),
        "blanchedalmond" => (255, 235, 205),
        "blue" => (0, 0, 255),
        "blueviolet" => (138, 43, 226),
        "brown" => (165, 42, 42),
        "burlywood" => (222, 184, 135),
        "cadetblue" => (95, 158, 160),
        "chartreuse" => (127, 255, 0),
        "chocolate" => (210, 105, 30),
        "coral" => (255, 127, 80),
        "cornflowerblue" => (100, 149, 237),
        "cornsilk" => (255, 248, 220),
        "crimson" => (220, 20, 60),
        "darkblue" => (0, 0, 139),
        "darkcyan" => (0, 139, 139),
        "darkgoldenrod" => (184, 134, 11),
        "darkgray" | "darkgrey" => (169, 169, 169),
        "darkgreen" => (0, 100, 0),
        "darkkhaki" => (189, 183, 107),
        "darkmagenta" => (139, 0, 139),
        "darkolivegreen" => (85, 107, 47),
        "darkorange" => (255, 140, 0),
        "darkorchid" => (153, 50, 204),
        "darkred" => (139, 0, 0),
        "darksalmon" => (233, 150, 122),
        "darkseagreen" => (143, 188, 143),
        "darkslateblue" => (72, 61, 139),
        "darkslategray" | "darkslategrey" => (47, 79, 79),
        "darkturquoise" => (0, 206, 209),
        "darkviolet" => (148, 0, 211),
        "deeppink" => (255, 20, 147),
        "deepskyblue" => (0, 191, 255),
        "dimgray" | "dimgrey" => (105, 105, 105),
        "dodgerblue" => (30, 144, 255),
        "firebrick" => (178, 34, 34),
        "floralwhite" => (255, 250, 240),
        "forestgreen" => (34, 139, 34),
        "fuchsia" | "magenta" => (255, 0, 255),
        "gainsboro" => (220, 220, 220),
        "ghostwhite" => (248, 248, 255),
        "gold" => (255, 215, 0),
        "goldenrod" => (218, 165, 32),
        "gray" | "grey" => (128, 128, 128),
        "green" => (0, 128, 0),
        "greenyellow" => (173, 255, 47),
        "honeydew" => (240, 255, 240),
        "hotpink" => (255, 105, 180),
        "indianred" => (205, 92, 92),
        "indigo" => (75, 0, 130),
        "ivory" => (255, 255, 240),
        "khaki" => (240, 230, 140),
        "lavender" => (230, 230, 250),
        "lavenderblush" => (255, 240, 245),
        "lawngreen" => (124, 252, 0),
        "lemonchiffon" => (255, 250, 205),
        "lightblue" => (173, 216, 230),
        "lightcoral" => (240, 128, 128),
        "lightcyan" => (224, 255, 255),
        "lightgoldenrodyellow" => (250, 250, 210),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "lightgreen" => (144, 238, 144),
        "lightpink" => (255, 182, 193),
        "lightsalmon" => (255, 160, 122),
        "lightseagreen" => (32, 178, 170),
        "lightskyblue" => (135, 206, 250),
        "lightslategray" | "lightslategrey" => (119, 136, 153),
        "lightsteelblue" => (176, 196, 222),
        "lightyellow" => (255, 255, 224),
        "lime" => (0, 255, 0),
        "limegreen" => (50, 205, 50),
        "linen" => (250, 240, 230),
        "maroon" => (128, 0, 0),
        "mediumaquamarine" => (102, 205, 170),
        "mediumblue" => (0, 0, 205),
        "mediumorchid" => (186, 85, 211),
        "mediumpurple" => (147, 112, 219),
        "mediumseagreen" => (60, 179, 113),
        "mediumslateblue" => (123, 104, 238),
        "mediumspringgreen" => (0, 250, 154),
        "mediumturquoise" => (72, 209, 204),
        "mediumvioletred" => (199, 21, 133),
        "midnightblue" => (25, 25, 112),
        "mintcream" => (245, 255, 250),
        "mistyrose" => (255, 228, 225),
        "moccasin" => (255, 228, 181),
        "navajowhite" => (255, 222, 173),
        "navy" => (0, 0, 128),
        "oldlace" => (253, 245, 230),
        "olive" => (128, 128, 0),
        "olivedrab" => (107, 142, 35),
        "orange" => (255, 165, 0),
        "orangered" => (255, 69, 0),
        "orchid" => (218, 112, 214),
        "palegoldenrod" => (238, 232, 170),
        "palegreen" => (152, 251, 152),
        "paleturquoise" => (175, 238, 238),
        "palevioletred" => (219, 112, 147),
        "papayawhip" => (255, 239, 213),
        "peachpuff" => (255, 218, 185),
        "peru" => (205, 133, 63),
        "pink" => (255, 192, 203),
        "plum" => (221, 160, 221),
        "powderblue" => (176, 224, 230),
        "purple" => (128, 0, 128),
        "rebeccapurple" => (102, 51, 153),
        "red" => (255, 0, 0),
        "rosybrown" => (188, 143, 143),
        "royalblue" => (65, 105, 225),
        "saddlebrown" => (139, 69, 19),
        "salmon" => (250, 128, 114),
        "sandybrown" => (244, 164, 96),
        "seagreen" => (46, 139, 87),
        "seashell" => (255, 245, 238),
        "sienna" => (160, 82, 45),
        "silver" => (192, 192, 192),
        "skyblue" => (135, 206, 235),
        "slateblue" => (106, 90, 205),
        "slategray" | "slategrey" => (112, 128, 144),
        "snow" => (255, 250, 250),
        "springgreen" => (0, 255, 127),
        "steelblue" => (70, 130, 180),
        "tan" => (210, 180, 140),
        "teal" => (0, 128, 128),
        "thistle" => (216, 191, 216),
        "tomato" => (255, 99, 71),
        "turquoise" => (64, 224, 208),
        "violet" => (238, 130, 238),
        "wheat" => (245, 222, 179),
        "white" => (255, 255, 255),
        "whitesmoke" => (245, 245, 245),
        "yellow" => (255, 255, 0),
        "yellowgreen" => (154, 205, 50),
        _ => return None,
    };
    Some(Rgba::new(r, g, b, 255))
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let h = ((h % 360.0) + 360.0) % 360.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to_u8 = |v: f64| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    (to_u8(r), to_u8(g), to_u8(b))
}

fn rgb_to_hsl(c: Rgba) -> (f64, f64, f64) {
    let (r, g, b) = (
        f64::from(c.r) / 255.0,
        f64::from(c.g) / 255.0,
        f64::from(c.b) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let delta = max - min;
    if delta.abs() < f64::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = delta / (1.0 - (2.0 * l - 1.0).abs());
    let h = if (max - r).abs() < f64::EPSILON {
        60.0 * (((g - b) / delta) % 6.0)
    } else if (max - g).abs() < f64::EPSILON {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (h, s, l)
}

/// Bends one colour, keeping its alpha.
///
/// Through HSL rather than RGB because the operations are about how a map
/// *reads* -- "mute this", "darken that" -- and doing them per channel turns
/// a saturated blue grey-brown instead of pale blue.
pub fn apply_filter(color: Rgba, filter: ColorFilter) -> Rgba {
    let (h, s, l) = rgb_to_hsl(color);
    let (h, s, l) = match filter {
        ColorFilter::Desaturate(amount) => (h, s * (1.0 - amount.clamp(0.0, 1.0)), l),
        ColorFilter::Darken(amount) => (h, s, l * (1.0 - amount.clamp(0.0, 1.0))),
        ColorFilter::Lighten(amount) => {
            let amount = amount.clamp(0.0, 1.0);
            (h, s, l + (1.0 - l) * amount)
        }
        ColorFilter::InvertLightness => (h, s, 1.0 - l),
        ColorFilter::Mix { target, amount } => {
            let amount = amount.clamp(0.0, 1.0);
            let blend = |a: u8, b: u8| {
                (f64::from(a) * (1.0 - amount) + f64::from(b) * amount).round() as u8
            };
            return Rgba::new(
                blend(color.r, target.r),
                blend(color.g, target.g),
                blend(color.b, target.b),
                color.a,
            );
        }
    };
    let (r, g, b) = hsl_to_rgb(h, s, l);
    Rgba::new(r, g, b, color.a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_forms_a_style_contains() {
        assert_eq!(parse("#fff"), Some(Rgba::new(255, 255, 255, 255)));
        assert_eq!(parse("#ffffff"), Some(Rgba::new(255, 255, 255, 255)));
        assert_eq!(parse("#00000080"), Some(Rgba::new(0, 0, 0, 128)));
        assert_eq!(parse("rgb(1, 2, 3)"), Some(Rgba::new(1, 2, 3, 255)));
        assert_eq!(parse("rgba(1,2,3,0.5)"), Some(Rgba::new(1, 2, 3, 128)));
        assert_eq!(parse("rgb(1 2 3 / 1)"), Some(Rgba::new(1, 2, 3, 255)));
        assert_eq!(parse("hsl(0, 100%, 50%)"), Some(Rgba::new(255, 0, 0, 255)));
        assert_eq!(parse("white"), Some(Rgba::new(255, 255, 255, 255)));
        // The whole CSS list, not a handful: an unknown name is a colour
        // MapLibre on iOS cannot put back.
        assert_eq!(parse("chartreuse"), Some(Rgba::new(127, 255, 0, 255)));
        assert_eq!(parse("rebeccapurple"), Some(Rgba::new(102, 51, 153, 255)));
        assert_eq!(parse("LightSlateGrey"), Some(Rgba::new(119, 136, 153, 255)));
        assert_eq!(parse("transparent"), Some(Rgba::new(0, 0, 0, 0)));
        assert_eq!(parse("  #f0f0f0 "), Some(Rgba::new(240, 240, 240, 255)));
    }

    /// A value this cannot read must come back `None` so the caller leaves it
    /// alone. Rewriting a colour it guessed at is worse than not rewriting.
    #[test]
    fn refuses_what_it_cannot_read() {
        for text in [
            "",
            "#ff",
            "#gggggg",
            // A name that is not CSS's. The whole CSS list is known, so
            // `chartreuse` no longer belongs here -- this is the shape of
            // thing a style might invent.
            "roadsurface",
            "var(--road)",
            "rgb(1, 2)",
            "color(display-p3 1 0 0)",
        ] {
            assert_eq!(parse(text), None, "should not have read {text:?}");
        }
    }

    #[test]
    fn writes_what_it_can_read_back() {
        for text in ["#123456", "rgba(1,2,3,0.5)", "#ffffff"] {
            let once = parse(text).expect("reads");
            let twice = parse(&once.to_style_text()).expect("reads its own output");
            assert_eq!(once, twice, "{text}");
        }
        assert_eq!(
            Rgba::new(1, 2, 3, 255).to_style_text(),
            "#010203",
            "opaque colours stay hex"
        );
    }

    #[test]
    fn filters_keep_the_alpha_and_move_the_right_way() {
        let blue = Rgba::new(0, 0, 255, 128);
        let darker = apply_filter(blue, ColorFilter::Darken(0.5));
        assert_eq!(darker.a, 128, "alpha is not a colour");
        assert!(darker.b < blue.b, "darker: {darker:?}");

        let grey = apply_filter(blue, ColorFilter::Desaturate(1.0));
        assert_eq!(grey.r, grey.g, "fully desaturated is grey: {grey:?}");
        assert_eq!(grey.g, grey.b);

        let flipped = apply_filter(Rgba::new(255, 255, 255, 255), ColorFilter::InvertLightness);
        assert_eq!(flipped, Rgba::new(0, 0, 0, 255), "white inverts to black");

        let mixed = apply_filter(
            Rgba::new(0, 0, 0, 255),
            ColorFilter::Mix {
                target: Rgba::new(255, 255, 255, 255),
                amount: 0.5,
            },
        );
        assert_eq!(mixed, Rgba::new(128, 128, 128, 255));
    }
}
