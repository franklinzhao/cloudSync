#!/bin/bash
# disable_autostart.sh — Remove CloudSync from macOS Login Items
set -e

osascript -e 'tell application "System Events"
    if exists (login item "CloudSync") then
        delete login item "CloudSync"
    end if
end tell'

echo "✓ CloudSync has been removed from macOS Login Items."
