//! Works out which source tiles a target tile needs, including overzoom.

use crate::style::Style;

/// One source tile the host must fetch before [`crate::render_tile`] can run.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceTileRequest {
    pub source_id: String,
    pub url: String,
    /// The tile actually fetched. Equal to the target tile unless overzoomed.
    pub z: u8,
    pub x: u32,
    pub y: u32,
    /// How much the fetched tile is magnified: `2^(target_z - z)`.
    pub scale: f32,
    /// Where the target tile sits inside the fetched tile, in 0..1 units.
    pub offset_x: f32,
    pub offset_y: f32,
    /// A neighbouring tile, fetched only so that a label straddling the edge
    /// can be drawn whole. Its geometry is not drawn -- this tile's own copy
    /// already covers the ground.
    pub labels_only: bool,
}

impl SourceTileRequest {
    pub fn is_overzoomed(&self) -> bool {
        self.scale > 1.0
    }
}

/// The full fetch list for one target tile, in a fixed order. The host fetches
/// these and hands the bytes back positionally — that keeps all network I/O on
/// the host side without needing a callback across the FFI boundary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TilePlan {
    pub requests: Vec<SourceTileRequest>,
}

impl TilePlan {
    pub fn urls(&self) -> Vec<&str> {
        self.requests.iter().map(|r| r.url.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    /// The zoom this plan was made for, taken from its own requests.
    ///
    /// A magnified tile records the zoom it was fetched at, not the one it is
    /// drawn at, so the target zoom is that plus the magnification.
    pub fn zoom_hint(&self) -> f64 {
        self.requests
            .first()
            .map(|r| r.z as f64 + (r.scale.max(1.0)).log2() as f64)
            .unwrap_or(0.0)
    }
}

/// Builds the fetch list for target tile `z/x/y`.
///
/// Sources whose `maxzoom` is below `z` fall back to the deepest available
/// ancestor tile, magnified — without this a style with `maxzoom: 14` would go
/// blank the moment the user zoomed past 14.
pub fn plan_tile(style: &Style, z: u8, x: u32, y: u32) -> TilePlan {
    let zoom = z as f64;
    let mut requests = Vec::new();
    let wants_labels = style.symbol_source_layers(zoom);

    for source in style.active_vector_sources(zoom) {
        if source.tiles.is_empty() {
            continue;
        }
        // Above the source's own minzoom there is simply no data.
        if z < source.minzoom {
            continue;
        }

        let src_z = z.min(source.maxzoom);
        let dz = z - src_z;
        let scale = (1u32 << dz) as f32;
        let src_x = x >> dz;
        let src_y = y >> dz;
        let offset_x = (x - (src_x << dz)) as f32 / scale;
        let offset_y = (y - (src_y << dz)) as f32 / scale;

        let template = pick_template(&source.tiles, src_x, src_y);
        requests.push(SourceTileRequest {
            source_id: source.id.clone(),
            url: expand_template(template, src_z, src_x, src_y),
            z: src_z,
            x: src_x,
            y: src_y,
            scale,
            offset_x,
            offset_y,
            labels_only: false,
        });

        // A label belongs to whichever tile holds the point it names, and the
        // tile generator clips those points to the tile: measured on a real
        // shortbread tile, the buffer around the edge carries lines and
        // polygons but not one single point. So a name anchored near an edge
        // is drawn by its own tile, cut off at the edge, and the neighbour
        // never knew about it. Fetching the ring of neighbours is what lets
        // the other half be drawn.
        //
        // Not needed when the tile is magnified from a shallower one: the
        // neighbours are then inside the same fetched tile already.
        if scale > 1.0 || !wants_labels.contains_key(&source.id) {
            continue;
        }
        let span = 1u32 << src_z;
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                // Wrapping east to west is right; there is no tile above the
                // north pole.
                let nx = (src_x as i64 + dx as i64).rem_euclid(span as i64) as u32;
                let ny = src_y as i64 + dy as i64;
                if ny < 0 || ny >= span as i64 {
                    continue;
                }
                let ny = ny as u32;
                let template = pick_template(&source.tiles, nx, ny);
                requests.push(SourceTileRequest {
                    source_id: source.id.clone(),
                    url: expand_template(template, src_z, nx, ny),
                    z: src_z,
                    x: nx,
                    y: ny,
                    scale,
                    // Shifted by a whole tile, so the neighbour's features
                    // land where they belong relative to this one.
                    offset_x: offset_x - dx as f32 / scale,
                    offset_y: offset_y - dy as f32 / scale,
                    labels_only: true,
                });
            }
        }
    }

