#!/bin/sh
# Wrap the built cdylib in a macOS .clap bundle (target/trigger-note.clap).
set -eu
cd "$(dirname "$0")/../.."
cargo build -p trigger-note
BUNDLE=target/trigger-note.clap
mkdir -p "$BUNDLE/Contents/MacOS"
cp target/debug/libtrigger_note.dylib "$BUNDLE/Contents/MacOS/trigger-note"
cat > "$BUNDLE/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>trigger-note</string>
    <key>CFBundleIdentifier</key>
    <string>org.skuiz.trigger-note</string>
    <key>CFBundleName</key>
    <string>Trigger Note</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
</dict>
</plist>
EOF
echo "built $BUNDLE"
