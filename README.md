# mapconductor-vectortile

A MapLibre-style vector tile rasteriser: `style.json` + `.pbf` tiles → PNG raster tiles.

Its job is to let map backends that **cannot render vector styles** — Google Maps,
MapKit, HERE, TomTom, ArcGIS, Cesium, OpenLayers, Leaflet — display a MapLibre
style through an ordinary raster layer, using the raster-layer plumbing the
MapConductor SDKs already have.

## Status

Working on all three platforms. A MapLibre style renders to raster tiles in the
browser, on an Android device, and on the iOS simulator — from one Rust core.

**The output is byte-identical everywhere.** Same style, same source tile, same
SHA-256 of the resulting PNG:

| | SHA-256 of tile 0/0/0 (512px) |
| --- | --- |
| macOS (`aarch64-apple-darwin`) | `0ec6e550…6d141e39` |
| Android device (`aarch64-linux-android`) | `0ec6e550…6d141e39` |
| iOS simulator (`aarch64-apple-ios-sim`) | `0ec6e550…6d141e39` |

That is the whole argument for a shared Rust core rather than three native
renderers or three MapLibre snapshotters, and it is now measured rather than
hoped for.

### Performance

Warm render of tile 0/0/0 — the heaviest in the demo style, 722 features across
5 layers — with source tiles already in hand, so this is decode plus rasterise:

| | median | range |
| --- | --- | --- |
| Web (Chromium, wasm, through the SW + worker round trip) | 98 ms | 87–363 |
| Android (Lenovo TB520FU tablet, Android 16) | 103 ms | 103–106 |
| Android (Pixel 5a, Android 14) | 312 ms | — |
| iOS simulator (iPhone 17 Pro, Release) | 87–195 ms | 77–5900 |

The Android figures are solid — a 3 ms spread across ten runs on the tablet.
The 3x gap between the two devices is the honest range for mid-range hardware,
and 312 ms per tile is slow enough to matter for a map being panned: rendering
belongs off the UI thread there for the same reason it does on the web.
**The iOS figure is not.** Repeated runs on the simulator gave medians from 87 ms
to 195 ms with occasional multi-second outliers, tracking whatever else the host
Mac was doing. Treat it as "the same order of magnitude as Android" and nothing
finer; a trustworthy number needs the `aarch64-apple-ios` slice and a phone.

The harness benchmarks street-level OSM tiles (z13 over central Tokyo, a
324-layer basemap style): **median ~126 ms**, ranging 1.6 ms for a water tile to
~330 ms for the densest. Benchmarking low zooms is misleading — the same
benchmark at z2 reports ~5 ms because those tiles are mostly empty ocean.

| | |
| --- | --- |
| Rust core tests | 49 passing |
| Playwright end-to-end | 10 passing |
| Android instrumented (on device) | 10 passing |
| iOS XCTest (on simulator) | 10 passing |
| wasm binary | 489 KB · Android `.so` 1.0 MB · iOS `.a` 6.3 MB |
| Main-thread stall, 12-tile burst | 5.4 ms in a worker vs 1709 ms inline |

On the web, rendering runs in a Web Worker by default. Inline, a 12-tile burst
blocked the main thread for the entire 190 ms it took; in a worker the longest
stall was 4.9 ms. Wall-clock time is unchanged because one worker renders
serially — a worker pool would cut that, and is the obvious next performance
step.

Styles can be swapped live with `setStyle`. Recolouring never invalidates
fetched vector tiles — the geometry is unchanged, only the paint applied to it —
so it costs one re-rasterise and zero network traffic, which the harness test
asserts by checking no `.pbf` request is made. The map still has to be told to
drop its *raster* tiles, which is what the `cacheKey` argument to
`urlTemplate()` is for.

The web worker belongs to the provider rather than going through
`LocalTileServer.attachRenderer()`. That hook routes *every* tile route through
one worker and bypasses main-thread providers entirely
(`LocalTileServer.ts:152`), so using it would break any page that also has a
heatmap or marker-tile layer.

### SDK modules

The renderer is placed in all three SDKs as an **independent module**, never a
core change — the same shape `android-heatmap`, `ios-heatmap` and
`react-heatmap` already use. Each implements its core's tile provider interface
and serves through the existing `LocalTileServer`, so a backend that cannot
render a vector style just sees a raster layer.

