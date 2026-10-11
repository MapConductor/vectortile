#!/usr/bin/env bash
# Builds the style compiler for android-vectorstyle.
#
# Separate from build-android.sh because the audience is different: an app
# that uses MapLibre, Mapbox or MapTiler can adjust a style without ever
# rasterising one, and must not have to link the rasteriser to do it. This
# carries serde_json and the compiler; nothing else.
#
# Every ABI, unlike build-android.sh: the library is small enough that
# leaving one out buys nothing, and a missing slice means the module simply
# does not work on that device.
set -euo pipefail

ANDROID_API="${ANDROID_API:-28}"   # matches the SDK's minSdk

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"

if [[ ! -d "$sdk/ndk" ]]; then
    echo "No NDK under $sdk/ndk. Set ANDROID_HOME or install the NDK." >&2
    exit 1
fi
ndk="$sdk/ndk/$(ls "$sdk/ndk" | sort -V | tail -1)"
host="$(uname -s | tr '[:upper:]' '[:lower:]')-x86_64"
bin="$ndk/toolchains/llvm/prebuilt/$host/bin"

# ABI -> rust target. Every ABI Google Play still accepts, because the core
# ships to every consumer and a missing slice means no library at all on that
# device — the Kotlin side falls back, but silently and slowly.
abis=("arm64-v8a:aarch64-linux-android" \
      "armeabi-v7a:armv7-linux-androideabi" \
      "x86_64:x86_64-linux-android")

cd "$root"
for entry in "${abis[@]}"; do
    abi="${entry%%:*}"
    target="${entry##*:}"
    # armv7's clang triple differs from its rust triple.
    clang_triple="$target"
    [[ "$target" == "armv7-linux-androideabi" ]] && clang_triple="armv7a-linux-androideabi"
    clang="$bin/${clang_triple}${ANDROID_API}-clang"
    if [[ ! -x "$clang" ]]; then
        echo "skip $abi: missing $(basename "$clang")" >&2
        continue
    fi
    upper="$(echo "$target" | tr '[:lower:]-' '[:upper:]_')"
    export "CARGO_TARGET_${upper}_LINKER=$clang"
    export "CC_${target//-/_}=$clang"
    export "AR_${target//-/_}=$bin/llvm-ar"
    cargo build -p mvt-style-jni --target "$target" --release

    dest="$root/bindings/android/style/jniLibs/$abi"
    mkdir -p "$dest"
    cp "$root/target/$target/release/libmvt_style_jni.so" "$dest/"
    echo "-> $dest/libmvt_style_jni.so ($(du -h "$dest/libmvt_style_jni.so" | cut -f1))"
done
