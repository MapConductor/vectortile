fn main() {
    let dir = std::env::args().nth(1).unwrap();
    let ch: char = std::env::args().nth(2).unwrap().chars().next().unwrap();
    let mut store = mvt_render::text::GlyphStore::new();
    let mut files = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("pbf") {
            continue;
        }
        files += 1;
        match mvt_render::glyphs::decode(&std::fs::read(&path).unwrap()) {
            Ok(stacks) => {
                for stack in stacks {
                    store.insert(stack);
                }
            }
            Err(e) => println!("FAILED {} {e}", path.display()),
        }
    }
    let fonts = vec!["noto_sans_regular".to_string()];
    let glyph = store.glyph(&fonts, ch as u32);
    match glyph {
        Some(g) => {
            let max = g.bitmap.iter().copied().max().unwrap_or(0);
            let inked = g.bitmap.iter().filter(|v| **v > 192).count();
            println!(
                "{files} files; {ch} U+{:04X}: {}x{} bitmap={} max={max} above-edge={inked} advance={} left={} top={}",
                ch as u32, g.width, g.height, g.bitmap.len(), g.advance, g.left, g.top
            );
        }
        None => println!("{ch} not present"),
    }
}
