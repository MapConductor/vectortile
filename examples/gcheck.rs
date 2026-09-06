fn main() {
    let bytes = std::fs::read(std::env::args().nth(1).unwrap()).unwrap();
    let stacks = mvt_render::glyphs::decode(&bytes).unwrap();
    for s in &stacks {
        println!("stack name={:?} range={:?} glyphs={}", s.name, s.range, s.glyphs.len());
    }
}
