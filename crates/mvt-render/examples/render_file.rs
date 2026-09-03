//! Renders one tile from files on disk, for eyeballing real-world styles.
//!
//! Fetching is deliberately left to the shell — the crate itself never does
//! network I/O.
//!
//!   cargo run --example render_file -- style.json 2 2 1 512 out.png tile0.pbf ...
//!
//! The trailing `.pbf` paths must be in the order printed by the plan, which
//! this example reports before rendering. Pass `-` for a tile that is missing.

use std::fs;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 6 {
        eprintln!("usage: render_file <style.json> <z> <x> <y> <size> <out.png> [tile.pbf|- ...]");
        std::process::exit(2);
    }

    let style_json = fs::read_to_string(&args[0]).expect("read style");
    let z: u8 = args[1].parse().expect("z");
    let x: u32 = args[2].parse().expect("x");
    let y: u32 = args[3].parse().expect("y");
    let size: u32 = args[4].parse().expect("size");
    let out = &args[5];

    let renderer = mvt_render::Renderer::new(&style_json).expect("parse style");

    let unsupported = renderer.unsupported_layer_types();
    if !unsupported.is_empty() {
        eprintln!("skipped layer types: {}", unsupported.join(", "));
    }
    for source in renderer.unresolved_sources() {
        eprintln!(
            "source {:?} needs TileJSON resolved from {:?}",
            source.id,
            source.url.as_deref().unwrap_or("")
        );
    }

    let plan = renderer.plan(z, x, y);
    eprintln!("plan for {z}/{x}/{y}:");
    for (i, request) in plan.requests.iter().enumerate() {
        eprintln!(
            "  [{i}] {} {}{}",
            request.source_id,
            request.url,
            if request.is_overzoomed() {
                format!("  (overzoomed x{})", request.scale)
            } else {
                String::new()
            }
        );
    }

    let buffers: Vec<Option<Vec<u8>>> = args[6..]
        .iter()
        .map(|path| {
            if path == "-" {
                None
            } else {
                Some(fs::read(path).expect("read tile"))
            }
        })
        .collect();
    let slices: Vec<Option<&[u8]>> = buffers.iter().map(|b| b.as_deref()).collect();

    // Repeat to separate one-off costs (process start, style parsing) from the
    // per-tile work that actually matters.
    let repeat: usize = std::env::var("REPEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    let mut render_times = Vec::with_capacity(repeat);
    let mut encode_times = Vec::with_capacity(repeat);
    let mut last = None;
    for _ in 0..repeat {
        let started = std::time::Instant::now();
        let (pixmap, stats) = renderer
            .render_pixmap(z, x, y, size, &slices)
            .expect("render");
        render_times.push(started.elapsed().as_secs_f64() * 1000.0);

        let started = std::time::Instant::now();
        let png = pixmap.encode_png().expect("encode");
        encode_times.push(started.elapsed().as_secs_f64() * 1000.0);
        last = Some((png, stats));
    }
    let (png, stats) = last.expect("at least one render");

    eprintln!(
        "drew {} layers, {} features",
        stats.layers_drawn, stats.features_drawn
    );
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    eprintln!(
        "render median {:.1} ms | png encode median {:.1} ms  (n={})",
        median(render_times),
        median(encode_times),
        repeat
    );
    fs::write(out, png).expect("write output");
    eprintln!("wrote {out}");
}
