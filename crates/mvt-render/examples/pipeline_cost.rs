//! Offline native benchmark, also executable on Android via adb.
//! pipeline_cost style.json tile.mvt z x y [glyph-directory] [samples]
//! Only the central source tile is supplied: this isolates native costs and
//! does not stand in for a viewport benchmark with nine distinct neighbours.
use std::{fs, hint::black_box, time::Instant};

fn hash(bytes: impl Iterator<Item = u8>) -> u64 {
    bytes.fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

fn measure(mut body: impl FnMut(), samples: usize) -> f64 {
    for _ in 0..3 {
        body();
    }
    let mut times = Vec::new();
    for _ in 0..samples {
        let start = Instant::now();
        body();
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    assert!(
        args.len() >= 5,
        "style.json tile.mvt z x y [glyph-directory] [samples]"
    );
    let renderer = mvt_render::Renderer::new(&fs::read_to_string(&args[0]).unwrap()).unwrap();
    let bytes = fs::read(&args[1]).unwrap();
    let (z, x, y) = (
        args[2].parse().unwrap(),
        args[3].parse().unwrap(),
        args[4].parse().unwrap(),
    );
    let samples = args.get(6).map_or(21, |v| v.parse::<usize>().unwrap());
    assert!(samples > 0);
    let plan = renderer.plan(z, x, y);
    assert_eq!(plan.requests.iter().filter(|r| !r.labels_only).count(), 1);
    let data: Vec<_> = plan
        .requests
        .iter()
        .map(|r| (!r.labels_only).then_some(bytes.as_slice()))
        .collect();
    let mut glyphs = 0;
    if let Some(dir) = args.get(5).filter(|dir| dir.as_str() != "-") {
        let mut paths: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        paths.sort();
        for path in paths {
            if path.is_file() {
                glyphs += renderer.add_glyphs(&fs::read(path).unwrap()).unwrap_or(0);
            }
        }
    }
    let mut packed = Vec::new();
    let geometry_ms = measure(
        || {
            packed = black_box(renderer.tessellate_packed(z, x, y, 512, &data).unwrap());
        },
        samples,
    );
    // Exclude the six timing fields from the output fingerprint.
    let geometry_hash = hash(
        packed[..7]
            .iter()
            .chain(&packed[13..])
            .flat_map(|f| f.to_bits().to_le_bytes()),
    );
    println!("geometry median={geometry_ms:.3}ms decode={:.3}ms tessellate={:.3}ms floats={} hash={geometry_hash:016x} n={samples}", packed[7], packed[8], packed.len());
    if glyphs > 0 {
        let missing_ms = measure(
            || {
                black_box(renderer.needed_glyphs(z, x, y, &data));
            },
            samples,
        );
        let mut rgba = vec![0; 1024 * 1024 * 4];
        let mut placed = 0;
        let labels_ms = measure(
            || {
                rgba.fill(0);
                placed = black_box(
                    renderer
                        .draw_labels_onto(z, x, y, 1024, &mut rgba, &data)
                        .unwrap(),
                );
            },
            samples,
        );
        assert!(placed > 0, "label benchmark must draw labels");
        let label_hash = hash(rgba.iter().copied());
        println!("labels median={labels_ms:.3}ms needed={missing_ms:.3}ms placed={placed} glyphs={glyphs} hash={label_hash:016x} n={samples}");
    }
}
