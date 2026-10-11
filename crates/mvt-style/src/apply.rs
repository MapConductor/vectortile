//! One pass over the style: match, merge, write, and record.

use serde_json::{json, Map, Value as Json};

use crate::color;
use crate::roles::{Evidence, Roles};
use crate::rules::{glob_matches, ColorFilter, Kind, Patch, RuleSet, Schema, Selector};

/// Layout properties, by name. Everything else in a layer is paint.
///
/// The spec splits the two and so does every renderer's API
/// (`setLayoutProperty` against `setPaintProperty`), so a rule naming a raw
/// key has to be put on the right side of that line. Taken from the
/// MapLibre style spec; a key missing here is treated as paint, which is
/// wrong in the direction that fails loudly rather than silently.
const LAYOUT_KEYS: &[&str] = &[
    "visibility",
    "fill-sort-key",
    "line-cap",
    "line-join",
    "line-miter-limit",
    "line-round-limit",
    "line-sort-key",
    "circle-sort-key",
    "symbol-placement",
    "symbol-spacing",
    "symbol-avoid-edges",
    "symbol-sort-key",
    "symbol-z-order",
    "icon-allow-overlap",
    "icon-overlap",
    "icon-ignore-placement",
    "icon-optional",
    "icon-rotation-alignment",
    "icon-size",
    "icon-text-fit",
    "icon-text-fit-padding",
    "icon-image",
    "icon-rotate",
    "icon-padding",
    "icon-keep-upright",
    "icon-offset",
    "icon-anchor",
    "icon-pitch-alignment",
    "text-pitch-alignment",
    "text-rotation-alignment",
    "text-field",
    "text-font",
    "text-size",
    "text-max-width",
    "text-line-height",
    "text-letter-spacing",
    "text-justify",
    "text-radial-offset",
    "text-variable-anchor",
    "text-anchor",
    "text-max-angle",
    "text-writing-mode",
    "text-rotate",
    "text-padding",
    "text-keep-upright",
    "text-transform",
    "text-offset",
    "text-allow-overlap",
    "text-overlap",
    "text-ignore-placement",
    "text-optional",
];

/// Expression operators the SDK's own rasteriser can evaluate.
///
/// Mirrors the match in `mvt-render`'s `expr::eval`. A rule whose value uses
/// anything else still works on a map that draws the style itself and
/// degrades to the property's default on the raster path -- silently, which
/// is exactly why it is worth saying at compile time instead.
pub const RASTER_SUPPORTED_OPS: &[&str] = &[
    "literal",
    "get",
    "has",
    "!has",
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
    // Interpolation types, which sit inside `interpolate` rather than being
    // operators in their own right.
    "linear",
    "exponential",
    "cubic-bezier",
];

/// Layer types the rasteriser draws. A rule aimed anywhere else changes the
/// direct path and does nothing on the raster one.
const RASTER_DRAWN_KINDS: &[Kind] = &[
    Kind::Background,
    Kind::Fill,
    Kind::Line,
    Kind::Circle,
    Kind::Symbol,
];

/// Which half of a split raster layer a change reaches.
///
/// The rasteriser serves the ground and the labels as two tile routes, so a
/// rule that only recolours text need not invalidate every tile on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affects {
    Nothing,
    Ground,
    Labels,
    Both,
}

impl Affects {
    fn with(self, kind: Kind) -> Affects {
        let half = if kind == Kind::Symbol {
            Affects::Labels
        } else {
            Affects::Ground
        };
        match (self, half) {
            (Affects::Nothing, other) => other,
            (current, other) if current == other => current,
            _ => Affects::Both,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Affects::Nothing => "none",
            Affects::Ground => "ground",
            Affects::Labels => "labels",
            Affects::Both => "both",
        }
    }
}

/// One change to one layer of a style that is already loaded.
///
/// This is what lets a map be restyled without being reloaded. Each carries
/// the value that was there before, so the change can be taken back without
/// reading the style again -- which matters because by then the renderer may
/// have its own idea of what the value is.
#[derive(Debug, Clone, PartialEq)]
pub enum Mutation {
    SetPaint {
        layer_id: String,
        key: String,
        value: Json,
        previous: Json,
    },
    SetLayout {
        layer_id: String,
        key: String,
        value: Json,
        previous: Json,
    },
    SetFilter {
        layer_id: String,
        value: Json,
        previous: Json,
    },
    SetZoomRange {
        layer_id: String,
        min_zoom: Json,
        max_zoom: Json,
        previous_min: Json,
        previous_max: Json,
    },
    AddLayer {
        /// The new layer's id, so no reader has to dig it back out of
        /// `layer`. Every binding needs it -- to skip an add the style
        /// already has, and to undo one -- and three JSON parses of the
        /// same object to recover something this side already knew is three
        /// chances to get it wrong.
        layer_id: String,
        layer: Json,
        before_id: Option<String>,
    },
}

impl Mutation {
    pub fn to_json(&self) -> Json {
        match self {
            Mutation::SetPaint {
                layer_id,
                key,
                value,
                previous,
            } => {
                json!({"op":"setPaint","layerId":layer_id,"key":key,"value":value,"previous":previous})
            }
            Mutation::SetLayout {
                layer_id,
                key,
                value,
                previous,
            } => {
                json!({"op":"setLayout","layerId":layer_id,"key":key,"value":value,"previous":previous})
            }
            Mutation::SetFilter {
                layer_id,
                value,
                previous,
            } => json!({"op":"setFilter","layerId":layer_id,"value":value,"previous":previous}),
            Mutation::SetZoomRange {
                layer_id,
                min_zoom,
                max_zoom,
                previous_min,
                previous_max,
            } => {
                json!({"op":"setZoomRange","layerId":layer_id,"minZoom":min_zoom,"maxZoom":max_zoom,
                        "previousMinZoom":previous_min,"previousMaxZoom":previous_max})
            }
            Mutation::AddLayer {
                layer_id,
                layer,
                before_id,
            } => json!({"op":"addLayer","layerId":layer_id,"layer":layer,"beforeId":before_id}),
        }
    }
}

