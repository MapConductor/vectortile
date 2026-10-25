//! A working subset of the MapLibre style expression language.
//!
//! Expressions are interpreted straight off `serde_json::Value` rather than
//! compiled to a typed AST. That keeps this module small, which matters while
//! the renderer is still a foundation; a compile step is the obvious
//! optimisation once profiles say it is needed.
//!
//! Deliberately unsupported: `let`/`var`, string formatting, collators,
//! `within`, and everything symbol-related. Unknown operators evaluate to
//! `Null` instead of failing, so an unsupported property degrades to its
//! default rather than blanking the tile.

use serde_json::Value as Json;

use crate::color::{self, Color};
use crate::model::{Feature, GeomType, Layer, Value as TileValue};

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Color(Color),
    Arr(Vec<Val>),
}

impl Val {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Num(n) => Some(*n),
            Val::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Val::Str(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    /// MapLibre truthiness: `false`, `0`, `""`, `null` and NaN are falsey.
    pub fn truthy(&self) -> bool {
        match self {
            Val::Null => false,
            Val::Bool(b) => *b,
            Val::Num(n) => *n != 0.0 && !n.is_nan(),
            Val::Str(s) => !s.is_empty(),
            Val::Color(_) => true,
            Val::Arr(a) => !a.is_empty(),
        }
    }

    pub fn as_color(&self) -> Option<Color> {
        match self {
            Val::Color(c) => Some(*c),
            Val::Str(s) => color::parse(s),
            _ => None,
        }
    }

    pub(crate) fn from_mvt(value: &TileValue) -> Val {
        match value {
            TileValue::String(s) => Val::Str(s.clone()),
            TileValue::Float(f) => Val::Num(*f),
            TileValue::Int(i) => Val::Num(*i as f64),
            TileValue::Uint(u) => Val::Num(*u as f64),
            TileValue::Bool(b) => Val::Bool(*b),
        }
    }

    pub(crate) fn from_json(json: &Json) -> Val {
        match json {
            Json::Null => Val::Null,
            Json::Bool(b) => Val::Bool(*b),
            Json::Number(n) => Val::Num(n.as_f64().unwrap_or(0.0)),
            Json::String(s) => Val::Str(s.clone()),
            Json::Array(items) => Val::Arr(items.iter().map(Val::from_json).collect()),
            Json::Object(_) => Val::Null,
        }
    }
}

/// Whether an expression's value can differ between features of a layer.
///
/// Lets a caller evaluate once per layer when the answer cannot change, and
/// per feature only when it can. Zoom-dependent expressions are common and
/// constant within a tile; feature-dependent ones are rarer but must not be
/// collapsed, or every feature in a layer gets the first one's paint.
pub fn depends_on_feature(json: &Json) -> bool {
    match json {
        Json::Array(items) => {
            match items.first().and_then(Json::as_str) {
                Some("get" | "has" | "id" | "geometry-type") => return true,
                // Legacy filter-style operands name a property directly, so a
                // bare string in the first argument slot is feature state.
                Some("==" | "!=" | "<" | "<=" | ">" | ">=" | "in" | "!in") => {
                    if matches!(items.get(1), Some(Json::String(_))) {
                        return true;
                    }
                }
                _ => {}
            }
            items.iter().any(depends_on_feature)
        }
        Json::Object(map) => {
            // Legacy `{ "property": … , "stops": … }` functions.
            map.contains_key("property") || map.values().any(depends_on_feature)
        }
        _ => false,
    }
}

/// Everything an expression is allowed to see.
pub struct Ctx<'a> {
    pub zoom: f64,
    pub layer: Option<&'a Layer>,
    pub feature: Option<&'a Feature>,
}

impl<'a> Ctx<'a> {
    pub fn zoom_only(zoom: f64) -> Self {
        Ctx {
            zoom,
            layer: None,
            feature: None,
        }
    }

