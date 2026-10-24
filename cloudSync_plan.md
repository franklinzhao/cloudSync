# CloudSync — Project Plan & Requirements

> **Context:** macOS Ventura 13.7 (older iMac) — Microsoft OneDrive desktop app dropped support.
> **Goal:** A lightweight, reliable Rust desktop app that syncs a local folder to OneDrive (and optionally other clouds) without needing the official client.

---

## 1. Problem Statement

Microsoft OneDrive's macOS desktop client has dropped support for macOS Ventura 13.x on older iMacs. The only fallback is the browser UI, which is cumbersome for day-to-day file workflows. A custom, minimal sync client that runs natively on the machine solves this permanently.

---

## 2. Goals

| Priority | Goal |
|----------|------|
| Must | Sync a local folder ↔ OneDrive reliably |
| Must | Run on macOS Ventura 13.x (Intel & Apple Silicon) |
| Must | Simple, low-maintenance — "set it and forget it" |
| Should | System tray icon with sync status |
| Should | Support multiple cloud targets (Google Drive, Dropbox) later |
| Nice | Conflict resolution UI |

---

## 3. Architecture Overview

```
┌─────────────────────────────────────────────────────┐
│                  CloudSync App                       │
│                                                     │
│  ┌─────────────┐    ┌──────────────┐               │
│  │  File       │    │  Sync        │               │
│  │  Watcher    │───▶│  Engine      │               │
│  │ (notify-rs) │    │  (core lib)  │               │
│  └─────────────┘    └──────┬───────┘               │
│                            │                        │
│                    ┌───────▼────────┐               │
│                    │  Cloud API     │               │
│                    │  Adapter Layer │               │
│                    │  (OneDrive,    │               │
│                    │   GDrive, etc) │               │
│                    └───────┬────────┘               │
│                            │ HTTPS                  │
│  ┌─────────────┐           │                        │
│  │  System     │           ▼                        │
│  │  Tray UI    │    ┌──────────────┐               │
│  │  (tray-icon)│    │  Cloud APIs  │               │
│  └─────────────┘    │  (REST/Graph)│               │
│                     └──────────────┘               │
│  ┌─────────────────────────────────────────────┐   │
│  │  Local SQLite State DB (sync state, hashes) │   │
│  └─────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────┘
```

### Key Layers

1. **File Watcher** — monitors the local sync folder for changes (create, modify, delete, rename).
2. **Sync Engine** — diffs local state vs. remote state, resolves what to upload/download/skip.
3. **Cloud API Adapter** — abstracts cloud-specific REST calls behind a common trait/interface.
4. **State DB** — SQLite database storing file hashes, last-sync timestamps, and conflict flags.
5. **System Tray UI** — minimal macOS menu bar icon showing sync status and quick settings.

---

## 4. Tech Stack

| Component | Technology | Rationale |
|-----------|-----------|-----------|
| Language | **Rust** (stable) | Performance, safety, single binary, no runtime |
| GUI / Tray | **tray-icon** crate + **tauri** (optional for settings window) | Native macOS integration |
| File watching | **notify** crate (v6) | Cross-platform, reliable FS events on macOS |
| HTTP client | **reqwest** (async) | Ergonomic, TLS support |
| Async runtime | **tokio** | Industry standard for async Rust |
| Auth (OAuth2) | **oauth2** crate + system browser | Standard OAuth 2.0 PKCE flow |
| Local DB | **rusqlite** (SQLite) | Zero-config, embedded, perfect for sync state |
| Config file | **serde** + TOML | Human-readable config |
| Logging | **tracing** + **tracing-subscriber** | Structured, filterable logs |
| Build/Package | **cargo** + **cargo-bundle** | Produces `.app` bundle for macOS |

---

## 5. Features

### MVP (Phase 1)
- [ ] **OAuth2 login** to OneDrive via system browser (no password stored in app)
- [ ] **One-way push** — upload new/modified local files to OneDrive
- [ ] **One-way pull** — download new/modified remote files to local folder
- [ ] **Two-way sync** — bidirectional sync with basic last-write-wins conflict strategy
- [ ] **System tray icon** — idle / syncing / error states
- [ ] **Sync on demand** — "Sync Now" menu item
- [ ] **Auto-sync on file change** — immediate sync trigger on FS event
- [ ] **Scheduled sync** — periodic sync every N minutes (configurable)
- [ ] **Single folder pair** — one local folder ↔ one OneDrive folder

