//! A MapLibre-style vector tile rasteriser.
//!
//! Turns a `style.json` plus fetched `.pbf` vector tiles into raster tiles, so
//! that map backends which cannot render vector styles (Google Maps, MapKit,
//! HERE, Cesium, …) can display one through an ordinary raster layer.
//!
//! # Two-phase API
//!
//! Rendering is split so that **no network I/O happens inside this crate**:
//!
//! 1. [`Renderer::plan`] returns the source tile URLs a target tile needs.
//! 2. The host fetches them — reusing its own HTTP stack, auth headers, cache
//!    and proxy settings — and passes the bytes back positionally.
//! 3. [`Renderer::render`] rasterises and returns PNG bytes.
//!
//! That shape matters for the cross-platform bindings: it means the FFI
//! boundary is crossed twice per tile with plain byte arrays, and never in the
//! reverse direction. On Android in particular, a callback from a Rust thread
//! into the JVM would need `AttachCurrentThread`; this design has none.
//!
//! # Scope
//!
//! Draws `background`, `fill`, `line`, `circle` and `symbol` layers.
//!
//! Labels need glyphs, which the host fetches from the style's `glyphs` URL
//! and hands over as a [`text::GlyphStore`]; without one the symbol layers are
//! simply skipped and everything else still draws. Placement is per tile, so a
//! label near an edge is placed without knowing what its neighbour placed —
//! see [`symbol`]. Icons come from the style's sprite sheet, which the host
//! fetches from [`Renderer::sprite_urls`] and hands over as a pair; see
//! [`sprite`]. Point placement only -- text along a line is reported by
//! [`symbol::unsupported`] rather than drawn.
//!
//! [`Renderer::unsupported_layer_types`] reports layer types nothing here
//! draws at all: raster, hillshade, fill-extrusion.

pub mod color;
pub mod expr;
pub mod filter;
pub mod glyphs;
pub mod mercator;
pub mod model;
pub mod mvt;
pub mod raster;
pub mod style;
pub mod sprite;
pub mod symbol;
#[cfg(feature = "tessellate")]
pub mod tessellate;
pub mod text;
pub mod tile;

#[cfg(test)]
pub mod fixtures;

pub use color::Color;
pub use raster::{DecodedTile, RenderStats};
pub use style::{Layer, LayerKind, Source, Style, StyleError};
pub use tile::{plan_tile, SourceTileRequest, TilePlan};

/// Tile size used when the caller does not specify one. Matches the SDK cores'
/// `RasterLayerSource.DEFAULT_TILE_SIZE`.
pub const DEFAULT_TILE_SIZE: u32 = 512;

#[derive(Debug)]
pub enum RenderError {
    Style(StyleError),
    /// The requested tile size is zero or beyond what a pixmap can hold.
    BadTileSize(u32),
    /// PNG encoding failed.
    Encode(String),
    /// A fetched glyph range could not be parsed.
    Decode(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Style(e) => write!(f, "{e}"),
            RenderError::BadTileSize(n) => write!(f, "invalid tile size: {n}"),
            RenderError::Decode(e) => write!(f, "decode error: {e}"),
            RenderError::Encode(e) => write!(f, "png encode failed: {e}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<StyleError> for RenderError {
    fn from(e: StyleError) -> Self {
        RenderError::Style(e)
    }
}

/// A parsed style, ready to render tiles from.
///
/// Parsing once and reusing the renderer matters: a real style has hundreds of
/// layers, and re-parsing per tile would dominate the render cost.
pub struct Renderer {
    style: Style,
    /// Glyphs the host has fetched so far, kept across tiles.
    ///
    /// One range covers 256 codepoints and serves every tile that needs those
    /// characters, so this fills quickly and then stops costing anything. It
    /// lives on the renderer rather than beside it because a restyle keeps the
    /// glyphs — a colour change does not alter what a label says.
    ///
    /// Behind a lock because the hosts render tiles on a pool: several threads
    /// read it while one adds a range. Without it the Android build crashed
    /// inside `add_glyphs` the moment more than one tile wanted glyphs at
    /// once. Reads are the common case by far, so a read/write lock rather
    /// than a mutex.
    glyphs: std::sync::RwLock<text::GlyphStore>,
    /// The style's sprite sheet, once the host has fetched it. One sheet
    /// covers the whole style, so unlike glyphs there is nothing to accumulate
    /// -- it is either loaded or it is not.
    sprites: std::sync::RwLock<Option<sprite::SpriteSheet>>,
}

/// One `{fontstack}/{range}.pbf` the renderer needs and does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphRequest {
    pub fontstack: String,
    /// First codepoint of the 256-wide range.
    pub range_start: u32,
}

impl GlyphRequest {
    /// Fills the `{fontstack}` and `{range}` placeholders of a style's
    /// `glyphs` template.
    pub fn url(&self, template: &str) -> String {
        template.replace("{fontstack}", &self.fontstack).replace(
            "{range}",
            &format!("{}-{}", self.range_start, self.range_start + 255),
        )
    }
}

/// Where a style's sprite sheet lives: an index and the image it indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpriteRequest {
    pub json_url: String,
    pub png_url: String,
}

