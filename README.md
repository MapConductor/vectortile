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
| Android (Lenovo TB520FU, arm64, Android 16) | 103 ms | 103–106 |
| iOS simulator (iPhone 17 Pro, Release) | 87–195 ms | 77–5900 |

The Android figure is solid — a 3 ms spread across ten runs on a real device.
**The iOS figure is not.** Repeated runs on the simulator gave medians from 87 ms
to 195 ms with occasional multi-second outliers, tracking whatever else the host
Mac was doing. Treat it as "the same order of magnitude as Android" and nothing
finer; a trustworthy number needs the `aarch64-apple-ios` slice and a phone.

A typical tile is far cheaper than this one: across a mixed z2 viewport the web
median is ~31 ms, with mostly-ocean tiles at ~5 ms.

| | |
| --- | --- |
| Rust core tests | 49 passing |
| Playwright end-to-end | 10 passing |
| Android instrumented (on device) | 10 passing |
| iOS XCTest (on simulator) | 10 passing |
| wasm binary | 489 KB · Android `.so` 1.0 MB · iOS `.a` 6.3 MB |
| Main-thread stall, 12-tile burst | 4.9 ms in a worker vs 189.9 ms inline |

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

Not yet done: wiring the Android and iOS bindings into `android-sdk-core` /
`ios-sdk-core` as `TileProviderInterface` / `TileProvider` implementations, and
an `aarch64-apple-ios` slice for real iPhones.

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
harness/                  Vite + Playwright end-to-end harness
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
