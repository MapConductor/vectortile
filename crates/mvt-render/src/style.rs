//! The subset of the MapLibre style spec this renderer understands.
//!
//! Layer types beyond `background`, `fill`, `line` and `circle` are parsed and
//! kept (so callers can report what was skipped) but never drawn. Symbol
//! layers in particular need cross-tile label collision, which a per-tile
//! renderer cannot do correctly — see the crate docs.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json};

use crate::color::Color;
use crate::expr::{self, Ctx, Val};

#[derive(Debug)]
pub struct StyleError(pub String);

impl std::fmt::Display for StyleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "style error: {}", self.0)
    }
}

impl std::error::Error for StyleError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Background,
    Fill,
    Line,
    Circle,
    Symbol,
    /// Recognised but not drawn (raster, hillshade, fill-extrusion, …).
    Unsupported,
}

impl LayerKind {
    fn from_str(s: &str) -> LayerKind {
        match s {
            "background" => LayerKind::Background,
            "fill" => LayerKind::Fill,
            "line" => LayerKind::Line,
            "circle" => LayerKind::Circle,
            "symbol" => LayerKind::Symbol,
            _ => LayerKind::Unsupported,
        }
    }

    pub fn is_drawable(self) -> bool {
        self != LayerKind::Unsupported
    }
}

#[derive(Debug, Clone)]
pub struct Source {
    pub id: String,
    pub kind: String,
    /// Tile URL templates. Empty when the source only carries a TileJSON `url`,
    /// which the host must resolve and feed back via [`Style::set_source_tiles`].
    pub tiles: Vec<String>,
    /// TileJSON endpoint, if the style used one instead of inline `tiles`.
    pub url: Option<String>,
    pub minzoom: u8,
    pub maxzoom: u8,
    /// Credit the source asks to be shown wherever its data is drawn.
    ///
    /// The style spec allows HTML here, and every basemap style uses it -- the
    /// text is a link to the licence. Kept verbatim rather than stripped: the
    /// SDK's attribution overlay renders HTML, and a licence link that arrives
    /// as plain text is a licence link nobody can follow.
    pub attribution: Option<String>,
}

impl Source {
    pub fn is_vector(&self) -> bool {
        self.kind == "vector"
    }
}

#[derive(Debug, Clone)]
pub struct Layer {
    pub id: String,
    pub kind: LayerKind,
    /// Raw `type` string, kept so callers can report unsupported layers.
    pub type_name: String,
    pub source: Option<String>,
    pub source_layer: Option<String>,
    pub minzoom: Option<f64>,
    pub maxzoom: Option<f64>,
    pub filter: Option<Json>,
    pub paint: Map<String, Json>,
    pub layout: Map<String, Json>,
}

impl Layer {
    /// `layout.visibility` is the only layout property that gates drawing here.
    pub fn is_visible(&self) -> bool {
        self.layout
            .get("visibility")
            .and_then(Json::as_str)
            .map(|v| v != "none")
            .unwrap_or(true)
    }

    /// The spec's half-open zoom range: shown when `minzoom <= z < maxzoom`.
    pub fn applies_at(&self, zoom: f64) -> bool {
        if let Some(min) = self.minzoom {
            if zoom < min {
                return false;
            }
        }
        if let Some(max) = self.maxzoom {
            if zoom >= max {
                return false;
            }
        }
        true
    }

    pub fn passes_filter(&self, ctx: &Ctx) -> bool {
        match &self.filter {
            None => true,
            Some(filter) => expr::eval(filter, ctx).truthy(),
        }
    }

    pub fn paint_value(&self, key: &str, ctx: &Ctx) -> Option<Val> {
        let raw = self.paint.get(key)?;
        let value = expr::eval(raw, ctx);
        if value == Val::Null {
            None
        } else {
            Some(value)
        }
    }

    pub fn layout_value(&self, key: &str, ctx: &Ctx) -> Option<Val> {
        let raw = self.layout.get(key)?;
        let value = expr::eval(raw, ctx);
        if value == Val::Null {
            None
        } else {
            Some(value)
        }
    }

    pub fn paint_number(&self, key: &str, ctx: &Ctx, default: f64) -> f64 {
        self.paint_value(key, ctx)
            .and_then(|v| v.as_f64())
            .unwrap_or(default)
    }

