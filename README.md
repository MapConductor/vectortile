# mapconductor-vectortile

A MapLibre-style vector tile rasteriser: `style.json` + `.pbf` tiles → PNG raster tiles.

Its job is to let map backends that **cannot render vector styles** — Google Maps,
MapKit, HERE, TomTom, ArcGIS, Cesium, OpenLayers, Leaflet — display a MapLibre
style through an ordinary raster layer, using the raster-layer plumbing the
MapConductor SDKs already have.

## Why a Rust core

One renderer, three platforms, identical output:

```
                    ┌─ wasm ──────────→ js-sdk-core      (LocalTileServer / Service Worker)
crates/mvt-render ──┼─ .so via JNI ───→ android-sdk-core (TileProviderInterface)
   (pure Rust)      └─ XCFramework ───→ ios-sdk-core     (TileProvider)
```

Writing the renderer three times in Kotlin, Swift and TypeScript would produce
three subtly different maps. Rendering via each platform's MapLibre Native
snapshotter would too — different font stacks, different anti-aliasing.

Every dependency is pure Rust (`tiny-skia`, `serde`, `serde_json`), so there is
no C toolchain and **no `protoc`** in any of the three build pipelines. The MVT
decoder is hand-written for exactly that reason.

## Two-phase API: no I/O inside the core

```rust
let renderer = Renderer::new(&style_json)?;

let plan = renderer.plan(z, x, y);        // 1. which source tiles are needed
let bytes = host_fetches(plan.urls());    // 2. the HOST does all network I/O
let png = renderer.render(z, x, y, 512, &bytes)?;  // 3. rasterise
```

The host fetches, so the platform's existing HTTP stack keeps working: auth
header injection (`RasterHeaderRules`), certificate pinning, proxies, OkHttp's
cache. Pushing an HTTP client into Rust would mean rebuilding all of that.

It also means the FFI boundary is only ever crossed with plain byte arrays, in
one direction. There is no callback from Rust into the host — which on Android
would require `AttachCurrentThread` on a non-JVM thread, the one place where JNI
overhead genuinely bites. Two calls per tile at ~0.1 µs each are noise next to a
5–50 ms rasterisation.

## What it draws

| Layer type | Status |
| --- | --- |
| `background`, `fill`, `line`, `circle` | drawn |
| `symbol` | **parsed, not drawn** |
| `raster`, `hillshade`, `fill-extrusion`, `heatmap` | parsed, not drawn |

`Renderer::unsupported_layer_types()` reports what a given style loses, so a host
can warn instead of silently dropping content.

**Symbol layers are the hard boundary, not an oversight.** Labels need collision
detection that stays consistent across tile boundaries; render tiles
independently and the same place name appears twice at a seam, or vanishes on
one side and not the other. Solving it needs a shared placement pass, which is a
different architecture from "one tile in, one tile out". Until then, this is a
renderer for thematic overlays and label-free basemaps.

Expressions cover literals, `get`/`has`/`id`/`geometry-type`/`zoom`,
`interpolate` (linear and exponential), `step`, `match`, `case`, `coalesce`,
comparisons, `all`/`any`/`!`, `in`, arithmetic, `rgb`/`rgba`, the legacy
`{"stops": …}` function objects, and legacy array filters. Unknown operators
evaluate to null so an unsupported property falls back to its default rather
than blanking the tile.

Known gaps: `line-dasharray`, `*-pattern`, `line-gradient`, the source `scheme:
"tms"` option, and cubic-bezier easing (approximated as linear).

## Layout

```
crates/mvt-render/        platform-independent core
  src/mvt.rs              hand-rolled MVT/protobuf decoder
  src/style.rs            style.json subset
  src/expr.rs             expression evaluator
  src/raster.rs           tiny-skia drawing
  src/tile.rs             fetch planning + overzoom
  src/mercator.rs         web mercator
crates/mvt-render-wasm/   wasm-bindgen surface
bindings/js/              @mapconductor/vectortile — TileProvider for js-sdk-core
harness/                  Vite + Playwright end-to-end harness
```

## Building

The core needs nothing beyond a stable Rust toolchain:

```sh
cargo test -p mvt-render
```

Rendering a real style to a PNG, with the shell doing the fetching:

```sh
curl -s https://demotiles.maplibre.org/style.json -o style.json
# resolve the TileJSON url into inline `tiles`, then:
cargo run --example render_file -- style.json 0 0 0 512 out.png tile.pbf
```

The wasm target additionally needs `rustup` (Homebrew's `rust` ships only the
host target):

```sh
rustup target add wasm32-unknown-unknown
cd bindings/js && npm run build:wasm
```

Then the browser harness:

```sh
cd harness && npm install && npm run dev     # http://localhost:5175
cd harness && npm test                        # Playwright
```

## Reused from earlier prototypes

- `vector-tile-rendering` (Rust/WASM): overzoom-to-parent handling, MVT extent
  normalisation, the feature→path approach. Its viewport, pan/zoom, prefetch and
  Canvas2D layers are intentionally *not* carried over — the host map SDK and
  `LocalTileServer` already own that half, and Canvas2D is browser-only.
- `tile_renderer` (C++/OpenGL): `projection.h`, ported to `src/mercator.rs`.
