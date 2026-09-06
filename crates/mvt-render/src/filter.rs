//! Compiles layer filters against a tile's shared value tables.
//!
//! The interpreter walks the filter's JSON tree once per (layer, feature)
//! pair. On a street-level basemap tile that is ~500,000 evaluations to find
//! ~1,700 matching features — 0.3% hits, and two thirds of the entire per-tile
//! cost, dwarfing rasterisation itself.
//!
//! The fix rests on how MVT stores properties: every feature holds (key index,
//! value index) pairs into tables owned by the layer. Any predicate on a
//! property value — `==`, `in`, `<`, `has` and their negations — can therefore
//! be evaluated **once per table entry** at compile time, producing a truth
//! table over value indices. Per feature, matching is then a scan of its index
//! pairs and one boolean lookup: no strings, no JSON, no allocation.
//!
//! Two folds fall out for free and matter as much as the fast path:
//! - a filter naming a key the layer does not have collapses to a constant, so
//!   the whole layer is skipped without touching a single feature;
//! - so does a filter naming a value absent from the table (`kind == "ferry"`
//!   on a tile with no ferries).
//!
//! Anything the compiler does not recognise falls back to the interpreter for
//! that node, so coverage is exact by construction; the equivalence test at
//! the bottom pins the two against each other over every operator.

use serde_json::Value as Json;

use crate::expr::{self, Ctx, Val};
use crate::model::{Feature, GeomType, Layer};

/// A filter compiled against one data layer's tables.
pub enum CompiledFilter {
    Const(bool),
    /// Predicate on one property, precomputed over the layer's value table.
    Key {
        key: u32,
        /// truth[value index] — whether a feature carrying that value matches.
        truth: Vec<bool>,
        /// Whether a feature lacking the property matches (`!=`, `!has`, `!in`).
        missing: bool,
    },
    /// Predicate on `$type`, indexed by [`geom_rank`].
    Geometry([bool; 4]),
    All(Vec<CompiledFilter>),
    Any(Vec<CompiledFilter>),
    Not(Box<CompiledFilter>),
    /// Interpreter escape hatch for unrecognised shapes.
    Fallback(Json, f64),
}

fn geom_rank(geom: GeomType) -> usize {
    match geom {
        GeomType::Unknown => 0,
        GeomType::Point => 1,
        GeomType::LineString => 2,
        GeomType::Polygon => 3,
    }
}

/// What a comparison's left-hand side refers to.
enum Subject {
    Key(String),
    Geometry,
}

impl CompiledFilter {
    pub fn compile(filter: Option<&Json>, layer: &Layer, zoom: f64) -> CompiledFilter {
        match filter {
            None => CompiledFilter::Const(true),
            Some(json) => compile_node(json, layer, zoom),
        }
    }

    /// True when no feature of the layer can ever match — the whole feature
    /// loop can be skipped.
    pub fn never_matches(&self) -> bool {
        matches!(self, CompiledFilter::Const(false))
    }

    pub fn matches(&self, layer: &Layer, feature: &Feature) -> bool {
        match self {
            CompiledFilter::Const(value) => *value,
            CompiledFilter::Key {
                key,
                truth,
                missing,
            } => {
                for (k, v) in feature.prop_pairs() {
                    if k == *key {
                        return truth.get(v as usize).copied().unwrap_or(false);
                    }
                }
                *missing
            }
            CompiledFilter::Geometry(mask) => mask[geom_rank(feature.geom_type)],
            CompiledFilter::All(children) => children.iter().all(|c| c.matches(layer, feature)),
            CompiledFilter::Any(children) => children.iter().any(|c| c.matches(layer, feature)),
            CompiledFilter::Not(child) => !child.matches(layer, feature),
            CompiledFilter::Fallback(json, zoom) => {
                let ctx = Ctx {
                    zoom: *zoom,
                    layer: Some(layer),
                    feature: Some(feature),
                };
                expr::eval(json, &ctx).truthy()
            }
        }
    }
}

fn compile_node(json: &Json, layer: &Layer, zoom: f64) -> CompiledFilter {
    match json {
        Json::Bool(value) => CompiledFilter::Const(*value),
        Json::Array(items) => compile_array(items, json, layer, zoom),
        _ => CompiledFilter::Fallback(json.clone(), zoom),
    }
}

