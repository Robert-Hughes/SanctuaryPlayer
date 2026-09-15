#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
sdk=${ANDROID_HOME:-"$HOME/Android/Sdk"}
ndk=${ANDROID_NDK_ROOT:-"$sdk/ndk/30.0.16248370"}
build_tools_version=36.0.0
native_build_tools="$sdk/build-tools/$build_tools_version"
ndk_strip="$ndk/toolchains/llvm/prebuilt/freebsd-x86_64/bin/llvm-strip"
gradle_wrapper="$repo_root/android-gradle/gradlew"

cargo_target="$repo_root/target/android-cargo"
game_build_tools_url=https://dl.google.com/android/repository/build-tools_r36_linux.zip
game_build_tools_sha256=5d9ac77fb6ff43d9da518a337b4fcf8f9097113df531d99ccefe80ef7ce8250b
game_sdk="$repo_root/target/android-game-sdk"
game_build_tools_archive="$repo_root/target/android-game-build-tools.zip"
game_build_tools_stamp="$game_sdk/.build-tools-sha256"

if [ ! -x "$gradle_wrapper" ]; then
    echo "Gradle wrapper not executable: $gradle_wrapper" >&2
    exit 1
fi
if [ ! -x "$ndk_strip" ]; then
    echo "Native Android strip tool not executable: $ndk_strip" >&2
    exit 1
fi
for tool in aapt2 zipalign apksigner; do
    if [ ! -x "$native_build_tools/$tool" ]; then
        echo "Missing native Android build tool: $native_build_tools/$tool" >&2
        exit 1
    fi
done
for tool in fetch sha256 unzip; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "Required Android build tool not found in PATH: $tool" >&2
        exit 1
    fi
done

cd "$repo_root"

# cargo-apk2 assumes Cargo intermediates live below target/<triple>/<profile>.
# Some Cargo configurations redirect build-dir away from target/<triple>/<profile>.
# Override that setting only for this invocation and keep Android intermediates isolated.
mkdir -p "$cargo_target"
ANDROID_HOME="$sdk" \
ANDROID_SDK_ROOT="$sdk" \
ANDROID_NDK_ROOT="$ndk" \
JAVA_SOURCE_VERSION="${JAVA_SOURCE_VERSION:-17}" \
JAVA_TARGET_VERSION="${JAVA_TARGET_VERSION:-17}" \
CARGO_BUILD_BUILD_DIR="$cargo_target" \
    cargo apk2 build \
    -p sanctuary-player-android \
    --lib \
    --target aarch64-linux-android \
    --target-dir "$cargo_target"

cargo_apk="$cargo_target/debug/apk/sanctuary_player_android.apk"
native_library="$cargo_target/aarch64-linux-android/debug/libsanctuary_player_android.so"
if [ ! -r "$cargo_apk" ] || [ ! -r "$native_library" ]; then
    echo "Native Android build did not produce the expected outputs" >&2
    exit 1
fi

package_line=$("$native_build_tools/aapt2" dump badging "$cargo_apk" | sed -n '1p')
version_code=$(printf '%s\n' "$package_line" | sed -n "s/.*versionCode='\([^']*\)'.*/\1/p")
version_name=$(printf '%s\n' "$package_line" | sed -n "s/.*versionName='\([^']*\)'.*/\1/p")
if [ -z "$version_code" ] || [ -z "$version_name" ]; then
    echo "Could not derive Android version from cargo-apk2 output" >&2
    exit 1
fi

# AGP expects the complete official Build Tools layout. The installed SDK uses
# native FreeBSD replacements, so keep Google's package only under ignored
# target/ for AGP metadata/Java tooling and override AAPT2 with the native tool.
if [ ! -r "$game_build_tools_stamp" ] ||
   [ "$(cat "$game_build_tools_stamp" 2>/dev/null || true)" != "$game_build_tools_sha256" ]; then
    rm -rf "$game_sdk"
    mkdir -p "$game_sdk/build-tools"

    archive_ok=false
    if [ -r "$game_build_tools_archive" ] &&
       [ "$(sha256 -q "$game_build_tools_archive")" = "$game_build_tools_sha256" ]; then
        archive_ok=true
    fi
    if [ "$archive_ok" != true ]; then
        rm -f "$game_build_tools_archive"
        fetch -qo "$game_build_tools_archive" "$game_build_tools_url"
    fi
    actual_sha256=$(sha256 -q "$game_build_tools_archive")
    if [ "$actual_sha256" != "$game_build_tools_sha256" ]; then
        echo "Android Build Tools archive checksum mismatch" >&2
        exit 1
    fi

    unzip -q "$game_build_tools_archive" -d "$game_sdk/build-tools"
    mv "$game_sdk/build-tools/android-16" "$game_sdk/build-tools/$build_tools_version"
    ln -s "$sdk/platforms" "$game_sdk/platforms"
    printf '%s\n' "$game_build_tools_sha256" > "$game_build_tools_stamp"
elif [ ! -e "$game_sdk/platforms" ]; then
    ln -s "$sdk/platforms" "$game_sdk/platforms"
fi

jni_dir="$repo_root/target/android-jniLibs/arm64-v8a"
mkdir -p "$jni_dir"
"$ndk_strip" --strip-debug \
    -o "$jni_dir/libsanctuary_player_android.so" \
    "$native_library"

rm -rf "$repo_root/android-gradle/app/build"
ANDROID_HOME="$game_sdk" \
ANDROID_SDK_ROOT="$game_sdk" \
"$gradle_wrapper" --no-daemon -p "$repo_root/android-gradle" \
    -Pandroid.aapt2FromMavenOverride="$native_build_tools/aapt2" \
    -PsanctuaryVersionName="$version_name" \
    -PsanctuaryVersionCode="$version_code" \
    :app:assembleDebug

unsigned_apk="$repo_root/android-gradle/app/build/outputs/apk/debug/app-debug-unsigned.apk"
if [ ! -r "$unsigned_apk" ]; then
    echo "Gradle did not produce the expected unsigned debug APK" >&2
    exit 1
fi

output_dir="$repo_root/target/debug/apk"
aligned_apk="$repo_root/target/android-sanctuary-aligned.apk"
signed_apk="$repo_root/target/android-sanctuary-signed.apk"
final_apk="$output_dir/sanctuary_player_android.apk"
mkdir -p "$output_dir"
rm -f "$aligned_apk" "$signed_apk" "$final_apk" "$final_apk.idsig"

"$native_build_tools/zipalign" -f 4 "$unsigned_apk" "$aligned_apk"
"$native_build_tools/apksigner" sign \
    --ks "$HOME/.android/debug.keystore" \
    --ks-pass pass:android \
    --out "$signed_apk" \
    "$aligned_apk"
"$native_build_tools/apksigner" verify --verbose --print-certs "$signed_apk"
mv "$signed_apk" "$final_apk"
rm -f "$aligned_apk"

echo "Sanctuary Player debug APK: $final_apk"