    pub fn layout_number(&self, key: &str, ctx: &Ctx, default: f64) -> f64 {
        self.layout_value(key, ctx)
            .and_then(|v| v.as_f64())
            .unwrap_or(default)
    }

    /// A paint property that is a list of numbers, such as `line-dasharray`.
    pub fn paint_numbers(&self, key: &str, ctx: &Ctx) -> Option<Vec<f32>> {
        match self.paint_value(key, ctx)? {
            Val::Arr(items) => {
                let numbers: Vec<f32> = items
                    .iter()
                    .filter_map(|v| v.as_f64())
                    .map(|v| v as f32)
                    .collect();
                (numbers.len() == items.len() && !numbers.is_empty()).then_some(numbers)
            }
            _ => None,
        }
    }

    pub fn paint_color(&self, key: &str, ctx: &Ctx, default: Color) -> Color {
        self.paint_value(key, ctx)
            .and_then(|v| v.as_color())
            .unwrap_or(default)
    }

    /// Whether this paint property can differ between features of a layer.
    pub fn paint_varies_per_feature(&self, key: &str) -> bool {
        self.paint
            .get(key)
            .map(expr::depends_on_feature)
            .unwrap_or(false)
    }

    pub fn paint_string(&self, key: &str, ctx: &Ctx) -> Option<String> {
        self.paint_value(key, ctx)
            .and_then(|v| v.as_str().map(str::to_owned))
    }

    pub fn layout_string(&self, key: &str, ctx: &Ctx) -> Option<String> {
        self.layout_value(key, ctx)
            .and_then(|v| v.as_str().map(str::to_owned))
    }
}

#[derive(Debug, Clone)]
pub struct Style {
    pub name: Option<String>,
    /// The style's `glyphs` URL template, e.g.
    /// `https://…/{fontstack}/{range}.pbf`.
    ///
    /// Kept rather than resolved here: fetching is the host's job, and a
    /// renderer that reached the network would not compile for wasm the same
    /// way it does for Android.
    pub glyphs: Option<String>,
    /// The style's `sprite` URL, without the `.json` or `.png` suffix.
    ///
    /// A style may instead list several sheets as `[{id, url}, ...]`; only the
    /// first is taken, because an icon name in a multi-sheet style is prefixed
    /// with its sheet's id and nothing here resolves those.
    pub sprite: Option<String>,
    /// The sheet's id when the style used the `[{id, url}]` form.
    ///
    /// It matters because icon names are then written `id:name`, and the
    /// index inside the sheet keys them without the prefix.
    pub sprite_id: Option<String>,
    pub sources: BTreeMap<String, Source>,
    pub layers: Vec<Layer>,
    /// Whether the style composes other styles through Mapbox's `imports`.
    ///
    /// Mapbox's v3 "Standard" styles put the entire basemap behind an import
    /// and keep only overlay layers locally. Nothing here can follow that, so
    /// it is recorded in order to be reported — a style that renders blank
    /// should say why.
    pub has_imports: bool,
}

impl Style {
    pub fn parse(json_text: &str) -> Result<Style, StyleError> {
        let root: Json = serde_json::from_str(json_text)
            .map_err(|e| StyleError(format!("invalid JSON: {e}")))?;
        Style::from_json(&root)
    }

    pub fn from_json(root: &Json) -> Result<Style, StyleError> {
        let obj = root
            .as_object()
            .ok_or_else(|| StyleError("style root is not an object".into()))?;

        let name = obj.get("name").and_then(Json::as_str).map(str::to_owned);
        let glyphs = obj.get("glyphs").and_then(Json::as_str).map(str::to_owned);
        let sprite_entry = obj.get("sprite").and_then(|value| match value {
            Json::String(url) => Some((url.clone(), None)),
            Json::Array(sheets) => sheets.first().and_then(|first| {
                let url = first.get("url").and_then(Json::as_str)?.to_owned();
                let id = first.get("id").and_then(Json::as_str).map(str::to_owned);
                Some((url, id))
            }),
            _ => None,
        });
        let (sprite, sprite_id) = match sprite_entry {
            Some((url, id)) => (Some(url), id),
            None => (None, None),
        };

        let mut sources = BTreeMap::new();
        if let Some(map) = obj.get("sources").and_then(Json::as_object) {
            for (id, value) in map {
                sources.insert(id.clone(), parse_source(id, value));
            }
        }

        let layer_list = obj
            .get("layers")
            .and_then(Json::as_array)
            .ok_or_else(|| StyleError("style has no `layers` array".into()))?;

        let layers = layer_list.iter().filter_map(parse_layer).collect();

        let has_imports = obj
            .get("imports")
            .and_then(Json::as_array)
            .map(|imports| !imports.is_empty())
            .unwrap_or(false);

        Ok(Style {
            name,
            glyphs,
            sprite,
            sprite_id,
            sources,
            layers,
            has_imports,
        })
    }