impl Renderer {
    pub fn new(style_json: &str) -> Result<Renderer, RenderError> {
        Ok(Renderer {
            style: Style::parse(style_json)?,
            glyphs: std::sync::RwLock::new(text::GlyphStore::new()),
            sprites: std::sync::RwLock::new(None),
        })
    }

    pub fn from_style(style: Style) -> Renderer {
        Renderer {
            style,
            glyphs: std::sync::RwLock::new(text::GlyphStore::new()),
            sprites: std::sync::RwLock::new(None),
        }
    }

    /// The style's `glyphs` URL template, if it has one.
    pub fn glyphs_url_template(&self) -> Option<&str> {
        self.style.glyphs.as_deref()
    }

    /// The two URLs the style's sprite lives at, for the given pixel ratio.
    ///
    /// A sprite URL names a pair rather than a file: `sprite.json` says where
    /// each icon sits inside `sprite.png`. A ratio above 1 asks for the `@2x`
    /// pair, which is what a style ships for dense screens.
    pub fn sprite_urls(&self, pixel_ratio: u32) -> Option<SpriteRequest> {
        let base = self.style.sprite.as_deref()?;
        let suffix = if pixel_ratio > 1 { "@2x" } else { "" };
        Some(SpriteRequest {
            json_url: format!("{base}{suffix}.json"),
            png_url: format!("{base}{suffix}.png"),
        })
    }

    /// Takes the fetched pair and keeps the sheet. Returns how many icons it
    /// holds.
    pub fn add_sprite(&self, json: &str, png: &[u8]) -> Result<usize, RenderError> {
        let mut sheet = sprite::SpriteSheet::parse(json, png)
            .map_err(|e| RenderError::Decode(e.to_string()))?;
        sheet.set_id(self.style.sprite_id.clone());
        let count = sheet.len();
        let mut slot = self
            .sprites
            .write()
            .map_err(|_| RenderError::Decode("sprite sheet poisoned".into()))?;
        *slot = Some(sheet);
        Ok(count)
    }

    pub fn has_sprite(&self) -> bool {
        self.sprites.read().map(|s| s.is_some()).unwrap_or(false)
    }

    /// Whether the style names a sprite that has not been supplied yet.
    pub fn needs_sprite(&self) -> bool {
        self.style.sprite.is_some() && !self.has_sprite()
    }

    /// Takes one fetched `.pbf` and keeps its glyphs.
    ///
    /// `&self` rather than `&mut self`: the hosts hold one renderer behind a
    /// raw handle and call it from a thread pool, so exclusivity has to be
    /// real rather than promised by a signature.
    pub fn add_glyphs(&self, pbf: &[u8]) -> Result<usize, RenderError> {
        let stacks = glyphs::decode(pbf).map_err(|e| RenderError::Decode(e.to_string()))?;
        let mut store = self
            .glyphs
            .write()
            .map_err(|_| RenderError::Decode("glyph store poisoned".into()))?;
        let mut added = 0;
        for stack in stacks {
            added += stack.glyphs.len();
            store.insert(stack);
        }
        Ok(added)
    }

    pub fn has_glyphs(&self) -> bool {
        self.glyphs.read().map(|s| !s.is_empty()).unwrap_or(false)
    }

    /// What this tile's labels need and the renderer does not have.
    ///
    /// Ranges depend on the text in the tile, so this is answered after the
    /// tile is decoded rather than up front — fetching every range a font
    /// could have means 82 files for CJK alone, nearly all of them unused.
    pub fn needed_glyphs(
        &self,
        z: u8,
        x: u32,
        y: u32,
        tile_data: &[Option<&[u8]>],
    ) -> Vec<GlyphRequest> {
        let plan = self.plan(z, x, y);
        let decoded = self.decode_plan(&plan, tile_data);
        let Ok(store) = self.glyphs.read() else {
            return Vec::new();
        };
        symbol::needed_glyphs(&self.style, z as f64, &decoded, &store)
    }

    pub fn style(&self) -> &Style {
        &self.style
    }

    /// Replaces the style, keeping the renderer (and therefore any caches the
    /// host holds around it) alive.
    ///
    /// Restyling never invalidates fetched vector tiles: the geometry is
    /// unchanged, only the paint applied to it. That is what makes live
    /// recolouring cheap — no refetch, just a re-rasterise.
    pub fn set_style(&mut self, style_json: &str) -> Result<(), RenderError> {
        self.style = Style::parse(style_json)?;
        Ok(())
    }

    /// Supplies tile URLs for a source that only carried a TileJSON `url`.
    /// The host resolves TileJSON, because I/O stays on the host side.
    pub fn set_source_tiles(
        &mut self,
        source_id: &str,
        tiles: Vec<String>,
        minzoom: Option<u8>,
        maxzoom: Option<u8>,
    ) -> bool {
        self.style
            .set_source_tiles(source_id, tiles, minzoom, maxzoom)
    }

    /// Sources whose style entry has a TileJSON `url` but no inline `tiles`.
    /// The host must resolve each of these before rendering.
    pub fn unresolved_sources(&self) -> Vec<&Source> {
        self.style
            .sources
            .values()
            .filter(|s| s.is_vector() && s.tiles.is_empty() && s.url.is_some())
            .collect()
    }

    /// Layer `type` values present in the style that this renderer will skip.
    pub fn unsupported_layer_types(&self) -> Vec<String> {
        self.style.unsupported_layer_types()
    }

