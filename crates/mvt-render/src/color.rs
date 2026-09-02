//! CSS colour parsing for style paint properties.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const TRANSPARENT: Color = Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };

    pub const BLACK: Color = Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };

    pub fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Color { r, g, b, a }
    }

    /// Linear interpolation in straight (non-premultiplied) RGBA.
    pub fn lerp(self, other: Color, t: f32) -> Color {
        Color {
            r: self.r + (other.r - self.r) * t,
            g: self.g + (other.g - self.g) * t,
            b: self.b + (other.b - self.b) * t,
            a: self.a + (other.a - self.a) * t,
        }
    }

    pub fn with_opacity(self, opacity: f32) -> Color {
        Color {
            a: self.a * opacity.clamp(0.0, 1.0),
            ..self
        }
    }

    pub fn to_tiny_skia(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba(
            self.r.clamp(0.0, 1.0),
            self.g.clamp(0.0, 1.0),
            self.b.clamp(0.0, 1.0),
            self.a.clamp(0.0, 1.0),
        )
        .unwrap_or(tiny_skia::Color::TRANSPARENT)
    }
}

/// Parses the colour notations the style spec allows: `#rgb`, `#rgba`,
/// `#rrggbb`, `#rrggbbaa`, `rgb()`, `rgba()`, `hsl()`, `hsla()`, plus the
/// handful of bare keywords that show up in real styles.
pub fn parse(input: &str) -> Option<Color> {
    let s = input.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex(hex);
    }
    if let Some(rest) = s.strip_prefix("rgba") {
        return parse_func(rest, true, false);
    }
    if let Some(rest) = s.strip_prefix("rgb") {
        return parse_func(rest, false, false);
    }
    if let Some(rest) = s.strip_prefix("hsla") {
        return parse_func(rest, true, true);
    }
    if let Some(rest) = s.strip_prefix("hsl") {
        return parse_func(rest, false, true);
    }
    parse_keyword(s)
}

fn parse_hex(hex: &str) -> Option<Color> {
    let nibble = |c: u8| -> Option<f32> {
        let v = (c as char).to_digit(16)? as f32;
        Some(v)
    };
    let bytes = hex.as_bytes();
    match bytes.len() {
        3 | 4 => {
            let mut ch = [1.0f32; 4];
            for (i, b) in bytes.iter().enumerate() {
                let v = nibble(*b)?;
                // #abc expands to #aabbcc
                ch[i] = (v * 16.0 + v) / 255.0;
            }
            Some(Color::new(ch[0], ch[1], ch[2], ch[3]))
        }
        6 | 8 => {
            let mut ch = [1.0f32; 4];
            for i in 0..bytes.len() / 2 {
                let hi = nibble(bytes[i * 2])?;
                let lo = nibble(bytes[i * 2 + 1])?;
                ch[i] = (hi * 16.0 + lo) / 255.0;
            }
            Some(Color::new(ch[0], ch[1], ch[2], ch[3]))
        }
        _ => None,
    }
}

fn parse_func(rest: &str, has_alpha: bool, is_hsl: bool) -> Option<Color> {
    let inner = rest.trim().strip_prefix('(')?.strip_suffix(')')?;
    let parts: Vec<&str> = inner
        .split(|c| c == ',' || c == '/' || c == ' ')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let expected = if has_alpha { 4 } else { 3 };
    if parts.len() < expected {
        return None;
    }

    let alpha = if has_alpha {
        parse_number(parts[3], 1.0)?
    } else {
        1.0
    };

    if is_hsl {
        let h = parse_number(parts[0], 360.0)? * 360.0;
        let s = parse_number(parts[1], 100.0)?;
        let l = parse_number(parts[2], 100.0)?;
        Some(hsl_to_rgb(h, s, l, alpha))
    } else {
        let r = parse_number(parts[0], 255.0)?;
        let g = parse_number(parts[1], 255.0)?;
        let b = parse_number(parts[2], 255.0)?;
        Some(Color::new(r, g, b, alpha))
    }
}

/// Parses one component, normalizing to 0..1. `scale` is the divisor used for
/// bare numbers; percentages always divide by 100.
fn parse_number(token: &str, scale: f32) -> Option<f32> {
    if let Some(pct) = token.strip_suffix('%') {
        return Some(pct.trim().parse::<f32>().ok()? / 100.0);
    }
    let raw = token.parse::<f32>().ok()?;
    Some(raw / scale)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32, a: f32) -> Color {
    let h = ((h % 360.0) + 360.0) % 360.0 / 360.0;
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    if s == 0.0 {
        return Color::new(l, l, l, a);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let channel = |mut t: f32| -> f32 {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    Color::new(
        channel(h + 1.0 / 3.0),
        channel(h),
        channel(h - 1.0 / 3.0),
        a,
    )
}

fn parse_keyword(name: &str) -> Option<Color> {
    let rgb = match name.to_ascii_lowercase().as_str() {
        "transparent" => return Some(Color::TRANSPARENT),
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "darkgray" | "darkgrey" => (169, 169, 169),
        "silver" => (192, 192, 192),
        "maroon" => (128, 0, 0),
        "olive" => (128, 128, 0),
        "navy" => (0, 0, 128),
        "purple" => (128, 0, 128),
        "teal" => (0, 128, 128),
        "orange" => (255, 165, 0),
        "brown" => (165, 42, 42),
        "pink" => (255, 192, 203),
        "beige" => (245, 245, 220),
        "wheat" => (245, 222, 179),
        _ => return None,
    };
    Some(Color::new(
        rgb.0 as f32 / 255.0,
        rgb.1 as f32 / 255.0,
        rgb.2 as f32 / 255.0,
        1.0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.005
    }

    #[test]
    fn parses_hex_forms() {
        let c = parse("#ff0000").unwrap();
        assert!(approx(c.r, 1.0) && approx(c.g, 0.0) && approx(c.a, 1.0));

        // Short form expands each nibble.
        let short = parse("#f00").unwrap();
        assert!(approx(short.r, 1.0) && approx(short.g, 0.0));

        let with_alpha = parse("#00ff0080").unwrap();
        assert!(approx(with_alpha.g, 1.0) && approx(with_alpha.a, 0.502));
    }

    #[test]
    fn parses_rgb_and_rgba() {
        let c = parse("rgb(255, 128, 0)").unwrap();
        assert!(approx(c.r, 1.0) && approx(c.g, 0.502) && approx(c.b, 0.0));

        let a = parse("rgba(0, 0, 255, 0.5)").unwrap();
        assert!(approx(a.b, 1.0) && approx(a.a, 0.5));
    }

    #[test]
    fn parses_hsl() {
        // hsl(0, 100%, 50%) is pure red.
        let c = parse("hsl(0, 100%, 50%)").unwrap();
        assert!(approx(c.r, 1.0) && approx(c.g, 0.0) && approx(c.b, 0.0));
    }

    #[test]
    fn parses_keywords_and_rejects_junk() {
        assert_eq!(parse("transparent"), Some(Color::TRANSPARENT));
        assert!(parse("white").is_some());
        assert!(parse("not-a-color").is_none());
        assert!(parse("#12345").is_none());
    }

    #[test]
    fn lerps_channelwise() {
        let mid = Color::new(0.0, 0.0, 0.0, 1.0).lerp(Color::new(1.0, 1.0, 1.0, 1.0), 0.5);
        assert!(approx(mid.r, 0.5) && approx(mid.a, 1.0));
    }
}