    pub fn property(&self, key: &str) -> Val {
        match (self.layer, self.feature) {
            (Some(layer), Some(feature)) => layer
                .property(feature, key)
                .map(Val::from_mvt)
                .unwrap_or(Val::Null),
            _ => Val::Null,
        }
    }

    fn geometry_type(&self) -> Val {
        match self.feature.map(|f| f.geom_type) {
            Some(GeomType::Point) => Val::Str("Point".into()),
            Some(GeomType::LineString) => Val::Str("LineString".into()),
            Some(GeomType::Polygon) => Val::Str("Polygon".into()),
            _ => Val::Null,
        }
    }

    fn feature_id(&self) -> Val {
        self.feature
            .and_then(|f| f.id)
            .map(|id| Val::Num(id as f64))
            .unwrap_or(Val::Null)
    }
}

/// Every operator [`eval`] handles.
///
/// Kept beside the match it describes, and checked against `mvt-style`'s
/// copy by a test: that crate tells apps at compile time which expressions
/// will not survive the raster path, and a list that drifts from this one
/// turns that promise into a lie in whichever direction it drifted.
pub const HANDLED_OPS: &[&str] = &[
    "literal",
    "get",
    "has",
    "geometry-type",
    "id",
    "zoom",
    "interpolate",
    "interpolate-hcl",
    "interpolate-lab",
    "step",
    "match",
    "case",
    "coalesce",
    "==",
    "!=",
    "<",
    ">",
    "<=",
    ">=",
    "all",
    "any",
    "none",
    "!",
    "in",
    "!in",
    "!has",
    "to-number",
    "to-boolean",
    "to-string",
    "concat",
    "rgb",
    "rgba",
    "+",
    "-",
    "*",
    "/",
    "%",
    "min",
    "max",
];

/// Evaluates an expression, a legacy function object, or a literal.
pub fn eval(json: &Json, ctx: &Ctx) -> Val {
    match json {
        Json::Array(items) => eval_array(items, ctx),
        Json::Object(_) => eval_legacy_function(json, ctx),
        other => Val::from_json(other),
    }
}

fn eval_array(items: &[Json], ctx: &Ctx) -> Val {
    let Some(op) = items.first().and_then(Json::as_str) else {
        // A bare array of literals, e.g. a `line-dasharray` value.
        return Val::Arr(items.iter().map(Val::from_json).collect());
    };
    let args = &items[1..];

    match op {
        "literal" => args.first().map(Val::from_json).unwrap_or(Val::Null),
        "get" => args
            .first()
            .and_then(Json::as_str)
            .map(|k| ctx.property(k))
            .unwrap_or(Val::Null),
        "has" => Val::Bool(
            args.first()
                .and_then(Json::as_str)
                .map(|k| ctx.property(k) != Val::Null)
                .unwrap_or(false),
        ),
        "geometry-type" => ctx.geometry_type(),
        "id" => ctx.feature_id(),
        "zoom" => Val::Num(ctx.zoom),
        "interpolate" | "interpolate-hcl" | "interpolate-lab" => eval_interpolate(args, ctx),
        "step" => eval_step(args, ctx),
        "match" => eval_match(args, ctx),
        "case" => eval_case(args, ctx),
        "coalesce" => {
            for arg in args {
                let v = eval(arg, ctx);
                if v != Val::Null {
                    return v;
                }
            }
            Val::Null
        }
        "==" | "!=" | "<" | ">" | "<=" | ">=" => eval_comparison(op, args, ctx),
        "all" => Val::Bool(args.iter().all(|a| eval(a, ctx).truthy())),
        "any" => Val::Bool(args.iter().any(|a| eval(a, ctx).truthy())),
        "none" => Val::Bool(!args.iter().any(|a| eval(a, ctx).truthy())),
        "!" => Val::Bool(!args.first().map(|a| eval(a, ctx).truthy()).unwrap_or(false)),
        "in" => eval_in(args, ctx),
        "!in" => Val::Bool(!eval_in(args, ctx).truthy()),
        // Legacy filter operator. Discovered missing by the filter compiler's
        // equivalence test: as an unknown operator it evaluated to Null, so
        // every `!has` filter silently matched nothing.
        "!has" => Val::Bool(
            args.first()
                .and_then(Json::as_str)
                .map(|k| ctx.property(k) == Val::Null)
                .unwrap_or(false),
        ),
        // Each argument in turn until one is a number, which is what the
        // fallback in `["to-number", ["get", "population"], 0]` is for.
        "to-number" => Val::Num(
            args.iter()
                .find_map(|a| eval(a, ctx).as_f64())
                .unwrap_or(0.0),
        ),
        "to-boolean" => Val::Bool(args.first().map(|a| eval(a, ctx).truthy()).unwrap_or(false)),
        "to-string" => Val::Str(
            args.first()
                .map(|a| stringify(&eval(a, ctx)))
                .unwrap_or_default(),
        ),
        "concat" => Val::Str(args.iter().map(|a| stringify(&eval(a, ctx))).collect()),
        "rgb" | "rgba" => eval_rgba(args, ctx),
        "+" | "-" | "*" | "/" | "%" => eval_arithmetic(op, args, ctx),
        "min" | "max" => {
            let nums: Vec<f64> = args.iter().filter_map(|a| eval(a, ctx).as_f64()).collect();
            let folded = if op == "min" {
                nums.iter().copied().fold(f64::INFINITY, f64::min)
            } else {
                nums.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            };
            if folded.is_finite() {
                Val::Num(folded)
            } else {
                Val::Null
            }
        }
        // Unknown operator: degrade to the property default rather than fail.
        _ => Val::Null,
    }
}

