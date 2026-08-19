#!/bin/sh
# Wrap the built cdylib in a macOS .clap bundle (target/shared-gain.clap).
set -eu
cd "$(dirname "$0")/../.."
cargo build -p shared-gain
# Crate versions are workspace-inherited; the root manifest is the source.
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)
BUNDLE=target/shared-gain.clap
mkdir -p "$BUNDLE/Contents/MacOS"
cp target/debug/libshared_gain.dylib "$BUNDLE/Contents/MacOS/shared-gain"
cat > "$BUNDLE/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>shared-gain</string>
    <key>CFBundleIdentifier</key>
    <string>org.skuiz.shared-gain</string>
    <key>CFBundleName</key>
    <string>Shared Gain</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
</dict>
</plist>
EOF
echo "built $BUNDLE"
