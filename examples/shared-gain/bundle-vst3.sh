#!/bin/sh
# Wrap the built cdylib in a macOS .vst3 bundle (target/shared-gain.vst3).
#
# Shipping a VST3 binary carries a Steinberg licensing obligation that CLAP
# does not: the VST3 format is licensed under GPLv3 or Steinberg's separate
# free-of-charge proprietary agreement. See crates/skuiz-vst3.
set -eu
cd "$(dirname "$0")/../.."
cargo build -p shared-gain --features vst3
BUNDLE=target/shared-gain.vst3
mkdir -p "$BUNDLE/Contents/MacOS"
cp target/debug/libshared_gain.dylib "$BUNDLE/Contents/MacOS/shared-gain"
cat > "$BUNDLE/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>shared-gain</string>
    <key>CFBundleIdentifier</key>
    <string>org.skuiz.shared-gain.vst3</string>
    <key>CFBundleName</key>
    <string>Shared Gain</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>CFBundleSignature</key>
    <string>????</string>
</dict>
</plist>
EOF
echo "built $BUNDLE"
