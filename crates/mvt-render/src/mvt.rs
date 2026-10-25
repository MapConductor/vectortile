//! Minimal Mapbox Vector Tile decoder.
//!
//! Hand-rolled rather than generated from the `.proto` on purpose: the MVT
//! schema is tiny, and depending on a protobuf codegen crate would drag a
//! build-time `protoc` requirement into three CI pipelines (Android, iOS,
//! wasm). The earlier `vector-tile-rendering` prototype hit exactly that
//! friction — see its WORKLOG step 8.
//!
//! This module knows about protobuf and nothing else. It produces
//! [`crate::model::Tile`], which is what the rest of the crate draws from.

use crate::model::{Feature, GeomType, Layer, Parts, Tile, Value};

#[derive(Debug)]
pub struct DecodeError(pub &'static str);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mvt decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

type Result<T> = std::result::Result<T, DecodeError>;

// ---------------------------------------------------------------------------
// protobuf primitives
// ---------------------------------------------------------------------------

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub(crate) fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub(crate) fn varint(&mut self) -> Result<u64> {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            if self.pos >= self.buf.len() {
                return Err(DecodeError("truncated varint"));
            }
            if shift > 63 {
                return Err(DecodeError("varint overflow"));
            }
            let byte = self.buf[self.pos];
            self.pos += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    pub(crate) fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(DecodeError("length overflow"))?;
        if end > self.buf.len() {
            return Err(DecodeError("truncated field"));
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub(crate) fn length_delimited(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()? as usize;
        self.bytes(len)
    }

    pub(crate) fn fixed32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn fixed64(&mut self) -> Result<u64> {
        let b = self.bytes(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Reads a tag, returning (field_number, wire_type).
    pub(crate) fn tag(&mut self) -> Result<(u32, u8)> {
        let key = self.varint()?;
        Ok(((key >> 3) as u32, (key & 0x7) as u8))
    }

    /// Skips a field whose tag has already been consumed.
    pub(crate) fn skip(&mut self, wire_type: u8) -> Result<()> {
        match wire_type {
            0 => {
                self.varint()?;
            }
            1 => {
                self.fixed64()?;
            }
            2 => {
                self.length_delimited()?;
            }
            5 => {
                self.fixed32()?;
            }
            _ => return Err(DecodeError("unsupported wire type")),
        }
        Ok(())
    }
}

fn zigzag(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

// ---------------------------------------------------------------------------
// MVT messages
// ---------------------------------------------------------------------------

/// Decodes a `.pbf` vector tile.
pub fn decode(buf: &[u8]) -> Result<Tile> {
    decode_filtered(buf, None)
}

/// Decodes only the layers named, and skips the rest without reading them.
///
/// A tile is mostly roads and buildings by weight, and the label pass wants
/// neither. When a tile is decoded only to find the labels near its edge --
/// which is what the eight neighbours of every tile are decoded for -- paying
/// for the geometry as well is most of the cost for none of the answer.
pub fn decode_only(buf: &[u8], wanted: &[String]) -> Result<Tile> {
    decode_filtered(buf, Some(wanted))
}

fn decode_filtered(buf: &[u8], wanted: Option<&[String]>) -> Result<Tile> {
    let mut reader = Reader::new(buf);
    let mut tile = Tile::default();
    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            // Tile.layers
            (3, 2) => {
                let bytes = reader.length_delimited()?;
                if let Some(wanted) = wanted {
                    // The name is the layer's first field, so this reads a
                    // few bytes and moves on.
                    match layer_name(bytes) {
                        Some(name) if wanted.iter().any(|w| w == &name) => {}
                        _ => continue,
                    }
                }
                tile.layers.push(decode_layer(bytes)?);
            }
            _ => reader.skip(wire)?,
        }
    }
    crate::seams::align_shortbread(&mut tile);
    Ok(tile)
}

/// Reads just a layer's name out of its bytes.
fn layer_name(buf: &[u8]) -> Option<String> {
    let mut reader = Reader::new(buf);
    while !reader.done() {
        let (field, wire) = reader.tag().ok()?;
        if field == 1 && wire == 2 {
            let bytes = reader.length_delimited().ok()?;
            return Some(String::from_utf8_lossy(bytes).into_owned());
        }
        reader.skip(wire).ok()?;
    }
    None
}

fn decode_layer(buf: &[u8]) -> Result<Layer> {
    let mut reader = Reader::new(buf);
    let mut name = String::new();
    let mut extent = 4096u32;
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut raw_features: Vec<&[u8]> = Vec::new();

    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, 2) => {
                let bytes = reader.length_delimited()?;
                name = String::from_utf8_lossy(bytes).into_owned();
            }
            (2, 2) => raw_features.push(reader.length_delimited()?),
            (3, 2) => {
                let bytes = reader.length_delimited()?;
                keys.push(String::from_utf8_lossy(bytes).into_owned());
            }
            (4, 2) => {
                let bytes = reader.length_delimited()?;
                values.push(decode_value(bytes)?);
            }
            (5, 0) => extent = reader.varint()? as u32,
            _ => reader.skip(wire)?,
        }
    }

    if extent == 0 {
        return Err(DecodeError("layer extent is zero"));
    }

    let mut features = Vec::with_capacity(raw_features.len());
    for bytes in raw_features {
        features.push(decode_feature(bytes)?);
    }

    // MVT's key/value tables are exactly the shape the model wants, so this
    // hands them over without an intermediate representation.
    Ok(Layer::from_tables(name, extent, keys, values, features))
}

