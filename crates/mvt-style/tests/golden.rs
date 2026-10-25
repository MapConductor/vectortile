//! What the compiler does to real styles, recorded.
//!
//! The unit tests in the crate check one behaviour each against a style
//! written to provoke it. These check the whole thing against styles nobody
//! wrote for this: VersaTiles Colorful (Shortbread, 280 layers, what the
//! sample apps load) and the MapLibre demo style. The recorded files are the
//! specification the three platform bindings are held to -- a binding that
//! produces something else is wrong, whatever its own tests say.
//!
//! Regenerate deliberately, and read the diff:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p mvt-style --test golden
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{json, Value as Json};

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Compares against the recorded file, or writes it when asked to.
fn golden(name: &str, actual: &Json) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    let text = format!("{}\n", serde_json::to_string_pretty(actual).unwrap());
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(&path, &text).expect("writing the golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} does not exist yet. UPDATE_GOLDEN=1 cargo test -p mvt-style --test golden",
            path.display()
        )
    });
    if expected != text {
        // Printed rather than asserted on the whole blob: a 300-line diff in
        // an assertion message is unreadable.
        let mut shown = 0;
        for (line, (want, got)) in expected.lines().zip(text.lines()).enumerate() {
            if want != got && shown < 12 {
                println!("line {}:\n  recorded: {want}\n  now:      {got}", line + 1);
                shown += 1;
            }
        }
        panic!(
            "{} no longer matches ({} recorded lines, {} now). \
             If the change is intended: UPDATE_GOLDEN=1 cargo test -p mvt-style --test golden",
            path.display(),
            expected.lines().count(),
            text.lines().count()
        );
    }
}

/// A compiled result, small enough to read.
///
/// Not the whole output document: 280 layers of style JSON tells a reviewer
/// nothing. What matters is which layers changed and to what, which is what
/// a binding has to reproduce.
fn digest(out: &mvt_style::Compiled) -> Json {
    let mut by_layer: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut added: Vec<Json> = Vec::new();
    for mutation in &out.mutations {
        let json = mutation.to_json();
        let op = json["op"].as_str().unwrap_or("");
        if op == "addLayer" {
            added.push(json["layer"].clone());
            continue;
        }
        let layer_id = json["layerId"].as_str().unwrap_or("?").to_string();
        let entry = match op {
            "setPaint" | "setLayout" => format!(
                "{} {}={}",
                op,
                json["key"].as_str().unwrap_or("?"),
                json["value"]
            ),
            other => format!("{other} {}", json["value"]),
        };
        by_layer.entry(layer_id).or_default().push(entry);
    }
    json!({
        "affects": out.affects.name(),
        "patchable": out.patchable,
        "diagnostics": out.diagnostics,
        "addedLayers": added,
        "changedLayers": by_layer,
    })
}

/// Everything muted, labels off, roads picked out: the example the design
/// was argued from, against a style that was not written for it.
const ROAD_EMPHASIS: &str = r##"{
  "schemaVersion": 1,
  "rules": [
    { "selector": "all", "patch": { "color": "#000000" } },
    { "selector": { "role": "label" }, "patch": { "visible": false } },
    { "selector": { "role": "road-casing" }, "patch": { "color": "#303030" } },
    { "selector": { "role": "road" }, "patch": { "color": "#ffffff", "widthScale": 1.3 } }
  ]
}"##;

/// A light style turned dark without naming a single colour. This is the
/// case a literal cannot express, and the one that proves the filter reaches
/// into expressions.
const DARK_MODE: &str = r##"{
  "schemaVersion": 1,
  "rules": [
    { "selector": "all", "patch": { "colorFilter": { "invertLightness": true } } },
    { "selector": { "role": "label" }, "patch": { "colorFilter": { "desaturate": 0.5 } } }
  ]
}"##;

#[test]
fn road_emphasis_on_versatiles_colorful() {
    let out =
        mvt_style::compile(&fixture("versatiles-colorful.json"), ROAD_EMPHASIS).expect("compiles");
    golden("versatiles-road-emphasis.json", &digest(&out));
}

#[test]
fn dark_mode_on_versatiles_colorful() {
    let out =
        mvt_style::compile(&fixture("versatiles-colorful.json"), DARK_MODE).expect("compiles");
    golden("versatiles-dark-mode.json", &digest(&out));
}

#[test]
fn road_emphasis_on_the_maplibre_demo_style() {
    let out = mvt_style::compile(&fixture("maplibre-demo.json"), ROAD_EMPHASIS).expect("compiles");
    golden("maplibre-demo-road-emphasis.json", &digest(&out));
}

/// The role table is the part most likely to go quietly wrong, so what it
/// says about a real style is recorded in full.
#[test]
fn roles_of_versatiles_colorful() {
    let found = mvt_style::describe(&fixture("versatiles-colorful.json")).expect("describes");
    let listed: Vec<Json> = found.iter().map(mvt_style::LayerInfo::to_json).collect();
    golden("versatiles-roles.json", &json!(listed));
}

/// Not a recorded file: a property that has to hold whatever the style is.
#[test]
fn every_layer_of_a_real_style_keeps_everything_it_had() {
    let text = fixture("versatiles-colorful.json");
    let before: Json = serde_json::from_str(&text).unwrap();
    let out = mvt_style::compile(&text, ROAD_EMPHASIS).expect("compiles");

    let before_layers = before["layers"].as_array().unwrap();
    let after_layers = out.style["layers"].as_array().unwrap();
    assert_eq!(
        before_layers.len(),
        after_layers.len(),
        "this style has a background already, so none should have been added"
    );
    for (before, after) in before_layers.iter().zip(after_layers) {
        assert_eq!(before["id"], after["id"], "layer order changed");
        assert_eq!(before["type"], after["type"]);
        assert_eq!(before["source"], after["source"]);
        assert_eq!(before["source-layer"], after["source-layer"]);
        assert_eq!(before["filter"], after["filter"], "no rule touched filters");
        // Every key the layer had is still there: the patch adds and
        // replaces inside `paint`/`layout`, never prunes.
        for key in before.as_object().unwrap().keys() {
            assert!(
                after.get(key).is_some(),
                "layer {} lost \"{key}\"",
                before["id"]
            );
        }
    }
    for key in before.as_object().unwrap().keys() {
        assert_eq!(
            before.get(key).is_some(),
            out.style.get(key).is_some(),
            "the style lost its top-level \"{key}\""
        );
    }
}

/// A rule set that names nothing this style has must say so rather than
/// quietly producing the style back.
#[test]
fn a_rule_set_that_matches_nothing_says_so() {
    let rules = r##"{"schemaVersion":1,"rules":[
        {"selector":{"sourceLayer":"nothing_like_this"},"patch":{"color":"#f00"}}
    ]}"##;
    let out = mvt_style::compile(&fixture("versatiles-colorful.json"), rules).expect("compiles");
    assert!(out.mutations.is_empty());
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.contains("matched no layer")),
        "{:?}",
        out.diagnostics
    );
}
