# Android performance measurements — 2026-09-07

Pixel 5a, arm64, OpenGL ES 3 with 4x MSAA. Results include controlled offline
fixed-input measurements and a cold-start Tokyo viewport observation. No
resolution, style, label placement or anti-aliasing was reduced.

## Changes

- `android-release` inherits the existing release profile and uses `opt-level=3`.
  `scripts/build-android.sh` now selects it. Other platforms keep their existing
  release settings. The arm64 JNI binary was rebuilt and copied into
  `android-sdk/android-vectortile/src/main/jniLibs/arm64-v8a/`.
- GPU tessellation now returns a Rust-owned `DirectByteBuffer`. OpenGL uploads
  a `FloatBuffer` view of it directly, removing both the Rust-to-Java
  `FloatArray` copy and the Java-heap-to-direct staging copy. The native
  allocation is released in `finally` after synchronous `glBufferData`, and is
  also released when a request is cancelled or the GPU is already closed.
- Geometry readback now goes into a pooled per-request `DirectByteBuffer` and
  that same buffer is passed to the Rust PNG encoder. The previous path copied
  each 512px tile from the GL direct buffer into a 1 MiB `ByteArray`, then back
  into another direct buffer. Per-request buffers let the GL thread proceed
  while an earlier worker encodes, without either copy.
- Standalone labels now follow the same ownership model: Rust creates the
  transparent premultiplied RGBA pixmap and exposes it as a
  `DirectByteBuffer` to the Android PNG encoder. This removes the 4 MiB Java
  label buffer at 1024px and the Rust-to-Java result copy. The allocation is
  released in `finally` after encoding. A Rust test verifies byte-for-byte
  equality with the previous draw-onto-transparent-buffer route.
- Solid, non-dashed lines are projected directly from decoded coordinates,
  avoiding a temporary projected `Vec` for each feature/style-layer pair. A
  sliding projection window also avoids recalculating shared segment and join
  points. The latter was neutral in an interleaved Pixel benchmark; the output
  fingerprint was unchanged, and packed vertex order is preserved.
- Provider logs distinguish `FULL`, `GEOMETRY`, and `LABELS`. Label logs split
  semaphore wait, drawing (including JNI and input packing), and PNG encoding.
  GPU logs split queue wait, vertex upload, command submission, MSAA resolve /
  readback, and readback copy. Command submission is CPU wall time, not GPU time;
  GPU completion waits can appear in resolve/readback.

## Native optimization comparison

`pipeline_cost` was built with rustc 1.98.0 (88d9e12ae, 2026-08-18), NDK
27.1.12297006, Android API 28, aarch64-linux-android. The `z`, `2`, and `3`
variants used the same compiler, source and inputs; only the release opt-level
changed. Three warmups and 21 samples per phase, two runs per variant, with
variants interleaved. No CPU affinity or frequency lock. Thermal status was 0
before the experiment; results can vary with scheduling and temperature.

Input: `benchapp-ios/Resources/{style.json,tile.mvt}`, z14/x14552/y6451, central
tile only (neighbour entries explicitly absent). Geometry output is 512px;
labels are 1024px. 30,067 glyphs were loaded from the example app's existing
cache, producing 24 placed labels. Glyph files were copied into a temporary
benchmark directory; the app cache was not modified. Sprites were not loaded.
This does not measure a complete 3x3 label neighbourhood or cold glyph loading.

| Median per run (ms) | z | 2 | 3 |
| --- | --- | --- | --- |
| Decode + tessellate + pack | 33.503 / 33.515 | 22.026 / 21.997 | 21.540 / 21.817 |
| Label clear + decode/cache + collect/place + CPU draw | 12.338 / 12.176 | 10.893 / 10.816 | 11.007 / 10.829 |
| Needed-glyph query | 1.690 / 1.690 | 1.142 / 1.158 | 1.153 / 1.126 |

Geometry improved about 35%; levels 2 and 3 were close. Level 3 was selected
for Android. Native measurements exclude JNI, GL, PNG encoding and network.
The benchmark prints decode/tessellate fields from the **last** sample in
addition to the median of the enclosing operation.

All six runs had the same FNV-1a fingerprints:

- Packed geometry, excluding timing fields: `806e52164c817228` (371,965 floats).
- Label RGBA: `baba0be9c6e2e8a3`.

