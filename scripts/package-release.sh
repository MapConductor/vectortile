#!/usr/bin/env bash
# Packs one platform's build products into the release assets.
#
#   scripts/package-release.sh {web|ios|android} <out-dir>
#
# Run after the matching build: `npm run build` in bindings/js and
# bindings/js-style for web, scripts/build-ios*.sh for ios,
# scripts/build-android*.sh for android. The asset names are what the SDKs
# fetch, so renaming one breaks every SDK that pins a release.
#
# Each asset carries exactly what scripts/sync-sdk.sh copies for that
# platform, so an SDK can take a release in place of a local build.
set -euo pipefail

usage="usage: package-release.sh web|ios|android <out-dir>"
platform="${1:?$usage}"
out="${2:?$usage}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

require() {
    for path in "$@"; do
        [[ -e "$root/$path" ]] || { echo "missing $path; build $platform first" >&2; exit 1; }
    done
}

zip_dir() {
    # Zips $2 (relative to $1) so that it is the archive's top entry.
    local parent="$1" entry="$2" asset="$3"
    (cd "$root/$parent" && zip -qry "$out/$asset" "$entry")
    echo "-> $asset"
}

web_package() {
    # The npm package as the SDK consumes it. wasm-pack's pkg/.gitignore
    # (`*`) is left out: copied into an SDK it hides the wasm from git.
    local dir="$1" asset="$2"
    shift 2
    require "$dir/pkg" "$dir/dist" "$dir/package.json"
    tar -czf "$out/$asset" -C "$root/$dir" --exclude pkg/.gitignore pkg dist package.json "$@"
    echo "-> $asset"
}

case "$platform" in
    web)
        web_package bindings/js vectortile-web.tgz README.md
        web_package bindings/js-style vectorstyle-web.tgz
        ;;
    ios)
        require bindings/ios/MvtRender.xcframework \
                bindings/ios-style/MvtStyle.xcframework \
                bindings/ios-core/TilePng.xcframework
        # SwiftPM's binaryTarget(url:checksum:) wants a zip whose top entry
        # is the .xcframework.
        zip_dir bindings/ios MvtRender.xcframework MvtRender.xcframework.zip
        zip_dir bindings/ios-style MvtStyle.xcframework MvtStyle.xcframework.zip
        zip_dir bindings/ios-core TilePng.xcframework TilePng.xcframework.zip
        # The Swift and C glue that has to match these binaries.
        tar -czf "$out/ios-sources.tgz" -C "$root" \
            bindings/ios/Sources/MapConductorVectorTile/VectorTileRenderer.swift \
            bindings/ios/Sources/MapConductorVectorTile/MetalTileRasterizer.swift \
            bindings/ios-style/include/mvt_style.h \
            bindings/ios-core/include/tile_png.h
        echo "-> ios-sources.tgz"
        ;;
    android)
        require bindings/android/vectortile/src/main/jniLibs \
                bindings/android/style/jniLibs \
                bindings/android/core/jniLibs
        zip_dir bindings/android/vectortile/src/main jniLibs android-vectortile-jniLibs.zip
        zip_dir bindings/android/style jniLibs android-vectorstyle-jniLibs.zip
        zip_dir bindings/android/core jniLibs android-core-jniLibs.zip
        # The Kotlin glue that has to match these binaries.
        tar -czf "$out/android-sources.tgz" -C "$root" \
            bindings/android/vectortile/src/main/java/com/mapconductor/vectortile/NativeRenderer.kt \
            bindings/android/vectortile/src/main/java/com/mapconductor/vectortile/VectorTileRenderer.kt \
            bindings/android/style/java/com/mapconductor/vectorstyle/NativeStyleCompiler.kt
        echo "-> android-sources.tgz"
        ;;
    *)
        echo "unknown platform: $platform" >&2
        exit 2
        ;;
esac

# Which toolchain built these, for the release notes.
{
    echo "$platform: $(rustc --version)"
    [[ "$platform" == web ]] && echo "$platform: $(wasm-pack --version)"
    true
} > "$out/toolchain-$platform.txt"
