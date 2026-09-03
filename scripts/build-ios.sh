#!/usr/bin/env bash
# Builds the C ABI static library for the iOS simulator (arm64).
#
# Only the simulator slice is built here; shipping to devices additionally needs
# aarch64-apple-ios, combined into an XCFramework.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# Both slices: the simulator for development, the device slice for real
# hardware. An XCFramework carries them side by side and Xcode picks.
for target in aarch64-apple-ios-sim aarch64-apple-ios; do
    cargo build -p mvt-render-ffi --target "$target" --release --features tessellate
done

dest="$root/bindings/ios/lib"
mkdir -p "$dest/sim" "$dest/device"
cp "$root/target/aarch64-apple-ios-sim/release/libmvt_render_ffi.a" "$dest/sim/"
cp "$root/target/aarch64-apple-ios/release/libmvt_render_ffi.a" "$dest/device/"
echo "-> $dest/{sim,device}/libmvt_render_ffi.a"

# Rebuild the XCFramework too. Copying only the .a once left a stale framework
# in place: the Swift package links the framework, so without this step an
# updated renderer silently never reaches iOS.
fw="$root/bindings/ios/MvtRender.xcframework"
rm -rf "$fw"
xcodebuild -create-xcframework \
    -library "$dest/sim/libmvt_render_ffi.a" \
    -headers "$root/bindings/ios/include" \
    -library "$dest/device/libmvt_render_ffi.a" \
    -headers "$root/bindings/ios/include" \
    -output "$fw" > /dev/null
echo "-> $fw"