Input SHA-256:

- Style: `4d9c9fa44e43649cd507678b9c26d5b93b1c0cd2722c167c001ff0a28e06df52`
- Tile: `ace7a7ca1e2591e12e08e9da64e410aa2b867ce143dec38372d9853d3c08cf6d`

Build each native comparison executable before copying it under a unique name
to the device (commands from this repository; select the Rust toolchain whose
Android standard library is installed):

```sh
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$ANDROID_HOME/ndk/27.1.12297006/toolchains/llvm/prebuilt/darwin-x86_64/bin/aarch64-linux-android28-clang"
cargo build -p mvt-render --example pipeline_cost --features tessellate \
  --target aarch64-linux-android --release
# Repeat with --config profile.release.opt-level=2 and then =3.
adb shell mkdir -p /data/local/tmp/mc-perf
adb push target/aarch64-linux-android/release/examples/pipeline_cost /data/local/tmp/mc-perf/cost-z
adb push benchapp-ios/Resources/style.json benchapp-ios/Resources/tile.mvt /data/local/tmp/mc-perf/
adb shell /data/local/tmp/mc-perf/cost-z \
  /data/local/tmp/mc-perf/style.json /data/local/tmp/mc-perf/tile.mvt \
  14 14552 6451 /data/local/tmp/mc-perf/glyphs 21
```

The optional glyph directory must already contain the style's glyph PBF files.
Use `-` in its place for a geometry-only benchmark. Copy each variant immediately
after building it: Cargo reuses the same executable path across opt-levels.

## Android integration

Existing `GpuRenderModeTest.reportsRelativeCostOfBothModes`: bundled world tile
z0/0/0 and demo style, 512px, no network, no rendered disk cache, two warmups +
seven measured renders, sequential requests. Debug APK, release native library;
debug phase logs enabled in both builds. GPU timings include native preparation,
JNI, GL upload/draw/readback and PNG encoding. This fixture has no drawn labels.

| Median (ms) | Size-first native + allocating upload | Speed native + reusable staging | Direct native buffer | + direct readback |
| --- | ---: | ---: | ---: | ---: |
| GPU renderTile | 143.2 | 96.6 | 71.6 | **66.0** |
| CPU renderTile | 239.9 | about 100 | 100.0 | 101.0 |

The final GPU path is about 54% faster than the original measurement on this
fixture and 32% faster than the staging-buffer version.
Representative warm upload calls fell from about 20–29 ms originally, to
8–10 ms with reusable staging, and to a 6.3 ms median when uploading the native
buffer directly. The first direct upload was 13.0 ms.

The shipped arm64 library grows from 935,224 to 1,084,552 bytes (+149,328 bytes,
about 146 KiB, uncompressed). Speed takes precedence over that size increase.

Validation: 144 Rust library tests passed; all nine selected Pixel 5a
instrumentation tests passed (eight GPU tests plus the geometry-neighbour
test), including CPU/GPU image comparison and requests through the tile
server's worker pool. Image comparison reported 0.00% of 8x8 blocks exceeding
the channel tolerance, worst block delta 18/255. This is not a pixel-exact
comparison between the two rasterizers. The example's non-empty Tokyo label
tiles also rendered correctly through the new direct-buffer path (1–84 placed
labels per observed tile). Test APK and example debug APK built.

The fixed-input concurrency test uses one provider and one GL thread, seven
batches per width. Direct-buffer results were:

| Request width | Median batch wall | Median tile latency | Throughput |
| ---: | ---: | ---: | ---: |
| 1 | 68.9 ms | 67.5 ms | 14.5 tiles/s |
| 2 | 147.8 ms | 142.6 ms | 13.5 tiles/s |
| 4 | 228.2 ms | 197.7 ms | 17.5 tiles/s |
| 8 | 435.4 ms | 309.2 ms | 18.4 tiles/s |

Compared with the staging-buffer version, width-1 wall time fell from 110.4 ms
and width-8 wall time from 545.4 ms. Compared with the first direct-vertex
buffer run, direct readback changed width-1 from 73.2 to 68.9 ms, width-4 from
242.1 to 228.2 ms and width-8 from 456.1 to 435.4 ms. Width 4 remains the
practical knee.