    /// Fills in tile URLs for a source whose style entry only had a TileJSON
    /// `url`. The host resolves TileJSON, because all network I/O stays on the
    /// host side of the FFI boundary.
    pub fn set_source_tiles(
        &mut self,
        source_id: &str,
        tiles: Vec<String>,
        minzoom: Option<u8>,
        maxzoom: Option<u8>,
    ) -> bool {
        let Some(source) = self.sources.get_mut(source_id) else {
            return false;
        };
        source.tiles = tiles;
        if let Some(z) = minzoom {
            source.minzoom = z;
        }
        if let Some(z) = maxzoom {
            source.maxzoom = z;
        }
        true
    }

    /// Sources that at least one drawable layer references at this zoom.
    pub fn active_vector_sources(&self, zoom: f64) -> Vec<&Source> {
        let mut seen: Vec<&str> = Vec::new();
        let mut result = Vec::new();
        for layer in &self.layers {
            if !layer.kind.is_drawable() || layer.kind == LayerKind::Background {
                continue;
            }
            if !layer.is_visible() || !layer.applies_at(zoom) {
                continue;
            }
            let Some(id) = layer.source.as_deref() else {
                continue;
            };
            if seen.contains(&id) {
                continue;
            }
            if let Some(source) = self.sources.get(id) {
                if source.is_vector() {
                    seen.push(id);
                    result.push(source);
                }
            }
        }
        result
    }

    /// Human-readable reasons this style may not render as its author intended.
    ///
    /// Exists because the failure mode that matters is a *blank* tile: a style
    /// this renderer cannot use should explain itself rather than quietly
    /// produce nothing.
    pub fn diagnostics(&self) -> Vec<String> {
        let mut out = Vec::new();

        if self.has_imports {
            out.push(
                "style composes other styles with `imports` (Mapbox Standard); \
                 imported layers are not rendered"
                    .to_string(),
            );
        }

        for source in self.sources.values() {
            if !source.is_vector() || !source.tiles.is_empty() {
                continue;
            }
            match source.url.as_deref() {
                Some(url) if !url.starts_with("http://") && !url.starts_with("https://") => {
                    let scheme = url.split(':').next().unwrap_or(url);
                    out.push(format!(
                        "source \"{}\" uses a `{}:` URL, which cannot be fetched directly; \
                         resolve it to an https TileJSON URL first",
                        source.id, scheme
                    ));
                }
                Some(_) => {}
                None => out.push(format!(
                    "source \"{}\" has neither `tiles` nor `url`",
                    source.id
                )),
            }
        }

        for layer in &self.layers {
            if !layer.kind.is_drawable() || layer.kind == LayerKind::Background {
                continue;
            }
            let Some(source_id) = layer.source.as_deref() else {
                continue;
            };
            if !self.sources.contains_key(source_id) {
                out.push(format!(
                    "layer \"{}\" references source \"{}\", which the style does not define",
                    layer.id, source_id
                ));
            }
        }

        let unsupported = self.unsupported_layer_types();
        if !unsupported.is_empty() {
            out.push(format!(
                "these layer types are not drawn: {}",
                unsupported.join(", ")
            ));
        }

        if !self
            .layers
            .iter()
            .any(|l| l.kind.is_drawable() && l.is_visible())
        {
            out.push("no layer in this style can be drawn; tiles will be empty".to_string());
        }

        out
    }

