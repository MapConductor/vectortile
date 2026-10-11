//! What a layer *is*, across tile schemas that name things differently.
//!
//! A rule written against layer ids only works on the style it was written
//! for. Roles are what make `{"role": "road"}` mean the same thing on an
//! OpenMapTiles style and a Shortbread one -- but only as far as the table
//! below is right, which is why every role assignment carries the evidence
//! for it and the compiler reports what matched.
//!
//! Roles are strings, not an enum. A custom schema can name roles this file
//! has never heard of, and adding one here must not be a breaking change on
//! three platforms.

use crate::rules::Schema;

/// The roles the built-in profiles assign. Listed for documentation and for
/// the diagnostics; nothing rejects a role outside this set.
pub const KNOWN_ROLES: &[&str] = &[
    "background",
    "water",
    "waterway",
    "land",
    "landuse",
    "park",
    "building",
    "road",
    "road-casing",
    "rail",
    "transit",
    "boundary",
    "aeroway",
    "label",
    "poi",
];

/// `source-layer` name to role, for one tile schema.
struct Profile {
    name: &'static str,
    /// Names that identify the schema when they appear in a style.
    fingerprint: &'static [&'static str],
    layers: &'static [(&'static str, &'static str)],
}

/// <https://openmaptiles.org/schema/>. What OSM Bright and most MapTiler
/// styles are cut to.
const OPENMAPTILES: Profile = Profile {
    name: "openmaptiles",
    fingerprint: &["transportation", "water", "building"],
    layers: &[
        ("water", "water"),
        ("waterway", "waterway"),
        ("landcover", "land"),
        ("landuse", "landuse"),
        ("park", "park"),
        ("building", "building"),
        ("transportation", "road"),
        ("aeroway", "aeroway"),
        ("boundary", "boundary"),
        ("water_name", "label"),
        ("transportation_name", "label"),
        ("place", "label"),
        ("mountain_peak", "label"),
        ("aerodrome_label", "label"),
        ("housenumber", "label"),
        ("poi", "poi"),
    ],
};

/// <https://shortbread-tiles.org/schema/>. What VersaTiles Colorful -- the
/// style the sample apps load -- is cut to.
const SHORTBREAD: Profile = Profile {
    name: "shortbread",
    fingerprint: &["streets", "water_polygons", "land"],
    layers: &[
        ("ocean", "water"),
        ("water_polygons", "water"),
        ("water_lines", "waterway"),
        ("dam_lines", "waterway"),
        ("dam_polygons", "water"),
        ("pier_lines", "land"),
        ("pier_polygons", "land"),
        ("land", "land"),
        ("sites", "landuse"),
        ("buildings", "building"),
        ("streets", "road"),
        ("street_polygons", "road"),
        ("bridges", "road"),
        ("ferries", "road"),
        ("aerialways", "transit"),
        ("public_transport", "transit"),
        ("boundaries", "boundary"),
        ("boundary_labels", "label"),
        ("place_labels", "label"),
        ("street_labels", "label"),
        ("street_labels_points", "label"),
        ("water_lines_labels", "label"),
        ("water_polygons_labels", "label"),
        ("addresses", "label"),
        ("pois", "poi"),
    ],
};

/// Mapbox Streets v8. Fingerprinted but not exhaustive: it is here so a
/// Mapbox style is not silently treated as OpenMapTiles.
const MAPBOX_STREETS: Profile = Profile {
    name: "mapbox-streets",
    fingerprint: &["road", "water", "landuse", "place_label"],
    layers: &[
        ("water", "water"),
        ("waterway", "waterway"),
        ("landuse", "landuse"),
        ("landuse_overlay", "landuse"),
        ("building", "building"),
        ("road", "road"),
        ("aeroway", "aeroway"),
        ("admin", "boundary"),
        ("transit_stop_label", "transit"),
        ("place_label", "label"),
        ("natural_label", "label"),
        ("water_label", "label"),
        ("road_label", "label"),
        ("housenum_label", "label"),
        ("poi_label", "poi"),
    ],
};

const PROFILES: &[&Profile] = &[&OPENMAPTILES, &SHORTBREAD, &MAPBOX_STREETS];

