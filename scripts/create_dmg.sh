#!/bin/bash
set -e

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE_DIR="$DIR/release"
APP_NAME="CloudSync"
VERSION="1.0.0"
DMG_NAME="${APP_NAME}-${VERSION}.dmg"
DMG_PATH="${RELEASE_DIR}/${DMG_NAME}"
TMP_STAGE="${RELEASE_DIR}/dmg_staging"

echo "=========================================================="
echo " Packaging CloudSync for iMac (macOS Disk Image .dmg)"
echo "=========================================================="

# 1. Ensure the app bundle is freshly built
echo "Step 1: Building and bundling ${APP_NAME}.app..."
bash "$DIR/scripts/bundle_app.sh"

echo "Step 2: Preparing disk image staging layout..."
rm -rf "$TMP_STAGE"
mkdir -p "$TMP_STAGE"

# Copy CloudSync.app into staging
cp -R "${RELEASE_DIR}/${APP_NAME}.app" "$TMP_STAGE/"

# Create /Applications symlink for standard macOS drag-and-drop installation
ln -s /Applications "$TMP_STAGE/Applications"

# Set custom volume icon if AppIcon.icns exists
if [ -f "$DIR/assets/AppIcon.icns" ]; then
    cp "$DIR/assets/AppIcon.icns" "$TMP_STAGE/.VolumeIcon.icns"
fi

# 2. Build the compressed UDZO DMG using native hdiutil
echo "Step 3: Creating Apple Disk Image (.dmg)..."
rm -f "$DMG_PATH"

hdiutil create \
    -volname "${APP_NAME}" \
    -srcfolder "$TMP_STAGE" \
    -ov \
    -format UDZO \
    "$DMG_PATH"

# 3. Clean up staging folder
rm -rf "$TMP_STAGE"

# 4. Also refresh /Applications/CloudSync.app if installed
if [ -d "/Applications/CloudSync.app" ]; then
    echo "Updating /Applications/CloudSync.app with new build..."
    cp -R "${RELEASE_DIR}/${APP_NAME}.app" /Applications/
fi

echo ""
echo "=========================================================="
echo "✓ macOS installable image created successfully:"
echo "  $DMG_PATH"
echo "=========================================================="