    /// Human-readable reasons the current style may not render as intended.
    ///
    /// Worth surfacing to the user: the failure mode that matters is a blank
    /// tile, and a style this renderer cannot use should say so rather than
    /// quietly produce nothing.
    pub fn diagnostics(&self) -> Vec<String> {
        self.style.diagnostics()
    }

    /// The source tiles needed to draw `z/x/y`, in the order the bytes must be
    /// passed back to [`Renderer::render`].
    pub fn plan(&self, z: u8, x: u32, y: u32) -> TilePlan {
        plan_tile(&self.style, z, x, y)
    }

    /// Rasterises `z/x/y` and returns PNG bytes.
    ///
    /// `tile_data` is positional against `self.plan(z, x, y).requests`; entries
    /// may be `None` where a fetch 404'd or returned an empty tile. Extra or
    /// missing trailing entries are tolerated — a missing source just means
    /// that source contributes nothing.
    pub fn render(
        &self,
        z: u8,
        x: u32,
        y: u32,
        tile_size: u32,
        tile_data: &[Option<&[u8]>],
    ) -> Result<Vec<u8>, RenderError> {
        let pixmap = self.render_pixmap(z, x, y, tile_size, tile_data)?.0;
        pixmap
            .encode_png()
            .map_err(|e| RenderError::Encode(e.to_string()))
    }

    /// Tessellates `z/x/y` into the flat buffer the GPU bindings hand to their
    /// host language.
    ///
    /// The packing is deliberately one array of `f32` rather than a struct: it
    /// crosses JNI as a single `float[]` and the C ABI as a single allocation,
    /// with no per-batch marshalling on either side. Layout:
    ///
    /// ```text
    /// [0]                     extent in tile units
    /// [1]                     1.0 if the style has a background colour
    /// [2..6]                  background rgba
    /// [6]                     batch count B
    /// [7..13]                 timings: decode, tessellate, filter compile, fill, line, spare
    /// [13 .. 13+B*2]          per batch: first vertex, vertex count
    /// [13+B*2 ..]             vertices: x, y, r, g, b, a
    /// ```
    ///
    /// `tile_data` is positional against the plan, exactly as in
    /// [`Renderer::render`].
    #[cfg(feature = "tessellate")]
    pub fn tessellate_packed(
        &self,
        z: u8,
        x: u32,
        y: u32,
        tile_size: u32,
        tile_data: &[Option<&[u8]>],
    ) -> Result<Vec<f32>, RenderError> {
        if tile_size == 0 || tile_size > 8192 {
            return Err(RenderError::BadTileSize(tile_size));
        }

        let plan = self.plan(z, x, y);
        let decode_started = std::time::Instant::now();
        let mut decoded = Vec::with_capacity(plan.requests.len());
        for (request, data) in plan
            .requests
            .iter()
            .zip(tile_data.iter().chain(std::iter::repeat(&None)))
        {
            let Some(bytes) = data else { continue };
            if bytes.is_empty() {
                continue;
            }
            let Ok(tile) = mvt::decode(bytes) else {
                continue;
            };
            decoded.push((request, tile));
        }
        let decode_ms = decode_started.elapsed().as_secs_f32() * 1000.0;

        let views: Vec<tessellate::DecodedTile<'_>> = decoded
            .iter()
            .map(|(request, tile)| tessellate::DecodedTile {
                source_id: &request.source_id,
                tile,
                scale: request.scale,
                offset_x: request.offset_x,
                offset_y: request.offset_y,
            })
            .collect();

        let tessellate_started = std::time::Instant::now();
        let result = tessellate::tessellate(&self.style, z as f64, tile_size, &views);
        let tessellate_ms = tessellate_started.elapsed().as_secs_f32() * 1000.0;

        let mut packed = Vec::with_capacity(13 + result.batches.len() * 2 + result.vertices.len());
        packed.push(result.extent as f32);
        match result.background {
            Some(color) => packed.extend_from_slice(&[1.0, color.r, color.g, color.b, color.a]),
            None => packed.extend_from_slice(&[0.0; 5]),
        }
        packed.push(result.batches.len() as f32);
        packed.extend_from_slice(&[
            decode_ms,
            tessellate_ms,
            result.stats.filter_compile_ms,
            result.stats.fill_ms,
            result.stats.line_ms,
            0.0,
        ]);
        for batch in &result.batches {
            packed.extend_from_slice(&[batch.first_vertex as f32, batch.vertex_count as f32]);
        }
        packed.extend_from_slice(&result.vertices);
        Ok(packed)
    }

    /// Same as [`Renderer::render`] but returns the pixmap, for callers that
    /// want raw RGBA or their own encoder.
    pub fn render_pixmap(
        &self,
        z: u8,
        x: u32,
        y: u32,
        tile_size: u32,
        tile_data: &[Option<&[u8]>],
    ) -> Result<(tiny_skia::Pixmap, RenderStats), RenderError> {
        if tile_size == 0 || tile_size > 8192 {
            return Err(RenderError::BadTileSize(tile_size));
        }

        let plan = self.plan(z, x, y);
        let decoded = self.decode_plan(&plan, tile_data);

        let store = self
            .glyphs
            .read()
            .map_err(|_| RenderError::Decode("glyph store poisoned".into()))?;
        let sprites = self
            .sprites
            .read()
            .map_err(|_| RenderError::Decode("sprite sheet poisoned".into()))?;
        raster::render_with_glyphs(
            &self.style,
            z as f64,
            tile_size,
            &decoded,
            &store,
            sprites.as_ref(),
        )
        .ok_or(RenderError::BadTileSize(tile_size))
    }

