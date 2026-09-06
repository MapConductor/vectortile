//! Draws only the label overlay for a tile -- the device's label layer -- and
//! reports the ink found in each column, so a missing glyph is visible as a
//! gap rather than argued about from a screenshot.
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
            renderer.add_glyphs(&std::fs::read(&path).unwrap()).unwrap();
        }
    }
    let plan = renderer.plan(z, x, y);
    let bytes: Vec<Option<Vec<u8>>> = plan
        .requests
        .iter()
        .map(|r| std::fs::read(format!("{dir}/n_{}_{}.mvt", r.x, r.y)).ok())
        .collect();
    let refs: Vec<Option<&[u8]>> = bytes.iter().map(|b| b.as_deref()).collect();

    let mut rgba = vec![0u8; 512 * 512 * 4];
    renderer
        .draw_labels_onto(z, x, y, 512, &mut rgba, &refs)
        .unwrap();

    let from: u32 = std::env::args().nth(7).unwrap().parse().unwrap();
    let to: u32 = std::env::args().nth(8).unwrap().parse().unwrap();
    let top: u32 = std::env::args().nth(9).unwrap().parse().unwrap();
    let bottom: u32 = std::env::args().nth(10).unwrap().parse().unwrap();
    for col in from..to {
        let ink = (top..bottom)
            .filter(|row| {
                let i = ((row * 512 + col) * 4) as usize;
                // Dark ink, not halo: the fill is near-black, the halo white.
                rgba[i + 3] > 60 && rgba[i] < 120
            })
            .count();
        println!("x={col} fill-pixels={ink}");
    }
}
