#!/bin/sh
# Wrap the built cdylib in a macOS .clap bundle (target/solid-synth.clap).
set -eu
cd "$(dirname "$0")/../.."
cargo build -p solid-synth
BUNDLE=target/solid-synth.clap
mkdir -p "$BUNDLE/Contents/MacOS"
cp target/debug/libsolid_synth.dylib "$BUNDLE/Contents/MacOS/solid-synth"
cat > "$BUNDLE/Contents/Info.plist" <<'EOF'
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
    <string>0.1.0</string>
</dict>
</plist>
EOF
echo "built $BUNDLE"