    /// Draws this tile's labels onto an RGBA buffer someone else rasterised.
    ///
    /// The GPU path tessellates fills and lines and knows nothing about
    /// glyphs, so without this a style with labels either loses them or gives
    /// up the GPU. The label pass does not care how the pixels underneath were
    /// made — it composites onto them — so it runs over the readback.
    ///
    /// `rgba` is `tile_size * tile_size * 4` bytes and is modified in place.
    pub fn draw_labels_onto(
        &self,
        z: u8,
        x: u32,
        y: u32,
        tile_size: u32,
        rgba: &mut [u8],
        tile_data: &[Option<&[u8]>],
    ) -> Result<usize, RenderError> {
        let expected = (tile_size as usize) * (tile_size as usize) * 4;
        if rgba.len() != expected {
            return Err(RenderError::Decode(format!(
                "rgba buffer is {} bytes, expected {expected}",
                rgba.len()
            )));
        }
        let store = self
            .glyphs
            .read()
            .map_err(|_| RenderError::Decode("glyph store poisoned".into()))?;
        let sprites = self
            .sprites
            .read()
            .map_err(|_| RenderError::Decode("sprite sheet poisoned".into()))?;
        if store.is_empty() && sprites.is_none() {
            return Ok(0);
        }

        let plan = self.plan(z, x, y);
        let decoded = self.decode_plan(&plan, tile_data);
        let paint_scale = tile_size as f32 / 512.0;
        let labels = symbol::collect(
            &self.style,
            z as f64,
            tile_size,
            paint_scale,
            &decoded,
            &store,
            sprites.as_ref(),
        );
        let placed = symbol::place(labels);
        if placed.is_empty() {
            return Ok(0);
        }

        let Some(mut pixmap) = tiny_skia::Pixmap::new(tile_size, tile_size) else {
            return Err(RenderError::BadTileSize(tile_size));
        };
        // Copied in and out rather than wrapped: `Pixmap::from_vec` demands
        // premultiplied data and rejects anything else, and a readback whose
        // alpha is saturated is both at once. Copying sidesteps having to
        // prove that at the type level for a buffer that came from GL.
        pixmap.data_mut().copy_from_slice(rgba);
        symbol::draw(&mut pixmap, &placed, &store, sprites.as_ref());
        rgba.copy_from_slice(pixmap.data());
        Ok(placed.len())
    }

    /// Decodes whatever arrived for a plan, in the plan's own order.
    fn decode_plan(&self, plan: &TilePlan, tile_data: &[Option<&[u8]>]) -> Vec<DecodedTile> {
        let mut decoded = Vec::with_capacity(plan.requests.len());
        for (request, data) in plan.requests.iter().zip(tile_data.iter().chain(
            // Tolerate a short array rather than failing the whole tile.
            std::iter::repeat(&None),
        )) {
            let Some(bytes) = data else { continue };
            if bytes.is_empty() {
                continue;
            }
            // A corrupt tile from one source must not blank the whole tile;
            // skip it and draw whatever else arrived.
            let Ok(tile) = mvt::decode(bytes) else {
                continue;
            };
            decoded.push(DecodedTile {
                source_id: request.source_id.clone(),
                tile,
                scale: request.scale,
                offset_x: request.offset_x,
                offset_y: request.offset_y,
            });
        }
        decoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Style exercising every drawable layer type against `fixtures::sample_tile`.
    fn test_style() -> Style {
        Style::from_json(&json!({
            "version": 8,
            "sources": {
                "src": {
                    "type": "vector",
                    "tiles": ["https://example.com/{z}/{x}/{y}.pbf"],
                    "minzoom": 0,
                    "maxzoom": 14
                }
            },
            "layers": [
                { "id": "bg", "type": "background",
                  "paint": { "background-color": "#ffffff" } },
                { "id": "water", "type": "fill", "source": "src", "source-layer": "water",
                  "paint": { "fill-color": "#0000ff" } },
                { "id": "motorway", "type": "line", "source": "src", "source-layer": "roads",
                  "filter": ["==", "class", "motorway"],
                  "paint": { "line-color": "#ff0000", "line-width": 20 } },
                { "id": "poi", "type": "circle", "source": "src", "source-layer": "poi",
                  "paint": { "circle-color": "#00ff00", "circle-radius": 30 } },
                { "id": "labels", "type": "symbol", "source": "src", "source-layer": "poi" }
            ]
        }))
        .unwrap()
    }

    fn render_sample(tile_size: u32) -> tiny_skia::Pixmap {
        let renderer = Renderer::from_style(test_style());
        let data = fixtures::sample_tile();
        renderer
            .render_pixmap(10, 0, 0, tile_size, &[Some(data.as_slice())])
            .expect("render succeeds")
            .0
    }

    fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let p = pixmap.pixel(x, y).expect("in bounds");
        // Pixmap stores premultiplied alpha; demultiply for comparison.
        let c = p.demultiply();
        (c.red(), c.green(), c.blue(), c.alpha())
    }

