//! wasm bindings for `mvt-style`.
//!
//! Its own wasm module rather than a corner of `mvt-render-wasm`, and that
//! is the point of the split: an app using MapLibre, Mapbox or MapTiler in a
//! browser can adjust a style without fetching the rasteriser. The renderer
//! is a worker and three quarters of a megabyte; this is `serde_json` and a
//! few hundred lines.
//!
//! Two calls, both JSON in and JSON out, with failures inside the JSON as
//! `{"error": "..."}` — the same shape the JNI and C bindings answer with,
//! so the host code on each platform branches on the same thing.

use wasm_bindgen::prelude::*;

/// Applies a rules document to a style document.
///
/// Returns JSON: the adjusted style, the mutations, what they affect, and
/// the diagnostics — or `{"error": "..."}`.
#[wasm_bindgen(js_name = compileStyle)]
pub fn compile_style(style_json: &str, rules_json: &str) -> String {
    mvt_style::compile_json(style_json, rules_json)
}

/// The style's layers and the role each would be given, as a JSON array.
#[wasm_bindgen(js_name = describeStyle)]
pub fn describe_style(style_json: &str) -> String {
    mvt_style::describe_json(style_json)
}

/// The rules document version this build understands.
///
/// Worth asking on the web and nowhere else: `@mapconductor/vectorstyle`
/// is its own npm package with its own version, so an app can end up with a
/// newer wrapper over an older wasm. Native platforms ship the host code and
/// the library together and cannot.
#[wasm_bindgen(js_name = rulesSchemaVersion)]
pub fn rules_schema_version() -> u32 {
    mvt_style::SCHEMA_VERSION
}
