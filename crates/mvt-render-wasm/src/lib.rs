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

/// Rebuilds the plan's per-source slices from the flat buffer the host packs.
///
/// Every entry point takes tiles the same way -- one buffer, one length table,
/// `0` for a tile that failed to fetch -- so it is unpacked in one place.
fn unpack<'a>(data: &'a [u8], lengths: &[u32]) -> Result<Vec<Option<&'a [u8]>>, JsError> {
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
    Ok(slices)
}

/// A drawn label tile: transparent ground, labels and icons over it.
#[wasm_bindgen]
pub struct LabelTile {
    pixels: Vec<u8>,
    placed: usize,
}

#[wasm_bindgen]
impl LabelTile {
    /// How many labels were placed. Zero means the tile is empty and the host
    /// should serve one shared transparent image rather than encoding this.
    #[wasm_bindgen(getter)]
    pub fn placed(&self) -> usize {
        self.placed
    }

    /// Takes the **premultiplied** RGBA pixels, `tileSize * tileSize * 4`
    /// bytes, leaving this empty.
    ///
    /// Named for what it does: crossing into JS copies the buffer, and a tile
    /// at label resolution is megabytes, so it is moved rather than cloned.
    /// Premultiplied, not straight -- encoding it as straight alpha turns
    /// every halo grey.
    #[wasm_bindgen(js_name = takePixels)]
    pub fn take_pixels(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pixels)
    }
}

#[wasm_bindgen]
pub struct VectorTileRenderer {
    inner: Renderer,
}

