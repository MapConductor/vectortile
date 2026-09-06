//! Decoder for the SDF glyph PBFs a MapLibre style points at.
//!
//! A style carries a `glyphs` URL template like
//! `https://…/{fontstack}/{range}.pbf`, where a range is 256 codepoints. Each
//! response holds one fontstack's glyphs for that range, and each glyph is a
//! signed-distance field rather than a bitmap: one 8-bit channel where 192 is
//! the edge, values fall off either side, and any size can be drawn from the
//! same data by thresholding. That is what makes labels cheap — a few hundred
//! kilobytes covers a script at every zoom.
//!
//! Hand-rolled from the schema for the same reason [`crate::mvt`] is: the
//! schema is six fields, and a protobuf codegen crate would put `protoc` in
//! three CI pipelines.
//!
//! ```proto
//! message glyph {
//!     required uint32 id      = 1;
//!     optional bytes  bitmap  = 2;   // (width + 2*BUFFER) * (height + 2*BUFFER)
//!     required uint32 width   = 3;   // ink extent, without the buffer
//!     required uint32 height  = 4;
//!     required sint32 left    = 5;
//!     required sint32 top     = 6;
//!     required uint32 advance = 7;
//! }
//! message fontstack { required string name = 1; required string range = 2; repeated glyph glyphs = 3; }
//! message glyphs    { repeated fontstack stacks = 1; }
//! ```

use std::collections::HashMap;

use crate::mvt::{DecodeError, Reader};

type Result<T> = std::result::Result<T, DecodeError>;

/// Padding baked around every glyph's ink, in SDF pixels.
///
/// The field has to extend past the ink or a halo would be clipped at the
/// glyph's edge. Fixed by the tooling that produces these files.
pub const BUFFER: u32 = 3;

/// Em size the SDFs were rasterised at. Drawing at any other size scales them.
pub const GLYPH_SIZE: f32 = 24.0;

/// Distance value that marks the outline. Below is outside, above is inside.
pub const EDGE: f32 = 192.0;

/// How many distance units cover one SDF pixel, used to turn a distance back
/// into a coverage ramp when drawing.
pub const RANGE: f32 = 8.0;

#[derive(Debug, Clone)]
pub struct Glyph {
    pub id: u32,
    /// SDF, `(width + 2 * BUFFER) * (height + 2 * BUFFER)` bytes, row-major.
    /// Empty for glyphs with no ink, such as a space.
    pub bitmap: Vec<u8>,
    /// Ink extent, without the buffer.
    pub width: u32,
    pub height: u32,
    /// Ink origin relative to the pen position, y up.
    pub left: i32,
    pub top: i32,
    /// How far to move the pen after drawing, in SDF pixels.
    pub advance: u32,
}

impl Glyph {
    /// Width of the stored bitmap, including the buffer on both sides.
    pub fn bitmap_width(&self) -> u32 {
        if self.bitmap.is_empty() {
            0
        } else {
            self.width + 2 * BUFFER
        }
    }

    pub fn bitmap_height(&self) -> u32 {
        if self.bitmap.is_empty() {
            0
        } else {
            self.height + 2 * BUFFER
        }
    }
}

/// One fontstack's glyphs for one 256-codepoint range.
#[derive(Debug, Clone, Default)]
pub struct FontStack {
    pub name: String,
    pub range: String,
    pub glyphs: HashMap<u32, Glyph>,
}

/// Decodes one `.pbf` response. A response may carry more than one stack.
pub fn decode(buf: &[u8]) -> Result<Vec<FontStack>> {
    let mut reader = Reader::new(buf);
    let mut stacks = Vec::new();
    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, 2) => stacks.push(decode_fontstack(reader.length_delimited()?)?),
            _ => reader.skip(wire)?,
        }
    }
    Ok(stacks)
}

fn decode_fontstack(buf: &[u8]) -> Result<FontStack> {
    let mut reader = Reader::new(buf);
    let mut stack = FontStack::default();
    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, 2) => stack.name = String::from_utf8_lossy(reader.length_delimited()?).into_owned(),
            (2, 2) => {
                stack.range = String::from_utf8_lossy(reader.length_delimited()?).into_owned()
            }
            (3, 2) => {
                let glyph = decode_glyph(reader.length_delimited()?)?;
                stack.glyphs.insert(glyph.id, glyph);
            }
            _ => reader.skip(wire)?,
        }
    }
    Ok(stack)
}

fn decode_glyph(buf: &[u8]) -> Result<Glyph> {
    let mut reader = Reader::new(buf);
    let mut glyph = Glyph {
        id: 0,
        bitmap: Vec::new(),
        width: 0,
        height: 0,
        left: 0,
        top: 0,
        advance: 0,
    };
    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, 0) => glyph.id = reader.varint()? as u32,
            (2, 2) => glyph.bitmap = reader.length_delimited()?.to_vec(),
            (3, 0) => glyph.width = reader.varint()? as u32,
            (4, 0) => glyph.height = reader.varint()? as u32,
            // sint32: zigzag, so -1 travels as 1 rather than as ten bytes of
            // sign extension.
            (5, 0) => glyph.left = unzigzag(reader.varint()?),
            (6, 0) => glyph.top = unzigzag(reader.varint()?),
            (7, 0) => glyph.advance = reader.varint()? as u32,
            _ => reader.skip(wire)?,
        }
    }

    // A glyph whose bitmap does not match its declared extent would read off
    // the end of the buffer while drawing. Dropping the bitmap keeps the
    // metrics, so the text still advances and only the ink is missing.
    let expected = ((glyph.width + 2 * BUFFER) * (glyph.height + 2 * BUFFER)) as usize;
    if !glyph.bitmap.is_empty() && glyph.bitmap.len() != expected {
        return Err(DecodeError("glyph bitmap does not match its extent"));
    }
    Ok(glyph)
}

