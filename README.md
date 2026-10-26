# CloudSync ☁️

A lightweight, high-performance macOS cloud synchronization client written in Rust.

CloudSync was designed as a drop-in replacement for the official Microsoft OneDrive desktop client on older Macs (such as macOS Ventura 13.x on iMacs) where official desktop sync support was discontinued. It runs silently as a native macOS menu bar agent with zero background desktop clutter, an embedded local HTTPS configuration interface, and a live web terminal status console.

---

## ✨ Features

- 🔄 **Bidirectional Two-Way Sync** — Seamless two-way synchronization between your local file system and OneDrive cloud storage.
- 📁 **Multi-Folder Pairings & Shared Parent Folder** — Map multiple folders individually or define a single `local_parent_folder` with multiple subfolders (e.g., `Documents`, `Pictures`, `Projects`) to eliminate path redundancy.
- ⚡️ **Parallel Transfers & Resumable Uploads** — Multi-threaded concurrent downloads (up to 8 streams) and uploads (up to 4 streams), featuring chunked resumable upload sessions for files > 4 MB.
- 👁 **Real-Time Watching & Periodic Reconcile** — File system events trigger instant synchronization via `notify`, reinforced by periodic background reconciliation passes (configurable interval).
- 🛡 **Smart Exclusions & Online-Only Protection**:
  - Automatically excludes OS-level metadata at any folder depth (`.DS_Store`, `Thumbs.db`, `.git`, `.Trash`, AppleDouble `._*` resource forks).
  - Detects macOS OneDrive "online-only" (placeholder / dataless) files via filesystem attributes (`SF_DATALESS` / extended attributes) to prevent uploading empty stubs or corrupting cloud files.
- 🍎 **Menu Bar Agent (Hidden from Desktop & Dock)** — Packaged as a native macOS application bundle (`CloudSync.app`) with `LSUIElement` enabled. CloudSync operates purely from your menu bar with no persistent terminal window on the desktop and no Dock icon.
- 💻 **Live Web Terminal Console (`/status`)** — Click **Status** in the menu bar to instantly open a dark macOS-themed terminal console in a native webview window (WKWebView) streaming real-time colored logs, level filters (`INFO`, `WARN`, `ERROR`, `DEBUG`), text search, and sync controls. Opens in under 1 second — no external browser needed.
- ⚙️ **Web Settings UI (`/config`)** — Instant native webview window to view, add, or edit sync folder pairings, set transfer limits, and adjust intervals without manually editing configuration files. Powered by WKWebView for zero-latency startup.
- 🔒 **Security & macOS Keychain** — Initial login via OAuth2 PKCE in your default browser. Refresh tokens are stored securely in macOS Keychain (`keyring`) and renewed silently in the background. Local web services run over TLS with auto-generated certificates.

---

## 🏗 Architecture

```
                       ┌────────────────────────┐
                       │  macOS Menu Bar Tray   │  <-- Status, Sync Now, Config
                       └───────────┬────────────┘
                                   │
┌──────────────────────────────────┴──────────────────────────────────┐
│                           CloudSync Daemon                          │
│                                                                     │
│  ┌────────────────────────┐              ┌───────────────────────┐  │
│  │   Local HTTPS Server   │              │   File Watcher Task   │  │
│  │  (localhost:8766)      │              │  (notify / debounce)  │  │
│  │  ├─ /status (Terminal) │              └──────────┬────────────┘  │
│  │  └─ /config (Settings) │                         │               │
│  └────────────────────────┘                         ▼               │
│                                            ┌─────────────────────┐  │
│  ┌────────────────────────┐   Reconcile    │     Sync Engine     │  │
│  │   SQLite State DB      │ ◄────────────► │ (Parallel UL / DL)  │  │
│  │  (hashes, eTags, dirs) │                └──────────┬──────────┘  │
│  └────────────────────────┘                           │             │
│                                                       ▼             │
│  ┌────────────────────────┐              ┌───────────────────────┐  │
│  │  macOS Keychain Store  │ ◄──────────► │    OneDrive Client    │  │
│  │  (OAuth2 Tokens)       │  Silent Auth │  (Microsoft Graph)    │  │
│  └────────────────────────┘              └──────────┬────────────┘  │
└─────────────────────────────────────────────────────┼───────────────┘
                                                      │
                                                      ▼
                                            Microsoft Graph API
                                             (OneDrive Personal)
```

---

## 📋 Requirements