#[wasm_bindgen]
impl VectorTileRenderer {
    /// Parses a `style.json`. Throws if the style is unusable.
    /// `displayTileSize` is how many CSS px one tile covers where the page
    /// shows it -- not the pixel count a render is asked for (a label pass
    /// draws at twice the pixels, and a 256 px tile is still 256 px wide on
    /// screen however many pixels it carries). It sets the size the style
    /// draws at and the zoom its expressions are read at; leaving it out
    /// keeps 512, which is what a raster source declared at 512 gets.
    #[wasm_bindgen(constructor)]
    pub fn new(
        style_json: &str,
        display_tile_size: Option<u32>,
    ) -> Result<VectorTileRenderer, JsError> {
        Renderer::new(style_json)
            .map(|renderer| match display_tile_size {
                Some(dp) if dp > 0 => renderer.with_display_tile_size(dp),
                _ => renderer,
            })
            .map(|inner| VectorTileRenderer { inner })
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// The default tile size, matching the SDK cores' raster layer default.
    #[wasm_bindgen(js_name = defaultTileSize)]
    pub fn default_tile_size() -> u32 {
        DEFAULT_TILE_SIZE
    }

    /// Source tiles needed to draw `z/x/y`, as a JSON array of
    /// `{ sourceId, url, z, x, y, scale, offsetX, offsetY, labelsOnly }`.
    ///
    /// `labelsOnly` marks a neighbour the label pass wants so a name at the
    /// tile edge can be drawn whole. Nothing else reads those tiles, so a host
    /// drawing the ground alone can leave their entries null and skip the
    /// fetch -- but it must leave the *entries*, because the bytes are handed
    /// back positionally.
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
                    "labelsOnly": r.labels_only,
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
        let slices = unpack(data, lengths)?;
        self.inner
            .render(z, x, y, tile_size, &slices)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Rasterises the ground alone -- fills, lines and circles, no labels or
    /// icons -- to PNG bytes.
    ///
    /// The half of a split layer that a GPU can draw and that fonts arriving
    /// never invalidate.
    #[wasm_bindgen(js_name = renderGeometry)]
    pub fn render_geometry(
        &self,
        z: u8,
        x: u32,
        y: u32,
        tile_size: u32,
        data: &[u8],
        lengths: &[u32],
    ) -> Result<Vec<u8>, JsError> {
        let slices = unpack(data, lengths)?;
        self.inner
            .render_geometry(z, x, y, tile_size, &slices)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Draws the labels and icons alone, on a transparent ground.
    ///
    /// The other half of a split layer: this is redrawn on its own when a font
    /// arrives, leaving the ground untouched.
    #[wasm_bindgen(js_name = renderLabels)]
    pub fn render_labels(
        &self,
        z: u8,
        x: u32,
        y: u32,
        tile_size: u32,
        data: &[u8],
        lengths: &[u32],
    ) -> Result<LabelTile, JsError> {
        let slices = unpack(data, lengths)?;
        self.inner
            .render_labels_rgba(z, x, y, tile_size, &slices)
            .map(|(pixels, placed)| LabelTile { pixels, placed })
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Draws the labels and icons alone, on a transparent ground, as a PNG.
    ///
    /// What a browser actually wants: a raster layer loads an image, and
    /// turning raw pixels into one through a canvas costs more than the
    /// drawing did. Returns an empty array when nothing was placed, so the
    /// host can serve one shared transparent tile instead of encoding a
    /// megabyte of nothing.
    #[wasm_bindgen(js_name = renderLabelsPng)]
    pub fn render_labels_png(
        &self,
        z: u8,
        x: u32,
        y: u32,
        tile_size: u32,
        data: &[u8],
        lengths: &[u32],
    ) -> Result<LabelTile, JsError> {
        let slices = unpack(data, lengths)?;
        self.inner
            .render_labels_png(z, x, y, tile_size, &slices)
            .map(|(pixels, placed)| LabelTile { pixels, placed })
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Draws labels and icons over pixels the host already has, in place.
    ///
    /// `rgba` must hold exactly `tileSize * tileSize * 4` bytes and is read as
    /// **straight** alpha, which is what a WebGL readback is. Returns how many
    /// labels were placed.
    #[wasm_bindgen(js_name = drawLabels)]
    pub fn draw_labels(
        &self,
        z: u8,
        x: u32,
        y: u32,
        tile_size: u32,
        rgba: &mut [u8],
        data: &[u8],
        lengths: &[u32],
    ) -> Result<usize, JsError> {
        let slices = unpack(data, lengths)?;
        self.inner
            .draw_labels_onto(z, x, y, tile_size, rgba, &slices)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// The style's `glyphs` URL template, or `undefined` when it names none.
    #[wasm_bindgen(js_name = glyphsUrlTemplate)]
    pub fn glyphs_url_template(&self) -> Option<String> {
        self.inner.glyphs_url_template().map(str::to_owned)
    }

    /// Features in the tile as a JSON array; see `Renderer::query_features_json`.
    /// Empty strings mean "no restriction". A filter that is not JSON errors.
    #[wasm_bindgen(js_name = queryFeatures)]
    #[allow(clippy::too_many_arguments)]
    pub fn query_features(
        &self,
        z: u8,
        x: u32,
        y: u32,
        data: &[u8],
        lengths: &[u32],
        source_layer: &str,
        filter_json: &str,
        text: &str,
        limit: u32,
    ) -> Result<String, JsError> {
        let slices = unpack(data, lengths)?;
        let none_if_empty = |s: &str| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_owned())
            }
        };
        let source_layer = none_if_empty(source_layer);
        let filter_json = none_if_empty(filter_json);
        let text = none_if_empty(text);
        self.inner
            .query_features_json(
                z,
                x,
                y,
                &slices,
                source_layer.as_deref(),
                filter_json.as_deref(),
                text.as_deref(),
                limit as usize,
            )
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// URLs of the glyph ranges this tile's labels need and the store has not
    /// got, as a JSON array of strings.
    ///
    /// Empty when the style names no template or everything is already loaded.
    #[wasm_bindgen(js_name = neededGlyphs)]
    pub fn needed_glyphs(
        &self,
        z: u8,
        x: u32,
        y: u32,
        data: &[u8],
        lengths: &[u32],
    ) -> Result<String, JsError> {
        let Some(template) = self.inner.glyphs_url_template().map(str::to_owned) else {
            return Ok("[]".to_string());
        };
        let slices = unpack(data, lengths)?;
        let urls: Vec<serde_json::Value> = self
            .inner
            .needed_glyphs(z, x, y, &slices)
            .iter()
            .map(|request| serde_json::Value::String(request.url(&template)))
            .collect();
        Ok(serde_json::Value::Array(urls).to_string())
    }

    /// Adds one fetched glyph range PBF. Returns how many glyphs it carried.
    #[wasm_bindgen(js_name = addGlyphs)]
    pub fn add_glyphs(&self, pbf: &[u8]) -> Result<usize, JsError> {
        self.inner
            .add_glyphs(pbf)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Whether any glyph has been loaded. Labels need at least one.
    #[wasm_bindgen(js_name = hasGlyphs)]
    pub fn has_glyphs(&self) -> bool {
        self.inner.has_glyphs()
    }

    /// The sprite sheet's `.json` and `.png` URLs at `pixelRatio`, as a JSON
    /// array of two strings, or `[]` when the style names no sprite.
    #[wasm_bindgen(js_name = spriteUrls)]
    pub fn sprite_urls(&self, pixel_ratio: u32) -> String {
        let urls: Vec<serde_json::Value> = match self.inner.sprite_urls(pixel_ratio.max(1)) {
            Some(request) => vec![
                serde_json::Value::String(request.json_url),
                serde_json::Value::String(request.png_url),
            ],
            None => Vec::new(),
        };
        serde_json::Value::Array(urls).to_string()
    }

    /// Adds the fetched sprite sheet. Returns how many icons it carried.
    #[wasm_bindgen(js_name = addSprite)]
    pub fn add_sprite(&self, json: &str, png: &[u8]) -> Result<usize, JsError> {
        self.inner
            .add_sprite(json, png)
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Whether the style names a sprite the renderer has not been given yet.
    #[wasm_bindgen(js_name = needsSprite)]
    pub fn needs_sprite(&self) -> bool {
        self.inner.needs_sprite()
    }

    /// Whether this tile has to be drawn on the CPU because the style paints
    /// something a GPU path cannot -- today, a patterned fill.
    #[wasm_bindgen(js_name = needsCpu)]
    pub fn needs_cpu(&self, z: u8, data: &[u8], lengths: &[u32]) -> Result<bool, JsError> {
        let slices = unpack(data, lengths)?;
        Ok(self.inner.needs_cpu(z, &slices))
    }

    /// Credits the style's sources ask to be shown, as a JSON array of
    /// strings.
    ///
    /// The host must display these: a style is data under someone's licence.
    /// May contain HTML, because a credit is normally a link to the licence.
    pub fn attributions(&self) -> String {
        let items: Vec<serde_json::Value> = self
            .inner
            .attributions()
            .into_iter()
            .map(serde_json::Value::String)
            .collect();
        serde_json::Value::Array(items).to_string()
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
        Ok(self
            .inner
            .set_source_tiles(source_id, tiles, minzoom, maxzoom))
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
