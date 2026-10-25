//! What an app asks for, and how it arrives.
//!
//! The canonical form is JSON, not a native type. Rules cross the FFI
//! boundary as text on every platform, and making that the definition rather
//! than a serialisation of something else means a rule set can be written by
//! hand, stored, shipped from a server or diffed, and still be exactly what
//! the compiler reads.

use serde_json::{Map, Value as Json};

use crate::color::{self, Rgba};

/// The version of the rules document this build understands.
///
/// Native platforms ship the host code and the library together, so the two
/// can never disagree. The web does not: `@mapconductor/vectorstyle` is its
/// own npm package with its own version, and an app can end up with a newer
/// wrapper over an older wasm. An unknown selector or patch key is therefore
/// an error rather than something to skip -- a rule that silently does
/// nothing is the failure this whole design is trying to avoid.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq)]
pub struct RuleSet {
    pub schema: Schema,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub selector: Selector,
    pub patch: Patch,
}

/// Which tile schema the style's `source-layer` names come from.
#[derive(Debug, Clone, PartialEq)]
pub enum Schema {
    /// Work it out from the style. The usual case.
    Auto,
    Named(String),
    /// Role name to the `source-layer` names that carry it.
    Custom(Vec<(String, Vec<String>)>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Selector {
    All,
    Role(String),
    /// Glob over the layer's `id`: `*` for any run, `?` for one character.
    LayerId(String),
    SourceLayer(String),
    Kind(Kind),
    AnyOf(Vec<Selector>),
    AllOf(Vec<Selector>),
    Not(Box<Selector>),
}

/// A style layer's `type`, as far as this cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Background,
    Fill,
    Line,
    Circle,
    Symbol,
    /// Everything else: raster, hillshade, heatmap, fill-extrusion, sky...
    Other,
}

impl Kind {
    pub fn of(type_name: &str) -> Kind {
        match type_name {
            "background" => Kind::Background,
            "fill" => Kind::Fill,
            "line" => Kind::Line,
            "circle" => Kind::Circle,
            "symbol" => Kind::Symbol,
            _ => Kind::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Background => "background",
            Kind::Fill => "fill",
            Kind::Line => "line",
            Kind::Circle => "circle",
            Kind::Symbol => "symbol",
            Kind::Other => "other",
        }
    }

    fn parse(name: &str) -> Option<Kind> {
        match name {
            "background" => Some(Kind::Background),
            "fill" => Some(Kind::Fill),
            "line" => Some(Kind::Line),
            "circle" => Some(Kind::Circle),
            "symbol" => Some(Kind::Symbol),
            "other" => Some(Kind::Other),
            _ => None,
        }
    }
}

/// What to change about the layers a selector matched.
///
/// Every field is optional and rules are applied in order, so a later rule
/// overrides an earlier one property by property rather than wholesale.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Patch {
    pub visible: Option<bool>,
    pub color: Option<Rgba>,
    pub color_filter: Option<ColorFilter>,
    pub opacity: Option<f64>,
    pub width_scale: Option<f64>,
    /// Replaces the layer's filter. `Some(Json::Null)` removes it.
    pub filter: Option<Json>,
    pub min_zoom: Option<f64>,
    pub max_zoom: Option<f64>,
    /// Raw style-spec keys, applied last. The escape hatch for anything the
    /// fields above do not name.
    pub properties: Vec<(String, Json)>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        *self == Patch::default()
    }

    /// `other` over `self`, property by property.
    pub fn merge(&mut self, other: &Patch) {
        if other.visible.is_some() {
            self.visible = other.visible;
        }
        // A literal colour and a filter over the existing one are two ways of
        // saying the same thing, so the later one replaces both.
        if other.color.is_some() {
            self.color = other.color;
            self.color_filter = None;
        }
        if other.color_filter.is_some() {
            self.color_filter = other.color_filter;
            self.color = None;
        }
        if other.opacity.is_some() {
            self.opacity = other.opacity;
        }
        // Scales compose: two rules each asking for 1.5x mean 2.25x, which is
        // what "make roads thicker" twice should do.
        if let Some(scale) = other.width_scale {
            self.width_scale = Some(self.width_scale.unwrap_or(1.0) * scale);
        }
        if other.filter.is_some() {
            self.filter = other.filter.clone();
        }
        if other.min_zoom.is_some() {
            self.min_zoom = other.min_zoom;
        }
        if other.max_zoom.is_some() {
            self.max_zoom = other.max_zoom;
        }
        for (key, value) in &other.properties {
            if let Some(slot) = self.properties.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value.clone();
            } else {
                self.properties.push((key.clone(), value.clone()));
            }
        }
    }
}

