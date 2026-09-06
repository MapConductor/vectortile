fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let text = std::env::args().nth(2).unwrap();
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
    for ch in text.chars() {
        println!(
            "{ch} U+{:04X} present={}",
            ch as u32,
            store.glyph(&fonts, ch as u32).is_some()
        );
    }
    let shaped = mvt_render::text::shape(&text, &fonts, &store, f32::MAX);
    println!("shaped {} glyphs:", shaped.glyphs.len());
    for g in &shaped.glyphs {
        println!("  U+{:04X} at ({:.1},{:.1})", g.code, g.x, g.y);
    }
}