fn decode_value(buf: &[u8]) -> Result<Value> {
    let mut reader = Reader::new(buf);
    let mut value = Value::Bool(false);
    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, 2) => {
                let bytes = reader.length_delimited()?;
                value = Value::String(String::from_utf8_lossy(bytes).into_owned());
            }
            (2, 5) => value = Value::Float(f32::from_bits(reader.fixed32()?) as f64),
            (3, 1) => value = Value::Float(f64::from_bits(reader.fixed64()?)),
            (4, 0) => value = Value::Int(reader.varint()? as i64),
            (5, 0) => value = Value::Uint(reader.varint()?),
            (6, 0) => value = Value::Int(zigzag(reader.varint()?)),
            (7, 0) => value = Value::Bool(reader.varint()? != 0),
            _ => reader.skip(wire)?,
        }
    }
    Ok(value)
}

fn decode_feature(buf: &[u8]) -> Result<Feature> {
    let mut reader = Reader::new(buf);
    let mut id = None;
    let mut tags = Vec::new();
    let mut geom_type = GeomType::Unknown;
    let mut geometry: Vec<u32> = Vec::new();

    while !reader.done() {
        let (field, wire) = reader.tag()?;
        match (field, wire) {
            (1, 0) => id = Some(reader.varint()?),
            // Packed, but tolerate the unpacked encoding some writers emit.
            (2, 2) => {
                let mut inner = Reader::new(reader.length_delimited()?);
                while !inner.done() {
                    tags.push(inner.varint()? as u32);
                }
            }
            (2, 0) => tags.push(reader.varint()? as u32),
            (3, 0) => {
                geom_type = match reader.varint()? {
                    1 => GeomType::Point,
                    2 => GeomType::LineString,
                    3 => GeomType::Polygon,
                    _ => GeomType::Unknown,
                }
            }
            (4, 2) => {
                let mut inner = Reader::new(reader.length_delimited()?);
                while !inner.done() {
                    geometry.push(inner.varint()? as u32);
                }
            }
            (4, 0) => geometry.push(reader.varint()? as u32),
            _ => reader.skip(wire)?,
        }
    }

    if tags.len() % 2 != 0 {
        return Err(DecodeError("odd number of feature tags"));
    }

    let parts = decode_geometry(&geometry, geom_type)?;
    Ok(Feature::new(id, geom_type, parts, tags))
}