/// A function of the colour already there, for the cases a literal cannot
/// express: turning a light basemap dark, or greying everything but one thing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColorFilter {
    Desaturate(f64),
    Darken(f64),
    Lighten(f64),
    InvertLightness,
    Mix { target: Rgba, amount: f64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError(pub String);

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rules error: {}", self.0)
    }
}

impl std::error::Error for RuleError {}

fn err<T>(message: impl Into<String>) -> Result<T, RuleError> {
    Err(RuleError(message.into()))
}

impl RuleSet {
    pub fn parse(text: &str) -> Result<RuleSet, RuleError> {
        let root: Json =
            serde_json::from_str(text).map_err(|e| RuleError(format!("invalid JSON: {e}")))?;
        RuleSet::from_json(&root)
    }

    pub fn from_json(root: &Json) -> Result<RuleSet, RuleError> {
        let Some(obj) = root.as_object() else {
            return err("rules document is not an object");
        };

        match obj.get("schemaVersion").and_then(Json::as_u64) {
            Some(v) if v as u32 == SCHEMA_VERSION => {}
            Some(v) => {
                return err(format!(
                    "rules say schemaVersion {v}, this build understands {SCHEMA_VERSION}"
                ))
            }
            None => return err("rules document has no `schemaVersion`"),
        }

        let schema = match obj.get("schema") {
            None => Schema::Auto,
            Some(Json::String(name)) if name == "auto" => Schema::Auto,
            Some(Json::String(name)) => Schema::Named(name.clone()),
            Some(Json::Object(custom)) => Schema::Custom(parse_custom_schema(custom)?),
            Some(_) => return err("`schema` must be a name or an object of roles"),
        };

        let Some(list) = obj.get("rules").and_then(Json::as_array) else {
            return err("rules document has no `rules` array");
        };
        let rules = list.iter().map(Rule::from_json).collect::<Result<_, _>>()?;

        Ok(RuleSet { schema, rules })
    }
}

fn parse_custom_schema(obj: &Map<String, Json>) -> Result<Vec<(String, Vec<String>)>, RuleError> {
    let Some(roles) = obj.get("roles").and_then(Json::as_object) else {
        return err("a custom `schema` needs a `roles` object");
    };
    let mut out = Vec::new();
    for (role, value) in roles {
        let Some(names) = value.as_array() else {
            return err(format!("role \"{role}\" must list source-layer names"));
        };
        let mut layers = Vec::new();
        for name in names {
            let Some(name) = name.as_str() else {
                return err(format!("role \"{role}\" has a non-string source-layer"));
            };
            layers.push(name.to_string());
        }
        out.push((role.clone(), layers));
    }
    Ok(out)
}

impl Rule {
    fn from_json(value: &Json) -> Result<Rule, RuleError> {
        let Some(obj) = value.as_object() else {
            return err("a rule is not an object");
        };
        let Some(selector) = obj.get("selector") else {
            return err("a rule has no `selector`");
        };
        let selector = Selector::from_json(selector)?;
        let patch = match obj.get("patch") {
            Some(patch) => Patch::from_json(patch)?,
            None => return err("a rule has no `patch`"),
        };
        for key in obj.keys() {
            if key != "selector" && key != "patch" {
                return err(format!("a rule has an unknown key \"{key}\""));
            }
        }
        Ok(Rule { selector, patch })
    }
}

