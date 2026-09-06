//! Renders one tile with the ring of neighbours the plan asks for, and writes
//! the PNG. The point is to see what a label at the edge actually looks like.
fn main() {
    let style_json = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let dir = std::env::args().nth(2).unwrap();
    let z: u8 = std::env::args().nth(3).unwrap().parse().unwrap();
    let x: u32 = std::env::args().nth(4).unwrap().parse().unwrap();
    let y: u32 = std::env::args().nth(5).unwrap().parse().unwrap();
    let glyph_dir = std::env::args().nth(6).unwrap();
    let out = std::env::args().nth(7).unwrap();

    let renderer = mvt_render::Renderer::new(&style_json).unwrap();
    for entry in std::fs::read_dir(&glyph_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("pbf") {
            renderer.add_glyphs(&std::fs::read(&path).unwrap()).ok();
        }
    }

    let plan = renderer.plan(z, x, y);
    let mut bytes: Vec<Option<Vec<u8>>> = Vec::new();
    for request in &plan.requests {
        let path = format!("{dir}/n_{}_{}.mvt", request.x, request.y);
        bytes.push(std::fs::read(&path).ok());
        println!(
            "{} {}/{} {}",
            if request.labels_only {
                "neighbour"
            } else {
                "tile     "
            },
            request.x,
            request.y,
            if std::path::Path::new(&path).exists() {
                "ok"
            } else {
                "MISSING"
            }
        );
    }
    let refs: Vec<Option<&[u8]>> = bytes.iter().map(|b| b.as_deref()).collect();
    let template = renderer.glyphs_url_template().unwrap_or("").to_string();
    for want in renderer.needed_glyphs(z, x, y, &refs) {
        println!("NEEDS {}", want.url(&template));
    }
    let report = renderer.label_report(z, x, y, 512, &refs);
    let straddling: Vec<_> = report
        .iter()
        .filter(|(_, b)| b[0] < 0.0 || b[1] < 0.0 || b[2] > 512.0 || b[3] > 512.0)
        .collect();
    println!(
        "LABELS total {} straddling an edge {}",
        report.len(),
        straddling.len()
    );
    for (content, bounds) in straddling.iter().take(8) {
        println!("  EDGE {content} {bounds:?}");
    }
    let started = std::time::Instant::now();
    for _ in 0..5 {
        renderer.render(z, x, y, 512, &refs).unwrap();
    }
    println!("render x5: {:?} each", started.elapsed() / 5);

    // The same tile without the ring, to price what the ring costs.
    let alone: Vec<Option<&[u8]>> = refs
        .iter()
        .enumerate()
        .map(|(i, b)| if i == 0 { *b } else { None })
        .collect();
    let started = std::time::Instant::now();
    for _ in 0..5 {
        renderer.render(z, x, y, 512, &alone).unwrap();
    }
    println!(
        "render x5 without the ring: {:?} each",
        started.elapsed() / 5
    );

    let png = renderer.render(z, x, y, 512, &refs).unwrap();
    std::fs::write(&out, png).unwrap();
    println!("wrote {out}");
}
