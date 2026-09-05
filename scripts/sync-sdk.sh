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
    # The framework-neutral package. React is one 120-line component on top of
    # this, and nothing here knows about it — the provider talks to a host tile
    # server through the structural TileServerLike interface, so the same build
    # serves React, Vue, or plain script tags.
    local dest="$sdks/react-sdk/vectortile"
    [[ -d "$dest" ]] || { echo "skip web: $dest not found"; return; }
    rm -rf "$dest/pkg" "$dest/dist"
    cp -R "$root/bindings/js/pkg" "$dest/pkg"
    cp -R "$root/bindings/js/dist" "$dest/dist"
    cp "$root/bindings/js/README.md" "$dest/README.md" 2>/dev/null || true
    # wasm-pack writes a `.gitignore` containing `*` into its output directory.
    # Copied verbatim it makes the vendored wasm untrackable, which is how the
    # package ends up unbuildable from a clean checkout.
    rm -f "$dest/pkg/.gitignore"
    echo "web     -> $dest"
}


sync_android
sync_android_core
sync_ios_core
sync_ios
sync_web
