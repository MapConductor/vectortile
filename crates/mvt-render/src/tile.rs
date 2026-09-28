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
    /// The zoom the style was read at to make this plan.
    ///
    /// Not always the target tile's own level: a host that shows a tile across
    /// 256 dp shows it where a 512 dp host would show the level above, so the
    /// style is read one zoom shallower. Carried on the plan because the decode
    /// step has to ask the style the same question the plan asked it -- which
    /// layers are symbol layers at this zoom -- and asking at a different zoom
    /// drops a layer the label pass then looks for.
    pub zoom: f64,
    pub requests: Vec<SourceTileRequest>,
}

impl TilePlan {
    pub fn urls(&self) -> Vec<&str> {
        self.requests.iter().map(|r| r.url.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    /// The zoom this plan was made for. See [`TilePlan::zoom`].
    pub fn zoom_hint(&self) -> f64 {
        self.zoom
    }
}

/// Builds the fetch list for target tile `z/x/y`.
///
/// Sources whose `maxzoom` is below `z` fall back to the deepest available
/// ancestor tile, magnified — without this a style with `maxzoom: 14` would go
/// blank the moment the user zoomed past 14.
pub fn plan_tile(style: &Style, z: u8, x: u32, y: u32) -> TilePlan {
    plan_tile_at(style, z, x, y, z as f64, 0)
}

/// The source tiles needed to draw `z/x/y`, read at `style_zoom` and sourced
/// `source_zoom_offset` levels shallower.
///
/// The two extra arguments exist because the level a tile is *named* by is not
/// always the level it is *drawn for*. A host that shows a tile across 256 dp
/// sees it where a 512 dp host would show the level above, so the style is read
/// one zoom shallower -- and the data has to come from that same shallower
/// level, or the two hosts draw different maps: a vector source's content
/// changes by level, and at zoom 6.5 the 新潟市 label that z=6 carries is not
/// in z=7 at all.
///
/// Sourcing shallower is the magnification the renderer already does past a
/// source's `maxzoom`, so the offset rides the same path: a scale and an offset
/// into a shallower tile.
pub fn plan_tile_at(
    style: &Style,
    z: u8,
    x: u32,
    y: u32,
    style_zoom: f64,
    source_zoom_offset: u8,
) -> TilePlan {
    let zoom = style_zoom;
    let mut requests = Vec::new();
    let wants_labels = style.symbol_source_layers(zoom);

    for source in style.active_vector_sources(zoom) {
        if source.tiles.is_empty() {
            continue;
        }
        // Below the source's own minzoom there is simply no data.
        if z.saturating_sub(source_zoom_offset) < source.minzoom {
            continue;
        }

        let src_z = z.saturating_sub(source_zoom_offset).min(source.maxzoom);
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
        // The ring is the eight tiles around the *target*, mapped to whatever
        // source tile each of them reads from. When nothing is magnified that
        // is the eight neighbours, as before. When the tile is magnified --
        // past the source's `maxzoom`, or because the host shows tiles across
        // 256 dp -- several of the eight land inside the tile already fetched
        // and collapse into it, and only the ones whose ground falls outside it
        // are asked for: three more for a tile in a parent's corner, none for
        // one deep inside. Before this the magnified path asked for no ring at
        // all, which lost exactly the labels the edge case is about.
        if !wants_labels.contains_key(&source.id) {
            continue;
        }
        let target_span = 1u64 << z;
        let src_span = 1i64 << src_z;
        let mut already = vec![(src_x, src_y)];
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                // Wrapping east to west is right; there is no tile above the
                // north pole.
                let tx = (x as i64 + dx as i64).rem_euclid(target_span as i64) as u32;
                let ty = y as i64 + dy as i64;
                if ty < 0 || ty >= target_span as i64 {
                    continue;
                }
                let (nx, ny) = (tx >> dz, (ty as u32) >> dz);
                if already.contains(&(nx, ny)) {
                    continue;
                }
                already.push((nx, ny));
                // How far this source tile sits from the one the target reads
                // its geometry from, in source tiles. Taken the short way round
                // so a tile at the antimeridian is one step from its neighbour
                // rather than a world away.
                let mut steps_x = nx as i64 - src_x as i64;
                if steps_x * 2 > src_span {
                    steps_x -= src_span;
                } else if steps_x * 2 < -src_span {
                    steps_x += src_span;
                }
                let steps_y = ny as i64 - src_y as i64;
                let template = pick_template(&source.tiles, nx, ny);
                requests.push(SourceTileRequest {
                    source_id: source.id.clone(),
                    url: expand_template(template, src_z, nx, ny),
                    z: src_z,
                    x: nx,
                    y: ny,
                    scale,
                    // Shifted by whole source tiles, so the neighbour's
                    // features land where they belong relative to this one.
                    offset_x: offset_x - steps_x as f32,
                    offset_y: offset_y - steps_y as f32,
                    labels_only: true,
                });
            }
        }
    }