fn stringify(v: &Val) -> String {
    match v {
        Val::Null => String::new(),
        Val::Bool(b) => b.to_string(),
        Val::Num(n) => {
            if n.fract() == 0.0 && n.is_finite() {
                format!("{}", *n as i64)
            } else {
                n.to_string()
            }
        }
        Val::Str(s) => s.clone(),
        Val::Color(_) => String::new(),
        Val::Arr(_) => String::new(),
    }
}

/// Legacy comparison filters name the property directly (`["==", "class",
/// "park"]`) while expressions wrap it (`["==", ["get", "class"], "park"]`).
/// A bare string in the operand slot is the documented way to tell them apart.
fn operand(json: &Json, ctx: &Ctx) -> Val {
    match json {
        Json::String(name) => match name.as_str() {
            "$type" => ctx.geometry_type(),
            "$id" => ctx.feature_id(),
            key => ctx.property(key),
        },
        other => eval(other, ctx),
    }
}

fn eval_comparison(op: &str, args: &[Json], ctx: &Ctx) -> Val {
    if args.len() < 2 {
        return Val::Bool(false);
    }
    let lhs = operand(&args[0], ctx);
    let rhs = eval(&args[1], ctx);

    let ordering = match (&lhs, &rhs) {
        (Val::Str(a), Val::Str(b)) => a.partial_cmp(b),
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b),
            _ => None,
        },
    };

    let result = match op {
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
    };
    Val::Bool(result)
}

/// Handles both `["in", needle, haystack]` and the legacy
/// `["in", "class", "park", "wood"]`.
fn eval_in(args: &[Json], ctx: &Ctx) -> Val {
    if args.len() < 2 {
        return Val::Bool(false);
    }
    let needle = operand(&args[0], ctx);

    if args.len() == 2 {
        return match eval(&args[1], ctx) {
            Val::Arr(items) => Val::Bool(items.contains(&needle)),
            Val::Str(haystack) => Val::Bool(
                needle
                    .as_str()
                    .map(|n| haystack.contains(n))
                    .unwrap_or(false),
            ),
            _ => Val::Bool(false),
        };
    }

    Val::Bool(args[1..].iter().any(|a| Val::from_json(a) == needle))
}