    TilePlan { requests }
}

/// Spreads requests across a source's mirror list the way tile clients do.
fn pick_template(tiles: &[String], x: u32, y: u32) -> &str {
    let index = (x.wrapping_add(y) as usize) % tiles.len();
    &tiles[index]
}

fn expand_template(template: &str, z: u8, x: u32, y: u32) -> String {
    template
        .replace("{z}", &z.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn style_with(maxzoom: u32, minzoom: u32, tiles: Vec<&str>) -> Style {
        Style::from_json(&json!({
            "version": 8,
            "sources": {
                "src": {
                    "type": "vector",
                    "tiles": tiles,
                    "minzoom": minzoom,
                    "maxzoom": maxzoom
                }
            },
            "layers": [{
                "id": "l", "type": "fill", "source": "src", "source-layer": "water"
            }]
        }))
        .unwrap()
    }

    #[test]
    fn plans_a_direct_fetch_below_maxzoom() {
        let style = style_with(14, 0, vec!["https://t/{z}/{x}/{y}.pbf"]);
        let plan = plan_tile(&style, 10, 512, 300);
        assert_eq!(plan.requests.len(), 1);
        let r = &plan.requests[0];
        assert_eq!(r.url, "https://t/10/512/300.pbf");
        assert_eq!((r.z, r.x, r.y), (10, 512, 300));
        assert_eq!(r.scale, 1.0);
        assert!(!r.is_overzoomed());
    }

    #[test]
    fn overzooms_to_the_parent_past_maxzoom() {
        let style = style_with(14, 0, vec!["https://t/{z}/{x}/{y}.pbf"]);
        // z=16 is two levels past maxzoom, so fetch the z=14 ancestor.
        let plan = plan_tile(&style, 16, 4, 7);
        let r = &plan.requests[0];
        assert_eq!((r.z, r.x, r.y), (14, 1, 1));
        assert_eq!(r.scale, 4.0);
        assert!(r.is_overzoomed());
        // Tile (4,7) is the (0, 3) cell of the 4x4 grid inside the ancestor.
        assert_eq!(r.offset_x, 0.0);
        assert_eq!(r.offset_y, 0.75);
    }

    #[test]
    fn skips_sources_below_their_minzoom() {
        let style = style_with(14, 6, vec!["https://t/{z}/{x}/{y}.pbf"]);
        assert!(plan_tile(&style, 3, 1, 1).is_empty());
        assert!(!plan_tile(&style, 6, 1, 1).is_empty());
    }

    #[test]
    fn skips_sources_with_no_tile_urls() {
        let style = style_with(14, 0, vec![]);
        assert!(plan_tile(&style, 5, 1, 1).is_empty());
    }

    #[test]
    fn distributes_across_mirror_templates() {
        let style = style_with(
            14,
            0,
            vec!["https://a/{z}/{x}/{y}.pbf", "https://b/{z}/{x}/{y}.pbf"],
        );
        let a = plan_tile(&style, 5, 0, 0).requests[0].url.clone();
        let b = plan_tile(&style, 5, 0, 1).requests[0].url.clone();
        assert!(a.starts_with("https://a/"));
        assert!(b.starts_with("https://b/"));
    }
}