### Phase 2
- [ ] **Multiple folder pairs** — sync multiple local↔cloud pairs
- [ ] **Exclude patterns** — `.gitignore`-style exclusion rules
- [ ] **Conflict UI** — show conflicts, let user choose which version to keep
- [ ] **Upload/download progress** in tray tooltip
- [ ] **Notifications** — macOS native notification on sync errors

### Phase 3 (Future)
- [ ] Google Drive support
- [ ] Dropbox support
- [ ] Bandwidth throttling

---

## 6. Workflow

```
App Start
    │
    ▼
Load config (~/Library/Application Support/cloudsync/config.toml)
    │
    ├── No credentials → Open browser for OAuth2 login → Store token
    │
    ▼
Initial Sync
    ├── Walk local folder → compute file hashes → compare with DB
    ├── Fetch remote file list from OneDrive API
    └── Reconcile differences → upload / download as needed
    │
    ▼
Watch Mode (continuous)
    ├── FS event detected → debounce (500ms) → queue file for sync
    └── Timer tick (e.g., every 5 min) → full reconcile pass
    │
    ▼
Sync Job (per file)
    ├── Compute local hash
    ├── Check remote eTag / lastModifiedDateTime
    ├── Decide: upload | download | skip | conflict
    └── Execute → update State DB → update tray icon
```

---

## 7. Non-Functional Requirements

| Requirement | Target |
|-------------|--------|
| macOS version | 13.x Ventura and above |
| CPU arch | x86_64 (Intel iMac) + arm64 (Apple Silicon) |
| Memory footprint | < 50 MB idle |
| Binary size | < 20 MB (single `.app` bundle) |
| Startup time | < 2 seconds |
| Sync latency | < 5 seconds after file save (FS-event driven) |
| Offline behavior | Queue changes locally, sync when reconnected |
| Security | OAuth2 tokens stored in macOS Keychain |
| Crash recovery | Resume interrupted uploads/downloads on restart |

---

## 8. File & Folder Layout (Project)

```
cloudsync/
├── Cargo.toml
├── config.toml.example
├── src/
│   ├── main.rs            # Entry point, tray setup
│   ├── config.rs          # Config loading/saving
│   ├── auth.rs            # OAuth2 flow
│   ├── watcher.rs         # FS event watching
│   ├── sync_engine.rs     # Core sync logic
│   ├── state_db.rs        # SQLite state management
│   └── cloud/
│       ├── mod.rs          # Cloud trait definition
│       └── onedrive.rs     # OneDrive API implementation
└── assets/
    ├── icon_idle.png
    ├── icon_syncing.png
    └── icon_error.png
```

---

## 9. Milestones

| Phase | Milestone | Est. Duration |
|-------|-----------|---------------|
| 1 | Project scaffold, config, logging | 1–2 days |
| 2 | OAuth2 OneDrive login + token refresh | 2–3 days |
| 3 | OneDrive API: list files, upload, download | 3–4 days |
| 4 | SQLite state DB + hash tracking | 1–2 days |
| 5 | File watcher + sync engine (two-way) | 3–5 days |
| 6 | System tray icon + basic UI | 2–3 days |
| 7 | Packaging as `.app` + testing on iMac | 2–3 days |
| **Total MVP** | | **~2–3 weeks** |

---

## 10. Open Questions for Review

> [!IMPORTANT]
> Please confirm these decisions before implementation begins.

1. **Sync direction** — Start with two-way sync, or one-way push first as a simpler MVP?
2. **Conflict strategy** — Last-write-wins auto-resolution, or pause and prompt user?
3. **Multiple folders** — MVP with a single folder pair, or multiple from day one?
4. **Google Drive / Dropbox** — Phase 3 only, or should the adapter layer be designed for it from day one?
5. **UI depth** — Just a tray icon (minimal), or a proper settings window via Tauri?
6. **Token storage** — macOS Keychain (secure, recommended) or encrypted local file?