fn eval_rgba(args: &[Json], ctx: &Ctx) -> Val {
    let component = |i: usize, scale: f64| -> f32 {
        args.get(i)
            .and_then(|a| eval(a, ctx).as_f64())
            .map(|v| (v / scale) as f32)
            .unwrap_or(0.0)
    };
    let alpha = if args.len() > 3 {
        component(3, 1.0)
    } else {
        1.0
    };
    Val::Color(Color::new(
        component(0, 255.0),
        component(1, 255.0),
        component(2, 255.0),
        alpha,
    ))
}

fn eval_arithmetic(op: &str, args: &[Json], ctx: &Ctx) -> Val {
    let nums: Vec<f64> = args.iter().filter_map(|a| eval(a, ctx).as_f64()).collect();
    if nums.is_empty() {
        return Val::Null;
    }
    // Unary minus.
    if op == "-" && nums.len() == 1 {
        return Val::Num(-nums[0]);
    }
    let mut acc = nums[0];
    for n in &nums[1..] {
        acc = match op {
            "+" => acc + n,
            "-" => acc - n,
            "*" => acc * n,
            "/" => {
                if *n == 0.0 {
                    return Val::Null;
                }
                acc / n
            }
            "%" => {
                if *n == 0.0 {
                    return Val::Null;
                }
                acc % n
            }
            _ => acc,
        };
    }
    Val::Num(acc)
}

fn eval_step(args: &[Json], ctx: &Ctx) -> Val {
    if args.len() < 2 {
        return Val::Null;
    }
    let input = eval(&args[0], ctx).as_f64().unwrap_or(0.0);
    let mut result = eval(&args[1], ctx);
    // Remaining args are (stop_input, stop_output) pairs in ascending order.
    for pair in args[2..].chunks_exact(2) {
        let stop = eval(&pair[0], ctx).as_f64().unwrap_or(f64::INFINITY);
        if input < stop {
            break;
        }
        result = eval(&pair[1], ctx);
    }
    result
}

fn eval_match(args: &[Json], ctx: &Ctx) -> Val {
    if args.len() < 2 {
        return Val::Null;
    }
    let input = eval(&args[0], ctx);
    let body = &args[1..args.len() - 1];
    for pair in body.chunks_exact(2) {
        let matched = match &pair[0] {
            Json::Array(labels) => labels.iter().any(|l| Val::from_json(l) == input),
            single => Val::from_json(single) == input,
        };
        if matched {
            return eval(&pair[1], ctx);
        }
    }
    eval(&args[args.len() - 1], ctx)
}

fn eval_case(args: &[Json], ctx: &Ctx) -> Val {
    if args.is_empty() {
        return Val::Null;
    }
    let body = &args[..args.len() - 1];
    for pair in body.chunks_exact(2) {
        if eval(&pair[0], ctx).truthy() {
            return eval(&pair[1], ctx);
        }
    }
    eval(&args[args.len() - 1], ctx)
}

fn eval_interpolate(args: &[Json], ctx: &Ctx) -> Val {
    if args.len() < 4 {
        return Val::Null;
    }
    let base = interpolation_base(&args[0]);
    let input = eval(&args[1], ctx).as_f64().unwrap_or(0.0);

    let stops: Vec<(f64, &Json)> = args[2..]
        .chunks_exact(2)
        .filter_map(|pair| pair[0].as_f64().map(|k| (k, &pair[1])))
        .collect();
    if stops.is_empty() {
        return Val::Null;
    }
    if input <= stops[0].0 {
        return eval(stops[0].1, ctx);
    }
    let last = stops[stops.len() - 1];
    if input >= last.0 {
        return eval(last.1, ctx);
    }

    let idx = stops.iter().rposition(|(k, _)| *k <= input).unwrap_or(0);
    let (lo_in, lo_out) = stops[idx];
    let (hi_in, hi_out) = stops[idx + 1];
    let t = curve_position(input, lo_in, hi_in, base);
    lerp_values(&eval(lo_out, ctx), &eval(hi_out, ctx), t)
}

