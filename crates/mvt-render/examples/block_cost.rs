//! Temporary: one block render against the tiles it would replace.
use mvt_render::Renderer;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("fixture dir"));
    let style = fs::read_to_string(dir.join("style.json")).unwrap();
    // What the iPad's ArcGIS view draws: level 13 tiles, 128 pt each, at 2x.
    let z: u8 = std::env::args()
        .nth(2)
        .map(|v| v.parse().unwrap())
        .unwrap_or(13);
    let (dp, px): (u32, u32) = (128, 256);
    let (cx, cy): (u32, u32) = match z {
        9 => (454, 201),
        11 => (1818, 806),
        _ => (7272, 3228),
    };
    let (x0, y0) = (cx & !3, cy & !3);

    let read = |q: &mvt_render::SourceTileRequest| {
        fs::read(dir.join(format!("{}-{}-{}.mvt", q.z, q.x, q.y))).ok()
    };

    // (a) Sixteen tiles, the way they are drawn now.
    let r = Renderer::new(&style).unwrap().with_display_tile_size(dp);
    let mut per_tile_ms = 0.0f64;
    let mut drawn = 0usize;
    for dy in 0..4u32 {
        for dx in 0..4u32 {
            let (x, y) = (x0 + dx, y0 + dy);
            let plan = r.plan(z, x, y);
            let owned: Vec<Option<Vec<u8>>> = plan.requests.iter().map(read).collect();
            let data: Vec<Option<&[u8]>> = owned.iter().map(|o| o.as_deref()).collect();
            let started = Instant::now();
            let _ = r.render(z, x, y, px, &data).unwrap();
            per_tile_ms += started.elapsed().as_secs_f64() * 1000.0;
            drawn += 1;
        }
    }

    // (b) The same ground, drawn once.
    let r2 = Renderer::new(&style).unwrap().with_display_tile_size(dp);
    let plan = r2.plan_source_block(z, x0, y0);
    let owned: Vec<Option<Vec<u8>>> = plan.requests.iter().map(read).collect();
    let data: Vec<Option<&[u8]>> = owned.iter().map(|o| o.as_deref()).collect();
    let started = Instant::now();
    let (pixmap, across) = r2.render_source_block(z, x0, y0, px, &data).unwrap();
    let block_ms = started.elapsed().as_secs_f64() * 1000.0;

    // Cutting the block up is what the host would do per tile.
    let started = Instant::now();
    let mut cut = 0usize;
    for dy in 0..across {
        for dx in 0..across {
            let rect =
                tiny_skia::IntRect::from_xywh((dx * px) as i32, (dy * px) as i32, px, px).unwrap();
            if let Some(piece) = pixmap.clone_rect(rect) {
                let _ = piece.encode_png().unwrap();
                cut += 1;
            }
        }
    }
    let cut_ms = started.elapsed().as_secs_f64() * 1000.0;

    println!("source plan: {} requests", plan.requests.len());
    println!(
        "(a) {drawn} tiles one by one : {per_tile_ms:7.1} ms  ({:.1} ms each)",
        per_tile_ms / drawn as f64
    );
    println!(
        "(b) one {}x{} block           : {block_ms:7.1} ms  + cut&encode {cut_ms:.1} ms for {cut} tiles",
        pixmap.width(),
        pixmap.height()
    );
    println!(
        "    total (b)                : {:7.1} ms",
        block_ms + cut_ms
    );
}
