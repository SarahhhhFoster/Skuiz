#!/bin/sh
# Wrap the built cdylib in a macOS .clap bundle (target/ducking-compressor.clap).
set -eu
cd "$(dirname "$0")/../.."
# Optimized build with BUILD_TYPE=release.
BUILD_TYPE=${BUILD_TYPE:-debug}
if [ "$BUILD_TYPE" = release ]; then
  cargo build -p ducking-compressor --release
else
  cargo build -p ducking-compressor
fi
# Crate versions are workspace-inherited; the root manifest is the source.
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)
BUNDLE=target/ducking-compressor.clap
mkdir -p "$BUNDLE/Contents/MacOS"
cp "target/$BUILD_TYPE/libducking_compressor.dylib" "$BUNDLE/Contents/MacOS/ducking-compressor"
cat > "$BUNDLE/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>ducking-compressor</string>
    <key>CFBundleIdentifier</key>
    <string>org.skuiz.ducking-compressor</string>
    <key>CFBundleName</key>
    <string>Ducking Compressor</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
</dict>
</plist>
EOF
echo "built $BUNDLE"