/// `["exponential", b]` bends the ramp; `["linear"]` is `b = 1`. Cubic-bezier
/// easing is approximated as linear — visually close at tile scale, and it
/// keeps the bezier solver out of the foundation.
fn interpolation_base(json: &Json) -> f64 {
    match json {
        Json::Array(items) => match items.first().and_then(Json::as_str) {
            Some("exponential") => items.get(1).and_then(Json::as_f64).unwrap_or(1.0),
            _ => 1.0,
        },
        _ => 1.0,
    }
}

fn curve_position(input: f64, lo: f64, hi: f64, base: f64) -> f64 {
    let span = hi - lo;
    if span <= 0.0 {
        return 0.0;
    }
    let progress = input - lo;
    if (base - 1.0).abs() < 1e-6 {
        progress / span
    } else {
        (base.powf(progress) - 1.0) / (base.powf(span) - 1.0)
    }
}

/// Interpolates numbers, colours, and equal-length numeric arrays. Anything
/// else snaps to the lower stop, matching the spec's treatment of
/// non-interpolatable types.
fn lerp_values(lo: &Val, hi: &Val, t: f64) -> Val {
    if let (Some(a), Some(b)) = (lo.as_f64(), hi.as_f64()) {
        return Val::Num(a + (b - a) * t);
    }
    if let (Some(a), Some(b)) = (lo.as_color(), hi.as_color()) {
        return Val::Color(a.lerp(b, t as f32));
    }
    if let (Val::Arr(a), Val::Arr(b)) = (lo, hi) {
        if a.len() == b.len() {
            return Val::Arr(a.iter().zip(b).map(|(x, y)| lerp_values(x, y, t)).collect());
        }
    }
    lo.clone()
}

// ---------------------------------------------------------------------------
// legacy `{ "stops": [...] }` functions
// ---------------------------------------------------------------------------

