//! wasm-bindgen surface for the vector tile renderer.
//!
//! The API deliberately exchanges only strings and byte arrays, so the same
//! shape ports directly to the JNI and C-ABI bindings. In particular
//! [`VectorTileRenderer::render`] takes every fetched tile concatenated into
//! one buffer plus a length table, which avoids needing an array-of-arrays
//! marshaller on any platform.

use mvt_render::{Renderer, DEFAULT_TILE_SIZE};
use wasm_bindgen::prelude::*;

/// Installs a panic hook that reports through `console.error` instead of an
/// opaque `unreachable` trap. Safe to call more than once.
#[wasm_bindgen(start)]
pub fn start() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub struct VectorTileRenderer {
    inner: Renderer,
}

#[wasm_bindgen]
impl VectorTileRenderer {
    /// Parses a `style.json`. Throws if the style is unusable.
    #[wasm_bindgen(constructor)]
    pub fn new(style_json: &str) -> Result<VectorTileRenderer, JsError> {
        Renderer::new(style_json)
            .map(|inner| VectorTileRenderer { inner })
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// The default tile size, matching the SDK cores' raster layer default.
    #[wasm_bindgen(js_name = defaultTileSize)]
    pub fn default_tile_size() -> u32 {
        DEFAULT_TILE_SIZE
    }

    /// Source tiles needed to draw `z/x/y`, as a JSON array of
    /// `{ sourceId, url, z, x, y, scale, offsetX, offsetY }`.
    ///
    /// The host must fetch these **in order** and pass the bytes back to
    /// [`VectorTileRenderer::render`] positionally.
    pub fn plan(&self, z: u8, x: u32, y: u32) -> String {
        let requests: Vec<serde_json::Value> = self
            .inner
            .plan(z, x, y)
            .requests
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "sourceId": r.source_id,
                    "url": r.url,
                    "z": r.z,
                    "x": r.x,
                    "y": r.y,
                    "scale": r.scale,
                    "offsetX": r.offset_x,
                    "offsetY": r.offset_y,
                })
            })
            .collect();
        serde_json::Value::Array(requests).to_string()
    }

    /// Rasterises `z/x/y` to PNG bytes.
    ///
    /// `data` is every fetched tile concatenated in plan order; `lengths` gives
    /// each tile's byte length, with `0` marking a tile that failed to fetch or
    /// came back empty.
    pub fn render(
        &self,
        z: u8,
        x: u32,
        y: u32,
        tile_size: u32,
        data: &[u8],
        lengths: &[u32],
    ) -> Result<Vec<u8>, JsError> {
        let mut slices: Vec<Option<&[u8]>> = Vec::with_capacity(lengths.len());
        let mut offset = 0usize;
        for len in lengths {
            let len = *len as usize;
            if len == 0 {
                slices.push(None);
                continue;
            }
            let end = offset
                .checked_add(len)
                .filter(|e| *e <= data.len())
                .ok_or_else(|| JsError::new("tile length table overruns the data buffer"))?;
            slices.push(Some(&data[offset..end]));
            offset = end;
        }

        self.inner
            .render(z, x, y, tile_size, &slices)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Replaces the style without discarding the renderer.
    ///
    /// Vector tiles the host already fetched stay valid — geometry is
    /// unchanged, only the paint applied to it — so a recolour costs one
    /// re-rasterise and no network traffic.
    #[wasm_bindgen(js_name = setStyle)]
    pub fn set_style(&mut self, style_json: &str) -> Result<(), JsError> {
        self.inner
            .set_style(style_json)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Sources that carry a TileJSON `url` but no inline `tiles`, as a JSON
    /// array of `{ sourceId, url }`. The host resolves these and calls
    /// [`VectorTileRenderer::setSourceTiles`].
    #[wasm_bindgen(js_name = unresolvedSources)]
    pub fn unresolved_sources(&self) -> String {
        let sources: Vec<serde_json::Value> = self
            .inner
            .unresolved_sources()
            .into_iter()
            .map(|s| serde_json::json!({ "sourceId": s.id, "url": s.url }))
            .collect();
        serde_json::Value::Array(sources).to_string()
    }

    /// Supplies tile URL templates for a source, from resolved TileJSON.
    /// Returns false if the style has no such source.
    #[wasm_bindgen(js_name = setSourceTiles)]
    pub fn set_source_tiles(
        &mut self,
        source_id: &str,
        tiles_json: &str,
        minzoom: Option<u8>,
        maxzoom: Option<u8>,
    ) -> Result<bool, JsError> {
        let tiles: Vec<String> = serde_json::from_str(tiles_json)
            .map_err(|e| JsError::new(&format!("tiles must be a JSON string array: {e}")))?;
        Ok(self.inner.set_source_tiles(source_id, tiles, minzoom, maxzoom))
    }

    /// Reasons the current style may not render as intended, as a JSON array
    /// of strings. Covers unsupported layer types, sources that cannot be
    /// fetched, layers pointing at undefined sources, and Mapbox `imports`.
    pub fn diagnostics(&self) -> String {
        serde_json::Value::Array(
            self.inner
                .diagnostics()
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        )
        .to_string()
    }

    /// Layer `type` values in this style that will not be drawn, as a JSON
    /// array of strings. Lets the host warn instead of silently dropping them.
    #[wasm_bindgen(js_name = unsupportedLayerTypes)]
    pub fn unsupported_layer_types(&self) -> String {
        serde_json::Value::Array(
            self.inner
                .unsupported_layer_types()
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        )
        .to_string()
    }
}
