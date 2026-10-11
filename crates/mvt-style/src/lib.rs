//! Declarative adjustments to a MapLibre style.
//!
//! A map in this SDK is drawn one of two ways: by a renderer that takes a
//! vector style directly (MapLibre, Mapbox, MapTiler), or by rasterising the
//! style to PNG tiles on the device and handing those to a backend that
//! cannot read a style at all (Google Maps, MapKit, HERE, ArcGIS...). Both
//! start from the same `style.json`, so "adjust the style" belongs *before*
//! that fork rather than on either side of it. That is this crate: style
//! document plus rules in, adjusted style document out — and, from the same
//! evaluation, the per-layer deltas a live renderer can be told about
//! without reloading anything.
//!
//! The two outputs cannot disagree, because they come from one pass over one
//! style. That is the whole argument for compiling rules here rather than
//! once per platform.
//!
//! ```
//! # fn main() {
//! let style = r##"{"version":8,"sources":{},"layers":[
//!     {"id":"roads","type":"line","source":"s","source-layer":"transportation",
//!      "paint":{"line-color":"#888"}}
//! ]}"##;
//! let rules = r##"{"schemaVersion":1,"rules":[
//!     {"selector":{"role":"road"},"patch":{"color":"#fff","widthScale":1.5}}
//! ]}"##;
//! let out = mvt_style::compile(style, rules).unwrap();
//! assert_eq!(out.mutations.len(), 2);
//! # }
//! ```
//!
//! ## What it does not do
//!
//! It never rewrites anything it was not asked to. The document that comes
//! out is the one that went in with paint, layout, filter and zoom range
//! changed on the layers a rule matched: `terrain`, `sky`, `projection`,
//! `imports`, `metadata` and layer types this crate has no opinion about
//! pass through untouched. A style handed to a native renderer must keep
//! everything that renderer understands, including the parts the SDK's own
//! rasteriser cannot draw.

mod apply;
pub mod color;
pub mod roles;
pub mod rules;

pub use apply::{Affects, Compiled, LayerInfo, Mutation, RASTER_SUPPORTED_OPS};
pub use rules::{RuleError, RuleSet, SCHEMA_VERSION};

/// Applies `rules_json` to `style_json`.
///
/// Fails only when one of the two documents cannot be read. A rule that
/// matches nothing is not an error -- it is a diagnostic, which is the
/// difference between "you asked for something impossible" and "you asked
/// for something this style has none of".
pub fn compile(style_json: &str, rules_json: &str) -> Result<Compiled, RuleError> {
    let style: serde_json::Value = serde_json::from_str(style_json)
        .map_err(|e| RuleError(format!("style is not JSON: {e}")))?;
    let rules = RuleSet::parse(rules_json)?;
    Ok(apply::compile(style, &rules))
}

/// Every layer in the style, with the role this crate would give it.
///
/// Rules are written against someone else's style, so being able to ask what
/// is in it -- and on what evidence a layer counts as a road -- is not a
/// debugging aid, it is how the rules get written in the first place.
pub fn describe(style_json: &str) -> Result<Vec<LayerInfo>, RuleError> {
    let style: serde_json::Value = serde_json::from_str(style_json)
        .map_err(|e| RuleError(format!("style is not JSON: {e}")))?;
    Ok(apply::describe(&style, &rules::Schema::Auto))
}

/// [`compile`] as one JSON string in and one out, for the FFI bindings.
///
/// Failures come back as `{"error": "..."}` rather than through a second
/// channel. Three bindings would otherwise each invent their own way to
/// carry an error -- a Java exception, a null pointer plus a getter, a
/// thrown `JsError` -- and the host code on each platform would branch
/// differently on the same condition. One shape, parsed the same way
/// everywhere.
pub fn compile_json(style_json: &str, rules_json: &str) -> String {
    match compile(style_json, rules_json) {
        Ok(out) => out.to_json().to_string(),
        Err(error) => error_json(&error.0),
    }
}

/// [`describe`] as JSON: an array of layers, or `{"error": "..."}`.
pub fn describe_json(style_json: &str) -> String {
    match describe(style_json) {
        Ok(found) => {
            serde_json::Value::Array(found.iter().map(apply::LayerInfo::to_json).collect())
                .to_string()
        }
        Err(error) => error_json(&error.0),
    }
}

fn error_json(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_ffi_entry_points_answer_with_json_either_way() {
        let style = r##"{"version":8,"sources":{},"layers":[]}"##;
        let rules = r##"{"schemaVersion":1,"rules":[]}"##;

        let out: serde_json::Value =
            serde_json::from_str(&super::compile_json(style, rules)).expect("JSON");
        assert!(out.get("style").is_some());
        assert!(out.get("mutations").is_some());
        assert!(out.get("error").is_none());

        let failed: serde_json::Value =
            serde_json::from_str(&super::compile_json("{", rules)).expect("JSON");
        assert!(failed["error"].as_str().is_some(), "{failed}");

        let described: serde_json::Value =
            serde_json::from_str(&super::describe_json(style)).expect("JSON");
        assert!(described.is_array());

        let described: serde_json::Value =
            serde_json::from_str(&super::describe_json("nope")).expect("JSON");
        assert!(described["error"].as_str().is_some());
    }

    /// A rule set written for a newer build must be refused loudly, not
    /// applied in part.
    #[test]
    fn a_newer_rules_version_is_an_error_not_a_shrug() {
        let out: serde_json::Value = serde_json::from_str(&super::compile_json(
            r##"{"version":8,"sources":{},"layers":[]}"##,
            r##"{"schemaVersion":99,"rules":[]}"##,
        ))
        .expect("JSON");
        assert!(out["error"].as_str().unwrap().contains("schemaVersion"));
    }
}
