fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let text = std::env::args().nth(2).unwrap();
    let size: f32 = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "12".into())
        .parse()
        .unwrap();
    let mut store = mvt_render::text::GlyphStore::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("pbf") {
            continue;
        }
        for stack in mvt_render::glyphs::decode(&std::fs::read(&path).unwrap()).unwrap() {
            store.insert(stack);
        }
    }
    let fonts = vec!["noto_sans_regular".to_string()];
    let shaped = mvt_render::text::shape(&text, &fonts, &store, f32::MAX);
    let paint = mvt_render::text::TextPaint {
        size,
        color: mvt_render::color::Color::new(0.0, 0.0, 0.0, 1.0),
        halo_color: mvt_render::color::Color::new(1.0, 1.0, 1.0, 0.8),
        halo_width: 1.0,
        halo_blur: 1.0,
        opacity: 1.0,
    };

    // Straight run, the way a flat road lays it out.
    let scale = size / 24.0;
    let glyphs: Vec<mvt_render::text::GlyphOnLine> = shaped
        .glyphs
        .iter()
        .map(|g| mvt_render::text::GlyphOnLine {
            code: g.code,
            at: (20.0 + g.x * scale, 40.0),
            angle: 1.3,
        })
        .collect();
    let mut pixmap = tiny_skia::Pixmap::new(160, 60).unwrap();
    mvt_render::text::draw_along_line(&mut pixmap, &glyphs, &store, &fonts, &paint);

    for g in &glyphs {
        let x0 = (g.at.0 - 4.0).max(0.0) as u32;
        let x1 = ((g.at.0 + size) as u32).min(160);
        let ink = (x0..x1)
            .flat_map(|x| (0..60u32).map(move |y| (x, y)))
            .filter(|(x, y)| pixmap.pixels()[(y * 160 + x) as usize].alpha() > 40)
            .count();
        println!("U+{:04X} at x={:.1}: inked pixels {ink}", g.code, g.at.0);
    }
}
