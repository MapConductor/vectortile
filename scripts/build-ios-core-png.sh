#!/usr/bin/env bash
# Builds the PNG encoder for ios-sdk-core as an XCFramework.
#
# Separate from build-ios.sh because the audience is different: this lands in
# the SDK core, which every consumer app links, so it carries only the encoder —
# not the vector tile renderer's rasteriser and tessellator.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

for target in aarch64-apple-ios-sim aarch64-apple-ios; do
    cargo build -p tile-png-ffi --target "$target" --release
done

dest="$root/bindings/ios-core/lib"
mkdir -p "$dest/sim" "$dest/device"
cp "$root/target/aarch64-apple-ios-sim/release/libtile_png_ffi.a" "$dest/sim/"
cp "$root/target/aarch64-apple-ios/release/libtile_png_ffi.a" "$dest/device/"

# Drop debug and local symbols. A static library carries every object Rust
# produced, including the parts of core and compiler_builtins the linker will
# never pull in, and the debug info for all of it: 5.6 MB becomes 2.1 MB. The
# global symbols the linker actually resolves against are kept.
for slice in sim device; do
    xcrun strip -S -x "$dest/$slice/libtile_png_ffi.a"
done

# Rebuild the framework every time. Copying only the .a once left a stale
# framework in place on the vector tile side, and the package links the
# framework — so an updated encoder would silently never reach iOS.
fw="$root/bindings/ios-core/TilePng.xcframework"
rm -rf "$fw"
# No -headers. Xcode copies every linked XCFramework's Headers into one
# include directory, so two of them shipping a module.modulemap collide with
# "Multiple commands produce .../include/module.modulemap" — which anything
# depending on both this and MvtRender hits, the vector tile module included.
# The header travels as a SwiftPM C target in ios-sdk-core instead.
xcodebuild -create-xcframework \
    -library "$dest/sim/libtile_png_ffi.a" \
    -library "$dest/device/libtile_png_ffi.a" \
    -output "$fw" > /dev/null
echo "-> $fw ($(du -sh "$fw" | cut -f1))"
