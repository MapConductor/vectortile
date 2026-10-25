#!/usr/bin/env bash
# Builds the style compiler for ios-vectorstyle as an XCFramework.
#
# Separate from build-ios.sh because the audience is different: an app that
# uses MapLibre, Mapbox or MapTiler can adjust a style without ever
# rasterising one, and must not have to link the rasteriser to do it. This
# carries serde_json and the compiler; nothing else.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

for target in aarch64-apple-ios-sim aarch64-apple-ios; do
    cargo build -p mvt-style-ffi --target "$target" --release
done

dest="$root/bindings/ios-style/lib"
mkdir -p "$dest/sim" "$dest/device"
cp "$root/target/aarch64-apple-ios-sim/release/libmvt_style_ffi.a" "$dest/sim/"
cp "$root/target/aarch64-apple-ios/release/libmvt_style_ffi.a" "$dest/device/"

# Drop debug and local symbols; see build-ios-core-png.sh for why a static
# library is several times larger than the code anyone links.
for slice in sim device; do
    xcrun strip -S -x "$dest/$slice/libmvt_style_ffi.a"
done

# Rebuilt every time: copying only the .a once left a stale framework in
# place on the vector tile side, and the package links the framework — so an
# updated compiler would silently never reach iOS.
fw="$root/bindings/ios-style/MvtStyle.xcframework"
rm -rf "$fw"
# No -headers, for the same reason as the PNG encoder: Xcode copies every
# linked XCFramework's Headers into one include directory and two
# module.modulemaps collide. The header travels as a SwiftPM C target.
xcodebuild -create-xcframework \
    -library "$dest/sim/libmvt_style_ffi.a" \
    -library "$dest/device/libmvt_style_ffi.a" \
    -output "$fw" > /dev/null
echo "-> $fw ($(du -sh "$fw" | cut -f1))"
