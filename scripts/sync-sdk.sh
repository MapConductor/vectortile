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
    local dest="$sdks/react-sdk/react-vectortile"
    [[ -d "$dest" ]] || { echo "skip web: $dest not found"; return; }
    mkdir -p "$dest/vendor"
    rm -rf "$dest/vendor/pkg" "$dest/vendor/dist"
    cp -R "$root/bindings/js/pkg" "$dest/vendor/pkg"
    cp -R "$root/bindings/js/dist" "$dest/vendor/dist"
    # wasm-pack writes a `.gitignore` containing `*` into its output directory.
    # Copied verbatim it makes the vendored wasm untrackable, which is how the
    # package ends up unbuildable from a clean checkout.
    rm -f "$dest/vendor/pkg/.gitignore"
    echo "web     -> $dest"
}

sync_android
sync_ios
sync_web
