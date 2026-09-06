//! Times the label pass against a real tile, with and without a floor on how
//! small a label may be.
use std::time::Instant;

fn main() {
    let style_path = std::env::args().nth(1).expect("style.json");
    let tile_path = std::env::args().nth(2).expect("tile.mvt");
    let zoom: u8 = std::env::args().nth(3).expect("zoom").parse().unwrap();
    let style_json = std::fs::read_to_string(style_path).unwrap();
    let tile = std::fs::read(tile_path).unwrap();

    let renderer = mvt_render::Renderer::new(&style_json).unwrap();
    let plan = renderer.plan(zoom, 0, 0);
    println!(
        "plan: {} requests ({} of them neighbours, for labels)",
        plan.requests.len(),
        plan.requests.iter().filter(|r| r.labels_only).count()
    );
    // The same bytes stand in for every neighbour, which is wrong
    // geographically and exactly right for measuring the cost.
    let data: Vec<Option<&[u8]>> = plan.requests.iter().map(|_| Some(&tile[..])).collect();

    let needed = renderer.needed_glyphs(zoom, 0, 0, &data);
    println!("glyph ranges wanted: {}", needed.len());
    // 文 is U+6587, in the 25856-26111 range: the character missing from
    // 文京区 on screen.
    let wants_that_range = needed.iter().any(|r| r.range_start == 25856);
    println!("asks for the range holding 文: {wants_that_range}");

    // Ranges come from a directory the caller filled, so this stays offline.
    if let Some(dir) = std::env::args().nth(4) {
        let mut loaded = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("pbf") {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            loaded += renderer.add_glyphs(&bytes).unwrap_or(0);
        }
        println!("glyphs loaded: {loaded}");
    }

    // What the label pass actually produces, so a claim about small text is
    // measured rather than inferred from the style.
    let tile_size: u32 = std::env::args()
        .nth(5)
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);
    let sizes = renderer.label_sizes(zoom, tile_size, &data);
    println!("tile size: {tile_size}");
    let mut buckets = std::collections::BTreeMap::new();
    for size in &sizes {
        *buckets.entry(size.round() as i32).or_insert(0) += 1;
    }
    println!("labels placed: {}", sizes.len());
    println!("sizes: {buckets:?}");

    let started = Instant::now();
    for _ in 0..5 {
        renderer.render_pixmap(zoom, 0, 0, 512, &data).unwrap();
    }
    println!("render x5: {:?}", started.elapsed() / 5);
}