/// Decodes the command/parameter integer stream into cursor-relative parts.
fn decode_geometry(data: &[u32], geom_type: GeomType) -> Result<Parts> {
    let mut parts: Parts = Vec::new();
    let mut current: Vec<[f32; 2]> = Vec::new();
    let (mut cx, mut cy) = (0i32, 0i32);
    let mut i = 0usize;

    while i < data.len() {
        let command = data[i] & 0x7;
        let count = (data[i] >> 3) as usize;
        i += 1;

        match command {
            // MoveTo
            1 => {
                for _ in 0..count {
                    if i + 1 >= data.len() {
                        return Err(DecodeError("truncated MoveTo parameters"));
                    }
                    cx += zigzag(data[i] as u64) as i32;
                    cy += zigzag(data[i + 1] as u64) as i32;
                    i += 2;
                    // A MoveTo starts a new part, except for multipoints where
                    // each MoveTo pair is its own single-coordinate part.
                    if geom_type == GeomType::Point {
                        parts.push(vec![[cx as f32, cy as f32]]);
                    } else {
                        if !current.is_empty() {
                            parts.push(std::mem::take(&mut current));
                        }
                        current.push([cx as f32, cy as f32]);
                    }
                }
            }
            // LineTo
            2 => {
                for _ in 0..count {
                    if i + 1 >= data.len() {
                        return Err(DecodeError("truncated LineTo parameters"));
                    }
                    cx += zigzag(data[i] as u64) as i32;
                    cy += zigzag(data[i + 1] as u64) as i32;
                    i += 2;
                    current.push([cx as f32, cy as f32]);
                }
            }
            // ClosePath (no parameters)
            7 => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => return Err(DecodeError("unknown geometry command")),
        }
    }

    if !current.is_empty() {
        parts.push(current);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(value: u64, out: &mut Vec<u8>) {
        let mut v = value;
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
    }

    fn tag(field: u32, wire: u8, out: &mut Vec<u8>) {
        varint(((field as u64) << 3) | wire as u64, out);
    }

    fn delimited(field: u32, payload: &[u8], out: &mut Vec<u8>) {
        tag(field, 2, out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    fn zz(n: i32) -> u64 {
        ((n << 1) ^ (n >> 31)) as u32 as u64
    }

    /// Builds a one-layer tile holding a single square polygon with `kind=park`.
    fn fixture() -> Vec<u8> {
        let mut feature = Vec::new();
        tag(1, 0, &mut feature);
        varint(1, &mut feature); // id
        let mut tags_buf = Vec::new();
        varint(0, &mut tags_buf); // key[0]
        varint(0, &mut tags_buf); // value[0]
        delimited(2, &tags_buf, &mut feature);
        tag(3, 0, &mut feature);
        varint(3, &mut feature); // POLYGON

        let mut geom = Vec::new();
        varint((1 << 3) | 1, &mut geom); // MoveTo, count 1
        varint(zz(10), &mut geom);
        varint(zz(10), &mut geom);
        varint((3 << 3) | 2, &mut geom); // LineTo, count 3
        varint(zz(100), &mut geom);
        varint(zz(0), &mut geom);
        varint(zz(0), &mut geom);
        varint(zz(100), &mut geom);
        varint(zz(-100), &mut geom);
        varint(zz(0), &mut geom);
        varint(7 << 3 | 7, &mut geom); // ClosePath
        delimited(4, &geom, &mut feature);

        let mut layer = Vec::new();
        tag(15, 0, &mut layer);
        varint(2, &mut layer); // version
        delimited(1, b"landuse", &mut layer);
        delimited(2, &feature, &mut layer);
        delimited(3, b"kind", &mut layer);
        let mut value = Vec::new();
        delimited(1, b"park", &mut value);
        delimited(4, &value, &mut layer);
        tag(5, 0, &mut layer);
        varint(4096, &mut layer); // extent

        let mut tile = Vec::new();
        delimited(3, &layer, &mut tile);
        tile
    }

    #[test]
    fn decodes_a_layer_with_one_polygon() {
        let tile = decode(&fixture()).expect("decode");
        assert_eq!(tile.layers.len(), 1);
        let layer = tile.layer("landuse").expect("landuse layer");
        assert_eq!(layer.extent, 4096);
        assert_eq!(layer.features.len(), 1);

        let feature = &layer.features[0];
        assert_eq!(feature.id, Some(1));
        assert_eq!(feature.geom_type, GeomType::Polygon);
        assert_eq!(feature.parts.len(), 1);
        assert_eq!(
            feature.parts[0],
            vec![[10.0, 10.0], [110.0, 10.0], [110.0, 110.0], [10.0, 110.0]]
        );
    }

    #[test]
    fn resolves_properties_through_the_layer_tables() {
        let tile = decode(&fixture()).unwrap();
        let layer = tile.layer("landuse").unwrap();
        let feature = &layer.features[0];
        assert_eq!(
            layer.property(feature, "kind"),
            Some(&Value::String("park".into()))
        );
        assert_eq!(layer.property(feature, "missing"), None);
    }

    #[test]
    fn rejects_truncated_input() {
        let full = fixture();
        assert!(decode(&full[..full.len() / 2]).is_err());
    }

    #[test]
    fn zigzag_matches_the_spec() {
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(1), -1);
        assert_eq!(zigzag(2), 1);
        assert_eq!(zigzag(4294967294), 2147483647);
    }
}