fn compile_array(items: &[Json], whole: &Json, layer: &Layer, zoom: f64) -> CompiledFilter {
    let Some(op) = items.first().and_then(Json::as_str) else {
        return CompiledFilter::Fallback(whole.clone(), zoom);
    };
    let args = &items[1..];

    match op {
        "all" => fold_all(args.iter().map(|a| compile_node(a, layer, zoom))),
        "any" => fold_any(args.iter().map(|a| compile_node(a, layer, zoom))),
        // `["none", a, b]` is `!(a || b)`.
        "none" => negate(fold_any(args.iter().map(|a| compile_node(a, layer, zoom)))),
        "!" if args.len() == 1 => negate(compile_node(&args[0], layer, zoom)),

        "==" | "!=" | "<" | "<=" | ">" | ">=" => compile_comparison(op, args, whole, layer, zoom),
        "in" | "!in" => compile_in(op == "!in", args, whole, layer, zoom),
        "has" | "!has" => compile_has(op == "!has", args, whole, layer, zoom),

        _ => CompiledFilter::Fallback(whole.clone(), zoom),
    }
}

fn negate(filter: CompiledFilter) -> CompiledFilter {
    match filter {
        CompiledFilter::Const(value) => CompiledFilter::Const(!value),
        CompiledFilter::Key {
            key,
            truth,
            missing,
        } => CompiledFilter::Key {
            key,
            truth: truth.into_iter().map(|t| !t).collect(),
            missing: !missing,
        },
        CompiledFilter::Geometry(mask) => {
            CompiledFilter::Geometry([!mask[0], !mask[1], !mask[2], !mask[3]])
        }
        other => CompiledFilter::Not(Box::new(other)),
    }
}

fn fold_all(children: impl Iterator<Item = CompiledFilter>) -> CompiledFilter {
    let mut kept = Vec::new();
    for child in children {
        match child {
            CompiledFilter::Const(true) => {}
            CompiledFilter::Const(false) => return CompiledFilter::Const(false),
            other => kept.push(other),
        }
    }
    match kept.len() {
        0 => CompiledFilter::Const(true),
        1 => kept.pop().unwrap(),
        _ => CompiledFilter::All(kept),
    }
}

fn fold_any(children: impl Iterator<Item = CompiledFilter>) -> CompiledFilter {
    let mut kept = Vec::new();
    for child in children {
        match child {
            CompiledFilter::Const(false) => {}
            CompiledFilter::Const(true) => return CompiledFilter::Const(true),
            other => kept.push(other),
        }
    }
    match kept.len() {
        0 => CompiledFilter::Const(false),
        1 => kept.pop().unwrap(),
        _ => CompiledFilter::Any(kept),
    }
}

/// Extracts what a comparison's first argument refers to. Mirrors the
/// interpreter's `operand()`: a bare string is a property name unless it is
/// `$type`; `["get", name]` and `["geometry-type"]` are their expression forms.
fn subject_of(json: &Json) -> Option<Subject> {
    match json {
        Json::String(name) => match name.as_str() {
            "$type" => Some(Subject::Geometry),
            "$id" => None,
            key => Some(Subject::Key(key.to_owned())),
        },
        Json::Array(items) => {
            let op = items.first()?.as_str()?;
            match (op, items.len()) {
                ("get", 2) => Some(Subject::Key(items[1].as_str()?.to_owned())),
                ("geometry-type", 1) => Some(Subject::Geometry),
                _ => None,
            }
        }
        _ => None,
    }
}

/// A literal right-hand side. Arrays and objects would be evaluated by the
/// interpreter, so anything non-scalar falls back.
fn literal_of(json: &Json) -> Option<Val> {
    match json {
        Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_) => {
            Some(Val::from_json(json))
        }
        _ => None,
    }
}

/// The interpreter's exact comparison semantics, reproduced for compile-time
/// use. Strings order as strings; everything else through `as_f64`, which also
/// parses numeric strings — so `"5" == 5` holds, as it does at runtime. The
/// equivalence test pins this against `expr::eval`.
fn compare(op: &str, lhs: &Val, rhs: &Val) -> bool {
    let ordering = match (lhs, rhs) {
        (Val::Str(a), Val::Str(b)) => a.partial_cmp(b),
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b),
            _ => None,
        },
    };
    match op {
        "==" => lhs == rhs || ordering == Some(std::cmp::Ordering::Equal),
        "!=" => !(lhs == rhs || ordering == Some(std::cmp::Ordering::Equal)),
        _ => match ordering {
            Some(o) => match op {
                "<" => o.is_lt(),
                ">" => o.is_gt(),
                "<=" => o.is_le(),
                ">=" => o.is_ge(),
                _ => false,
            },
            None => false,
        },
    }
}

fn geometry_val(rank: usize) -> Val {
    match rank {
        1 => Val::Str("Point".into()),
        2 => Val::Str("LineString".into()),
        3 => Val::Str("Polygon".into()),
        _ => Val::Null,
    }
}

