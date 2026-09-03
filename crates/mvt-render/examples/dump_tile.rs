//! Lists the layers inside a `.pbf` vector tile.
//!
//! Useful when a style renders blank: it answers "does the tile actually
//! contain the `source-layer` names the style asks for?" without guessing.
//!
//!   cargo run --example dump_tile -- tile.pbf
//!
//! Note the input must be *decompressed*. Tile servers commonly send
//! `Content-Encoding: gzip`; HTTP clients undo that transparently, but
//! `curl` needs `--compressed`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: dump_tile <tile.pbf>");
        return ExitCode::from(2);
    };

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    if bytes.starts_with(&[0x1f, 0x8b]) {
        eprintln!("{path} is gzip-compressed; refetch with `curl --compressed`");
        return ExitCode::FAILURE;
    }

    let tile = match mvt_render::mvt::decode(&bytes) {
        Ok(tile) => tile,
        Err(error) => {
            eprintln!("{path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let total: usize = tile.layers.iter().map(|l| l.features.len()).sum();
    println!("{} layers, {total} features", tile.layers.len());
    for layer in &tile.layers {
        println!(
            "  {:34} extent={:<5} features={}",
            layer.name,
            layer.extent,
            layer.features.len()
        );
    }
    ExitCode::SUCCESS
}
