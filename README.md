# CloudSync

A lightweight macOS desktop app (Rust) that syncs a local folder to OneDrive — a drop-in replacement for the official OneDrive client on older Macs (macOS Ventura 13.x).

## Why?

Microsoft dropped OneDrive desktop support for macOS Ventura 13.x on older iMacs. This app restores that sync capability with zero dependencies on Microsoft's app.

## Features (MVP)

- 🔄 **Two-way sync** — bidirectional sync between a local folder and OneDrive
- 👁 **Real-time watching** — syncs within seconds of a file change
- ⏱ **Periodic reconcile** — full pass every 3–5 minutes (configurable)
- 🔒 **OAuth2 PKCE** — secure login via system browser, browser opens **once only**
- 🔑 **macOS Keychain** — refresh token stored in the OS credential store, never in a plain file
- 🔁 **Silent refresh** — subsequent runs refresh the token silently in the background
- 🗄 **SQLite state** — local DB tracks hashes/eTags to avoid redundant transfers
- 📥 **Large file support** — resumable upload sessions for files > 4 MB
- 🍎 **Menu bar icon** — idle/syncing/error states with "Sync Now", "Open Folder", "Quit"

## Requirements

- macOS 13.x Ventura or later
- Rust stable toolchain (`rustup` — https://rustup.rs)
- A Microsoft Entra ID app registration (free — see Setup)

## Setup

### Step 1 — Register the App in Microsoft Entra ID

> **Use [entra.microsoft.com](https://entra.microsoft.com), NOT the old Azure Portal app registration page.**
> Sign in with your personal Microsoft account (Outlook / Hotmail / Live / Microsoft 365 Family).

1. Go to **https://entra.microsoft.com** → sign in with your Microsoft account
2. Left menu → **Applications** → **App registrations** → **New registration**
3. Fill in:
   | Field | Value |
   |-------|-------|
   | Name | `CloudSync` |
   | Supported account types | **Personal Microsoft accounts only** |
   | Platform type | **Mobile and desktop applications** |
   | Redirect URI | `http://localhost:8765/callback` |

4. Click **Register**
5. Left menu → **API permissions** → **Add a permission** → **Microsoft Graph** → **Delegated**
6. Add: `Files.ReadWrite` and `offline_access`
7. Back on the **Overview** page, copy the **Application (client) ID**

> **Why "Mobile and desktop applications"?**
> This platform type allows any `localhost` port for the redirect URI, which is required for native desktop apps. "Web" requires an exact URI match and is only for server-hosted apps.

---

### Step 2 — Create the Config File

```bash
mkdir -p "$HOME/Library/Application Support/cloudsync"
cp config.toml.example "$HOME/Library/Application Support/cloudsync/config.toml"
```

Edit `~/Library/Application Support/cloudsync/config.toml` and set:

```toml
local_folder = "/path/to/your/local/folder"   # folder to sync
remote_folder = "/testCloudSync"               # folder name on OneDrive

[onedrive]
client_id = "YOUR-CLIENT-ID-HERE"             # from Step 1
```

---

### Step 3 — Build & Run

```bash
cargo build --release
./target/release/cloudsync
```

**First run:** your browser opens to the Microsoft login page. Sign in and approve the permissions. The app captures the token and saves the **refresh token to your macOS Keychain**.

**Every subsequent run:** the app silently refreshes the token from the Keychain — no browser, no interaction needed.

---

## How Authentication Works

```
App start
  │
  ├─ Refresh token in macOS Keychain?
  │      │
  │    YES → POST silent refresh (~100 ms network call)
  │             │
  │           Success → ✅ Ready (no browser opened)
  │             │
  │           Failed (expired / revoked)
  │             │
  └─── NO / Failed ──→ Open browser once → user logs in
                              │
                        Save refresh token to Keychain
                              │
                        ✅ Ready
```

**Token lifetime notes:**
- Access tokens expire after ~1 hour — the app refreshes automatically before each sync pass
- Refresh tokens for personal Microsoft accounts last ~90 days of inactivity
- Each use renews the refresh token (rotation) — a regularly running sync daemon stays signed in indefinitely
- If you change your Microsoft password or revoke the app, one new browser login is required

**To sign out** (removes the Keychain entry):
```bash
# Currently via the Keychain Access app:
# Open Keychain Access → search "cloudsync" → delete the entry
```

---

### Optional: Launch at Login

```bash
# Copy the built binary to /usr/local/bin
sudo cp target/release/cloudsync /usr/local/bin/

# Create a LaunchAgent plist
cat > ~/Library/LaunchAgents/com.cloudsync.plist << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.cloudsync</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/cloudsync</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
EOF
launchctl load ~/Library/LaunchAgents/com.cloudsync.plist
```

---

## Project Structure

```
src/
├── main.rs          # Entry point + main event loop
├── config.rs        # Config loading/saving (TOML)
├── auth.rs          # OAuth2 PKCE login + silent Keychain refresh
├── watcher.rs       # FS event watcher (notify crate)
├── sync_engine.rs   # Core two-way sync logic
├── state_db.rs      # SQLite sync state (hashes, eTags)
├── tray.rs          # macOS menu bar icon + menu
└── cloud/
    ├── mod.rs       # CloudProvider trait
    └── onedrive.rs  # Microsoft Graph API implementation
```

## Troubleshooting

| Problem | Fix |
|---------|-----|
| Browser login fails with "tenant" error | Make sure you selected **Personal Microsoft accounts only** when registering the app |
| "No code returned" after login | Check the redirect URI is exactly `http://localhost:8765/callback` and the platform is **Mobile and desktop applications** |
| Token expired after long break | The app will automatically open the browser for a one-time re-login |
| Want to sign out / switch accounts | Open **Keychain Access**, search for `cloudsync`, delete the entry, then restart the app |

## License

MIT