/// What a style holds, as the rules see it.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerInfo {
    pub id: String,
    pub kind: String,
    pub source_layer: Option<String>,
    pub roles: Vec<String>,
    /// How each role was decided, in the same order.
    pub evidence: Vec<String>,
}

impl LayerInfo {
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id,
            "kind": self.kind,
            "sourceLayer": self.source_layer,
            "roles": self.roles,
            "evidence": self.evidence,
        })
    }
}

pub struct Compiled {
    /// The adjusted document. Everything the input had, with the matched
    /// layers changed.
    pub style: Json,
    pub mutations: Vec<Mutation>,
    pub affects: Affects,
    pub diagnostics: Vec<String>,
    /// False when the mutations alone cannot reproduce the document, so a
    /// caller holding a live map has to reload rather than patch.
    pub patchable: bool,
}

impl Compiled {
    pub fn style_text(&self) -> String {
        self.style.to_string()
    }

    pub fn to_json(&self) -> Json {
        json!({
            "style": self.style,
            "mutations": self.mutations.iter().map(Mutation::to_json).collect::<Vec<_>>(),
            "affects": self.affects.name(),
            "patchable": self.patchable,
            "diagnostics": self.diagnostics,
        })
    }
}

/// Per-layer remarks, gathered and said once.
///
/// A rule that reaches 184 layers will reach some they do not quite fit --
/// `widthScale` over a role that includes pedestrian areas, say. That is
/// worth one line, not one line per layer: diagnostics nobody reads are the
/// same as no diagnostics.
#[derive(Default)]
struct Notes {
    no_width: Vec<(String, String)>,
    no_color: Vec<(String, String)>,
    unreadable_color: Vec<String>,
}

impl Notes {
    fn summarise(&self, diagnostics: &mut Vec<String>) {
        for (what, layers) in [
            ("have no width to scale", &self.no_width),
            (
                "have no colour this understands (name the key under `properties`)",
                &self.no_color,
            ),
        ] {
            if layers.is_empty() {
                continue;
            }
            let mut kinds: Vec<&str> = layers.iter().map(|(_, kind)| kind.as_str()).collect();
            kinds.sort_unstable();
            kinds.dedup();
            diagnostics.push(format!(
                "{} layers ({}) {what}: {}",
                layers.len(),
                kinds.join("/"),
                summarise_ids(layers.iter().map(|(id, _)| id.as_str())),
            ));
        }
        if !self.unreadable_color.is_empty() {
            diagnostics.push(format!(
                "`colorFilter` left {} layers alone because their colour could not be read: {}",
                self.unreadable_color.len(),
                summarise_ids(self.unreadable_color.iter().map(String::as_str)),
            ));
        }
    }
}

/// The first few ids and a count of the rest. Long enough to recognise the
/// layers, short enough to read in a log line.
fn summarise_ids<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    let all: Vec<&str> = ids.collect();
    if all.len() <= 4 {
        return all.join(", ");
    }
    format!("{}, and {} more", all[..4].join(", "), all.len() - 4)
}

/// What one layer looks like before any rule is applied.
struct Target {
    index: usize,
    id: String,
    kind: Kind,
    type_name: String,
    source_layer: Option<String>,
    roles: Vec<(String, Evidence)>,
}