/// Builds a `Key` predicate by evaluating `predicate` once per table value.
fn key_pred(
    layer: &Layer,
    key: &str,
    missing: bool,
    predicate: impl Fn(&Val) -> bool,
) -> CompiledFilter {
    let Some(key_idx) = layer.key_index(key) else {
        // No feature has the property, so every feature takes the missing
        // branch — the filter is a constant for this tile.
        return CompiledFilter::Const(missing);
    };
    let truth: Vec<bool> = layer
        .values()
        .iter()
        .map(|value| predicate(&Val::from_mvt(value)))
        .collect();
    if !missing && truth.iter().all(|t| !t) {
        // The value the filter wants does not occur in this tile at all.
        return CompiledFilter::Const(false);
    }
    CompiledFilter::Key {
        key: key_idx,
        truth,
        missing,
    }
}

fn geometry_pred(predicate: impl Fn(&Val) -> bool) -> CompiledFilter {
    let mask = [
        predicate(&geometry_val(0)),
        predicate(&geometry_val(1)),
        predicate(&geometry_val(2)),
        predicate(&geometry_val(3)),
    ];
    if mask.iter().all(|m| !m) {
        return CompiledFilter::Const(false);
    }
    CompiledFilter::Geometry(mask)
}

fn compile_comparison(
    op: &str,
    args: &[Json],
    whole: &Json,
    layer: &Layer,
    zoom: f64,
) -> CompiledFilter {
    if args.len() < 2 {
        // The interpreter returns false for a short comparison.
        return CompiledFilter::Const(false);
    }
    let (Some(subject), Some(rhs)) = (subject_of(&args[0]), literal_of(&args[1])) else {
        return CompiledFilter::Fallback(whole.clone(), zoom);
    };
    // A missing property compares as Null: equal to nothing, ordered with
    // nothing — so `!=` matches and everything else does not.
    let missing = compare(op, &Val::Null, &rhs);
    match subject {
        Subject::Key(key) => key_pred(layer, &key, missing, |v| compare(op, v, &rhs)),
        Subject::Geometry => geometry_pred(|v| compare(op, v, &rhs)),
    }
}

/// `in` has three interpreter forms, each with its own semantics:
/// legacy `["in", key, a, b, …]` (plain equality), expression
/// `["in", needle, ["literal", […]]]` (contains), and a string haystack
/// (substring). All three reduce to a per-value predicate.
fn compile_in(
    negated: bool,
    args: &[Json],
    whole: &Json,
    layer: &Layer,
    zoom: f64,
) -> CompiledFilter {
    if args.len() < 2 {
        return CompiledFilter::Const(negated);
    }
    let Some(subject) = subject_of(&args[0]) else {
        return CompiledFilter::Fallback(whole.clone(), zoom);
    };

    let predicate: Box<dyn Fn(&Val) -> bool> = if args.len() == 2 {
        match &args[1] {
            Json::Array(rhs_items) => {
                let is_literal_wrap = rhs_items.first().and_then(Json::as_str) == Some("literal");
                if is_literal_wrap {
                    let Some(Json::Array(list)) = rhs_items.get(1) else {
                        return CompiledFilter::Fallback(whole.clone(), zoom);
                    };
                    let items: Vec<Val> = list.iter().map(Val::from_json).collect();
                    Box::new(move |v| items.contains(v))
                } else if rhs_items.first().map(Json::is_string).unwrap_or(false) {
                    // The interpreter would treat this as an operator
                    // expression; let it.
                    return CompiledFilter::Fallback(whole.clone(), zoom);
                } else {
                    let items: Vec<Val> = rhs_items.iter().map(Val::from_json).collect();
                    Box::new(move |v| items.contains(v))
                }
            }
            Json::String(haystack) => {
                let haystack = haystack.clone();
                Box::new(move |v| match v {
                    Val::Str(needle) => haystack.contains(needle.as_str()),
                    _ => false,
                })
            }
            // Numbers etc. — the interpreter answers false.
            _ => Box::new(|_| false),
        }
    } else {
        // Legacy list: plain Val equality, no numeric cross-typing.
        let items: Vec<Val> = args[1..].iter().map(Val::from_json).collect();
        Box::new(move |v| items.contains(v))
    };

    let compiled = match subject {
        Subject::Key(key) => key_pred(layer, &key, false, |v| predicate(v)),
        Subject::Geometry => geometry_pred(|v| predicate(v)),
    };
    if negated {
        negate(compiled)
    } else {
        compiled
    }
}

