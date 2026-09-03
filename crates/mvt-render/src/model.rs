//! The decoded tile model every renderer stage works against.
//!
//! This is the seam between *reading a tile format* and *drawing a map*. The
//! style parser, expression evaluator and rasteriser all consume these types
//! and know nothing about protobuf, MVT tags, or any other wire format. A
//! second decoder — MapLibre Tile is the obvious candidate — plugs in by
//! producing a [`Tile`], with nothing else in the crate changing.
//!
//! Properties are stored as tables owned by the [`Layer`], with features
//! holding index pairs into them. That is not an MVT detail leaking through:
//! it is how both MVT (key/value tables) and MLT (column names plus
//! dictionaries) already represent properties, and it keeps repeated strings
//! stored once per tile rather than once per feature.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeomType {
    Unknown,
    Point,
    LineString,
    Polygon,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Float(f64),
    Int(i64),
    Uint(u64),
    Bool(bool),
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(v) => Some(*v),
            Value::Int(v) => Some(*v as f64),
            Value::Uint(v) => Some(*v as f64),
            Value::Bool(v) => Some(if *v { 1.0 } else { 0.0 }),
            Value::String(_) => None,
        }
    }
}

/// A feature's geometry, in tile-local units (0..extent).
///
/// For points each part holds a single coordinate; for linestrings each part is
/// one line; for polygons each part is one ring, with winding direction
/// preserved so the non-zero fill rule cuts holes without classifying rings.
pub type Parts = Vec<Vec<[f32; 2]>>;

#[derive(Debug, Clone)]
pub struct Feature {
    pub id: Option<u64>,
    pub geom_type: GeomType,
    pub parts: Parts,
    /// Flat (name index, value index) pairs into the owning layer's tables.
    /// Private so the storage layout stays a decoder-side concern.
    props: Vec<u32>,
}

impl Feature {
    /// Builds a feature. `props` is flat (name index, value index) pairs into
    /// the tables the owning [`Layer`] is constructed with.
    pub fn new(id: Option<u64>, geom_type: GeomType, parts: Parts, props: Vec<u32>) -> Feature {
        debug_assert!(props.len() % 2 == 0, "props must be index pairs");
        Feature {
            id,
            geom_type,
            parts,
            props,
        }
    }

    pub fn property_count(&self) -> usize {
        self.props.len() / 2
    }
}

#[derive(Debug, Clone)]
pub struct Layer {
    pub name: String,
    pub extent: u32,
    pub features: Vec<Feature>,
    property_names: Vec<String>,
    values: Vec<Value>,
    name_index: HashMap<String, u32>,
}

impl Layer {
    /// Builds a layer from tables a decoder already has.
    ///
    /// Both supported wire formats produce properties this way, so no decoder
    /// has to invent an intermediate representation to call this.
    pub fn from_tables(
        name: String,
        extent: u32,
        property_names: Vec<String>,
        values: Vec<Value>,
        features: Vec<Feature>,
    ) -> Layer {
        let name_index = property_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i as u32))
            .collect();
        Layer {
            name,
            extent,
            features,
            property_names,
            values,
            name_index,
        }
    }

    /// Looks up a property on one of this layer's features.
    pub fn property<'a>(&'a self, feature: &Feature, key: &str) -> Option<&'a Value> {
        let wanted = *self.name_index.get(key)?;
        feature
            .props
            .chunks_exact(2)
            .find(|pair| pair[0] == wanted)
            .and_then(|pair| self.values.get(pair[1] as usize))
    }

    pub fn property_names(&self) -> &[String] {
        &self.property_names
    }
}

#[derive(Debug, Clone, Default)]
pub struct Tile {
    pub layers: Vec<Layer>,
}

impl Tile {
    pub fn layer(&self, name: &str) -> Option<&Layer> {
        self.layers.iter().find(|l| l.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for a decoder that is not MVT: it builds the model directly
    /// from tables, which is the whole point of this module existing.
    fn hand_built_layer() -> Layer {
        Layer::from_tables(
            "places".into(),
            4096,
            vec!["name".into(), "population".into()],
            vec![Value::String("Tokyo".into()), Value::Uint(13_960_000)],
            vec![
                Feature::new(
                    Some(1),
                    GeomType::Point,
                    vec![vec![[100.0, 200.0]]],
                    vec![0, 0, 1, 1],
                ),
                // A feature carrying only some of the layer's properties.
                Feature::new(Some(2), GeomType::Point, vec![vec![[1.0, 2.0]]], vec![0, 0]),
            ],
        )
    }

    #[test]
    fn resolves_properties_through_the_layer_tables() {
        let layer = hand_built_layer();
        let first = &layer.features[0];
        assert_eq!(
            layer.property(first, "name"),
            Some(&Value::String("Tokyo".into()))
        );
        assert_eq!(
            layer.property(first, "population"),
            Some(&Value::Uint(13_960_000))
        );
        assert_eq!(layer.property(first, "missing"), None);
    }

    #[test]
    fn a_feature_without_a_property_reports_none() {
        let layer = hand_built_layer();
        let second = &layer.features[1];
        assert_eq!(layer.property(second, "population"), None);
        assert_eq!(second.property_count(), 1);
    }

    #[test]
    fn values_convert_to_numbers_where_it_makes_sense() {
        assert_eq!(Value::Int(-3).as_f64(), Some(-3.0));
        assert_eq!(Value::Bool(true).as_f64(), Some(1.0));
        assert_eq!(Value::String("x".into()).as_f64(), None);
    }

    #[test]
    fn tiles_look_layers_up_by_name() {
        let tile = Tile {
            layers: vec![hand_built_layer()],
        };
        assert!(tile.layer("places").is_some());
        assert!(tile.layer("nope").is_none());
    }
}