The final provider admits up to six simultaneous native GPU preparations. This
does not queue the usual six z11 viewport tiles, but bounds excess work before
the single GL thread. With width 8, its final fixed-input median was 435.4 ms
batch wall time, 309.2 ms tile latency and 18.4 tiles/s. A limit of four improved
the synthetic result slightly but delayed ordinary viewport tiles, so it was
rejected.

## Remaining work candidates

Build a repeatable viewport benchmark with fixed source bytes before
extrapolating to the user's ~190 ms figure. Label preparation before `render=`
(needed-glyph query and warming), source fetching, and rendered-cache writes are
not included in that field. Native preparation, GL queueing and label semaphore
waiting are included where applicable. Disk-cache hits do not measure rendering.

Further options depend on those measurements: index label collisions spatially
or move rotated SDF glyph drawing to the GPU. PBO readback needs pipelining
across tiles; immediately mapping the same PBO would still wait. Neither larger
change is included here.

## Tokyo viewport follow-up

The example's initial camera requested six z11 tiles (x1818–1819, y805–807)
plus a z7 fallback tile. The existing PNG cache was moved to sibling backup
directories inside the app cache for each experiment; glyphs and sprites were
kept warm on disk (122 ranges, 112 icons). Each row below is one cold-start
viewport, seven geometry requests, with live source network traffic. These
small, uncontrolled samples are observations, not stable speedup estimates.

| Configuration | Geometry render median | Geometry fetch median |
| --- | ---: | ---: |
| Speed build + reusable upload buffer, nine inputs | 202 ms | 2321 ms |
| Only central inputs for geometry | 219 ms | 487 ms |
| Central inputs + shared CPU semaphore (rejected) | 239 ms | 360 ms |
| Central inputs + direct native buffer | **154 ms** | 410 ms |
| Direct native buffer + direct readback | **136 ms** | 367 ms |

The `render` values include JNI, preparation, GL queueing and encoding. Fetch
times depend on the network and changed request scheduling, so the decrease
must not be attributed entirely to the code change. The shared semaphore
experiment introduced 118–143 ms preparation waits for two geometry requests
and a 105 ms wait for the z7 label request. It was removed. The final
independent six-slot GPU preparation bound described above showed only 1–5 ms
waits in observed viewport runs and does not serialize label work.

The retained change adds `labelsOnly` to JNI plan JSON. The geometry provider
replaces these entries with null **without removing their positional slots**,
so it fetches and packs one tile instead of nine for this style. Labels and
FULL requests still receive the complete plan. Missing `labelsOnly` defaults to
false for compatibility with older native libraries. This also avoids CPU
pattern fallback caused solely by a pattern in an unused neighbour.

`GeometryNeighbourTest` verifies exact PNG equality for geometry with/without
neighbours on both CPU and GPU, preserves full neighbour fetching by the label
route, checks overzoom (already parent-only), and tests a wrapped x=0 tile.
It uses no loaded glyphs to isolate geometry equality, so it does not claim a
new pixel comparison for label rendering. The actual Tokyo runs produced the
same label counts and PNG sizes across these experiments. GPU mode tests and
the new test passed on Pixel 5a. Final code also checks cancellation before
native preparation and again before GL submission.

The direct-label build was then observed twice with the same six z11 tiles.
Geometry medians were 171.5 ms and 264 ms; the second run had heavy parallel
tessellation contention. Label-draw medians were 20.7 ms and 19.1 ms, with
PNG-encode medians of 13.0 ms and 13.7 ms. These runs establish that non-empty
labels use the new path and preserve visible output, but do not establish a
label latency improvement over the earlier 15–29 ms observations.

The final direct-readback viewport produced six z11 geometry render times of
28, 75, 124, 148, 168 and 218 ms (136 ms median). Every GL log reported zero
readback-copy time. Its label-draw median was 12.7 ms and PNG-encode median was
11.0 ms.

The best cold-start viewport observation is 28% below the reported ~190 ms
median and 38% below the preceding central-input run. Because each viewport
row is a single run with live source traffic and different concurrent
scheduling, the controlled fixed-input test is the stronger evidence: 96.6 to
66.0 ms across the retained direct-buffer changes. Repeated viewport
batches with fixed source bytes would be needed for a statistically stable
end-to-end percentage.