- **Operating System:** macOS 11.0 (Big Sur), macOS 12 (Monterey), macOS 13 (Ventura), or newer.
- **Rust Toolchain:** Stable Rust (edition 2024 or later) installed via [rustup.rs](https://rustup.rs).
- **Microsoft Account:** A personal Microsoft account (Outlook / Hotmail / Microsoft 365 Family).
- **Entra ID App Registration:** Free registration to obtain an OAuth Client ID (see Step 1 below).

---

## 🚀 Setup Guide

### Step 1 — Register an App in Microsoft Entra ID

> **Note:** Use [entra.microsoft.com](https://entra.microsoft.com), not the legacy Azure Portal. Sign in with your personal Microsoft account.

1. Navigate to **https://entra.microsoft.com** and sign in.
2. In the sidebar, go to **Applications** → **App registrations** → **New registration**.
3. Configure the following fields:
   | Setting | Value |
   |---|---|
   | **Name** | `CloudSync` |
   | **Supported account types** | **Personal Microsoft accounts only** |
   | **Platform configuration** | Click *Add a platform* → **Mobile and desktop applications** |
   | **Redirect URI** | `http://localhost:8765/callback` |
4. Click **Register**.
5. In the left menu, select **API permissions** → **Add a permission** → **Microsoft Graph** → **Delegated permissions**.
6. Check and add:
   - `Files.ReadWrite`
   - `offline_access`
7. Return to the **Overview** page and copy your **Application (client) ID**.

---

### Step 2 — Configuration

Create the CloudSync configuration directory and initial config file:

```bash
mkdir -p "$HOME/Library/Application Support/cloudsync"
cp config.toml.example "$HOME/Library/Application Support/cloudsync/config.toml"
```

Edit `~/Library/Application Support/cloudsync/config.toml`:

```toml
# ==============================================================================
# CloudSync Configuration
# ==============================================================================

# Common parent folder for local sync directories
local_parent_folder = "/Users/yourusername/Library/CloudStorage/OneDrive-Personal"

# Configure one or more folder pairs
[[sync_folders]]
subfolder = "SheridanFiles/Documents"   # Relative to local_parent_folder
remote_folder = "/Documents"           # Path in OneDrive

[[sync_folders]]
subfolder = "SheridanFiles/Pictures"
remote_folder = "/Pictures"

# Sync timing & concurrency
sync_interval_secs = 300               # Periodic reconcile every 5 minutes
max_parallel_downloads = 4             # Concurrent download streams (1-8)
max_parallel_uploads = 2               # Concurrent upload streams (1-4)
log_level = "info"                     # trace, debug, info, warn, error

# Custom exclusion patterns
exclude = [
    "*.tmp",
    "*.swp",
    "*.bak",
    "node_modules",
]

[onedrive]
client_id = "YOUR_MICROSOFT_CLIENT_ID_HERE"
```

> **Tip:** You can also configure all sync folder pairs and preferences visually through the web UI at `https://localhost:8766/config` once the application is running.

---

### Step 3 — Build & Bundle the macOS App

Use the included build script to compile the release binary and assemble the native `CloudSync.app` bundle:

```bash
./scripts/bundle_app.sh
```

The compiled application bundle will be created at:
```
release/CloudSync.app
```

---

### Step 4 — Run CloudSync

#### Running in the Background (Recommended):
Run the launcher script to start CloudSync silently in the background menu bar:
```bash
./run-hidden.sh
```

*(You can also double-click `release/CloudSync.app` in Finder or copy it to `/Applications`.)*

#### Initial Authentication:
On the first run, CloudSync will automatically open your default browser to Microsoft's secure login page. Grant the requested permissions. CloudSync captures the authorization code, exchanges it for a refresh token, and safely saves it to your **macOS Keychain**. 

On all subsequent launches, CloudSync authenticates silently in the background without opening any browser windows.

---

## 🖥 Using CloudSync

### macOS Menu Bar Tray

Click the cloud icon in the macOS menu bar to access the control menu:

| Menu Item | Action |
|---|---|
| **Status** | Opens the native webview console showing the **Terminal Status** tab |
| **Sync Now** | Manually triggers an immediate two-way reconciliation pass |
| **Open Sync Folder** | Opens your primary local sync folder in macOS Finder |
| **Preferences…** | Opens the native webview window focused on the **Preferences** tab |
| **Reveal Config In Finder** | Opens the `~/Library/Application Support/cloudsync/` directory in Finder |
| **Quit CloudSync** | Gracefully shuts down the background daemon |

### Unified Native Webview Window

CloudSync uses **wry** (the same native web engine behind Tauri) to render both the Terminal Status and Preferences in a single, high-performance window powered by macOS **WKWebView**:
- **Segmented Tabs:** Seamlessly toggle between **Terminal Status** (live colored logs, search, level filters, log clear/copy) and **Preferences** (sync folders, concurrency, schedule, autostart) inside the same window.
- **Sub-second startup** — no waiting for an external browser to cold-launch.
- **Instant activation & focus** — brought directly to the foreground when clicked from the menu bar.
- **Zero memory overhead** — shares macOS's built-in WebKit engine.
- **Hide/Show** — closing the window hides it in memory; re-opening from the tray is instantaneous.

---

## 🔄 Launch at Login (Automatic Startup)

You can have CloudSync start automatically whenever you log into macOS:

### Method 1: Web Settings UI (Recommended)
1. Click the CloudSync icon in your menu bar.
2. Select **Preferences…** (or open `https://localhost:8766/config`).
3. Under **Sync Schedule & Logging**, toggle on **"🚀 Open at Login"**.

### Method 2: macOS System Settings
CloudSync registers directly with macOS's native Login Items:
* Open **System Settings** ➔ **General** ➔ **Login Items & Extensions**.
* You will see **CloudSync** listed under **"Open at Login"** where you can enable or disable it anytime.

### Method 3: Command Line Script
```bash
# Enable auto-start at login
bash scripts/enable_autostart.sh

# Disable auto-start at login
bash scripts/disable_autostart.sh
```

### Method 4: macOS LaunchAgent (Advanced / Daemon Mode)
If you prefer managing the process via `launchd`:
```bash
cat > ~/Library/LaunchAgents/com.franklinzhao.cloudsync.plist << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.franklinzhao.cloudsync</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/CloudSync.app/Contents/MacOS/cloudsync</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
EOF
launchctl load ~/Library/LaunchAgents/com.franklinzhao.cloudsync.plist
```

To stop or remove the autostart agent:
```bash
launchctl unload ~/Library/LaunchAgents/com.franklinzhao.cloudsync.plist
rm ~/Library/LaunchAgents/com.franklinzhao.cloudsync.plist
```

---

## 📁 Repository Structure

```
cloudSync/
├── Cargo.toml                  # Rust dependencies and package configuration
├── config.toml.example         # Template configuration file
├── run-hidden.sh               # Shell launcher for background menu bar execution
├── scripts/
│   ├── bundle_app.sh           # Builds release binary and creates release/CloudSync.app
│   └── generate_icns.py        # Generates multi-resolution AppIcon.icns from PNG
├── assets/                     # High-contrast menu bar icons & application assets
│   ├── icon_idle.png
│   ├── icon_syncing.png
│   ├── icon_error.png
│   └── AppIcon.icns
├── release/                    # Output directory for CloudSync.app bundle
│   └── CloudSync.app
└── src/
    ├── main.rs                 # Application entry point, event loop, and orchestration
    ├── config.rs               # Configuration parser, folder normalization, TOML writer
    ├── config_server.rs        # Local HTTPS server (/status console, /config web UI)
    ├── auth.rs                 # OAuth2 PKCE login server and macOS Keychain token storage
    ├── sync_engine.rs          # Two-way reconciliation, parallel queue, online-only filter
    ├── state_db.rs             # SQLite database tracking file hashes, timestamps, and eTags
    ├── watcher.rs              # Recursive file system watcher (notify debouncer)
    ├── tray.rs                 # macOS menu bar tray icon and context menus
    └── cloud/
        ├── mod.rs              # CloudProvider trait definition
        └── onedrive.rs         # Microsoft Graph API implementation (resumable uploads)
```

---

## ❓ Troubleshooting

| Issue | Cause & Solution |
|---|---|
| **Browser login shows "Tenant error"** | Ensure you selected **Personal Microsoft accounts only** when creating your Entra ID app registration. |
| **"No authorization code returned"** | Verify that the redirect URI is exactly `http://localhost:8765/callback` and registered under the **Mobile and desktop applications** platform. |
| **Online-only OneDrive files skipped** | CloudSync intentionally skips OneDrive placeholder (dataless) files to prevent uploading zero-byte stubs. Right-click the folder in OneDrive or Finder and choose *"Always Keep on This Device"* if you wish to sync them. |
| **Browser warns about self-signed HTTPS certificate** | This issue no longer applies — CloudSync now uses native webview windows that connect via plain HTTP to `127.0.0.1`. If you access the web UIs externally via browser, the HTTPS endpoint (`https://localhost:8766`) uses a locally generated certificate. In Safari or Chrome, click *"Advanced"* and proceed to `localhost`. |
| **Reset authentication / switch accounts** | Open macOS **Keychain Access**, search for `cloudsync`, delete the stored token entry, and restart CloudSync to re-authenticate. |

---

## 📄 License

This project is licensed under the [MIT License](LICENSE).