fn source_layers_of(style: &Json) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(layers) = style.get("layers").and_then(Json::as_array) {
        for layer in layers {
            if let Some(name) = layer.get("source-layer").and_then(Json::as_str) {
                if !out.iter().any(|existing| existing == name) {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

fn targets(style: &Json, roles: &Roles) -> Vec<Target> {
    let Some(layers) = style.get("layers").and_then(Json::as_array) else {
        return Vec::new();
    };
    layers
        .iter()
        .enumerate()
        .filter_map(|(index, layer)| {
            let id = layer.get("id").and_then(Json::as_str)?.to_string();
            let type_name = layer
                .get("type")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string();
            let source_layer = layer
                .get("source-layer")
                .and_then(Json::as_str)
                .map(str::to_string);
            let found = roles.of(&id, &type_name, source_layer.as_deref());
            Some(Target {
                index,
                kind: Kind::of(&type_name),
                id,
                type_name,
                source_layer,
                roles: found,
            })
        })
        .collect()
}

pub fn describe(style: &Json, schema: &Schema) -> Vec<LayerInfo> {
    let roles = Roles::resolve(schema, &source_layers_of(style));
    targets(style, &roles)
        .into_iter()
        .map(|target| LayerInfo {
            id: target.id,
            kind: target.type_name,
            source_layer: target.source_layer,
            roles: target.roles.iter().map(|(r, _)| r.clone()).collect(),
            evidence: target.roles.iter().map(|(_, e)| e.describe()).collect(),
        })
        .collect()
}

fn matches(selector: &Selector, target: &Target) -> bool {
    match selector {
        Selector::All => true,
        Selector::Role(role) => target.roles.iter().any(|(found, _)| found == role),
        Selector::LayerId(pattern) => glob_matches(pattern, &target.id),
        Selector::SourceLayer(name) => target.source_layer.as_deref() == Some(name.as_str()),
        Selector::Kind(kind) => target.kind == *kind,
        Selector::AnyOf(list) => list.iter().any(|s| matches(s, target)),
        Selector::AllOf(list) => list.iter().all(|s| matches(s, target)),
        Selector::Not(inner) => !matches(inner, target),
    }
}

pub fn compile(mut style: Json, set: &RuleSet) -> Compiled {
    let roles = Roles::resolve(&set.schema, &source_layers_of(&style));
    let found = targets(&style, &roles);
    let mut diagnostics = Vec::new();
    diagnostics.push(format!(
        "schema: {} ({} layers)",
        roles.schema_name(),
        found.len()
    ));

    // Every rule against every layer first, so the diagnostics can report
    // what each rule reached before anything is written.
    let mut merged: Vec<Patch> = (0..found.len()).map(|_| Patch::default()).collect();
    for (number, rule) in set.rules.iter().enumerate() {
        let mut hits = 0;
        for (slot, target) in found.iter().enumerate() {
            if matches(&rule.selector, target) {
                merged[slot].merge(&rule.patch);
                hits += 1;
            }
        }
        if hits == 0 {
            diagnostics.push(format!(
                "rule {} matched no layer; nothing will change for it",
                number + 1
            ));
        } else {
            diagnostics.push(format!("rule {} matched {hits} layers", number + 1));
        }
        check_expressions(&rule.patch, number + 1, &mut diagnostics);
    }

    let mut mutations = Vec::new();
    let mut affects = Affects::Nothing;
    let mut undrawn: Vec<String> = Vec::new();
    let mut notes = Notes::default();

    for (slot, target) in found.iter().enumerate() {
        let patch = &merged[slot];
        if patch.is_empty() {
            continue;
        }
        let layer = style
            .get_mut("layers")
            .and_then(Json::as_array_mut)
            .and_then(|layers| layers.get_mut(target.index))
            .expect("target index came from this array");
        let changed = write_patch(layer, target, patch, &mut mutations, &mut notes);
        if changed {
            affects = affects.with(target.kind);
            if !RASTER_DRAWN_KINDS.contains(&target.kind) && !undrawn.contains(&target.type_name) {
                undrawn.push(target.type_name.clone());
            }
        }
    }

    notes.summarise(&mut diagnostics);

    if !undrawn.is_empty() {
        diagnostics.push(format!(
            "these layer types changed but the SDK's own rasteriser does not draw them, \
             so the change shows only on a map that renders the style itself: {}",
            undrawn.join(", ")
        ));
    }

    // "Everything else black" has nothing to paint where no layer draws, and
    // the two paths disagree about what shows through: the raster tiles are
    // transparent there and the map underneath shows, while a map drawing
    // the style itself shows its own background. A background layer is the
    // only way to make the two agree, so one is added when a rule asked for
    // a background colour and the style has none.
    let mut patchable = true;
    if let Some(added) = insert_background(&mut style, set, &found) {
        mutations.push(added);
        affects = affects.with(Kind::Background);
        patchable = false;
        diagnostics.push(
            "the style had no background layer, so one was added underneath: without it \
             the raster path leaves the gaps transparent and the two paths differ"
                .to_string(),
        );
    }

    let roles_seen = role_census(&found);
    if !roles_seen.is_empty() {
        diagnostics.push(format!("roles found: {roles_seen}"));
    }

    Compiled {
        style,
        mutations,
        affects,
        diagnostics,
        patchable,
    }
}

fn role_census(found: &[Target]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for target in found {
        for (role, _) in &target.roles {
            match counts.iter_mut().find(|(name, _)| name == role) {
                Some(slot) => slot.1 += 1,
                None => counts.push((role.clone(), 1)),
            }
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts
        .iter()
        .map(|(role, count)| format!("{role}={count}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reports rule values the raster path will not be able to evaluate.
fn check_expressions(patch: &Patch, number: usize, diagnostics: &mut Vec<String>) {
    let mut unsupported: Vec<String> = Vec::new();
    let mut walk = |value: &Json| {
        collect_unsupported_ops(value, &mut unsupported);
    };
    if let Some(filter) = &patch.filter {
        walk(filter);
    }
    for (_, value) in &patch.properties {
        walk(value);
    }
    if !unsupported.is_empty() {
        unsupported.sort();
        unsupported.dedup();
        diagnostics.push(format!(
            "rule {number} uses expressions the SDK's own rasteriser cannot evaluate ({}); \
             on the raster path those fall back to the property's default",
            unsupported.join(", ")
        ));
    }
}

fn collect_unsupported_ops(value: &Json, out: &mut Vec<String>) {
    match value {
        Json::Array(items) => {
            if let Some(op) = items.first().and_then(Json::as_str) {
                if !RASTER_SUPPORTED_OPS.contains(&op) {
                    out.push(op.to_string());
                }
            }
            for item in items {
                collect_unsupported_ops(item, out);
            }
        }
        Json::Object(map) => {
            for (_, item) in map {
                collect_unsupported_ops(item, out);
            }
        }
        _ => {}
    }
}

/// The paint keys `color` and `opacity` expand to, for one layer type.
fn color_keys(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Background => &["background-color"],
        Kind::Fill => &["fill-color"],
        Kind::Line => &["line-color"],
        Kind::Circle => &["circle-color"],
        // Both halves of a symbol, because "make the labels white" means the
        // icon too when the style draws one.
        Kind::Symbol => &["text-color", "icon-color"],
        Kind::Other => &[],
    }
}

fn opacity_keys(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Background => &["background-opacity"],
        Kind::Fill => &["fill-opacity"],
        Kind::Line => &["line-opacity"],
        Kind::Circle => &["circle-opacity"],
        Kind::Symbol => &["text-opacity", "icon-opacity"],
        Kind::Other => &[],
    }
}

/// A width multiplied by `scale`, in a form every renderer will take.
///
/// Not `["*", current, scale]`. The value is almost always an `interpolate`
/// over zoom, and the style spec says `["zoom"]` may appear **only** as the
/// input of a top-level `step` or `interpolate` -- so wrapping one in `*`
/// produces a document that is invalid by the spec. MapLibre on Android
/// evaluates it anyway, which is how this survived; MapLibre on iOS refuses
/// the property outright, and the app is told "this map cannot set
/// line-width by name" about a perfectly ordinary width. Five hundred of
/// them, in the sample.
///
/// So the curve is scaled where it is allowed to be: at its outputs.
/// Interpolation is affine in the output values -- linear, exponential and
/// cubic-bezier alike pick a `t` from the input and blend the outputs with
/// it -- so scaling every output scales the result, and `step` selects an
/// output rather than blending, which is the same thing again. The `zoom`
/// stays exactly where the spec wants it.
fn scaled(current: &Json, scale: f64) -> Json {
    if let Some(number) = current.as_f64() {
        // Rounded, because binary floating point turns 3 x 1.3 into
        // 3.9000000000000004 and that ends up in a style document a person
        // may read. Six places is far below anything a width can show.
        return json!((number * scale * 1e6).round() / 1e6);
    }
    let Some(parts) = current.as_array() else {
        return json!(["*", current.clone(), scale]);
    };
    // `["interpolate", interpolation, input, in, out, in, out, ...]` and
    // `["step", input, default, in, out, in, out, ...]`: in both, the
    // outputs are every other element from a fixed offset.
    let first_output = match parts.first().and_then(Json::as_str) {
        Some("interpolate" | "interpolate-hcl" | "interpolate-lab") => 4,
        Some("step") => 2,
        _ => return json!(["*", current.clone(), scale]),
    };
    if parts.len() <= first_output {
        return json!(["*", current.clone(), scale]);
    }
    // Both forms then alternate output, input, output, input: `step` is
    // `[op, input, default, in, out, ...]` so its outputs are 2, 4, 6...,
    // and `interpolate` is `[op, interpolation, input, in, out, ...]` so
    // its outputs are 4, 6, 8....
    let mut out = parts.clone();
    let mut index = first_output;
    while index < out.len() {
        out[index] = scaled(&out[index], scale);
        index += 2;
    }
    Json::Array(out)
}

/// What `widthScale` multiplies: how fat the thing is drawn.
fn width_key(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::Line => Some("line-width"),
        Kind::Circle => Some("circle-radius"),
        _ => None,
    }
}

fn is_layout(key: &str) -> bool {
    LAYOUT_KEYS.contains(&key)
}

/// Writes one layer's merged patch, recording a mutation for each change.
fn write_patch(
    layer: &mut Json,
    target: &Target,
    patch: &Patch,
    mutations: &mut Vec<Mutation>,
    notes: &mut Notes,
) -> bool {
    let before = layer.clone();
    let mut wrote = false;

    if let Some(visible) = patch.visible {
        let value = Json::String(if visible { "visible" } else { "none" }.to_string());
        set_in(layer, "layout", "visibility", value.clone());
        mutations.push(Mutation::SetLayout {
            layer_id: target.id.clone(),
            key: "visibility".to_string(),
            value,
            previous: read_in(&before, "layout", "visibility"),
        });
        wrote = true;
    }

    if let Some(rgba) = patch.color {
        let keys = color_keys(target.kind);
        if keys.is_empty() {
            notes
                .no_color
                .push((target.id.clone(), target.type_name.clone()));
        }
        for key in keys {
            let value = Json::String(rgba.to_style_text());
            // An icon's colour only means anything for an SDF sprite, but
            // setting it on a style that has none changes nothing, and
            // leaving it out when it does have one looks like a bug.
            set_in(layer, "paint", key, value.clone());
            mutations.push(Mutation::SetPaint {
                layer_id: target.id.clone(),
                key: key.to_string(),
                value,
                previous: read_in(&before, "paint", key),
            });
            wrote = true;
        }
    }

    if let Some(filter) = patch.color_filter {
        for key in color_keys(target.kind) {
            let Some(current) = read_existing(&before, "paint", key) else {
                // Nothing written means the spec default, which is black for
                // every one of these. Bending black is a no-op for darken
                // and desaturate and a surprise for the rest, so leave it:
                // a filter is for the colours a style chose.
                continue;
            };
            let Some(bent) = bend_colors(&current, filter) else {
                notes.unreadable_color.push(target.id.clone());
                continue;
            };
            set_in(layer, "paint", key, bent.clone());
            mutations.push(Mutation::SetPaint {
                layer_id: target.id.clone(),
                key: key.to_string(),
                value: bent,
                previous: current,
            });
            wrote = true;
        }
    }

    if let Some(opacity) = patch.opacity {
        for key in opacity_keys(target.kind) {
            let value = json!(opacity);
            set_in(layer, "paint", key, value.clone());
            mutations.push(Mutation::SetPaint {
                layer_id: target.id.clone(),
                key: key.to_string(),
                value,
                previous: read_in(&before, "paint", key),
            });
            wrote = true;
        }
    }

    if let Some(scale) = patch.width_scale {
        match width_key(target.kind) {
            None => notes
                .no_width
                .push((target.id.clone(), target.type_name.clone())),
            Some(key) => {
                let current = read_existing(&before, "paint", key)
                    .unwrap_or_else(|| json!(default_width(key)));
                let value = scaled(&current, scale);
                set_in(layer, "paint", key, value.clone());
                mutations.push(Mutation::SetPaint {
                    layer_id: target.id.clone(),
                    key: key.to_string(),
                    value,
                    previous: read_in(&before, "paint", key),
                });
                wrote = true;
            }
        }
    }

    if let Some(filter) = &patch.filter {
        let previous = before.get("filter").cloned().unwrap_or(Json::Null);
        if filter.is_null() {
            if let Some(obj) = layer.as_object_mut() {
                obj.remove("filter");
            }
        } else if let Some(obj) = layer.as_object_mut() {
            obj.insert("filter".to_string(), filter.clone());
        }
        mutations.push(Mutation::SetFilter {
            layer_id: target.id.clone(),
            value: filter.clone(),
            previous,
        });
        wrote = true;
    }

    if patch.min_zoom.is_some() || patch.max_zoom.is_some() {
        let previous_min = before.get("minzoom").cloned().unwrap_or(Json::Null);
        let previous_max = before.get("maxzoom").cloned().unwrap_or(Json::Null);
        if let Some(obj) = layer.as_object_mut() {
            if let Some(min) = patch.min_zoom {
                obj.insert("minzoom".to_string(), json!(min));
            }
            if let Some(max) = patch.max_zoom {
                obj.insert("maxzoom".to_string(), json!(max));
            }
        }
        mutations.push(Mutation::SetZoomRange {
            layer_id: target.id.clone(),
            min_zoom: patch.min_zoom.map_or(previous_min.clone(), |v| json!(v)),
            max_zoom: patch.max_zoom.map_or(previous_max.clone(), |v| json!(v)),
            previous_min,
            previous_max,
        });
        wrote = true;
    }

    // Raw keys last: the escape hatch has the final word over the
    // shorthands, including over a `color` in the same patch.
    for (key, value) in &patch.properties {
        let section = if is_layout(key) { "layout" } else { "paint" };
        // Canonical for the same reason `read_existing` is: a raw key is the
        // escape hatch, so a colour can arrive here written any way at all.
        let value = canonical(key, value.clone());
        set_in(layer, section, key, value.clone());
        let previous = read_in(&before, section, key);
        mutations.push(if section == "layout" {
            Mutation::SetLayout {
                layer_id: target.id.clone(),
                key: key.clone(),
                value,
                previous,
            }
        } else {
            Mutation::SetPaint {
                layer_id: target.id.clone(),
                key: key.clone(),
                value,
                previous,
            }
        });
        wrote = true;
    }

    wrote
}

/// The spec defaults for the two widths, for a layer that never set one.
fn default_width(key: &str) -> f64 {
    match key {
        "circle-radius" => 5.0,
        _ => 1.0,
    }
}

/// Rewrites every colour literal inside a paint value, however deep.
///
/// A paint value is rarely a bare colour: it is an `interpolate` over zoom
/// or a `match` on a property, with colours at the leaves. Bending only a
/// top-level literal would leave every real basemap untouched, so the whole
/// tree is walked and each readable colour replaced. Returns `None` when
/// there was no colour in it at all, which is how "this cannot be read" is
/// told apart from "this was bent".
fn bend_colors(value: &Json, filter: ColorFilter) -> Option<Json> {
    fn walk(value: &Json, filter: ColorFilter, found: &mut bool) -> Json {
        match value {
            Json::String(text) => match color::parse(text) {
                Some(rgba) => {
                    *found = true;
                    let bent = color::apply_filter(rgba, filter);
                    // A colour the filter did not move keeps the text it had.
                    // Rewriting it would change the document -- and with it
                    // every cache key derived from the document -- to say the
                    // same thing in different words.
                    if bent == rgba {
                        value.clone()
                    } else {
                        Json::String(bent.to_style_text())
                    }
                }
                None => value.clone(),
            },
            Json::Array(items) => {
                Json::Array(items.iter().map(|item| walk(item, filter, found)).collect())
            }
            Json::Object(map) => Json::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), walk(item, filter, found)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let mut found = false;
    let bent = walk(value, filter, &mut found);
    found.then_some(bent)
}

fn set_in(layer: &mut Json, section: &str, key: &str, value: Json) {
    let Some(obj) = layer.as_object_mut() else {
        return;
    };
    let section = obj
        .entry(section.to_string())
        .or_insert_with(|| Json::Object(Map::new()));
    if let Some(section) = section.as_object_mut() {
        section.insert(key.to_string(), value);
    }
}

/// What to put back when this change is taken off again.
///
/// The value the layer has now, or -- when it has none -- the style spec's
/// default for that property. Not `Null`: no renderer has an "unset this"
/// call. MapLibre on Android takes a value or nothing at all, and "nothing"
/// leaves the adjustment in place, so a rule that hid every label could
/// never be taken off again. The spec default is what the layer was
/// actually drawing, so putting it back is exactly right.
///
/// `Null` is still the answer for a key this does not know, which is every
/// raw key a rule named through `properties`. A provider reports those as
/// not applied rather than guessing.
fn read_in(layer: &Json, section: &str, key: &str) -> Json {
    read_existing(layer, section, key).unwrap_or_else(|| spec_default(key))
}

/// The style spec's default for the properties this crate's shorthands set.
///
/// Deliberately short: only what `color`, `opacity`, `widthScale` and
/// `visible` can write. Carrying the whole spec here would be carrying a
/// copy of someone else's document that nothing checks.
fn spec_default(key: &str) -> Json {
    match key {
        "background-color" | "fill-color" | "line-color" | "circle-color" | "text-color"
        | "icon-color" => json!("#000000"),
        "background-opacity" | "fill-opacity" | "line-opacity" | "circle-opacity"
        | "text-opacity" | "icon-opacity" => json!(1),
        "line-width" => json!(1),
        "circle-radius" => json!(5),
        "visibility" => json!("visible"),
        _ => Json::Null,
    }
}

fn read_existing(layer: &Json, section: &str, key: &str) -> Option<Json> {
    Some(canonical(key, layer.get(section)?.get(key).cloned()?))
}

/**
A colour written the one way every renderer in the chain can read.

The style spec allows a colour to be written six ways -- `#rgb`, `#rrggbb`,
`#rrggbbaa`, `rgb()`, `rgba()`, `hsl()`, `hsla()`, and the CSS names on top
of that -- and a document is free to use any of them. Two of the three
renderers take the string as written and hand it to their own parser, so it
never mattered which.

MapLibre on iOS is the one that cannot: its colour properties hold a
`UIColor`, there is no call that takes a style colour string, and the
expression route that used to work (`CAST(x, 'UIColor')`) is refused by
Foundation on current iOS -- it ends the process. So that provider has to
parse the colour itself, and the fewer syntaxes it has to know, the smaller
the chance of a colour that silently will not apply.

The values this crate writes are already canonical, because they come from
[`Rgba::to_style_text`]. This is for the ones it reads back out of the
author's document to record as `previous`: a colour that came in as
`hsl(32, 40%, 90%)` leaves as `#f0e8dc`, which is the same colour and is
readable everywhere. Anything that is not a colour, or is a colour nothing
can parse, is left exactly as it was.
*/
fn canonical(key: &str, value: Json) -> Json {
    if !key.ends_with("-color") {
        return value;
    }
    match value.as_str().and_then(color::parse) {
        Some(rgba) => Json::String(rgba.to_style_text()),
        None => value,
    }
}

/// Adds a background layer when a rule wanted one and the style has none.
fn insert_background(style: &mut Json, set: &RuleSet, found: &[Target]) -> Option<Mutation> {
    if found.iter().any(|t| t.kind == Kind::Background) {
        return None;
    }
    // Only a rule that could have coloured a background counts. A rule that
    // merely hides labels must not conjure a black sheet under the map.
    let wanted = set.rules.iter().find_map(|rule| {
        let asks_for_background = match &rule.selector {
            Selector::All => true,
            Selector::Role(role) => role == "background",
            Selector::Kind(Kind::Background) => true,
            _ => false,
        };
        (asks_for_background).then_some(rule.patch.color).flatten()
    })?;

    let layers = style.get_mut("layers")?.as_array_mut()?;
    let before_id = layers
        .first()
        .and_then(|l| l.get("id"))
        .and_then(Json::as_str)
        .map(str::to_string);
    let layer = json!({
        "id": "mapconductor-background",
        "type": "background",
        "paint": { "background-color": wanted.to_style_text() },
    });
    layers.insert(0, layer.clone());
    Some(Mutation::AddLayer {
        layer_id: "mapconductor-background".to_string(),
        layer,
        before_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::RuleSet;

    fn style() -> Json {
        json!({
            "version": 8,
            "name": "test",
            "sky": { "sky-color": "#aaccff" },
            "sources": { "s": { "type": "vector", "tiles": ["https://x/{z}/{x}/{y}"] } },
            "layers": [
                { "id": "bg", "type": "background", "paint": { "background-color": "#f8f4f0" } },
                { "id": "water", "type": "fill", "source": "s", "source-layer": "water",
                  "paint": { "fill-color": "#a0c8f0" } },
                { "id": "highway-motorway-casing", "type": "line", "source": "s",
                  "source-layer": "transportation",
                  "paint": { "line-color": "#e9ac77",
                             "line-width": ["interpolate", ["linear"], ["zoom"], 5, 1, 18, 32] } },
                { "id": "highway-motorway", "type": "line", "source": "s",
                  "source-layer": "transportation",
                  "paint": { "line-color": "#fc8", "line-width": 2 } },
                { "id": "place-label", "type": "symbol", "source": "s", "source-layer": "place",
                  "layout": { "text-field": "{name}" }, "paint": { "text-color": "#333" } },
                { "id": "hillshade", "type": "hillshade", "source": "s" }
            ]
        })
    }

    fn rules(json: Json) -> RuleSet {
        RuleSet::from_json(&json).expect("rules parse")
    }

    fn paint(style: &Json, layer_id: &str, key: &str) -> Json {
        style
            .get("layers")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l.get("id").unwrap() == layer_id)
            .unwrap_or_else(|| panic!("no layer {layer_id}"))
            .get("paint")
            .and_then(|p| p.get(key))
            .cloned()
            .unwrap_or(Json::Null)
    }

    /// The example the whole design started from, and the one thing it must
    /// get right: everything muted, labels gone, roads picked out, and the
    /// casing dealt with so the roads are lines rather than a slab.
    #[test]
    fn emphasises_one_role_over_the_rest() {
        let out = compile(
            style(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [
                    { "selector": "all", "patch": { "color": "#000000" } },
                    { "selector": { "role": "label" }, "patch": { "visible": false } },
                    { "selector": { "role": "road-casing" }, "patch": { "color": "#303030" } },
                    { "selector": { "role": "road" }, "patch": { "color": "#ffffff", "widthScale": 1.3 } },
                ],
            })),
        );

        assert_eq!(
            paint(&out.style, "bg", "background-color"),
            json!("#000000")
        );
        assert_eq!(paint(&out.style, "water", "fill-color"), json!("#000000"));
        assert_eq!(
            paint(&out.style, "highway-motorway", "line-color"),
            json!("#ffffff")
        );
        // The casing rule comes before the road rule, so the road rule wins
        // on colour -- which is wrong for a casing, and exactly why the
        // ordering is the caller's to choose. Here it proves last-wins.
        assert_eq!(
            paint(&out.style, "highway-motorway-casing", "line-color"),
            json!("#ffffff")
        );
        // Both roads were scaled, and the curve survived -- scaled at its
        // outputs, with `["zoom"]` left where the spec allows it.
        assert_eq!(
            paint(&out.style, "highway-motorway", "line-width"),
            json!(2.6)
        );
        assert_eq!(
            paint(&out.style, "highway-motorway-casing", "line-width"),
            json!([
                "interpolate",
                ["linear"],
                ["zoom"],
                5,
                1.3,
                18,
                41.6
            ])
        );
        assert_eq!(
            out.style
                .pointer("/layers/4/layout/visibility")
                .cloned()
                .unwrap(),
            json!("none")
        );
        assert_eq!(out.affects, Affects::Both);
    }

    /// `["zoom"]` may appear **only** as the input of a top-level `step` or
    /// `interpolate`. Wrapping such a curve in `["*", …, scale]` breaks that
    /// rule, and a renderer that enforces it -- MapLibre on iOS -- refuses
    /// the property and reports it as one the map "cannot set by name".
    /// MapLibre on Android evaluates it regardless, which is why this was
    /// invisible for so long.
    #[test]
    fn scaling_a_zoom_curve_keeps_the_zoom_at_the_top() {
        let curve = json!(["interpolate", ["linear"], ["zoom"], 5, 1, 18, 32]);
        assert_eq!(
            scaled(&curve, 1.5),
            json!(["interpolate", ["linear"], ["zoom"], 5, 1.5, 18, 48.0])
        );

        // `step` selects an output rather than blending, and its default
        // output sits before the first stop. All of them scale.
        let steps = json!(["step", ["zoom"], 2, 10, 4, 14, 8]);
        assert_eq!(
            scaled(&steps, 0.5),
            json!(["step", ["zoom"], 1.0, 10, 2.0, 14, 4.0])
        );
    }

    /// A plain number is just multiplied, and anything else is still
    /// wrapped -- an expression with no top-level zoom is allowed inside
    /// `*`, and rewriting one by hand would be guessing at its shape.
    #[test]
    fn scaling_falls_back_to_multiplying_what_it_does_not_recognise() {
        assert_eq!(scaled(&json!(2), 1.3), json!(2.6));
        let by_property = json!(["get", "width"]);
        assert_eq!(
            scaled(&by_property, 1.3),
            json!(["*", ["get", "width"], 1.3])
        );
    }

    /// Binary floating point turns 3 x 1.3 into 3.9000000000000004, and
    /// these numbers end up in a document a person may open.
    #[test]
    fn a_scaled_width_is_not_floating_point_noise() {
        assert_eq!(scaled(&json!(3), 1.3), json!(3.9));
        assert_eq!(
            scaled(&json!(["interpolate", ["linear"], ["zoom"], 5, 3]), 1.3),
            json!(["interpolate", ["linear"], ["zoom"], 5, 3.9])
        );
    }

    /// Reordered, the casing keeps its own colour. Same rules, different
    /// order, different map: that is the contract.
    #[test]
    fn order_decides_which_rule_wins() {
        let out = compile(
            style(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [
                    { "selector": { "role": "road" }, "patch": { "color": "#ffffff" } },
                    { "selector": { "role": "road-casing" }, "patch": { "color": "#303030" } },
                ],
            })),
        );
        assert_eq!(
            paint(&out.style, "highway-motorway-casing", "line-color"),
            json!("#303030")
        );
        assert_eq!(
            paint(&out.style, "highway-motorway", "line-color"),
            json!("#ffffff")
        );
    }

    /// The document handed to a native renderer must keep everything that
    /// renderer understands, including what this crate has no opinion about.
    #[test]
    fn carries_through_everything_it_was_not_asked_to_change() {
        let before = style();
        let out = compile(
            before.clone(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": { "role": "water" }, "patch": { "color": "#000" } }],
            })),
        );
        assert_eq!(out.style.get("sky"), before.get("sky"));
        assert_eq!(out.style.get("name"), before.get("name"));
        assert_eq!(out.style.get("sources"), before.get("sources"));
        // The untouched layers are untouched, byte for byte.
        assert_eq!(
            out.style.pointer("/layers/5"),
            before.pointer("/layers/5"),
            "the hillshade layer was rewritten"
        );
        assert_eq!(
            paint(&out.style, "highway-motorway", "line-color"),
            json!("#fc8")
        );
    }

    #[test]
    fn mutations_say_what_changed_and_what_was_there_before() {
        let out = compile(
            style(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": { "layerId": "water" }, "patch": { "color": "#000000" } }],
            })),
        );
        assert_eq!(out.mutations.len(), 1);
        assert_eq!(
            out.mutations[0],
            Mutation::SetPaint {
                layer_id: "water".into(),
                key: "fill-color".into(),
                value: json!("#000000"),
                previous: json!("#a0c8f0"),
            }
        );
        assert!(out.patchable);
        assert_eq!(out.affects, Affects::Ground);
    }

    /// Recolouring only text must not cost a redraw of every tile on screen.
    #[test]
    fn a_label_only_change_touches_only_the_label_half() {
        let out = compile(
            style(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": { "kind": "symbol" }, "patch": { "color": "#fff" } }],
            })),
        );
        assert_eq!(out.affects, Affects::Labels);
    }

    #[test]
    fn a_colour_filter_reaches_the_leaves_of_an_expression() {
        let styled = json!({
            "version": 8, "sources": {},
            "layers": [{ "id": "roads", "type": "line", "source": "s", "source-layer": "transportation",
                "paint": { "line-color": ["interpolate", ["linear"], ["zoom"],
                                          5, "#ffffff", 18, "rgba(255,0,0,0.5)"] } }]
        });
        let out = compile(
            styled,
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": "all", "patch": { "colorFilter": { "invertLightness": true } } }],
            })),
        );
        assert_eq!(
            paint(&out.style, "roads", "line-color"),
            json!([
                "interpolate",
                ["linear"],
                ["zoom"],
                5,
                "#000000",
                18,
                "rgba(255,0,0,0.5)"
            ]),
            "white inverts to black; the half-transparent red keeps its lightness at 0.5"
        );
    }

    #[test]
    fn a_missing_background_is_added_so_both_paths_agree() {
        let styled = json!({
            "version": 8, "sources": {},
            "layers": [{ "id": "water", "type": "fill", "source": "s", "source-layer": "water",
                         "paint": { "fill-color": "#a0c8f0" } }]
        });
        let out = compile(
            styled,
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": "all", "patch": { "color": "#000000" } }],
            })),
        );
        assert_eq!(
            out.style.pointer("/layers/0/id").unwrap(),
            "mapconductor-background"
        );
        assert_eq!(
            out.style
                .pointer("/layers/0/paint/background-color")
                .unwrap(),
            "#000000"
        );
        assert!(!out.patchable, "a new layer cannot be expressed as a patch");
        assert!(out
            .diagnostics
            .iter()
            .any(|d| d.contains("no background layer")));
    }

    /// Hiding labels must not conjure a black sheet under the map.
    #[test]
    fn a_rule_that_colours_nothing_adds_no_background() {
        let styled = json!({
            "version": 8, "sources": {},
            "layers": [{ "id": "labels", "type": "symbol", "source": "s", "source-layer": "place" }]
        });
        let out = compile(
            styled,
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": { "role": "label" }, "patch": { "visible": false } }],
            })),
        );
        assert_eq!(out.style.pointer("/layers/0/id").unwrap(), "labels");
        assert!(out.patchable);
    }

    /// A property the layer never set still has something to go back to:
    /// the spec default, which is what it was drawing. Without it a rule
    /// that hid every label could be applied and never taken off.
    #[test]
    fn what_to_put_back_is_the_spec_default_when_the_layer_set_nothing() {
        let out = compile(
            style(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": { "kind": "symbol" },
                            "patch": { "visible": false, "opacity": 0.5 } }],
            })),
        );
        let layout = out
            .mutations
            .iter()
            .find_map(|m| match m {
                Mutation::SetLayout { key, previous, .. } if key == "visibility" => Some(previous),
                _ => None,
            })
            .expect("a visibility mutation");
        assert_eq!(layout, &json!("visible"));

        let opacity = out
            .mutations
            .iter()
            .find_map(|m| match m {
                Mutation::SetPaint { key, previous, .. } if key == "text-opacity" => Some(previous),
                _ => None,
            })
            .expect("a text-opacity mutation");
        assert_eq!(opacity, &json!(1));
    }

    /// What goes back is the same colour, written the one way every
    /// renderer can read.
    ///
    /// MapLibre on iOS has to parse a colour itself -- its properties hold a
    /// `UIColor` -- so a `previous` that came out of the document as
    /// `hsl(...)` or a CSS name would be a colour it could not restore. See
    /// [`canonical`].
    #[test]
    fn a_colour_read_back_out_of_the_document_is_canonical() {
        let mut document = style();
        document["layers"][1]["paint"] = json!({
            "fill-color": "hsl(210, 60%, 80%)",
            "fill-outline-color": "white",
        });
        let out = compile(
            document,
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": { "layerId": "water" },
                            "patch": { "color": "#000000",
                                       "properties": { "fill-outline-color": "rebeccapurple" } } }],
            })),
        );
        let previous = |key: &str| {
            out.mutations
                .iter()
                .find_map(|m| match m {
                    Mutation::SetPaint {
                        key: k, previous, ..
                    } if k == key => Some(previous.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("a {key} mutation"))
        };
        assert_eq!(previous("fill-color"), json!("#adcceb"));
        assert_eq!(previous("fill-outline-color"), json!("#ffffff"));
        // The escape hatch's own value is canonical too, for the same
        // reason: it can be written any way at all.
        let value = out
            .mutations
            .iter()
            .find_map(|m| match m {
                Mutation::SetPaint { key, value, .. } if key == "fill-outline-color" => {
                    Some(value.clone())
                }
                _ => None,
            })
            .expect("a fill-outline-color mutation");
        assert_eq!(value, json!("#663399"));
    }

    /// A raw key this does not know the default for stays null, and the
    /// provider says it could not take the change back rather than guessing.
    #[test]
    fn a_raw_key_it_does_not_know_has_no_default() {
        let out = compile(
            style(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": { "layerId": "water" },
                            "patch": { "properties": { "fill-antialias": false } } }],
            })),
        );
        let Mutation::SetPaint { previous, .. } = &out.mutations[0] else {
            panic!("expected a paint mutation");
        };
        assert_eq!(previous, &Json::Null);
    }

    #[test]
    fn says_when_a_rule_matched_nothing() {
        let out = compile(
            style(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": { "role": "ferry" }, "patch": { "color": "#000" } }],
            })),
        );
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.contains("matched no layer")),
            "{:?}",
            out.diagnostics
        );
        assert!(out.mutations.is_empty());
        assert_eq!(out.affects, Affects::Nothing);
    }

    #[test]
    fn says_when_a_change_lands_only_on_the_direct_path() {
        let out = compile(
            style(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": { "layerId": "hillshade" },
                            "patch": { "properties": { "hillshade-exaggeration": 0.8 } } }],
            })),
        );
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.contains("does not draw them") && d.contains("hillshade")),
            "{:?}",
            out.diagnostics
        );
    }

    #[test]
    fn says_when_a_rule_uses_an_expression_the_rasteriser_cannot_read() {
        let out = compile(
            style(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": "all", "patch": {
                    "properties": { "line-color": ["let", "x", 1, ["var", "x"]] } } }],
            })),
        );
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.contains("cannot evaluate")),
            "{:?}",
            out.diagnostics
        );
    }

    #[test]
    fn raw_keys_land_on_the_right_side_of_paint_and_layout() {
        let out = compile(
            style(),
            &rules(json!({
                "schemaVersion": 1,
                "rules": [{ "selector": { "kind": "symbol" }, "patch": { "properties": {
                    "text-field": ["get", "name:ja"],
                    "text-halo-width": 2
                } } }],
            })),
        );
        assert_eq!(
            out.style.pointer("/layers/4/layout/text-field").unwrap(),
            &json!(["get", "name:ja"])
        );
        assert_eq!(
            out.style
                .pointer("/layers/4/paint/text-halo-width")
                .unwrap(),
            &json!(2)
        );
    }

    #[test]
    fn describes_what_is_in_a_style() {
        let found = describe(&style(), &Schema::Auto);
        let roads = found
            .iter()
            .find(|l| l.id == "highway-motorway-casing")
            .expect("the casing layer");
        assert_eq!(roads.roles, vec!["road", "road-casing"]);
        assert_eq!(roads.evidence[0], "openmaptiles:transportation");
        assert_eq!(roads.kind, "line");
    }
}