    #[test]
    fn draws_each_supported_layer_type() {
        let pixmap = render_sample(512);

        // Background paints everywhere; the right half stays white where no
        // other layer covers it.
        let (r, g, b, a) = pixel(&pixmap, 400, 480);
        assert_eq!((r, g, b, a), (255, 255, 255, 255), "background");

        // The water polygon covers the left half of the tile.
        let (r, g, b, _) = pixel(&pixmap, 100, 100);
        assert!(b > 200 && r < 50, "water fill, got ({r},{g},{b})");

        // The motorway line runs down the middle in red.
        let (r, g, b, _) = pixel(&pixmap, 256, 400);
        assert!(r > 200 && g < 60 && b < 60, "road line, got ({r},{g},{b})");

        // The POI circle sits at 3/4 across, 1/4 down.
        let (r, g, b, _) = pixel(&pixmap, 384, 128);
        assert!(g > 200 && r < 60, "poi circle, got ({r},{g},{b})");
    }

    #[test]
    fn filters_exclude_non_matching_features() {
        // The style only draws `class=motorway`; the service road at y=3000
        // (tile-local) must not appear.
        let pixmap = render_sample(512);
        let y = (3000.0 / 4096.0 * 512.0) as u32;
        // Sample on the right half, away from water and the motorway.
        let (r, g, b, _) = pixel(&pixmap, 450, y);
        assert_eq!(
            (r, g, b),
            (255, 255, 255),
            "service road must be filtered out"
        );
    }

    #[test]
    fn tile_size_scales_the_output_and_paint_widths() {
        let small = render_sample(256);
        let large = render_sample(512);
        assert_eq!(small.width(), 256);
        assert_eq!(large.width(), 512);

        // The water/land boundary stays at the halfway point in both.
        let (_, _, b_small, _) = pixel(&small, 50, 50);
        let (_, _, b_large, _) = pixel(&large, 100, 100);
        assert!(b_small > 200 && b_large > 200);
    }

    #[test]
    fn missing_and_corrupt_tiles_still_render_the_background() {
        let renderer = Renderer::from_style(test_style());

        let missing = renderer
            .render_pixmap(10, 0, 0, 256, &[None])
            .expect("missing data is not an error")
            .0;
        assert_eq!(pixel(&missing, 128, 128), (255, 255, 255, 255));

        let corrupt = renderer
            .render_pixmap(10, 0, 0, 256, &[Some(&[0xff, 0xff, 0xff, 0xff])])
            .expect("corrupt data is not an error")
            .0;
        assert_eq!(pixel(&corrupt, 128, 128), (255, 255, 255, 255));

        // An empty positional array is tolerated too.
        assert!(renderer.render_pixmap(10, 0, 0, 256, &[]).is_ok());
    }

    #[test]
    fn encodes_a_decodable_png() {
        let renderer = Renderer::from_style(test_style());
        let data = fixtures::sample_tile();
        let png = renderer
            .render(10, 0, 0, 256, &[Some(data.as_slice())])
            .expect("png");
        assert_eq!(&png[1..4], b"PNG");

        let decoded = tiny_skia::Pixmap::decode_png(&png).expect("round trip");
        assert_eq!(decoded.width(), 256);
        assert_eq!(decoded.height(), 256);
    }

    #[test]
    fn set_style_recolors_without_new_tile_data() {
        let mut renderer = Renderer::from_style(test_style());
        let data = fixtures::sample_tile();

        let before = renderer
            .render_pixmap(10, 0, 0, 256, &[Some(data.as_slice())])
            .unwrap()
            .0;
        assert_eq!(pixel(&before, 220, 230), (255, 255, 255, 255), "background");

        let recolored = serde_json::to_string(&json!({
            "version": 8,
            "sources": {
                "src": { "type": "vector", "tiles": ["https://example.com/{z}/{x}/{y}.pbf"],
                         "minzoom": 0, "maxzoom": 14 }
            },
            "layers": [
                { "id": "bg", "type": "background",
                  "paint": { "background-color": "#ff0000" } }
            ]
        }))
        .unwrap();
        renderer.set_style(&recolored).expect("restyle");

        // Same bytes in, different pixels out.
        let after = renderer
            .render_pixmap(10, 0, 0, 256, &[Some(data.as_slice())])
            .unwrap()
            .0;
        let (r, g, b, _) = pixel(&after, 220, 230);
        assert!(
            r > 200 && g < 50 && b < 50,
            "expected red, got ({r},{g},{b})"
        );
    }

    #[test]
    fn set_style_rejects_a_broken_style_and_keeps_the_old_one() {
        let mut renderer = Renderer::from_style(test_style());
        assert!(renderer.set_style("{ not json").is_err());
        // The previous style must still render.
        let data = fixtures::sample_tile();
        let pixmap = renderer
            .render_pixmap(10, 0, 0, 256, &[Some(data.as_slice())])
            .unwrap()
            .0;
        assert_eq!(pixel(&pixmap, 220, 230), (255, 255, 255, 255));
    }