    TilePlan { zoom, requests }
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

    /// As [`style_with`], plus a symbol layer -- which is what makes a plan ask
    /// for the ring of neighbours at all.
    fn labelled_style(maxzoom: u32, minzoom: u32) -> Style {
        Style::from_json(&json!({
            "version": 8,
            "sources": {
                "src": {
                    "type": "vector",
                    "tiles": ["https://t/{z}/{x}/{y}.pbf"],
                    "minzoom": minzoom,
                    "maxzoom": maxzoom
                }
            },
            "layers": [
                { "id": "l", "type": "fill", "source": "src", "source-layer": "water" },
                {
                    "id": "n", "type": "symbol", "source": "src",
                    "source-layer": "place_labels",
                    "layout": { "text-field": "{name}", "text-font": ["f"] }
                }
            ]
        }))
        .unwrap()
    }

    /// The eight neighbours, when nothing is magnified.
    #[test]
    fn asks_for_the_whole_ring_at_the_tile_s_own_level() {
        let plan = plan_tile(&labelled_style(14, 0), 10, 512, 300);
        assert_eq!(plan.requests.len(), 9);
        let west = plan
            .requests
            .iter()
            .find(|r| (r.x, r.y) == (511, 300))
            .expect("the tile to the west");
        assert!(west.labels_only);
        assert_eq!((west.offset_x, west.offset_y), (1.0, 0.0));
    }

    /// A tile read from its parent asks only for the parents its own ring
    /// reaches into -- the rest of the ring is inside the parent it already
    /// fetched.
    #[test]
    fn a_magnified_tile_asks_only_for_the_parents_outside_it() {
        // 7/113/50 is the north-east quarter of 6/56/25, so its ring crosses
        // into the parent to the east, the one to the north, and the corner.
        let plan = plan_tile_at(&labelled_style(14, 0), 7, 113, 50, 6.0, 1);
        assert_eq!(plan.zoom, 6.0);
        let listed: Vec<_> = plan
            .requests
            .iter()
            .map(|r| (r.z, r.x, r.y, r.labels_only))
            .collect();
        assert_eq!(
            listed,
            vec![
                (6, 56, 25, false),
                (6, 56, 24, true),
                (6, 57, 24, true),
                (6, 57, 25, true),
            ]
        );
        let corner = &plan.requests[2];
        assert_eq!(corner.scale, 2.0);
        // The target sits half a tile west of that parent's own origin and a
        // whole tile south of it.
        assert_eq!((corner.offset_x, corner.offset_y), (-0.5, 1.0));
    }

    /// Deep inside an ancestor there is no ring to ask for: every neighbour's
    /// ground is in the tile already fetched.
    #[test]
    fn a_tile_inside_its_ancestor_needs_no_ring() {
        // Two levels past maxzoom, and (5, 5) of the 4x4 grid is interior.
        let plan = plan_tile(&labelled_style(14, 0), 16, 5, 5);
        assert_eq!(plan.requests.len(), 1);
        assert_eq!((plan.requests[0].x, plan.requests[0].y), (1, 1));
    }

    /// The ring wraps the short way round at the antimeridian rather than
    /// placing the neighbour a world away.
    #[test]
    fn the_ring_wraps_east_to_west() {
        let plan = plan_tile(&labelled_style(14, 0), 2, 0, 1);
        let wrapped = plan
            .requests
            .iter()
            .find(|r| (r.x, r.y) == (3, 1))
            .expect("the tile across the antimeridian");
        assert_eq!((wrapped.offset_x, wrapped.offset_y), (1.0, 0.0));
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
