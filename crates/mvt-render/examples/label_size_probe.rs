fn main() {
    let style = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let tile = std::fs::read(std::env::args().nth(2).unwrap()).unwrap();
    let glyphs = std::fs::read(std::env::args().nth(3).unwrap()).unwrap();
    let z: u8 = std::env::args().nth(4).unwrap().parse().unwrap();
    let r = mvt_render::Renderer::new(&style).unwrap();
    r.add_glyphs(&glyphs).ok();
    for size in [256u32, 512, 1024] {
        let plan = r.plan(z, 0, 0);
        let data: Vec<Option<&[u8]>> = plan.requests.iter().map(|_| Some(tile.as_slice())).collect();
        let sizes = r.label_sizes(z, size, &data);
        let mut uniq: Vec<String> = sizes.iter().map(|s| format!("{s:.1}")).collect();
        uniq.sort();
        uniq.dedup();
        println!("tile_size={size:4} labels={:3} drawn text sizes(px)={:?}", sizes.len(), uniq);
    }
}
