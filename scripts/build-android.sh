#!/usr/bin/env bash
# Builds the JNI library for Android and drops it into the Gradle module's
# jniLibs. Locates the NDK from ANDROID_HOME rather than hard-coding a path, so
# the checked-in config does not depend on one machine's layout.
set -euo pipefail

ANDROID_API="${ANDROID_API:-28}"   # matches the SDK's minSdk
ABI="arm64-v8a"
TARGET="aarch64-linux-android"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"

if [[ ! -d "$sdk/ndk" ]]; then
    echo "No NDK under $sdk/ndk. Set ANDROID_HOME or install the NDK." >&2
    exit 1
fi
# Highest installed NDK.
ndk="$sdk/ndk/$(ls "$sdk/ndk" | sort -V | tail -1)"
host="$(uname -s | tr '[:upper:]' '[:lower:]')-x86_64"
bin="$ndk/toolchains/llvm/prebuilt/$host/bin"
clang="$bin/${TARGET}${ANDROID_API}-clang"

if [[ ! -x "$clang" ]]; then
    echo "Missing $clang" >&2
    exit 1
fi

echo "NDK:    $ndk"
echo "Linker: $(basename "$clang")"

export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$clang"
export CC_aarch64_linux_android="$clang"
export AR_aarch64_linux_android="$bin/llvm-ar"

cd "$root"
# `tessellate` is on: the SDK module offers a GPU rendering mode, which needs
# triangles and the fast PNG encoder. It costs a little binary size and nothing
# at runtime for callers that stay on the CPU path.
cargo build -p mvt-render-jni --features tessellate --target "$TARGET" --release

dest="$root/bindings/android/vectortile/src/main/jniLibs/$ABI"
mkdir -p "$dest"
cp "$root/target/$TARGET/release/libmvt_render_jni.so" "$dest/"
echo "-> $dest/libmvt_render_jni.so ($(du -h "$dest/libmvt_render_jni.so" | cut -f1))"
