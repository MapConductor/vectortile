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
//! Draws `background`, `fill`, `line` and `circle` layers. `symbol` layers are
//! parsed but not drawn: label placement needs collision detection that is
//! consistent across tile boundaries, which a per-tile renderer cannot do
//! without a shared placement pass. [`Renderer::unsupported_layer_types`]
//! reports what a given style loses.

pub mod color;
pub mod expr;
pub mod mercator;
pub mod mvt;
pub mod raster;
pub mod style;
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
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Style(e) => write!(f, "{e}"),
            RenderError::BadTileSize(n) => write!(f, "invalid tile size: {n}"),
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
}

impl Renderer {
    pub fn new(style_json: &str) -> Result<Renderer, RenderError> {
        Ok(Renderer {
            style: Style::parse(style_json)?,
        })
    }

    pub fn from_style(style: Style) -> Renderer {
        Renderer { style }
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

        raster::render(&self.style, z as f64, tile_size, &decoded)
            .ok_or(RenderError::BadTileSize(tile_size))
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
        assert_eq!((r, g, b), (255, 255, 255), "service road must be filtered out");
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
        assert!(r > 200 && g < 50 && b < 50, "expected red, got ({r},{g},{b})");
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

    #[test]
    fn reports_unsupported_layers_rather_than_failing() {
        let renderer = Renderer::from_style(test_style());
        assert_eq!(renderer.unsupported_layer_types(), vec!["symbol".to_string()]);
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
