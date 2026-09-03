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
    /// Recognised but not drawn (symbol, raster, hillshade, fill-extrusion, …).
    Unsupported,
}

impl LayerKind {
    fn from_str(s: &str) -> LayerKind {
        match s {
            "background" => LayerKind::Background,
            "fill" => LayerKind::Fill,
            "line" => LayerKind::Line,
            "circle" => LayerKind::Circle,
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

    pub fn paint_color(&self, key: &str, ctx: &Ctx, default: Color) -> Color {
        self.paint_value(key, ctx)
            .and_then(|v| v.as_color())
            .unwrap_or(default)
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
        let root: Json =
            serde_json::from_str(json_text).map_err(|e| StyleError(format!("invalid JSON: {e}")))?;
        Style::from_json(&root)
    }

    pub fn from_json(root: &Json) -> Result<Style, StyleError> {
        let obj = root
            .as_object()
            .ok_or_else(|| StyleError("style root is not an object".into()))?;

        let name = obj.get("name").and_then(Json::as_str).map(str::to_owned);

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

        if !self.layers.iter().any(|l| l.kind.is_drawable() && l.is_visible()) {
            out.push("no layer in this style can be drawn; tiles will be empty".to_string());
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
        assert_eq!(style.layers[2].kind, LayerKind::Unsupported);
        assert_eq!(style.unsupported_layer_types(), vec!["symbol".to_string()]);
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
        // Only the fill layer contributes a vector source; symbol layers are
        // not drawn, so they must not cause a fetch.
        let active = style.active_vector_sources(5.0);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "osm");

        // Below the fill layer's minzoom nothing needs fetching.
        assert!(style.active_vector_sources(0.0).is_empty());
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
    fn a_healthy_style_reports_only_undrawn_layer_types() {
        let messages = sample().diagnostics();
        // The sample has a symbol layer and a raster source with a plain https
        // TileJSON url; neither is a defect worth more than one line.
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].contains("symbol"));
    }

    #[test]
    fn flags_a_style_with_nothing_drawable() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sources": {},
            "layers": [{ "id": "labels", "type": "symbol" }]
        }))
        .unwrap();
        assert!(style.diagnostics().iter().any(|m| m.contains("tiles will be empty")));
    }

    #[test]
    fn rejects_a_style_without_layers() {
        assert!(Style::from_json(&json!({ "version": 8 })).is_err());
        assert!(Style::parse("not json").is_err());
    }
}
