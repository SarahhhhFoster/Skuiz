#!/bin/sh
# Wrap the built cdylib in a macOS .clap bundle (target/solid-synth.clap).
set -eu
cd "$(dirname "$0")/../.."
# Optimized build with BUILD_TYPE=release.
BUILD_TYPE=${BUILD_TYPE:-debug}
if [ "$BUILD_TYPE" = release ]; then
  cargo build -p solid-synth --release
else
  cargo build -p solid-synth
fi
# Crate versions are workspace-inherited; the root manifest is the source.
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)
BUNDLE=target/solid-synth.clap
mkdir -p "$BUNDLE/Contents/MacOS"
cp "target/$BUILD_TYPE/libsolid_synth.dylib" "$BUNDLE/Contents/MacOS/solid-synth"
cat > "$BUNDLE/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>solid-synth</string>
    <key>CFBundleIdentifier</key>
    <string>org.skuiz.solid-synth</string>
    <key>CFBundleName</key>
    <string>Solid Synth</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
</dict>
</plist>
EOF
echo "built $BUNDLE"