fn compile_has(
    negated: bool,
    args: &[Json],
    whole: &Json,
    layer: &Layer,
    zoom: f64,
) -> CompiledFilter {
    // The interpreter's `has` looks the argument up as a property name
    // verbatim — `$type` gets no special treatment there.
    let Some(key) = args.first().and_then(Json::as_str) else {
        return CompiledFilter::Fallback(whole.clone(), zoom);
    };
    let compiled = key_pred(layer, key, false, |_| true);
    if negated {
        negate(compiled)
    } else {
        compiled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Feature, GeomType, Layer, Value};
    use serde_json::json;

    /// A layer with enough value-type variety to exercise every comparison
    /// path: strings, a numeric string, ints, floats, bools.
    fn layer() -> Layer {
        Layer::from_tables(
            "roads".into(),
            4096,
            vec!["kind".into(), "lanes".into(), "oneway".into(), "ref".into()],
            vec![
                Value::String("residential".into()),
                Value::String("motorway".into()),
                Value::String("5".into()),
                Value::Int(5),
                Value::Float(2.5),
                Value::Bool(true),
            ],
            vec![
                // kind=residential, lanes=5(int)
                Feature::new(
                    Some(1),
                    GeomType::LineString,
                    vec![vec![[0.0, 0.0]]],
                    vec![0, 0, 1, 3],
                ),
                // kind=motorway, lanes="5"(string), oneway=true
                Feature::new(
                    Some(2),
                    GeomType::LineString,
                    vec![vec![[0.0, 0.0]]],
                    vec![0, 1, 1, 2, 2, 5],
                ),
                // kind=2.5(float!), ref=residential
                Feature::new(
                    Some(3),
                    GeomType::Polygon,
                    vec![vec![[0.0, 0.0]]],
                    vec![0, 4, 3, 0],
                ),
                // no properties at all
                Feature::new(Some(4), GeomType::Point, vec![vec![[0.0, 0.0]]], vec![]),
            ],
        )
    }

    /// Every supported operator shape, plus edge cases the compiler folds.
    fn filters() -> Vec<serde_json::Value> {
        vec![
            json!(["==", "kind", "residential"]),
            json!(["==", ["get", "kind"], "motorway"]),
            json!(["!=", "kind", "residential"]),
            json!(["==", "lanes", 5]), // matches both Int 5 and Str "5"
            json!(["==", "lanes", "5"]),
            json!(["<", "kind", 3]), // float kind compares numerically
            json!([">=", ["get", "kind"], "m"]), // string ordering
            json!(["in", "kind", "residential", "motorway"]),
            json!(["!in", "kind", "residential"]),
            json!(["in", ["get", "kind"], ["literal", ["motorway"]]]),
            json!(["in", "kind", "a residential road"]), // substring haystack
            json!(["has", "oneway"]),
            json!(["!has", "oneway"]),
            json!(["has", "$type"]), // property literally named $type
            json!(["==", "$type", "Polygon"]),
            json!(["in", "$type", "Point", "LineString"]),
            json!(["all", ["==", "kind", "motorway"], ["has", "oneway"]]),
            json!(["any", ["==", "kind", "residential"], ["==", "kind", 2.5]]),
            json!(["none", ["has", "oneway"], ["==", "kind", "residential"]]),
            json!(["!", ["has", "lanes"]]),
            json!(["==", "absent-key", "x"]), // folds to Const(false)
            json!(["!=", "absent-key", "x"]), // folds to Const(true)
            json!(["==", "kind", "ferry"]),   // value absent from table
            json!(["==", ["get", "kind"], ["get", "ref"]]), // dynamic rhs -> fallback
            json!(["==", "$id", 2]),          // unsupported subject -> fallback
            json!(["all"]),
            json!(["any"]),
        ]
    }

    /// The one test that matters: for every filter and every feature, the
    /// compiled answer equals the interpreter's.
    #[test]
    fn compiled_filters_match_the_interpreter_exactly() {
        let layer = layer();
        for filter in filters() {
            let compiled = CompiledFilter::compile(Some(&filter), &layer, 14.0);
            for feature in &layer.features {
                let ctx = Ctx {
                    zoom: 14.0,
                    layer: Some(&layer),
                    feature: Some(feature),
                };
                let expected = expr::eval(&filter, &ctx).truthy();
                assert_eq!(
                    compiled.matches(&layer, feature),
                    expected,
                    "filter {filter} on feature {:?}",
                    feature.id
                );
            }
        }
    }

    #[test]
    fn impossible_filters_fold_to_constants() {
        let layer = layer();
        // Key absent from the layer: nothing to scan per feature.
        let absent = CompiledFilter::compile(Some(&json!(["==", "nope", "x"])), &layer, 14.0);
        assert!(absent.never_matches());

        // Value absent from the table: same fold, different reason.
        let ferry = CompiledFilter::compile(Some(&json!(["==", "kind", "ferry"])), &layer, 14.0);
        assert!(ferry.never_matches());

        // An `all` containing an impossible arm dies with it.
        let all = CompiledFilter::compile(
            Some(&json!([
                "all",
                ["==", "kind", "motorway"],
                ["==", "nope", "x"]
            ])),
            &layer,
            14.0,
        );
        assert!(all.never_matches());
    }

    #[test]
    fn no_filter_matches_everything() {
        let layer = layer();
        let compiled = CompiledFilter::compile(None, &layer, 14.0);
        for feature in &layer.features {
            assert!(compiled.matches(&layer, feature));
        }
    }
}
