#!/bin/bash
# run-hidden.sh — Run CloudSync in the background menu bar, hidden from the desktop
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Kill any existing running instance of cloudsync
pkill -f "cloudsync" 2>/dev/null || true
sleep 0.5

# If release/CloudSync.app exists, launch via open
if [ -d "$DIR/release/CloudSync.app" ]; then
    open "$DIR/release/CloudSync.app"
    echo "✓ CloudSync launched from release/CloudSync.app"
elif [ -f "$DIR/target/release/cloudsync" ]; then
    nohup "$DIR/target/release/cloudsync" > /dev/null 2>&1 &
    echo "✓ CloudSync launched in background (PID: $!)"
else
    echo "Release binary not found. Building and bundling..."
    "$DIR/scripts/bundle_app.sh"
    open "$DIR/release/CloudSync.app"
fi

echo "CloudSync is now running in your macOS menu bar tray (hidden from desktop)."
echo "Click the tray icon and select 'Status' to open the live terminal log console."
