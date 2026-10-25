#!/usr/bin/env bash
# Copies the renderer and its native library into the SDK repos' modules.
#
# This repo owns the renderer; each SDK module owns only its integration layer
# (the TileProvider implementation and the platform's layer component). Keeping
# the copy scripted rather than manual makes the direction of the dependency
# obvious and the copies reproducible.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sdks="$(cd "$root/.." && pwd)"

sync_android() {
    local dest="$sdks/android-sdk/android-vectortile"
    [[ -d "$dest" ]] || { echo "skip android: $dest not found"; return; }
    local src="$root/bindings/android/vectortile/src/main"
    mkdir -p "$dest/src/main/java/com/mapconductor/vectortile" "$dest/src/main/jniLibs"
    cp "$src/java/com/mapconductor/vectortile/NativeRenderer.kt" \
       "$src/java/com/mapconductor/vectortile/VectorTileRenderer.kt" \
       "$dest/src/main/java/com/mapconductor/vectortile/"
    cp -R "$src/jniLibs/." "$dest/src/main/jniLibs/"
    echo "android -> $dest"
}

sync_android_core() {
    # The PNG encoder, which lives in the SDK core rather than the vector tile
    # module: MarkerTileRenderer needs it, and core cannot depend on a module
    # that depends on core.
    local dest="$sdks/android-sdk/android-sdk-core/src/main/jniLibs"
    [[ -d "$sdks/android-sdk/android-sdk-core" ]] || { echo "skip android core: not found"; return; }
    mkdir -p "$dest"
    cp -R "$root/bindings/android/core/jniLibs/." "$dest/"
    echo "android core -> $dest"
}

sync_ios_core() {
    # The PNG encoder, which lives in the SDK core rather than the vector tile
    # module: MarkerTileRenderer needs it, and core cannot depend on a module
    # that depends on core.
    local dest="$sdks/ios-sdk/ios-sdk-core"
    [[ -d "$dest" ]] || { echo "skip ios core: $dest not found"; return; }
    rm -rf "$dest/TilePng.xcframework"
    cp -R "$root/bindings/ios-core/TilePng.xcframework" "$dest/"
    mkdir -p "$dest/Sources/CTilePng/include"
    cp "$root/bindings/ios-core/include/tile_png.h" "$dest/Sources/CTilePng/include/"
    echo "ios core -> $dest"
}

sync_ios() {
    local dest="$sdks/ios-sdk/ios-vectortile"
    [[ -d "$dest" ]] || { echo "skip ios: $dest not found"; return; }
    mkdir -p "$dest/Sources/MapConductorVectorTile"
    for file in VectorTileRenderer.swift MetalTileRasterizer.swift; do
        cp "$root/bindings/ios/Sources/MapConductorVectorTile/$file" \
       "$dest/Sources/MapConductorVectorTile/"
    done
    rm -rf "$dest/MvtRender.xcframework"
    cp -R "$root/bindings/ios/MvtRender.xcframework" "$dest/"
    echo "ios     -> $dest"
}

sync_web() {
    # Into react-vectortile's vendor/, where scripts/fetch-vector-assets.mjs
    # otherwise unpacks the pinned release. The stamp says this is a local
    # build, so the next plain fetch puts the release back.
    web_vendor bindings/js "$sdks/react-sdk/react-vectortile/vendor/vectortile" README.md
}

web_vendor() {
    local src="$root/$1" dest="$2"
    shift 2
    local package="${dest%/vendor/*}"
    [[ -d "$package" ]] || { echo "skip web: $package not found"; return; }
    rm -rf "$dest"
    mkdir -p "$dest"
    cp -R "$src/pkg" "$src/dist" "$src/package.json" "$dest/"
    for extra in "$@"; do cp "$src/$extra" "$dest/" 2>/dev/null || true; done
    # wasm-pack writes a `.gitignore` containing `*` into its output directory.
    rm -f "$dest/pkg/.gitignore"
    echo "local $src" > "$dest/.release"
    echo "web     -> $dest"
}

sync_android_style() {
    # The style compiler, which is its own module: an app using MapLibre,
    # Mapbox or MapTiler adjusts a style without linking the rasteriser.
    local dest="$sdks/android-sdk/android-vectorstyle"
    [[ -d "$dest" ]] || { echo "skip android style: $dest not found"; return; }
    local src="$root/bindings/android/style"
    mkdir -p "$dest/src/main/java/com/mapconductor/vectorstyle" "$dest/src/main/jniLibs"
    cp "$src/java/com/mapconductor/vectorstyle/NativeStyleCompiler.kt" \
       "$dest/src/main/java/com/mapconductor/vectorstyle/"
    cp -R "$src/jniLibs/." "$dest/src/main/jniLibs/"
    echo "android style -> $dest"
}

sync_ios_style() {
    local dest="$sdks/ios-sdk/ios-vectorstyle"
    [[ -d "$dest" ]] || { echo "skip ios style: $dest not found"; return; }
    rm -rf "$dest/MvtStyle.xcframework"
    cp -R "$root/bindings/ios-style/MvtStyle.xcframework" "$dest/"
    mkdir -p "$dest/Sources/CMvtStyle/include"
    cp "$root/bindings/ios-style/include/mvt_style.h" "$dest/Sources/CMvtStyle/include/"
    echo "ios style -> $dest"
}

sync_web_style() {
    web_vendor bindings/js-style "$sdks/react-sdk/react-vectorstyle/vendor/vectorstyle"
}


sync_android
sync_android_core
sync_android_style
sync_ios_core
sync_ios
sync_ios_style
sync_web
sync_web_style