impl Selector {
    fn from_json(value: &Json) -> Result<Selector, RuleError> {
        match value {
            Json::String(name) if name == "all" => Ok(Selector::All),
            Json::String(name) => err(format!("unknown selector \"{name}\"")),
            Json::Object(obj) => {
                if obj.len() != 1 {
                    return err("a selector object names exactly one of role, layerId, sourceLayer, kind, anyOf, allOf, not");
                }
                let (key, value) = obj.iter().next().expect("checked above");
                match key.as_str() {
                    "role" => Ok(Selector::Role(expect_str(value, "role")?)),
                    "layerId" => Ok(Selector::LayerId(expect_str(value, "layerId")?)),
                    "sourceLayer" => Ok(Selector::SourceLayer(expect_str(value, "sourceLayer")?)),
                    "kind" => {
                        let name = expect_str(value, "kind")?;
                        match Kind::parse(&name) {
                            Some(kind) => Ok(Selector::Kind(kind)),
                            None => err(format!("unknown layer kind \"{name}\"")),
                        }
                    }
                    "anyOf" => Ok(Selector::AnyOf(expect_list(value, "anyOf")?)),
                    "allOf" => Ok(Selector::AllOf(expect_list(value, "allOf")?)),
                    "not" => Ok(Selector::Not(Box::new(Selector::from_json(value)?))),
                    other => err(format!("unknown selector \"{other}\"")),
                }
            }
            _ => err("a selector is neither \"all\" nor an object"),
        }
    }
}

fn expect_str(value: &Json, key: &str) -> Result<String, RuleError> {
    match value.as_str() {
        Some(text) => Ok(text.to_string()),
        None => Err(RuleError(format!("`{key}` must be a string"))),
    }
}

fn expect_list(value: &Json, key: &str) -> Result<Vec<Selector>, RuleError> {
    let Some(items) = value.as_array() else {
        return err(format!("`{key}` must be an array of selectors"));
    };
    items.iter().map(Selector::from_json).collect()
}

impl Patch {
    fn from_json(value: &Json) -> Result<Patch, RuleError> {
        let Some(obj) = value.as_object() else {
            return err("a patch is not an object");
        };
        let mut patch = Patch::default();
        for (key, value) in obj {
            match key.as_str() {
                "visible" => {
                    patch.visible = Some(
                        value
                            .as_bool()
                            .ok_or_else(|| RuleError("`visible` must be true or false".into()))?,
                    )
                }
                "color" => {
                    let text = expect_str(value, "color")?;
                    patch.color =
                        Some(color::parse(&text).ok_or_else(|| {
                            RuleError(format!("`color` is not a colour: {text}"))
                        })?);
                }
                "colorFilter" => patch.color_filter = Some(parse_color_filter(value)?),
                "opacity" => patch.opacity = Some(expect_number(value, "opacity")?),
                "widthScale" => patch.width_scale = Some(expect_number(value, "widthScale")?),
                "filter" => patch.filter = Some(value.clone()),
                "minZoom" => patch.min_zoom = Some(expect_number(value, "minZoom")?),
                "maxZoom" => patch.max_zoom = Some(expect_number(value, "maxZoom")?),
                "properties" => {
                    let Some(props) = value.as_object() else {
                        return err("`properties` must be an object of style-spec keys");
                    };
                    for (name, value) in props {
                        patch.properties.push((name.clone(), value.clone()));
                    }
                }
                other => return err(format!("unknown patch key \"{other}\"")),
            }
        }
        Ok(patch)
    }
}

fn expect_number(value: &Json, key: &str) -> Result<f64, RuleError> {
    match value.as_f64() {
        Some(number) => Ok(number),
        None => Err(RuleError(format!("`{key}` must be a number"))),
    }
}

fn parse_color_filter(value: &Json) -> Result<ColorFilter, RuleError> {
    let Some(obj) = value.as_object() else {
        return err("`colorFilter` is not an object");
    };
    if obj.len() != 1 {
        return err(
            "`colorFilter` names exactly one of desaturate, darken, lighten, invertLightness, mix",
        );
    }
    let (key, value) = obj.iter().next().expect("checked above");
    match key.as_str() {
        "desaturate" => Ok(ColorFilter::Desaturate(expect_number(value, "desaturate")?)),
        "darken" => Ok(ColorFilter::Darken(expect_number(value, "darken")?)),
        "lighten" => Ok(ColorFilter::Lighten(expect_number(value, "lighten")?)),
        "invertLightness" => Ok(ColorFilter::InvertLightness),
        "mix" => {
            let Some(obj) = value.as_object() else {
                return err("`mix` needs a colour and an amount");
            };
            let text = obj
                .get("color")
                .and_then(Json::as_str)
                .ok_or_else(|| RuleError("`mix` needs a `color`".into()))?;
            let target = color::parse(text)
                .ok_or_else(|| RuleError(format!("`mix.color` is not a colour: {text}")))?;
            let amount = obj.get("amount").and_then(Json::as_f64).unwrap_or(0.5);
            Ok(ColorFilter::Mix { target, amount })
        }
        other => err(format!("unknown colour filter \"{other}\"")),
    }
}