/// How a layer got the role it has. Carried into the diagnostics, because
/// the id heuristics below are the part most likely to be wrong on a style
/// nobody has tried yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// The schema's table named this `source-layer`.
    Profile {
        schema: String,
        source_layer: String,
    },
    /// The layer's `type` alone was enough.
    Kind(&'static str),
    /// The layer's id looked like it.
    LayerId(&'static str),
    None,
}

impl Evidence {
    pub fn describe(&self) -> String {
        match self {
            Evidence::Profile {
                schema,
                source_layer,
            } => format!("{schema}:{source_layer}"),
            Evidence::Kind(reason) => format!("kind:{reason}"),
            Evidence::LayerId(pattern) => format!("id~{pattern}"),
            Evidence::None => "none".to_string(),
        }
    }
}

/// The table in use for one style, built once per compile.
pub struct Roles {
    schema_name: String,
    table: Vec<(String, String)>,
}

impl Roles {
    /// Picks a profile, or builds one from what the app supplied.
    ///
    /// `source_layers` is every `source-layer` the style mentions; detection
    /// scores profiles by how much of their fingerprint is present rather
    /// than demanding all of it, because a style that draws no buildings is
    /// still an OpenMapTiles style.
    pub fn resolve(schema: &Schema, source_layers: &[String]) -> Roles {
        match schema {
            Schema::Custom(roles) => Roles {
                schema_name: "custom".to_string(),
                table: roles
                    .iter()
                    .flat_map(|(role, layers)| {
                        layers.iter().map(move |l| (l.clone(), role.clone()))
                    })
                    .collect(),
            },
            Schema::Named(name) => match PROFILES.iter().find(|p| p.name == name) {
                Some(profile) => Roles::from_profile(profile),
                // An unknown name is not an error: the id and kind
                // heuristics still work, and the diagnostics say so.
                None => Roles {
                    schema_name: format!("{name} (unknown)"),
                    table: Vec::new(),
                },
            },
            Schema::Auto => {
                let best = PROFILES
                    .iter()
                    .map(|profile| {
                        let hits = profile
                            .fingerprint
                            .iter()
                            .filter(|name| source_layers.iter().any(|l| l == *name))
                            .count();
                        (hits, profile)
                    })
                    .max_by_key(|(hits, _)| *hits);
                match best {
                    Some((hits, profile)) if hits >= 2 => Roles::from_profile(profile),
                    _ => Roles {
                        schema_name: "unrecognised".to_string(),
                        table: Vec::new(),
                    },
                }
            }
        }
    }

    fn from_profile(profile: &Profile) -> Roles {
        Roles {
            schema_name: profile.name.to_string(),
            table: profile
                .layers
                .iter()
                .map(|(layer, role)| (layer.to_string(), role.to_string()))
                .collect(),
        }
    }

    pub fn schema_name(&self) -> &str {
        &self.schema_name
    }

    /// The roles one layer carries, most specific first, with the evidence.
    ///
    /// A layer has more than one: a road casing is both `road` and
    /// `road-casing`, so a rule about roads catches it and a rule about
    /// casings can still single it out.
    pub fn of(
        &self,
        layer_id: &str,
        type_name: &str,
        source_layer: Option<&str>,
    ) -> Vec<(String, Evidence)> {
        let mut out: Vec<(String, Evidence)> = Vec::new();
        fn add(out: &mut Vec<(String, Evidence)>, role: &str, evidence: Evidence) {
            if !out.iter().any(|(existing, _)| existing == role) {
                out.push((role.to_string(), evidence));
            }
        }

        if type_name == "background" {
            add(&mut out, "background", Evidence::Kind("background"));
        }

        if let Some(source_layer) = source_layer {
            if let Some((_, role)) = self.table.iter().find(|(name, _)| name == source_layer) {
                add(
                    &mut out,
                    role,
                    Evidence::Profile {
                        schema: self.schema_name.clone(),
                        source_layer: source_layer.to_string(),
                    },
                );
            }
        }

        // A symbol layer is a label whatever the schema calls its source.
        // This is the one kind-based rule that holds everywhere.
        if type_name == "symbol" {
            add(&mut out, "label", Evidence::Kind("symbol"));
        }

        let lower = layer_id.to_ascii_lowercase();
        // Casings are an id convention, not a schema one: OSM Bright draws
        // every road twice, a wide dark line under a narrow light one, and
        // "make roads white" applied to both gives a white slab. Nothing in
        // the tile distinguishes them -- only the id does.
        // The separator is the style author's taste, not a standard: OSM
        // Bright writes `highway-motorway-casing`, VersaTiles Colorful writes
        // `street-motorway:outline`. Checking only one spelling found none of
        // the 81 casings in Colorful, which is the kind of silence this
        // design exists to avoid.
        let looks_like_a_casing = ["casing", "outline", "case"].iter().any(|word| {
            ['-', '_', ':', '.']
                .iter()
                .any(|sep| lower.ends_with(&format!("{sep}{word}")))
        });
        // Only a road has a casing. A building outline is an outline.
        if looks_like_a_casing && out.iter().any(|(role, _)| role == "road") {
            add(&mut out, "road-casing", Evidence::LayerId("*-casing"));
        }
        if out.is_empty() {
            for (needle, role) in [
                ("water", "water"),
                ("building", "building"),
                ("road", "road"),
                ("highway", "road"),
                ("street", "road"),
                ("motorway", "road"),
                ("rail", "rail"),
                ("boundary", "boundary"),
                ("admin", "boundary"),
                ("landuse", "landuse"),
                ("park", "park"),
                ("label", "label"),
                ("poi", "poi"),
            ] {
                if lower.contains(needle) {
                    add(&mut out, role, Evidence::LayerId(needle));
                    break;
                }
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn recognises_the_two_schemas_the_samples_use() {
        let osm_bright = layers(&["water", "transportation", "building", "place"]);
        assert_eq!(
            Roles::resolve(&Schema::Auto, &osm_bright).schema_name(),
            "openmaptiles"
        );

        let versatiles = layers(&["land", "water_polygons", "streets", "place_labels"]);
        assert_eq!(
            Roles::resolve(&Schema::Auto, &versatiles).schema_name(),
            "shortbread"
        );
    }

    #[test]
    fn says_so_rather_than_guessing_at_a_schema_it_does_not_know() {
        let roles = Roles::resolve(&Schema::Auto, &layers(&["parcels", "zoning"]));
        assert_eq!(roles.schema_name(), "unrecognised");
        // The id heuristics still work, which is what keeps an unknown
        // schema usable rather than inert.
        let found = roles.of("building-fill", "fill", Some("parcels"));
        assert_eq!(found[0].0, "building");
    }

    #[test]
    fn a_road_casing_is_both_a_road_and_a_casing() {
        let roles = Roles::resolve(&Schema::Auto, &layers(&["transportation", "water"]));
        let casing = roles.of("highway-motorway-casing", "line", Some("transportation"));
        let found: Vec<&str> = casing.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(found, ["road", "road-casing"]);

        let fill = roles.of("highway-motorway", "line", Some("transportation"));
        let found: Vec<&str> = fill.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(found, ["road"], "the fill half is not a casing");
    }

    /// Only roads get casings. A building outline is not a road casing, and
    /// a rule about casings must not reach it.
    #[test]
    fn an_outline_that_is_not_a_road_is_not_a_road_casing() {
        let roles = Roles::resolve(&Schema::Auto, &layers(&["transportation", "building"]));
        let found = roles.of("building-outline", "line", Some("building"));
        assert!(!found.iter().any(|(role, _)| role == "road-casing"));
        assert_eq!(found[0].0, "building");
    }

    #[test]
    fn every_symbol_layer_is_a_label() {
        let roles = Roles::resolve(&Schema::Auto, &layers(&["transportation", "water"]));
        let found = roles.of("road-shields", "symbol", Some("transportation"));
        let names: Vec<&str> = found.iter().map(|(r, _)| r.as_str()).collect();
        assert!(names.contains(&"label"), "{names:?}");
        assert!(names.contains(&"road"), "{names:?}");
    }

    #[test]
    fn a_custom_schema_can_name_roles_nobody_here_has_heard_of() {
        let schema = Schema::Custom(vec![("parcel".into(), vec!["zoning".into()])]);
        let roles = Roles::resolve(&schema, &[]);
        let found = roles.of("zoning-fill", "fill", Some("zoning"));
        assert_eq!(found[0].0, "parcel");
    }

    #[test]
    fn evidence_says_where_a_role_came_from() {
        let roles = Roles::resolve(&Schema::Auto, &layers(&["transportation", "water"]));
        let found = roles.of("highway-motorway-casing", "line", Some("transportation"));
        assert_eq!(found[0].1.describe(), "openmaptiles:transportation");
        assert_eq!(found[1].1.describe(), "id~*-casing");
    }
}
