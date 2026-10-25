//! Shortbread supplies the same road twice: buffered `streets` geometry and
//! unbuffered `street_labels` geometry. Independently quantized buffer endpoints
//! do not interpolate to the original edge vertex, visibly moving roads at high
//! overzoom. Restore that vertex from the unbuffered copy, matching an exact
//! interior vertex rather than guessing a grid or joining unrelated nearby roads.
use crate::model::{Feature, GeomType, Tile};
use std::collections::HashMap;

type References = HashMap<[i32; 2], Vec<[f32; 2]>>;

pub(crate) fn align_shortbread(tile: &mut Tile) {
    let Some(labels) = tile.layer("street_labels") else {
        return;
    };
    let extent = labels.extent as f32;
    let mut references = References::new();
    for feature in &labels.features {
        if feature.geom_type != GeomType::LineString {
            continue;
        }
        for part in &feature.parts {
            for pair in part.windows(2) {
                for (edge, interior) in [(pair[0], pair[1]), (pair[1], pair[0])] {
                    if on_edge(edge, extent) && !on_edge(interior, extent) {
                        references.entry(key(interior)).or_default().push(edge);
                    }
                }
            }
        }
    }
    let Some(streets) = tile
        .layers
        .iter_mut()
        .find(|l| l.name == "streets" && l.extent as f32 == extent)
    else {
        return;
    };
    for feature in &mut streets.features {
        if feature.geom_type != GeomType::LineString {
            continue;
        }
        let mut changed = false;
        for part in &mut feature.parts {
            let mut replacement: Option<Vec<[f32; 2]>> = None;
            for (index, pair) in part.windows(2).enumerate() {
                let mut crossings = Vec::new();
                for axis in 0..2 {
                    for border in [0.0, extent] {
                        let a = pair[0];
                        let b = pair[1];
                        if !((a[axis] < border && b[axis] > border)
                            || (b[axis] < border && a[axis] > border))
                        {
                            continue;
                        }
                        let t = (border - a[axis]) / (b[axis] - a[axis]);
                        let other = 1 - axis;
                        let crossing = a[other] + t * (b[other] - a[other]);
                        // An exact interior endpoint identifies the same source
                        // segment. A 1.5-unit bound tolerates endpoint rounding
                        // without borrowing a different segment's edge vertex.
                        let mut candidates = pair
                            .iter()
                            .filter_map(|p| references.get(&key(*p)))
                            .flatten()
                            .filter(|p| p[axis] == border && (p[other] - crossing).abs() <= 1.5);
                        if let Some(&point) = candidates.next() {
                            if candidates.all(|p| *p == point) {
                                crossings.push((t, point));
                            }
                        }
                    }
                }
                if !crossings.is_empty() {
                    let output = replacement.get_or_insert_with(|| part[..=index].to_vec());
                    crossings.sort_by(|a, b| a.0.total_cmp(&b.0));
                    for (_, point) in crossings {
                        if output.last() != Some(&point) {
                            output.push(point);
                        }
                    }
                }
                if let Some(output) = &mut replacement {
                    output.push(pair[1]);
                }
            }
            if let Some(output) = replacement {
                *part = output;
                changed = true;
            }
        }
        if changed {
            let tags = feature.prop_pairs().flat_map(|(k, v)| [k, v]).collect();
            *feature = Feature::new(
                feature.id,
                feature.geom_type,
                std::mem::take(&mut feature.parts),
                tags,
            );
        }
    }
}
fn on_edge(p: [f32; 2], extent: f32) -> bool {
    p[0] == 0.0 || p[0] == extent || p[1] == 0.0 || p[1] == extent
}
fn key(p: [f32; 2]) -> [i32; 2] {
    [p[0] as i32, p[1] as i32]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Layer;
    fn fixture(road: Vec<[f32; 2]>, label: Vec<[f32; 2]>) -> Tile {
        let layer = |name: &str, part| {
            Layer::from_tables(
                name.into(),
                4096,
                vec![],
                vec![],
                vec![Feature::new(None, GeomType::LineString, vec![part], vec![])],
            )
        };
        Tile {
            layers: vec![layer("streets", road), layer("street_labels", label)],
        }
    }
    #[test]
    fn restores_actual_shortbread_edge_vertices_including_rounding_disagreement() {
        for (road, label, edge) in [
            (
                vec![[4160., 1638.], [3792., 1656.]],
                vec![[4096., 1641.], [3792., 1656.]],
                [4096., 1641.],
            ),
            (
                vec![[3772., 1836.], [4160., 1828.]],
                vec![[3772., 1836.], [4096., 1830.]],
                [4096., 1830.],
            ),
            (
                vec![[-64., 1921.], [195., 1915.]],
                vec![[0., 1919.], [195., 1915.]],
                [0., 1919.],
            ),
        ] {
            let mut tile = fixture(road.clone(), label);
            align_shortbread(&mut tile);
            assert_eq!(
                tile.layers[0].features[0].parts[0],
                vec![road[0], edge, road[1]]
            );
        }
    }
    #[test]
    fn does_not_join_a_nearby_unrelated_road() {
        let road = vec![[3772., 1836.], [4160., 1828.]];
        let mut tile = fixture(road.clone(), vec![[3771., 1836.], [4096., 1830.]]);
        align_shortbread(&mut tile);
        assert_eq!(tile.layers[0].features[0].parts[0], road);
    }
    #[test]
    fn generic_tiles_and_interior_vertices_are_unchanged() {
        let road = vec![[50., 50.], [100., 100.]];
        let mut tile = fixture(road.clone(), vec![[0., 100.], [100., 100.]]);
        align_shortbread(&mut tile);
        assert_eq!(tile.layers[0].features[0].parts[0], road);
        tile.layers.pop();
        align_shortbread(&mut tile);
        assert_eq!(tile.layers[0].features[0].parts[0], road);
    }
}
