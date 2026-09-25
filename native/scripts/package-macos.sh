#!/bin/zsh
set -euo pipefail

SCRIPT_DIR="${0:A:h}"
NATIVE_DIR="${SCRIPT_DIR:h}"
cd "$NATIVE_DIR"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This packaging script must run on macOS." >&2
  exit 1
fi

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"

APP_NAME="SanctuaryPlayer"
BINARY_NAME="sanctuary-player"
BUNDLE_ID="com.xman2.sanctuaryplayer"
MIN_MACOS="12.0"
VERSION="${SANCTUARY_VERSION:-0.1.0}"
BUILD_NUMBER="${SANCTUARY_BUILD_NUMBER:-1}"

RELEASE_DIR="$NATIVE_DIR/target/release"
BUNDLE_DIR="$RELEASE_DIR/bundle/macos/$APP_NAME.app"
CONTENTS_DIR="$BUNDLE_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
ICONSET_DIR="$RELEASE_DIR/bundle/macos/$APP_NAME.iconset"
INSTALL_DIR="${SANCTUARY_INSTALL_DIR:-$HOME/Applications}"
INSTALL_LINK="$INSTALL_DIR/$APP_NAME.app"

echo "Building $APP_NAME release with $CARGO_BUILD_JOBS Cargo job(s)..."
cargo build --workspace --release -j "$CARGO_BUILD_JOBS"

rm -rf "$BUNDLE_DIR" "$ICONSET_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR" "$ICONSET_DIR"

cp "$RELEASE_DIR/$BINARY_NAME" "$MACOS_DIR/$BINARY_NAME"
chmod +x "$MACOS_DIR/$BINARY_NAME"

make_icon() {
  local source="$1"
  local size="$2"
  local destination="$3"
  if [[ "$source" == "$size" ]]; then
    cp "$NATIVE_DIR/assets/app-icon-$source.png" "$ICONSET_DIR/$destination"
  else
    sips -z "$size" "$size" "$NATIVE_DIR/assets/app-icon-$source.png" --out "$ICONSET_DIR/$destination" >/dev/null
  fi
}

make_icon 16 16 icon_16x16.png
make_icon 32 32 icon_16x16@2x.png
make_icon 32 32 icon_32x32.png
make_icon 64 64 icon_32x32@2x.png
make_icon 128 128 icon_128x128.png
make_icon 256 256 icon_128x128@2x.png
make_icon 256 256 icon_256x256.png
make_icon 256 512 icon_256x256@2x.png
make_icon 256 512 icon_512x512.png
make_icon 256 1024 icon_512x512@2x.png

iconutil -c icns "$ICONSET_DIR" -o "$RESOURCES_DIR/$APP_NAME.icns"

cat > "$CONTENTS_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleExecutable</key>
    <string>$BINARY_NAME</string>
    <key>CFBundleIconFile</key>
    <string>$APP_NAME.icns</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$BUILD_NUMBER</string>
    <key>LSMinimumSystemVersion</key>
    <string>$MIN_MACOS</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

plutil -lint "$CONTENTS_DIR/Info.plist"

echo "Applying ad-hoc signature..."
codesign --force --deep --sign - "$BUNDLE_DIR"
codesign --verify --deep --strict "$BUNDLE_DIR"

mkdir -p "$INSTALL_DIR"
ln -sfn "$BUNDLE_DIR" "$INSTALL_LINK"

rm -rf "$ICONSET_DIR"

echo
echo "Packaged: $BUNDLE_DIR"
echo "Installed symlink: $INSTALL_LINK -> $BUNDLE_DIR"
