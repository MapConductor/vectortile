#!/usr/bin/env bash
# Builds the C ABI static library for the iOS simulator (arm64).
#
# Only the simulator slice is built here; shipping to devices additionally needs
# aarch64-apple-ios, combined into an XCFramework.
set -euo pipefail

TARGET="${IOS_TARGET:-aarch64-apple-ios-sim}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$root"
cargo build -p mvt-render-ffi --target "$TARGET" --release

lib="$root/target/$TARGET/release/libmvt_render_ffi.a"
dest="$root/bindings/ios/lib"
mkdir -p "$dest"
cp "$lib" "$dest/"
echo "-> $dest/libmvt_render_ffi.a ($(du -h "$lib" | cut -f1))"

# Rebuild the XCFramework too. Copying only the .a once left a stale framework
# in place: the Swift package links the framework, so without this step an
# updated renderer silently never reaches iOS.
fw="$root/bindings/ios/MvtRender.xcframework"
rm -rf "$fw"
xcodebuild -create-xcframework \
    -library "$dest/libmvt_render_ffi.a" \
    -headers "$root/bindings/ios/include" \
    -output "$fw" > /dev/null
echo "-> $fw"
