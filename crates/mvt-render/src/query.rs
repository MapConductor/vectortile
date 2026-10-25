//! Features in a tile, looked up rather than drawn.
//!
//! A vector tile is data with a picture in it, not the other way round: every
//! feature the renderer paints carries its properties and its shape. This
//! module reads them back out -- for "what is on screen", "what is under the
//! finger", or a search box -- so a host can answer those on any backend and
//! offline, from the same tiles it draws.
//!
//! Scope, on purpose: this walks the tiles it is handed. It is a viewport's
//! worth of features, not a gazetteer. Anything beyond a few dozen tiles
//! belongs in an index the host builds from packages.

use crate::filter::CompiledFilter;
use crate::mercator;
use crate::model::{Feature, GeomType, Layer, Value};
use serde_json::Value as Json;

/// What to look for.
#[derive(Debug, Clone, Default)]
pub struct FeatureQuery {
    /// Only this source-layer (`"pois"`, `"streets"`...). None: every layer.
    pub source_layer: Option<String>,
    /// A MapLibre filter expression, as JSON. None: every feature.
    pub filter: Option<Json>,
    /// A case-insensitive substring that some string property must contain.
    /// The common search box. None: no text test.
    pub text: Option<String>,
    /// Stop after this many hits per tile. 0: no limit.
    pub limit: usize,
}

/// One feature found.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureHit {
    pub source_id: String,
    /// The source-layer the feature is in.
    pub layer: String,
    pub id: Option<u64>,
    pub geom_type: GeomType,
    /// Properties in the tile's own storage order.
    pub properties: Vec<(String, Value)>,
    /// A point to hang the feature on: the point itself, the middle vertex of
    /// a line, the centre of a polygon's box.
    pub lon: f64,
    pub lat: f64,
    /// West, south, east, north, degrees.
    pub bounds: [f64; 4],
}

/// Where a source tile's units land on the world: `(z, x, y, extent)`.
#[derive(Debug, Clone, Copy)]
pub struct TileFrame {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub extent: u32,
}

impl TileFrame {
    /// Tile-local units to (lon, lat).
    pub fn to_lonlat(&self, px: f32, py: f32) -> (f64, f64) {
        let n = (1u64 << self.z) as f64;
        let wx = (self.x as f64 + px as f64 / self.extent as f64) / n;
        let wy = (self.y as f64 + py as f64 / self.extent as f64) / n;
        (mercator::inv_merc_x(wx), mercator::inv_merc_y(wy))
    }
}

/// Everything in one decoded layer that the query asks for.
///
/// `window` is the part of the tile that belongs to the target: the whole
/// tile normally, one quadrant of a magnified ancestor. Features outside it
/// belong to a sibling and are that sibling's to report.
pub fn find_in_layer(
    source_id: &str,
    layer: &Layer,
    frame: TileFrame,
    window: [f32; 4],
    query: &FeatureQuery,
    zoom: f64,
    out: &mut Vec<FeatureHit>,
) {
    if let Some(wanted) = &query.source_layer {
        if &layer.name != wanted {
            return;
        }
    }
    let filter = CompiledFilter::compile(query.filter.as_ref(), layer, zoom);
    if filter.never_matches() {
        return;
    }
    // The text test is folded over the layer's value table once, like a
    // filter key: a feature then matches if any of its values does.
    let text_truth: Option<Vec<bool>> = query.text.as_deref().map(|needle| {
        let needle = needle.to_lowercase();
        layer
            .values()
            .iter()
            .map(|value| match value {
                Value::String(s) => s.to_lowercase().contains(&needle),
                _ => false,
            })
            .collect()
    });
    let names = layer.property_names();
    let values = layer.values();
    for feature in &layer.features {
        if query.limit > 0 && out.len() >= query.limit {
            return;
        }
        if !feature.touches_window(window) {
            continue;
        }
        if let Some(truth) = &text_truth {
            let hit = feature
                .prop_pairs()
                .any(|(_, v)| truth.get(v as usize).copied().unwrap_or(false));
            if !hit {
                continue;
            }
        }
        if !filter.matches(layer, feature) {
            continue;
        }
        let properties = feature
            .prop_pairs()
            .filter_map(|(k, v)| {
                Some((
                    names.get(k as usize)?.clone(),
                    values.get(v as usize)?.clone(),
                ))
            })
            .collect();
        let (px, py) = anchor(feature);
        let (lon, lat) = frame.to_lonlat(px, py);
        let b = feature.bounds();
        let (west, north) = frame.to_lonlat(b[0], b[1]);
        let (east, south) = frame.to_lonlat(b[2], b[3]);
        out.push(FeatureHit {
            source_id: source_id.to_string(),
            layer: layer.name.clone(),
            id: feature.id,
            geom_type: feature.geom_type,
            properties,
            lon,
            lat,
            bounds: [west, south, east, north],
        });
    }
}

/// The point a hit is reported at, in tile units.
fn anchor(feature: &Feature) -> (f32, f32) {
    match feature.geom_type {
        GeomType::LineString => {
            let part = feature.parts.first().map(Vec::as_slice).unwrap_or(&[]);
            if part.is_empty() {
                let b = feature.bounds();
                return ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
            }
            let mid = part[part.len() / 2];
            (mid[0], mid[1])
        }
        GeomType::Point => feature
            .parts
            .first()
            .and_then(|p| p.first())
            .map(|p| (p[0], p[1]))
            .unwrap_or((0.0, 0.0)),
        _ => {
            let b = feature.bounds();
            ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0)
        }
    }
}

/// Hits as the JSON the bindings hand across: an array of objects with
/// `sourceId`, `layer`, `id`, `geometry`, `lon`, `lat`, `bounds` and
/// `properties`.
pub fn hits_to_json(hits: &[FeatureHit]) -> String {
    let array: Vec<Json> = hits
        .iter()
        .map(|hit| {
            let properties: serde_json::Map<String, Json> = hit
                .properties
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            serde_json::json!({
                "sourceId": hit.source_id,
                "layer": hit.layer,
                "id": hit.id,
                "geometry": match hit.geom_type {
                    GeomType::Point => "Point",
                    GeomType::LineString => "LineString",
                    GeomType::Polygon => "Polygon",
                    GeomType::Unknown => "Unknown",
                },
                "lon": hit.lon,
                "lat": hit.lat,
                "bounds": hit.bounds,
                "properties": properties,
            })
        })
        .collect();
    Json::Array(array).to_string()
}

fn value_to_json(value: &Value) -> Json {
    match value {
        Value::String(s) => Json::String(s.clone()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::Int(i) => Json::from(*i),
        Value::Uint(u) => Json::from(*u),
        Value::Bool(b) => Json::Bool(*b),
    }
}