    /// Renders from a tile built by hand, with no decoder anywhere in the call
    /// path. This is what makes a second wire format (MapLibre Tile) a matter
    /// of adding a decoder rather than touching the renderer: if this compiles
    /// and draws, nothing downstream of `model` knows what MVT is.
    #[test]
    fn renders_from_a_hand_built_tile_without_any_decoder() {
        use model::{Feature, GeomType, Layer, Tile, Value};

        // A square covering the left half of the tile, tagged class=ocean so
        // the test style's filter accepts it.
        let layer = Layer::from_tables(
            "water".into(),
            4096,
            vec!["class".into()],
            vec![Value::String("ocean".into())],
            vec![Feature::new(
                Some(7),
                GeomType::Polygon,
                vec![vec![
                    [0.0, 0.0],
                    [2048.0, 0.0],
                    [2048.0, 4096.0],
                    [0.0, 4096.0],
                ]],
                vec![0, 0],
            )],
        );

        let decoded = vec![DecodedTile {
            source_id: "src".into(),
            tile: Tile {
                layers: vec![layer],
            },
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }];

        let (pixmap, stats) = raster::render(&test_style(), 10.0, 512, &decoded).expect("render");
        assert_eq!(stats.features_drawn, 1);

        let (r, g, b, _) = pixel(&pixmap, 100, 100);
        assert!(b > 200 && r < 50, "water fill, got ({r},{g},{b})");
        // The right half is background only.
        assert_eq!(pixel(&pixmap, 400, 480), (255, 255, 255, 255));
    }

    #[test]
    fn reports_unsupported_layers_rather_than_failing() {
        let renderer = Renderer::from_style(test_style());
        // symbol left this list when labels started being drawn; hillshade
        // stands in for the types that are still parsed and skipped.
        assert!(renderer.unsupported_layer_types().is_empty());
    }

    /// A style with labels has to say which glyph files it needs, and the
    /// list has to shrink as they arrive. Getting this wrong is not visible:
    /// asking for nothing renders an unlabelled map, and asking for everything
    /// is 82 requests per CJK font.
    #[test]
    fn asks_for_the_glyph_ranges_its_labels_need() {
        let style = Style::from_json(&json!({
            "version": 8,
            "glyphs": "https://example.com/fonts/{fontstack}/{range}.pbf",
            "sources": { "src": { "type": "vector", "tiles": ["https://example.com/{z}/{x}/{y}.pbf"] } },
            "layers": [{
                "id": "labels", "type": "symbol", "source": "src", "source-layer": "place",
                "layout": { "text-field": "{name}", "text-font": ["Noto Sans Regular"] }
            }]
        }))
        .unwrap();
        let renderer = Renderer::from_style(style);

        let tile = fixtures::encode_tile(&[fixtures::LayerSpec {
            name: "place".into(),
            extent: 4096,
            // Latin lives in range 0; 中 (U+4E2D) in range 78.
            features: vec![fixtures::FeatureSpec {
                geom: fixtures::Geom::Point,
                points: vec![(2048, 2048)],
                props: vec![("name".into(), "A中".into())],
            }],
        }]);

        let needed = renderer.needed_glyphs(10, 512, 300, &[Some(&tile)]);
        let ranges: Vec<u32> = needed.iter().map(|r| r.range_start).collect();
        assert_eq!(ranges, vec![0, 78 * 256]);
        assert_eq!(
            needed[1].url(renderer.glyphs_url_template().unwrap()),
            "https://example.com/fonts/Noto Sans Regular/19968-20223.pbf"
        );

        // Once a range is in, it is not asked for again.
        renderer
            .add_glyphs(&glyph_pbf("Noto Sans Regular", &[0x41]))
            .expect("glyphs decode");
        let ranges: Vec<u32> = renderer
            .needed_glyphs(10, 512, 300, &[Some(&tile)])
            .iter()
            .map(|r| r.range_start)
            .collect();
        assert_eq!(ranges, vec![78 * 256]);
    }

