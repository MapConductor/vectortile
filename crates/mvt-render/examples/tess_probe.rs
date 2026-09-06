fn main() {
    let style = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let dir = std::env::args().nth(2).unwrap();
    let z: u8 = std::env::args().nth(3).unwrap().parse().unwrap();
    let x: u32 = std::env::args().nth(4).unwrap().parse().unwrap();
    let y: u32 = std::env::args().nth(5).unwrap().parse().unwrap();
    let renderer = mvt_render::Renderer::new(&style).unwrap();
    let plan = renderer.plan(z, x, y);
    let bytes: Vec<Option<Vec<u8>>> = plan
        .requests
        .iter()
        .map(|r| std::fs::read(format!("{dir}/n_{}_{}.mvt", r.x, r.y)).ok())
        .collect();
    let refs: Vec<Option<&[u8]>> = bytes.iter().map(|b| b.as_deref()).collect();
    let started = std::time::Instant::now();
    let packed = renderer.tessellate_packed(z, x, y, 512, &refs).unwrap();
    println!(
        "tessellate {:?}: decode={:.0}ms tess={:.0}ms fills={:.0}ms lines={:.0}ms loop={:.0}ms",
        started.elapsed(),
        packed[7],
        packed[8],
        packed[10],
        packed[11],
        packed[12]
    );
}