    /// The `source-layer` names the visible symbol layers read at this zoom,
    /// grouped by source.
    ///
    /// Used to decode a neighbouring tile for its labels alone: everything
    /// else in it is already drawn from this tile's own copy.
    pub fn symbol_source_layers(&self, zoom: f64) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for layer in &self.layers {
            if layer.kind != LayerKind::Symbol || !layer.is_visible() || !layer.applies_at(zoom) {
                continue;
            }
            let (Some(source), Some(source_layer)) =
                (layer.source.as_deref(), layer.source_layer.as_deref())
            else {
                continue;
            };
            let names = out.entry(source.to_string()).or_default();
            if !names.iter().any(|n| n == source_layer) {
                names.push(source_layer.to_string());
            }
        }
        out
    }

    /// Credits owed for what this style draws, in the order the layers ask for
    /// them.
    ///
    /// Only sources a drawable layer references, so a style that carries a
    /// source it never draws does not credit it. Deliberately not filtered by
    /// zoom, unlike [`Style::active_vector_sources`]: a credit that blinks out
    /// as the user zooms is worse than one shown a little too often, and the
    /// renderer draws a source's data above its `maxzoom` anyway by magnifying
    /// the deepest tile it has.
    pub fn attributions(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for layer in &self.layers {
            if !layer.kind.is_drawable() || layer.kind == LayerKind::Background {
                continue;
            }
            let Some(id) = layer.source.as_deref() else {
                continue;
            };
            let Some(text) = self.sources.get(id).and_then(|s| s.attribution.as_deref()) else {
                continue;
            };
            if !out.iter().any(|seen| seen == text) {
                out.push(text.to_owned());
            }
        }
        out
    }

    /// Layer `type` values that were parsed but will not be drawn.
    pub fn unsupported_layer_types(&self) -> Vec<String> {
        let mut kinds: Vec<String> = self
            .layers
            .iter()
            .filter(|l| !l.kind.is_drawable())
            .map(|l| l.type_name.clone())
            .collect();
        kinds.sort();
        kinds.dedup();
        kinds
    }
}

