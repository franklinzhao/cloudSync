#!/bin/bash
set -e

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE_DIR="$DIR/release"
APP_DIR="$RELEASE_DIR/CloudSync.app"
CONTENTS="$APP_DIR/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

echo "Building release binary..."
cd "$DIR"
cargo build --release

# Ensure AppIcon.icns is generated from assets/icon_syncing.png
if [ ! -f "$DIR/assets/AppIcon.icns" ]; then
    echo "Generating AppIcon.icns from assets/icon_syncing.png..."
    python3 "$DIR/scripts/generate_icns.py"
fi

# Stop any currently running instance before rebuilding bundle
pkill -f "cloudsync" 2>/dev/null || true
sleep 0.5

echo "Assembling $APP_DIR..."
rm -rf "$APP_DIR"
mkdir -p "$MACOS"
mkdir -p "$RESOURCES"

cp "$DIR/target/release/cloudsync" "$MACOS/cloudsync"
chmod +x "$MACOS/cloudsync"

# Copy AppIcon.icns to Contents/Resources
cp "$DIR/assets/AppIcon.icns" "$RESOURCES/AppIcon.icns"

# Copy other assets
if [ -d "$DIR/assets" ]; then
    cp -R "$DIR/assets" "$RESOURCES/"
fi

# Create Info.plist with LSUIElement=true (hidden from Dock and Desktop, runs as menu bar agent)
# and CFBundleIconFile pointing to AppIcon
cat << 'EOF' > "$CONTENTS/Info.plist"
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>cloudsync</string>
    <key>CFBundleIdentifier</key>
    <string>com.franklinzhao.cloudsync</string>
    <key>CFBundleName</key>
    <string>CloudSync</string>
    <key>CFBundleDisplayName</key>
    <string>CloudSync</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIconName</key>
    <string>AppIcon</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

# Touch app and notify LaunchServices
touch "$APP_DIR"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister"
if [ -x "$LSREGISTER" ]; then
    "$LSREGISTER" -f "$APP_DIR" 2>/dev/null || true
fi

# Set custom Finder icon via AppKit
swift -e "
import Cocoa
if let img = NSImage(contentsOfFile: \"$DIR/assets/icon_syncing.png\") {
    _ = NSWorkspace.shared.setIcon(img, forFile: \"$APP_DIR\", options: [])
}
" 2>/dev/null || true

echo "CloudSync.app assembled successfully at $APP_DIR with icon_syncing.png as AppIcon.icns"