/// Whether `id` matches a glob of `*` and `?`.
pub fn glob_matches(pattern: &str, id: &str) -> bool {
    fn walk(pattern: &[u8], id: &[u8]) -> bool {
        match pattern.first() {
            None => id.is_empty(),
            Some(b'*') => {
                // Greedy would need backtracking anyway; try every split.
                (0..=id.len()).any(|take| walk(&pattern[1..], &id[take..]))
            }
            Some(b'?') => !id.is_empty() && walk(&pattern[1..], &id[1..]),
            Some(c) => id.first() == Some(c) && walk(&pattern[1..], &id[1..]),
        }
    }
    walk(pattern.as_bytes(), id.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_a_rule_set() {
        let set = RuleSet::parse(
            &json!({
                "schemaVersion": 1,
                "rules": [
                    { "selector": "all", "patch": { "color": "#000" } },
                    { "selector": { "role": "road" }, "patch": { "widthScale": 1.5 } },
                ],
            })
            .to_string(),
        )
        .expect("parses");
        assert_eq!(set.schema, Schema::Auto);
        assert_eq!(set.rules.len(), 2);
        assert_eq!(set.rules[0].selector, Selector::All);
        assert_eq!(set.rules[0].patch.color, Some(Rgba::new(0, 0, 0, 255)));
        assert_eq!(set.rules[1].patch.width_scale, Some(1.5));
    }

    /// The whole point of refusing rather than skipping: a rule written for a
    /// newer build must not quietly become a no-op on an older one.
    #[test]
    fn refuses_what_it_does_not_understand() {
        let cases = [
            json!({ "rules": [] }),
            json!({ "schemaVersion": 99, "rules": [] }),
            json!({ "schemaVersion": 1, "rules": [{ "selector": "everything", "patch": {} }] }),
            json!({ "schemaVersion": 1, "rules": [{ "selector": "all", "patch": { "glow": 2 } }] }),
            json!({ "schemaVersion": 1, "rules": [{ "selector": { "role": "road", "kind": "line" }, "patch": {} }] }),
            json!({ "schemaVersion": 1, "rules": [{ "selector": "all" }] }),
        ];
        for case in cases {
            assert!(
                RuleSet::parse(&case.to_string()).is_err(),
                "should have been refused: {case}"
            );
        }
    }

    #[test]
    fn later_rules_win_property_by_property() {
        let mut first = Patch {
            color: Some(Rgba::new(255, 0, 0, 255)),
            opacity: Some(0.5),
            ..Patch::default()
        };
        first.merge(&Patch {
            opacity: Some(1.0),
            visible: Some(false),
            ..Patch::default()
        });
        assert_eq!(first.color, Some(Rgba::new(255, 0, 0, 255)), "untouched");
        assert_eq!(first.opacity, Some(1.0), "overridden");
        assert_eq!(first.visible, Some(false), "added");
    }

    #[test]
    fn width_scales_compose_rather_than_replace() {
        let mut patch = Patch {
            width_scale: Some(1.5),
            ..Patch::default()
        };
        patch.merge(&Patch {
            width_scale: Some(2.0),
            ..Patch::default()
        });
        assert_eq!(patch.width_scale, Some(3.0));
    }

    #[test]
    fn a_colour_and_a_filter_over_it_replace_each_other() {
        let mut patch = Patch {
            color: Some(Rgba::new(255, 0, 0, 255)),
            ..Patch::default()
        };
        patch.merge(&Patch {
            color_filter: Some(ColorFilter::Darken(0.5)),
            ..Patch::default()
        });
        assert_eq!(patch.color, None);
        assert!(patch.color_filter.is_some());
    }

    #[test]
    fn globs_match_the_way_a_layer_id_is_written() {
        assert!(glob_matches("highway-*", "highway-motorway"));
        assert!(glob_matches("*-casing", "highway-motorway-casing"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("road", "road"));
        assert!(!glob_matches("highway-*", "railway-rail"));
        assert!(!glob_matches("road", "roads"));
        assert!(glob_matches("ro?d", "road"));
    }
}