fn parse_source(id: &str, value: &Json) -> Source {
    let obj = value.as_object();
    let get = |key: &str| obj.and_then(|o| o.get(key));

    let tiles = get("tiles")
        .and_then(Json::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Json::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    Source {
        id: id.to_owned(),
        kind: get("type")
            .and_then(Json::as_str)
            .unwrap_or("vector")
            .to_owned(),
        tiles,
        url: get("url").and_then(Json::as_str).map(str::to_owned),
        minzoom: get("minzoom").and_then(Json::as_u64).unwrap_or(0) as u8,
        maxzoom: get("maxzoom").and_then(Json::as_u64).unwrap_or(14) as u8,
        attribution: get("attribution")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
    }
}

fn parse_layer(value: &Json) -> Option<Layer> {
    let obj = value.as_object()?;
    let id = obj.get("id").and_then(Json::as_str)?.to_owned();
    let type_name = obj
        .get("type")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_owned();

    Some(Layer {
        id,
        kind: LayerKind::from_str(&type_name),
        type_name,
        source: obj.get("source").and_then(Json::as_str).map(str::to_owned),
        source_layer: obj
            .get("source-layer")
            .and_then(Json::as_str)
            .map(str::to_owned),
        minzoom: obj.get("minzoom").and_then(Json::as_f64),
        maxzoom: obj.get("maxzoom").and_then(Json::as_f64),
        filter: obj.get("filter").cloned(),
        paint: obj
            .get("paint")
            .and_then(Json::as_object)
            .cloned()
            .unwrap_or_default(),
        layout: obj
            .get("layout")
            .and_then(Json::as_object)
            .cloned()
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Both spellings are in the wild: the old string and the list that
    /// carries an id, which is also the prefix every icon name then uses.
    #[test]
    fn reads_either_spelling_of_sprite() {
        let plain = Style::parse(
            r##"{"version": 8, "sprite": "https://example.com/sprite",
                 "sources": {}, "layers": []}"##,
        )
        .expect("parses");
        assert_eq!(plain.sprite.as_deref(), Some("https://example.com/sprite"));
        assert_eq!(plain.sprite_id, None);

        let listed = Style::parse(
            r##"{"version": 8,
                 "sprite": [{"id": "basics", "url": "https://example.com/basics"}],
                 "sources": {}, "layers": []}"##,
        )
        .expect("parses");
        assert_eq!(listed.sprite.as_deref(), Some("https://example.com/basics"));
        assert_eq!(listed.sprite_id.as_deref(), Some("basics"));

        let none =
            Style::parse(r##"{"version": 8, "sources": {}, "layers": []}"##).expect("parses");
        assert_eq!(none.sprite, None);
    }

    /// A style is data under someone's licence. The credit has to survive the
    /// trip from the style into the renderer, or the host cannot show it.
    #[test]
    fn reads_the_credit_a_source_asks_for() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sources": {
                "osm": {
                    "type": "vector",
                    "tiles": ["https://example.com/{z}/{x}/{y}.pbf"],
                    "attribution": "© <a href=\"https://osm.org/copyright\">OpenStreetMap</a>"
                },
                "hills": {
                    "type": "vector",
                    "tiles": ["https://example.com/h/{z}/{x}/{y}.pbf"],
                    "attribution": "  © Hills  "
                },
                "unused": {
                    "type": "vector",
                    "tiles": ["https://example.com/u/{z}/{x}/{y}.pbf"],
                    "attribution": "© Nobody"
                },
                "bare": { "type": "vector", "tiles": ["https://example.com/b/{z}/{x}/{y}.pbf"] }
            },
            "layers": [
                { "id": "bg", "type": "background" },
                { "id": "b", "type": "fill", "source": "bare", "source-layer": "x" },
                { "id": "h", "type": "fill", "source": "hills", "source-layer": "x" },
                { "id": "w", "type": "fill", "source": "osm", "source-layer": "water" },
                { "id": "l", "type": "symbol", "source": "osm", "source-layer": "place" }
            ]
        }))
        .expect("parses");

        // The HTML is kept whole -- the credit is a licence link, and the
        // overlay that shows it renders HTML.
        // Surrounding space is trimmed, an absent credit is not invented, and
        // a source no drawable layer references is not credited.
        assert_eq!(
            style.attributions(),
            vec![
                "© Hills".to_string(),
                "© <a href=\"https://osm.org/copyright\">OpenStreetMap</a>".to_string(),
            ]
        );
    }

    /// Two layers over one source must not credit it twice; the overlay would
    /// print the same line side by side.
    #[test]
    fn credits_a_shared_source_once() {
        let style = sample();
        assert!(style.attributions().is_empty(), "no source declares one");

        let credited = Style::from_json(&json!({
            "version": 8,
            "sources": {
                "osm": {
                    "type": "vector",
                    "tiles": ["https://example.com/{z}/{x}/{y}.pbf"],
                    "attribution": "© OpenStreetMap"
                }
            },
            "layers": [
                { "id": "a", "type": "fill", "source": "osm", "source-layer": "water" },
                { "id": "b", "type": "line", "source": "osm", "source-layer": "roads" },
                { "id": "c", "type": "symbol", "source": "osm", "source-layer": "place" }
            ]
        }))
        .expect("parses");
        assert_eq!(credited.attributions(), vec!["© OpenStreetMap".to_string()]);
    }

    /// The renderer magnifies the deepest tile it has past a source's maxzoom,
    /// so the data is still on screen there and still has to be credited.
    #[test]
    fn keeps_the_credit_past_the_source_maxzoom() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sources": {
                "osm": {
                    "type": "vector",
                    "tiles": ["https://example.com/{z}/{x}/{y}.pbf"],
                    "maxzoom": 14,
                    "attribution": "© OpenStreetMap"
                }
            },
            "layers": [{
                "id": "w", "type": "fill", "source": "osm", "source-layer": "water",
                "minzoom": 4, "maxzoom": 12
            }]
        }))
        .expect("parses");
        // Well outside the layer's own zoom range, and past the source's.
        assert_eq!(style.attributions().len(), 1);
    }

    fn sample() -> Style {
        Style::from_json(&json!({
            "version": 8,
            "name": "test",
            "sources": {
                "osm": {
                    "type": "vector",
                    "tiles": ["https://example.com/{z}/{x}/{y}.pbf"],
                    "minzoom": 0,
                    "maxzoom": 14
                },
                "sat": { "type": "raster", "url": "https://example.com/tiles.json" }
            },
            "layers": [
                { "id": "bg", "type": "background", "paint": { "background-color": "#eee" } },
                {
                    "id": "water",
                    "type": "fill",
                    "source": "osm",
                    "source-layer": "water",
                    "minzoom": 2,
                    "filter": ["==", "class", "ocean"],
                    "paint": { "fill-color": "#1e90ff" }
                },
                { "id": "labels", "type": "symbol", "source": "osm", "source-layer": "place" }
            ]
        }))
        .expect("style parses")
    }

    #[test]
    fn parses_sources_and_layers() {
        let style = sample();
        assert_eq!(style.name.as_deref(), Some("test"));
        assert_eq!(style.sources.len(), 2);
        assert_eq!(style.layers.len(), 3);

        let osm = &style.sources["osm"];
        assert!(osm.is_vector());
        assert_eq!(osm.maxzoom, 14);
        assert_eq!(osm.tiles.len(), 1);

        // A source with only a TileJSON url has no tiles until the host resolves it.
        assert!(style.sources["sat"].tiles.is_empty());
        assert_eq!(
            style.sources["sat"].url.as_deref(),
            Some("https://example.com/tiles.json")
        );
    }

    #[test]
    fn classifies_layer_kinds() {
        let style = sample();
        assert_eq!(style.layers[0].kind, LayerKind::Background);
        assert_eq!(style.layers[1].kind, LayerKind::Fill);
        assert_eq!(style.layers[2].kind, LayerKind::Symbol);
        assert!(style.unsupported_layer_types().is_empty());
    }

    #[test]
    fn honours_the_half_open_zoom_range() {
        let layer = &sample().layers[1];
        assert!(!layer.applies_at(1.0));
        assert!(layer.applies_at(2.0));
        assert!(layer.applies_at(22.0));
    }

    #[test]
    fn active_sources_skip_background_and_unsupported_layers() {
        let style = sample();
        // Background needs no source. The fill and the symbol layer share one,
        // and a symbol layer counts now that labels are drawn — a style whose
        // only user of a source is its labels still has to fetch it.
        let active = style.active_vector_sources(5.0);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "osm");

        // The symbol layer has no zoom range, so it keeps the source live even
        // below the fill layer's minzoom.
        assert_eq!(style.active_vector_sources(0.0).len(), 1);
    }

    #[test]
    fn host_can_inject_resolved_tilejson() {
        let mut style = sample();
        assert!(style.set_source_tiles(
            "sat",
            vec!["https://cdn.example.com/{z}/{x}/{y}.png".into()],
            Some(1),
            Some(18),
        ));
        assert_eq!(style.sources["sat"].maxzoom, 18);
        assert!(!style.set_source_tiles("nope", vec![], None, None));
    }

    #[test]
    fn diagnoses_a_mapbox_standard_style_instead_of_rendering_blank() {
        // Mapbox v3 keeps the whole basemap behind an import; nothing here can
        // follow that, and the failure mode without a diagnostic is a silently
        // empty tile.
        let style = Style::from_json(&json!({
            "version": 8,
            "imports": [{ "id": "basemap", "url": "mapbox://styles/mapbox/standard" }],
            "sources": {},
            "layers": [
                { "id": "overlay", "type": "fill", "source": "custom",
                  "source-layer": "zones", "slot": "middle" }
            ]
        }))
        .unwrap();

        assert!(style.has_imports);
        let messages = style.diagnostics().join("\n");
        assert!(messages.contains("imports"), "{messages}");
        assert!(messages.contains("does not define"), "{messages}");
    }

    #[test]
    fn diagnoses_a_source_url_that_cannot_be_fetched() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sources": {
                "composite": { "type": "vector", "url": "mapbox://mapbox.mapbox-streets-v8" }
            },
            "layers": [
                { "id": "water", "type": "fill", "source": "composite", "source-layer": "water" }
            ]
        }))
        .unwrap();

        let messages = style.diagnostics().join("\n");
        assert!(messages.contains("mapbox:"), "{messages}");
        assert!(messages.contains("TileJSON"), "{messages}");
    }

    #[test]
    fn a_healthy_style_reports_nothing() {
        // Every layer type in the sample is drawn now that symbol layers are,
        // so there is nothing to report.
        assert!(sample().diagnostics().is_empty());
    }

    #[test]
    fn flags_a_style_with_nothing_drawable() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sources": {},
            "layers": [{ "id": "hills", "type": "hillshade" }]
        }))
        .unwrap();
        assert!(style
            .diagnostics()
            .iter()
            .any(|m| m.contains("tiles will be empty")));
    }

    #[test]
    fn rejects_a_style_without_layers() {
        assert!(Style::from_json(&json!({ "version": 8 })).is_err());
        assert!(Style::parse("not json").is_err());
    }
}
