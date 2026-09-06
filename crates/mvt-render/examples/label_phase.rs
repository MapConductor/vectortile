//! Times the label pass against a rendered tile, the way the GPU path runs it.
fn main() {
    let style = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let dir = std::env::args().nth(2).unwrap();
    let z: u8 = std::env::args().nth(3).unwrap().parse().unwrap();
    let x: u32 = std::env::args().nth(4).unwrap().parse().unwrap();
    let y: u32 = std::env::args().nth(5).unwrap().parse().unwrap();
    let glyph_dir = std::env::args().nth(6).unwrap();

    let renderer = mvt_render::Renderer::new(&style).unwrap();
    for entry in std::fs::read_dir(&glyph_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("pbf") {
            renderer.add_glyphs(&std::fs::read(&path).unwrap()).ok();
        }
    }
    // The sprite sheet, if a cached pair sits beside the glyphs.
    let json = std::fs::read_to_string(format!("{glyph_dir}/sprites.json"));
    let png = std::fs::read(format!("{glyph_dir}/sprites.png"));
    if let (Ok(json), Ok(png)) = (json, png) {
        renderer.add_sprite(&json, &png).ok();
    }

    let plan = renderer.plan(z, x, y);
    let bytes: Vec<Option<Vec<u8>>> = plan
        .requests
        .iter()
        .map(|r| std::fs::read(format!("{dir}/n_{}_{}.mvt", r.x, r.y)).ok())
        .collect();
    let refs: Vec<Option<&[u8]>> = bytes.iter().map(|b| b.as_deref()).collect();

    let mut rgba = vec![0u8; 512 * 512 * 4];
    for _ in 0..5 {
        renderer
            .draw_labels_onto(z, x, y, 512, &mut rgba, &refs)
            .unwrap();
    }
}