| | Module | Verified |
| --- | --- | --- |
| Android | `android-sdk/android-vectortile` | 5 tests, on device |
| iOS | `ios-sdk/ios-vectortile` | 5 tests, on simulator |
| Web | `react-sdk/react-vectortile` | browser resolution check |

This repo owns the renderer; each module owns only its integration layer.
`scripts/sync-sdk.sh` copies the renderer and its native artifacts out, which
keeps the direction of the dependency obvious.

Consumers of the Android module need a network security config permitting
cleartext to `127.0.0.1` — true of every tile-server-backed layer in that SDK,
and the reason `example-app` already carries one.

Not yet done: an `aarch64-apple-ios` slice for real iPhones (the simulator
slice is all that exists today), and symbol layers.

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

### Compiled filters

Layer filters are compiled per (style layer, data layer) pair instead of
interpreted per feature. The key observation is MVT's own storage model: every
feature holds (key index, value index) pairs into tables owned by the layer, so
any predicate on a property value can be evaluated **once per table entry**,
producing a truth table over value indices — per feature, matching is an
integer scan and a boolean lookup. Filters naming keys or values a tile does
not contain fold to constants, skipping entire layers without touching a
feature.

Anything the compiler does not recognise falls back to the interpreter for
that node, and an equivalence test pins the two against each other over every
operator. That test immediately caught a real interpreter bug: legacy `!has`
was not implemented, so every `!has` filter silently matched nothing.

Measured on the z14 Tokyo tile (VersaTiles Shortbread, 324 layers), output
byte-identical before and after:

| | interpreted | compiled |
| --- | --- | --- |
| CPU render, desktop | 86.0 ms | **29.0 ms** |
| Tessellation, desktop | 59.6 ms | **4.8 ms** |
| CPU render + PNG, Pixel 5a | 513 ms | **322 ms** |
| GPU path total, Pixel 5a | ~410 ms | **~103 ms** |
| Web warm render, Chromium | 114.9 ms | **65.8 ms** |

The GPU path benefits most because filtering was nearly all of its CPU cost:
what remains on the CPU is ~94 ms of tessellation, with drawing, readback and
bitmap conversion on the GPU side at under 10 ms combined.

### Mapbox styles

MapLibre's style spec is a fork of Mapbox's v8 spec (MapLibre GL JS split from
Mapbox GL JS v1.13 in 2020), so the classic subset is shared and a classic
Mapbox v8 style parses and renders here unchanged — same layer types, same
expressions, same filters. Two things differ:

- **`mapbox://` URLs.** Sources, sprites and glyphs addressed this way need an
  access token and must be translated to `https://api.mapbox.com/...` before
  they can be fetched. That resolution belongs on the host side, next to the
  token — the same place TileJSON resolution already happens.
- **Mapbox v3 "Standard".** It introduced `imports`, `slot`, `fragment` and
  `config`, which compose a style out of other styles. That is a genuinely
  different model, not a superset of v8, and it is not supported. A Standard
  style keeps the whole basemap behind an import, so it would otherwise render
  blank.

Both cases are reported by `diagnostics()` rather than failing or silently
producing empty tiles. Mapbox-only layer types (`sky`, `model`,
`raster-particle`) and MapLibre-only ones show up there too.

## Design decisions

### The decoder is the only format-specific part

`src/model.rs` holds the decoded tile — layers, features, geometry, properties —
and the style parser, expression evaluator and rasteriser consume only that. The
MVT decoder's job ends at producing one. A second wire format is a second
decoder and nothing else, which is what makes the single-module decision below
workable rather than merely convenient.

The renderer is tested against a tile built by hand, with no decoder in the call
path at all, so the seam cannot rot unnoticed.

### One module per platform, formats behind Cargo features

Not `*-mvt-tile` / `*-mlt-tile` split by tile format. Three reasons, measured:

1. **The decoder is the small part.** `src/mvt.rs` is ~530 lines; the style
   spec, expressions, rasteriser, tile planning and projection are ~2,000 and
   are identical whatever the tile format is. Splitting by format leaves 80% of
   the code with nowhere natural to live.
2. **The module boundary would not match the binary boundary.** A Gradle module
   ships a `.so`, and a Rust cdylib cannot be split across two of them without
   either duplicating the shared core in both (~1.07 MB each, so ~2.1 MB) or
   dynamically linking your own native libraries — awkward on Android and
   unavailable for iOS static frameworks. Adding MLT to a *single* library
   measured at **+0.3–0.7 MB**. Splitting costs more bytes, not fewer.
