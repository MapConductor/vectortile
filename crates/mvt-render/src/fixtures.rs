//! Hand-built MVT tiles for tests. Keeps the test suite free of binary blobs
//! and network access.

fn varint(value: u64, out: &mut Vec<u8>) {
    let mut v = value;
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn tag(field: u32, wire: u8, out: &mut Vec<u8>) {
    varint(((field as u64) << 3) | wire as u64, out);
}

fn delimited(field: u32, payload: &[u8], out: &mut Vec<u8>) {
    tag(field, 2, out);
    varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

fn zigzag(n: i32) -> u64 {
    ((n << 1) ^ (n >> 31)) as u32 as u64
}

#[derive(Clone, Copy)]
pub enum Geom {
    Point,
    Line,
    Polygon,
}

impl Geom {
    fn code(self) -> u64 {
        match self {
            Geom::Point => 1,
            Geom::Line => 2,
            Geom::Polygon => 3,
        }
    }
}

pub struct FeatureSpec {
    pub geom: Geom,
    /// Absolute tile-local coordinates; deltas are computed here.
    pub points: Vec<(i32, i32)>,
    /// Property values, indexed against the layer's key list.
    pub props: Vec<(String, String)>,
}

pub struct LayerSpec {
    pub name: String,
    pub extent: u32,
    pub features: Vec<FeatureSpec>,
}

fn encode_geometry(spec: &FeatureSpec) -> Vec<u8> {
    let mut geom = Vec::new();
    let (mut cx, mut cy) = (0i32, 0i32);

    match spec.geom {
        Geom::Point => {
            varint((spec.points.len() as u64) << 3 | 1, &mut geom); // MoveTo xN
            for (x, y) in &spec.points {
                varint(zigzag(x - cx), &mut geom);
                varint(zigzag(y - cy), &mut geom);
                cx = *x;
                cy = *y;
            }
        }
        Geom::Line | Geom::Polygon => {
            let first = spec.points[0];
            varint(1 << 3 | 1, &mut geom); // MoveTo x1
            varint(zigzag(first.0 - cx), &mut geom);
            varint(zigzag(first.1 - cy), &mut geom);
            cx = first.0;
            cy = first.1;

            let rest = &spec.points[1..];
            varint((rest.len() as u64) << 3 | 2, &mut geom); // LineTo xN
            for (x, y) in rest {
                varint(zigzag(x - cx), &mut geom);
                varint(zigzag(y - cy), &mut geom);
                cx = *x;
                cy = *y;
            }

            if matches!(spec.geom, Geom::Polygon) {
                varint(1 << 3 | 7, &mut geom); // ClosePath
            }
        }
    }
    geom
}

fn encode_layer(spec: &LayerSpec) -> Vec<u8> {
    // Build the layer-wide key/value tables first.
    let mut keys: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::new();
    for feature in &spec.features {
        for (k, v) in &feature.props {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
            if !values.contains(v) {
                values.push(v.clone());
            }
        }
    }

    let mut layer = Vec::new();
    tag(15, 0, &mut layer);
    varint(2, &mut layer); // version
    delimited(1, spec.name.as_bytes(), &mut layer);

    for feature in &spec.features {
        let mut buf = Vec::new();
        let mut tags_buf = Vec::new();
        for (k, v) in &feature.props {
            varint(keys.iter().position(|x| x == k).unwrap() as u64, &mut tags_buf);
            varint(values.iter().position(|x| x == v).unwrap() as u64, &mut tags_buf);
        }
        if !tags_buf.is_empty() {
            delimited(2, &tags_buf, &mut buf);
        }
        tag(3, 0, &mut buf);
        varint(feature.geom.code(), &mut buf);
        delimited(4, &encode_geometry(feature), &mut buf);
        delimited(2, &buf, &mut layer);
    }

    for key in &keys {
        delimited(3, key.as_bytes(), &mut layer);
    }
    for value in &values {
        let mut v = Vec::new();
        delimited(1, value.as_bytes(), &mut v);
        delimited(4, &v, &mut layer);
    }

    tag(5, 0, &mut layer);
    varint(spec.extent as u64, &mut layer);
    layer
}

/// Encodes a complete `.pbf` tile.
pub fn encode_tile(layers: &[LayerSpec]) -> Vec<u8> {
    let mut tile = Vec::new();
    for layer in layers {
        delimited(3, &encode_layer(layer), &mut tile);
    }
    tile
}

/// A tile with a water polygon covering the left half, a road line down the
/// middle, and one POI point — enough to exercise fill, line and circle.
pub fn sample_tile() -> Vec<u8> {
    let extent = 4096;
    encode_tile(&[
        LayerSpec {
            name: "water".into(),
            extent,
            features: vec![FeatureSpec {
                geom: Geom::Polygon,
                points: vec![(0, 0), (2048, 0), (2048, 4096), (0, 4096)],
                props: vec![("class".into(), "ocean".into())],
            }],
        },
        LayerSpec {
            name: "roads".into(),
            extent,
            features: vec![
                FeatureSpec {
                    geom: Geom::Line,
                    points: vec![(2048, 0), (2048, 4096)],
                    props: vec![("class".into(), "motorway".into())],
                },
                FeatureSpec {
                    geom: Geom::Line,
                    points: vec![(0, 3000), (4096, 3000)],
                    props: vec![("class".into(), "service".into())],
                },
            ],
        },
        LayerSpec {
            name: "poi".into(),
            extent,
            features: vec![FeatureSpec {
                geom: Geom::Point,
                points: vec![(3072, 1024)],
                props: vec![("class".into(), "cafe".into())],
            }],
        },
    ])
}