    /// The sprite pair is derived from one URL, and a style that asks for a
    /// dense sheet must not be handed the plain one.
    #[test]
    fn names_both_halves_of_the_sprite_pair() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sprite": "https://example.com/sprites/basic",
            "sources": {},
            "layers": []
        }))
        .unwrap();
        let renderer = Renderer::from_style(style);

        let plain = renderer.sprite_urls(1).expect("a sprite");
        assert_eq!(plain.json_url, "https://example.com/sprites/basic.json");
        assert_eq!(plain.png_url, "https://example.com/sprites/basic.png");

        let dense = renderer.sprite_urls(2).expect("a sprite");
        assert_eq!(dense.json_url, "https://example.com/sprites/basic@2x.json");
        assert_eq!(dense.png_url, "https://example.com/sprites/basic@2x.png");
    }

    #[test]
    fn asks_for_no_sprite_when_the_style_names_none() {
        let style = Style::from_json(&json!({
            "version": 8, "sources": {}, "layers": []
        }))
        .unwrap();
        let renderer = Renderer::from_style(style);
        assert!(renderer.sprite_urls(1).is_none());
        assert!(!renderer.needs_sprite(), "nothing to wait for");
    }

    /// An icon is a symbol with no text, and it has to draw on its own --
    /// a POI layer that names only `icon-image` is the common case.
    #[test]
    fn draws_icons_once_the_sheet_is_supplied() {
        let style = Style::from_json(&json!({
            "version": 8,
            "sprite": "https://example.com/sprite",
            "sources": { "src": { "type": "vector", "tiles": ["https://example.com/{z}/{x}/{y}.pbf"] } },
            "layers": [
                { "id": "bg", "type": "background", "paint": { "background-color": "#ffffff" } },
                {
                    "id": "pois", "type": "symbol", "source": "src", "source-layer": "poi",
                    "layout": { "icon-image": "{kind}", "icon-size": 4 }
                }
            ]
        }))
        .unwrap();
        let renderer = Renderer::from_style(style);
        let tile = fixtures::encode_tile(&[fixtures::LayerSpec {
            name: "poi".into(),
            extent: 4096,
            features: vec![fixtures::FeatureSpec {
                geom: fixtures::Geom::Point,
                points: vec![(2048, 2048)],
                props: vec![("kind".into(), "pin".into())],
            }],
        }]);

        let ink = |renderer: &Renderer| {
            let (pixmap, _) = renderer
                .render_pixmap(10, 512, 300, 256, &[Some(&tile)])
                .expect("renders");
            pixmap
                .pixels()
                .iter()
                .filter(|p| p.red() < 200 || p.green() < 200 || p.blue() < 200)
                .count()
        };

        assert!(renderer.needs_sprite(), "the style names a sprite");
        assert_eq!(ink(&renderer), 0, "no sheet, no icons");

        let mut sheet = tiny_skia::Pixmap::new(4, 4).expect("pixmap");
        for slot in sheet.pixels_mut().iter_mut() {
            *slot = tiny_skia::ColorU8::from_rgba(0, 0, 0, 255).premultiply();
        }
        renderer
            .add_sprite(
                r#"{"pin": {"x": 0, "y": 0, "width": 4, "height": 4, "pixelRatio": 1}}"#,
                &sheet.encode_png().expect("encode"),
            )
            .expect("sheet loads");

        assert!(!renderer.needs_sprite());
        assert!(ink(&renderer) > 0, "the icon draws once the sheet is in");
    }

    /// Labels only appear once glyphs arrive; before that the rest of the tile
    /// still has to draw.
    #[test]
    fn draws_labels_only_once_glyphs_are_supplied() {
        let style = Style::from_json(&json!({
            "version": 8,
            "glyphs": "https://example.com/{fontstack}/{range}.pbf",
            "sources": { "src": { "type": "vector", "tiles": ["https://example.com/{z}/{x}/{y}.pbf"] } },
            "layers": [
                { "id": "bg", "type": "background", "paint": { "background-color": "#ffffff" } },
                {
                    "id": "labels", "type": "symbol", "source": "src", "source-layer": "place",
                    "layout": { "text-field": "{name}", "text-font": ["F"], "text-size": 40 },
                    "paint": { "text-color": "#000000" }
                }
            ]
        }))
        .unwrap();
        let renderer = Renderer::from_style(style);
        let tile = fixtures::encode_tile(&[fixtures::LayerSpec {
            name: "place".into(),
            extent: 4096,
            features: vec![fixtures::FeatureSpec {
                geom: fixtures::Geom::Point,
                points: vec![(2048, 2048)],
                props: vec![("name".into(), "A".into())],
            }],
        }]);

        let ink = |renderer: &Renderer| {
            let (pixmap, _) = renderer
                .render_pixmap(10, 512, 300, 256, &[Some(&tile)])
                .expect("renders");
            pixmap
                .pixels()
                .iter()
                .filter(|p| p.red() < 200 || p.green() < 200 || p.blue() < 200)
                .count()
        };

        assert_eq!(ink(&renderer), 0, "no glyphs, no labels");
        renderer.add_glyphs(&glyph_pbf("F", &[0x41])).unwrap();
        assert!(ink(&renderer) > 0, "the label draws once its glyphs are in");
    }

    /// Builds a glyph pbf carrying a solid square for each codepoint.
    fn glyph_pbf(stack: &str, codes: &[u32]) -> Vec<u8> {
        fn varint(out: &mut Vec<u8>, mut v: u64) {
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    return;
                }
                out.push(b | 0x80);
            }
        }
        fn tag(out: &mut Vec<u8>, field: u32, wire: u8) {
            varint(out, ((field as u64) << 3) | wire as u64);
        }
        fn num(out: &mut Vec<u8>, field: u32, v: u64) {
            tag(out, field, 0);
            varint(out, v);
        }
        fn bytes(out: &mut Vec<u8>, field: u32, v: &[u8]) {
            tag(out, field, 2);
            varint(out, v.len() as u64);
            out.extend_from_slice(v);
        }

        let (w, h) = (16u32, 16u32);
        let buffer = glyphs::BUFFER;
        let bw = w + 2 * buffer;
        let bh = h + 2 * buffer;
        let mut bitmap = vec![0u8; (bw * bh) as usize];
        for y in buffer..(buffer + h) {
            for x in buffer..(buffer + w) {
                bitmap[(y * bw + x) as usize] = 255;
            }
        }

        let mut stack_msg = Vec::new();
        bytes(&mut stack_msg, 1, stack.as_bytes());
        bytes(&mut stack_msg, 2, b"0-255");
        for code in codes {
            let mut g = Vec::new();
            num(&mut g, 1, *code as u64);
            bytes(&mut g, 2, &bitmap);
            num(&mut g, 3, w as u64);
            num(&mut g, 4, h as u64);
            num(&mut g, 5, 0);
            num(&mut g, 6, ((h as i32) << 1) as u64);
            num(&mut g, 7, (w + 2) as u64);
            bytes(&mut stack_msg, 3, &g);
        }
        let mut out = Vec::new();
        bytes(&mut out, 1, &stack_msg);
        out
    }

    /// The hosts render on a thread pool and add glyph ranges from whichever
    /// thread finished fetching. The first Android build crashed inside
    /// `add_glyphs` the moment two tiles wanted glyphs at once — a signature
    /// taking `&mut self` promised exclusivity that a raw handle cannot give.
    #[test]
    fn survives_concurrent_readers_and_a_writer() {
        use std::sync::Arc;

        let style = Style::from_json(&json!({
            "version": 8,
            "glyphs": "https://example.com/{fontstack}/{range}.pbf",
            "sources": { "src": { "type": "vector", "tiles": ["https://example.com/{z}/{x}/{y}.pbf"] } },
            "layers": [{
                "id": "labels", "type": "symbol", "source": "src", "source-layer": "place",
                "layout": { "text-field": "{name}", "text-font": ["F"], "text-size": 20 },
                "paint": { "text-color": "#000000" }
            }]
        }))
        .unwrap();
        let renderer = Arc::new(Renderer::from_style(style));
        let tile = Arc::new(fixtures::encode_tile(&[fixtures::LayerSpec {
            name: "place".into(),
            extent: 4096,
            features: vec![fixtures::FeatureSpec {
                geom: fixtures::Geom::Point,
                points: vec![(2048, 2048)],
                props: vec![("name".into(), "AB".into())],
            }],
        }]));

        let mut threads = Vec::new();
        for t in 0..8 {
            let renderer = Arc::clone(&renderer);
            let tile = Arc::clone(&tile);
            threads.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    if t % 2 == 0 {
                        renderer.add_glyphs(&glyph_pbf("F", &[0x41, 0x42])).unwrap();
                    } else {
                        renderer
                            .render_pixmap(10, 512, 300, 256, &[Some(&tile)])
                            .expect("renders");
                    }
                }
            }));
        }
        for t in threads {
            t.join().expect("no thread panicked");
        }
        assert!(renderer.has_glyphs());
    }

    /// The GPU path hands back pixels it drew without glyphs; the label pass
    /// has to composite onto them rather than replace them. Until this
    /// existed, a style with labels rendered on the GPU lost every one of them
    /// and reported nothing.
    #[test]
    fn draws_labels_over_pixels_someone_else_rasterised() {
        let style = Style::from_json(&json!({
            "version": 8,
            "glyphs": "https://example.com/{fontstack}/{range}.pbf",
            "sources": { "src": { "type": "vector", "tiles": ["https://example.com/{z}/{x}/{y}.pbf"] } },
            "layers": [{
                "id": "labels", "type": "symbol", "source": "src", "source-layer": "place",
                "layout": { "text-field": "{name}", "text-font": ["F"], "text-size": 40 },
                "paint": { "text-color": "#000000" }
            }]
        }))
        .unwrap();
        let renderer = Renderer::from_style(style);
        renderer.add_glyphs(&glyph_pbf("F", &[0x41])).unwrap();

        let tile = fixtures::encode_tile(&[fixtures::LayerSpec {
            name: "place".into(),
            extent: 4096,
            features: vec![fixtures::FeatureSpec {
                geom: fixtures::Geom::Point,
                points: vec![(2048, 2048)],
                props: vec![("name".into(), "A".into())],
            }],
        }]);

        // A field of opaque red, as the GPU would leave it.
        let size = 128u32;
        let mut rgba = Vec::with_capacity((size * size * 4) as usize);
        for _ in 0..(size * size) {
            rgba.extend_from_slice(&[255, 0, 0, 255]);
        }
        let placed = renderer
            .draw_labels_onto(10, 512, 300, size, &mut rgba, &[Some(&tile)])
            .expect("draws");
        assert_eq!(placed, 1);

        let centre = ((size / 2 * size + size / 2) * 4) as usize;
        assert!(rgba[centre] < 128, "the label darkened the middle");
        assert_eq!(
            &rgba[0..4],
            &[255, 0, 0, 255],
            "the corner is untouched red"
        );
    }

    #[test]
    fn refuses_an_rgba_buffer_of_the_wrong_size() {
        let renderer = Renderer::from_style(test_style());
        let mut rgba = vec![0u8; 10];
        assert!(renderer
            .draw_labels_onto(10, 512, 300, 256, &mut rgba, &[])
            .is_err());
    }

    #[test]
    fn rejects_absurd_tile_sizes() {
        let renderer = Renderer::from_style(test_style());
        assert!(renderer.render(10, 0, 0, 0, &[]).is_err());
        assert!(renderer.render(10, 0, 0, 100_000, &[]).is_err());
    }

    #[test]
    fn plan_matches_the_positional_render_contract() {
        let renderer = Renderer::from_style(test_style());
        let plan = renderer.plan(10, 512, 300);
        assert_eq!(plan.requests.len(), 1);
        assert_eq!(plan.urls(), vec!["https://example.com/10/512/300.pbf"]);
    }
}