3. **Format is a runtime property of the source, not a build-time property of
   the app.** One style can reference several sources, and during a migration
   some could be MVT and others MLT — so an app would end up depending on both
   modules anyway.

`mlt-core` also re-exports `fast_mvt`, so an MLT-only module would contain an
MVT decoder regardless.

"Don't pay for formats you don't use" is a real concern, but it belongs one
layer down, as Cargo features that decide what goes into the binary.

**When to revisit:** when a format changes something *other than the decoder*.
Concretely — a non-Web-Mercator tiling scheme (measured: 5 of 12 public Esri
vector tile services are in other CRSs), a different rendering model (3D,
terrain, globe), a different style language (HERE ships its own), or an
untiled source. Those touch the planner, the rasteriser or the style layer, and
a new decoder never does.

### Formats surveyed

Every vendor checked serves MVT bytes. What actually differs is the *style*
format and the authentication, not the tile encoding.

| Source | Tile | Style | Key | Verified |
| --- | --- | --- | --- | --- |
| OSM (OSMF Shortbread) | MVT | open schema, many styles | none | rendered |
| MapLibre demo | MVT | MapLibre spec | none | rendered |
| ArcGIS | MVT | Mapbox spec v8 (`root.json`) | some | rendered |
| Mapbox classic v8 | MVT | Mapbox spec v8 | yes | parses; needs `mapbox://` resolution |
| Mapbox Standard v3 | MVT | `imports` — different model | yes | unsupported, reported by `diagnostics()` |
| HERE | MVT (OMV) | proprietary | yes | data readable; style would need authoring |

Vendor-specific quirks that belong on the host side, not in the renderer:
ArcGIS orders tile URLs `{z}/{y}/{x}` (handled for free, since templates are
expanded by placeholder name), uses style-relative URLs like `"../../"`, and
its services are not always Web Mercator. `Content-Encoding: gzip` is common and
every real HTTP client undoes it transparently.

## Layout

```
crates/mvt-render/        platform-independent core
  src/model.rs            decoded tile model — the seam between formats and drawing
  src/mvt.rs              hand-rolled MVT/protobuf decoder
  src/style.rs            style.json subset
  src/expr.rs             expression evaluator
  src/raster.rs           tiny-skia drawing
  src/tile.rs             fetch planning + overzoom
  src/mercator.rs         web mercator
crates/mvt-render-wasm/   wasm-bindgen surface
crates/mvt-render-jni/    JNI surface for Android
crates/mvt-render-ffi/    C ABI surface for iOS
bindings/js/              @mapconductor/vectortile — TileProvider for js-sdk-core
bindings/android/         Gradle module + instrumented tests
bindings/ios/             SwiftPM package + XCTest
harness/                  Vite + Playwright end-to-end harness, against the
                          OSMF Shortbread tile service (no API key)
scripts/                  build-android.sh, build-ios.sh
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

The harness renders the [OSMF Shortbread tiles](https://vector.openstreetmap.org/shortbread_v1/tilejson.json)
with a [VersaTiles](https://versatiles.org/) style — a demonstration of what an
open schema buys you, since the style and the tiles come from different hosts.
Neither needs an API key. Add `?style=demo` for the MapLibre demo style, which
is kept around because it has per-feature colour *expressions* and Shortbread's
style does not.

Anything beyond local development should respect the
[OSMF Vector Tile Usage Policy](https://operations.osmfoundation.org/policies/vector/)
and host its own tiles; Shortbread can be generated from an OSM extract.

Android, against a connected device:

```sh
./scripts/build-android.sh                   # locates the NDK via ANDROID_HOME
cd bindings/android && ./gradlew :vectortile:connectedDebugAndroidTest
```

iOS, on a simulator:

```sh
./scripts/build-ios.sh
cd bindings/ios
xcodebuild -create-xcframework -library lib/libmvt_render_ffi.a \
    -headers include -output MvtRender.xcframework
xcodebuild -scheme MapConductorVectorTile -configuration Release \
    -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test
```

## Reused from earlier prototypes

- `vector-tile-rendering` (Rust/WASM): overzoom-to-parent handling, MVT extent
  normalisation, the feature→path approach. Its viewport, pan/zoom, prefetch and
  Canvas2D layers are intentionally *not* carried over — the host map SDK and
  `LocalTileServer` already own that half, and Canvas2D is browser-only.
- `tile_renderer` (C++/OpenGL): `projection.h`, ported to `src/mercator.rs`.
