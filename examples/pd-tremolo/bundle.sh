#!/bin/sh
# Wrap the built cdylib in a macOS .clap bundle (target/pd-tremolo.clap).
# The libpd feature is what makes the crate an actual plugin; without it the
# cdylib is empty by design (see Cargo.toml).
set -eu
cd "$(dirname "$0")/../.."
# Optimized build with BUILD_TYPE=release.
BUILD_TYPE=${BUILD_TYPE:-debug}
if [ "$BUILD_TYPE" = release ]; then
  cargo build -p pd-tremolo --features libpd --release
else
  cargo build -p pd-tremolo --features libpd
fi
# Crate versions are workspace-inherited; the root manifest is the source.
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)
BUNDLE=target/pd-tremolo.clap
mkdir -p "$BUNDLE/Contents/MacOS"
cp "target/$BUILD_TYPE/libpd_tremolo.dylib" "$BUNDLE/Contents/MacOS/pd-tremolo"
cat > "$BUNDLE/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>pd-tremolo</string>
    <key>CFBundleIdentifier</key>
    <string>org.skuiz.pd-tremolo</string>
    <key>CFBundleName</key>
    <string>Pd Tremolo</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
</dict>
</plist>
EOF
echo "built $BUNDLE"