fn unzigzag(value: u64) -> i32 {
    ((value >> 1) as i64 ^ -((value & 1) as i64)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a PBF by hand so the decoder is tested against the wire format
    /// rather than against another copy of itself.
    fn encode_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn field(out: &mut Vec<u8>, number: u32, wire: u8) {
        encode_varint(out, ((number as u64) << 3) | wire as u64);
    }

    fn varint_field(out: &mut Vec<u8>, number: u32, value: u64) {
        field(out, number, 0);
        encode_varint(out, value);
    }

    fn bytes_field(out: &mut Vec<u8>, number: u32, value: &[u8]) {
        field(out, number, 2);
        encode_varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }

    fn zigzag(value: i32) -> u64 {
        ((value << 1) ^ (value >> 31)) as u32 as u64
    }

    fn glyph_message(
        id: u32,
        width: u32,
        height: u32,
        left: i32,
        top: i32,
        advance: u32,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        varint_field(&mut out, 1, id as u64);
        let size = ((width + 2 * BUFFER) * (height + 2 * BUFFER)) as usize;
        bytes_field(&mut out, 2, &vec![7u8; size]);
        varint_field(&mut out, 3, width as u64);
        varint_field(&mut out, 4, height as u64);
        varint_field(&mut out, 5, zigzag(left));
        varint_field(&mut out, 6, zigzag(top));
        varint_field(&mut out, 7, advance as u64);
        out
    }

    fn pbf(stack_name: &str, range: &str, glyphs: &[Vec<u8>]) -> Vec<u8> {
        let mut stack = Vec::new();
        bytes_field(&mut stack, 1, stack_name.as_bytes());
        bytes_field(&mut stack, 2, range.as_bytes());
        for g in glyphs {
            bytes_field(&mut stack, 3, g);
        }
        let mut out = Vec::new();
        bytes_field(&mut out, 1, &stack);
        out
    }

    #[test]
    fn decodes_a_stack_of_glyphs() {
        let data = pbf(
            "Noto Sans Regular",
            "0-255",
            &[
                glyph_message(65, 10, 14, 1, 12, 11),
                glyph_message(66, 9, 14, 1, 12, 10),
            ],
        );
        let stacks = decode(&data).expect("decodes");
        assert_eq!(stacks.len(), 1);
        let stack = &stacks[0];
        assert_eq!(stack.name, "Noto Sans Regular");
        assert_eq!(stack.range, "0-255");
        assert_eq!(stack.glyphs.len(), 2);

        let a = &stack.glyphs[&65];
        assert_eq!((a.width, a.height, a.advance), (10, 14, 11));
        assert_eq!(a.bitmap_width(), 10 + 2 * BUFFER);
        assert_eq!(a.bitmap_height(), 14 + 2 * BUFFER);
        assert_eq!(
            a.bitmap.len(),
            (a.bitmap_width() * a.bitmap_height()) as usize
        );
    }

    /// `left` and `top` are sint32 and routinely negative — a glyph that hangs
    /// below the baseline has a negative `top`. Reading them as plain varints
    /// puts the ink somewhere else entirely, and nothing throws.
    #[test]
    fn reads_negative_metrics() {
        let data = pbf("F", "0-255", &[glyph_message(103, 8, 12, -2, -3, 9)]);
        let stacks = decode(&data).expect("decodes");
        let g = &stacks[0].glyphs[&103];
        assert_eq!(g.left, -2);
        assert_eq!(g.top, -3);
    }

    /// A space has metrics and no ink. It has to keep advancing the pen.
    #[test]
    fn keeps_glyphs_with_no_ink() {
        let mut message = Vec::new();
        varint_field(&mut message, 1, 32);
        varint_field(&mut message, 3, 0);
        varint_field(&mut message, 4, 0);
        varint_field(&mut message, 5, zigzag(0));
        varint_field(&mut message, 6, zigzag(0));
        varint_field(&mut message, 7, 6);
        let data = pbf("F", "0-255", &[message]);

        let stacks = decode(&data).expect("decodes");
        let space = &stacks[0].glyphs[&32];
        assert!(space.bitmap.is_empty());
        assert_eq!(space.bitmap_width(), 0);
        assert_eq!(space.advance, 6);
    }

    /// A bitmap that disagrees with its own extent would be read off the end
    /// while drawing, so it is rejected at the door.
    #[test]
    fn rejects_a_bitmap_that_does_not_match_its_extent() {
        let mut message = Vec::new();
        varint_field(&mut message, 1, 65);
        bytes_field(&mut message, 2, &[0u8; 4]);
        varint_field(&mut message, 3, 10);
        varint_field(&mut message, 4, 14);
        varint_field(&mut message, 7, 11);
        let data = pbf("F", "0-255", &[message]);

        assert!(decode(&data).is_err());
    }

    /// Unknown fields turn up when a server adds something; they must be
    /// skipped rather than derailing the parse.
    #[test]
    fn skips_unknown_fields() {
        let mut message = glyph_message(65, 4, 4, 0, 4, 5);
        varint_field(&mut message, 15, 12345);
        bytes_field(&mut message, 16, b"whatever");
        let data = pbf("F", "0-255", &[message]);

        let stacks = decode(&data).expect("decodes");
        assert_eq!(stacks[0].glyphs[&65].advance, 5);
    }
}
