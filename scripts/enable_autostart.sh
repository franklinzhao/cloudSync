#!/bin/bash
# enable_autostart.sh — Add CloudSync to macOS Login Items
set -e

APP_PATH="/Applications/CloudSync.app"

if [ ! -d "$APP_PATH" ]; then
    echo "Warning: $APP_PATH was not found."
    echo "Copying CloudSync.app to /Applications/..."
    DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    if [ -d "$DIR/release/CloudSync.app" ]; then
        cp -R "$DIR/release/CloudSync.app" /Applications/
    elif [ -d "$DIR/CloudSync.app" ]; then
        cp -R "$DIR/CloudSync.app" /Applications/
    fi
fi

osascript -e 'tell application "System Events"
    if not (exists (login item "CloudSync")) then
        make login item at end with properties {path:"/Applications/CloudSync.app", hidden:false, name:"CloudSync"}
    end if
end tell'

echo "✓ CloudSync has been added to macOS Login Items."
echo "It will automatically start in your menu bar when you log into this Mac."
echo "You can check or modify this anytime in System Settings → General → Login Items."