fn eval_legacy_function(json: &Json, ctx: &Ctx) -> Val {
    let Some(obj) = json.as_object() else {
        return Val::Null;
    };
    let default = obj.get("default").map(Val::from_json).unwrap_or(Val::Null);

    let property = obj.get("property").and_then(Json::as_str);
    let input = match property {
        Some(key) => ctx.property(key),
        None => Val::Num(ctx.zoom),
    };

    let kind = obj.get("type").and_then(Json::as_str).unwrap_or(
        // Property functions default to categorical only for non-numeric input.
        if property.is_some() && input.as_f64().is_none() {
            "categorical"
        } else {
            "exponential"
        },
    );

    if kind == "identity" {
        return if input == Val::Null { default } else { input };
    }

    let Some(stops) = obj.get("stops").and_then(Json::as_array) else {
        return default;
    };
    if stops.is_empty() {
        return default;
    }

    let pairs: Vec<(&Json, &Json)> = stops
        .iter()
        .filter_map(|s| {
            let arr = s.as_array()?;
            Some((arr.first()?, arr.get(1)?))
        })
        .collect();
    if pairs.is_empty() {
        return default;
    }

    if kind == "categorical" {
        return pairs
            .iter()
            .find(|(k, _)| Val::from_json(k) == input)
            .map(|(_, v)| eval(v, ctx))
            .unwrap_or(default);
    }

    let numeric = input.as_f64().unwrap_or(0.0);
    if kind == "interval" {
        let mut result = eval(pairs[0].1, ctx);
        for (k, v) in &pairs {
            match k.as_f64() {
                Some(stop) if numeric >= stop => result = eval(v, ctx),
                _ => break,
            }
        }
        return result;
    }

    // exponential
    let base = obj.get("base").and_then(Json::as_f64).unwrap_or(1.0);
    let keys: Vec<f64> = pairs.iter().filter_map(|(k, _)| k.as_f64()).collect();
    if keys.len() != pairs.len() {
        return default;
    }
    if numeric <= keys[0] {
        return eval(pairs[0].1, ctx);
    }
    if numeric >= keys[keys.len() - 1] {
        return eval(pairs[pairs.len() - 1].1, ctx);
    }
    let idx = keys.iter().rposition(|k| *k <= numeric).unwrap_or(0);
    let t = curve_position(numeric, keys[idx], keys[idx + 1], base);
    lerp_values(&eval(pairs[idx].1, ctx), &eval(pairs[idx + 1].1, ctx), t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn zoom(z: f64) -> Ctx<'static> {
        Ctx::zoom_only(z)
    }

    fn num(json: Json, z: f64) -> f64 {
        eval(&json, &zoom(z)).as_f64().expect("numeric result")
    }

    #[test]
    fn evaluates_literals() {
        assert_eq!(eval(&json!(3), &zoom(0.0)), Val::Num(3.0));
        assert_eq!(eval(&json!("#fff"), &zoom(0.0)), Val::Str("#fff".into()));
        assert_eq!(eval(&json!(true), &zoom(0.0)), Val::Bool(true));
        assert_eq!(
            eval(&json!(["literal", [1, 2]]), &zoom(0.0)),
            Val::Arr(vec![Val::Num(1.0), Val::Num(2.0)])
        );
    }

    #[test]
    fn interpolates_linearly_between_zoom_stops() {
        let e = json!(["interpolate", ["linear"], ["zoom"], 10, 1.0, 20, 11.0]);
        assert_eq!(num(e.clone(), 10.0), 1.0);
        assert_eq!(num(e.clone(), 15.0), 6.0);
        assert_eq!(num(e.clone(), 20.0), 11.0);
        // Outside the stop range the ramp clamps.
        assert_eq!(num(e.clone(), 0.0), 1.0);
        assert_eq!(num(e, 30.0), 11.0);
    }

    #[test]
    fn exponential_base_bends_the_ramp() {
        let e = json!([
            "interpolate",
            ["exponential", 2],
            ["zoom"],
            0,
            0.0,
            10,
            10.0
        ]);
        let mid = num(e, 5.0);
        // A base above 1 pushes the curve below the linear midpoint.
        assert!(mid < 5.0, "expected sub-linear midpoint, got {mid}");
    }

    #[test]
    fn interpolates_colors() {
        let e = json!([
            "interpolate",
            ["linear"],
            ["zoom"],
            0,
            "#000000",
            10,
            "#ffffff"
        ]);
        let c = eval(&e, &zoom(5.0)).as_color().expect("color");
        assert!((c.r - 0.5).abs() < 0.01);
    }

    #[test]
    fn step_holds_the_previous_output() {
        let e = json!(["step", ["zoom"], "small", 10, "medium", 15, "large"]);
        assert_eq!(eval(&e, &zoom(0.0)).as_str(), Some("small"));
        assert_eq!(eval(&e, &zoom(9.99)).as_str(), Some("small"));
        assert_eq!(eval(&e, &zoom(10.0)).as_str(), Some("medium"));
        assert_eq!(eval(&e, &zoom(99.0)).as_str(), Some("large"));
    }

    #[test]
    fn match_and_case_fall_through_to_defaults() {
        let m = json!(["match", ["zoom"], 1, "one", [2, 3], "few", "many"]);
        assert_eq!(eval(&m, &zoom(1.0)).as_str(), Some("one"));
        assert_eq!(eval(&m, &zoom(3.0)).as_str(), Some("few"));
        assert_eq!(eval(&m, &zoom(9.0)).as_str(), Some("many"));

        let c = json!(["case", ["<", ["zoom"], 5], "low", "high"]);
        assert_eq!(eval(&c, &zoom(1.0)).as_str(), Some("low"));
        assert_eq!(eval(&c, &zoom(7.0)).as_str(), Some("high"));
    }

    #[test]
    fn logical_and_arithmetic_operators() {
        assert!(eval(&json!(["all", true, ["<", ["zoom"], 5]]), &zoom(1.0)).truthy());
        assert!(!eval(&json!(["all", true, ["<", ["zoom"], 5]]), &zoom(9.0)).truthy());
        assert!(eval(&json!(["any", false, true]), &zoom(0.0)).truthy());
        assert!(eval(&json!(["!", false]), &zoom(0.0)).truthy());
        assert_eq!(num(json!(["+", 1, 2, 3]), 0.0), 6.0);
        assert_eq!(num(json!(["*", ["zoom"], 2]), 4.0), 8.0);
        assert_eq!(num(json!(["-", 5]), 0.0), -5.0);
        // Division by zero yields Null rather than an infinity that would
        // silently become a giant line width.
        assert_eq!(eval(&json!(["/", 1, 0]), &zoom(0.0)), Val::Null);
    }

    #[test]
    fn unknown_operators_degrade_to_null() {
        assert_eq!(eval(&json!(["format", "x"]), &zoom(0.0)), Val::Null);
        assert_eq!(eval(&json!(["within", {}]), &zoom(0.0)), Val::Null);
    }

    #[test]
    fn legacy_zoom_function_object() {
        let f = json!({ "base": 1.0, "stops": [[6, 0.5], [20, 30.0]] });
        assert_eq!(num(f.clone(), 6.0), 0.5);
        assert_eq!(num(f.clone(), 20.0), 30.0);
        let mid = num(f, 13.0);
        assert!(mid > 0.5 && mid < 30.0);
    }

    #[test]
    fn legacy_interval_function() {
        let f = json!({ "type": "interval", "stops": [[0, "a"], [10, "b"]] });
        assert_eq!(eval(&f, &zoom(5.0)).as_str(), Some("a"));
        assert_eq!(eval(&f, &zoom(10.0)).as_str(), Some("b"));
    }

    #[test]
    fn rgba_expression_builds_a_color() {
        let c = eval(&json!(["rgba", 255, 0, 0, 0.5]), &zoom(0.0))
            .as_color()
            .unwrap();
        assert!((c.r - 1.0).abs() < 0.01 && (c.a - 0.5).abs() < 0.01);
    }

    #[test]
    fn truthiness_follows_the_spec() {
        assert!(!Val::Null.truthy());
        assert!(!Val::Num(0.0).truthy());
        assert!(!Val::Str(String::new()).truthy());
        assert!(Val::Num(0.1).truthy());
        assert!(Val::Str("x".into()).truthy());
    }
}

#[cfg(test)]
mod shared_with_mvt_style {
    /// `mvt-style` warns an app when a rule uses an expression this renderer
    /// cannot evaluate. Its list is a copy of [`super::HANDLED_OPS`] plus the
    /// interpolation types, which are arguments to `interpolate` rather than
    /// operators in their own right.
    #[test]
    fn the_two_lists_of_operators_agree() {
        let here: std::collections::BTreeSet<&str> = super::HANDLED_OPS.iter().copied().collect();
        let there: std::collections::BTreeSet<&str> =
            mvt_style::RASTER_SUPPORTED_OPS.iter().copied().collect();
        let interpolation: std::collections::BTreeSet<&str> =
            ["linear", "exponential", "cubic-bezier"]
                .into_iter()
                .collect();

        let missing: Vec<&&str> = here.difference(&there).collect();
        assert!(
            missing.is_empty(),
            "mvt-style will warn about operators this renderer does handle: {missing:?}"
        );
        let extra: Vec<&str> = there.difference(&here).copied().collect();
        let extra: std::collections::BTreeSet<&str> = extra.into_iter().collect();
        assert_eq!(
            extra, interpolation,
            "mvt-style promises operators this renderer does not handle"
        );
    }
}
