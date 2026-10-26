//! Local Configuration Server & Web Interface
//! Serves the configuration UI on http://localhost:8766/config (and https://localhost:8767/config)
//! Powered by Axum + Tokio async runtime for non-blocking concurrent connections.

use anyhow::{anyhow, Result};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use hyper_util::service::TowerToHyperService;
use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};

use crate::config::Config;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub id: u64,
    pub text: String,
}

static PORT: AtomicU16 = AtomicU16::new(8766);
static LOG_BUFFER: OnceLock<Arc<Mutex<VecDeque<LogEntry>>>> = OnceLock::new();
static LOG_REVISION: AtomicU64 = AtomicU64::new(0);
static TOTAL_LOG_COUNT: AtomicU64 = AtomicU64::new(0);

/// Get or initialize the global in-memory log ring buffer
pub fn get_log_buffer() -> Arc<Mutex<VecDeque<LogEntry>>> {
    LOG_BUFFER
        .get_or_init(|| Arc::new(Mutex::new(VecDeque::with_capacity(3000))))
        .clone()
}

/// Writer for tracing that duplicates logs to stdout, cloudsync.log, and the in-memory ring buffer
#[derive(Clone)]
pub struct AppLogWriter {
    buffer: Arc<Mutex<VecDeque<LogEntry>>>,
    file: Option<Arc<Mutex<std::fs::File>>>,
}

impl AppLogWriter {
    pub fn new() -> Self {
        let buffer = get_log_buffer();
        let config_dir = Config::config_path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let _ = std::fs::create_dir_all(&config_dir);
        let log_path = config_dir.join("cloudsync.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .ok()
            .map(|f| Arc::new(Mutex::new(f)));

        Self { buffer, file }
    }
}

impl std::io::Write for AppLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stdout().write(buf);
        if let Some(f) = &self.file {
            if let Ok(mut file) = f.lock() {
                let _ = file.write_all(buf);
            }
        }
        if let Ok(s) = std::str::from_utf8(buf) {
            if let Ok(mut q) = self.buffer.lock() {
                for line in s.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let id = TOTAL_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                        if q.len() >= 3000 {
                            q.pop_front();
                        }
                        q.push_back(LogEntry {
                            id,
                            text: trimmed.to_string(),
                        });
                        LOG_REVISION.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stdout().flush();
        if let Some(f) = &self.file {
            if let Ok(mut file) = f.lock() {
                let _ = file.flush();
            }
        }
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for AppLogWriter {
    type Writer = AppLogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Get the active port for the config server
pub fn get_config_server_port() -> u16 {
    PORT.load(Ordering::Relaxed)
}

/// Commands sent from the config web server to the main loop
#[derive(Debug, Clone)]
pub enum ConfigServerEvent {
    ConfigUpdated(Config),
    SyncNow,
}

#[derive(Clone)]
struct AppState {
    config: Arc<RwLock<Config>>,
    event_tx: mpsc::Sender<ConfigServerEvent>,
}

/// Load existing TLS certificate/key from disk, or generate a new self-signed pair
fn load_or_generate_cert_bytes() -> Result<(Vec<u8>, Vec<u8>)> {
    let config_dir = Config::config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    std::fs::create_dir_all(&config_dir)?;
    let cert_path = config_dir.join("cert.pem");
    let key_path = config_dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        Ok((std::fs::read(&cert_path)?, std::fs::read(&key_path)?))
    } else {
        info!("Generating self-signed SSL certificate for https://localhost / https://127.0.0.1…");
        let subject_alt_names = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ];
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(subject_alt_names)
                .map_err(|e| anyhow!("Failed to generate SSL certificate: {}", e))?;

        let cert_pem = cert.pem();
        let key_pem = signing_key.serialize_pem();

        let cert_bytes = cert_pem.into_bytes();
        let key_bytes = key_pem.into_bytes();

        std::fs::write(&cert_path, &cert_bytes)?;
        std::fs::write(&key_path, &key_bytes)?;

        Ok((cert_bytes, key_bytes))
    }
}

/// Build Rustls ServerConfig from cert and key bytes
fn build_tls_acceptor() -> Result<TlsAcceptor> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (cert_bytes, key_bytes) = load_or_generate_cert_bytes()?;

    let mut cert_reader = &cert_bytes[..];
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| anyhow!("Failed to parse certificate PEM: {}", e))?;

    let mut key_reader = &key_bytes[..];
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| anyhow!("Failed to parse private key PEM: {}", e))?
        .ok_or_else(|| anyhow!("No private key found in key.pem"))?;

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow!("Failed to create TLS server config: {}", e))?;

    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

/// Start the universal dual-mode (HTTPS + HTTP) configuration server on port 8766.
/// Supports both IPv4 (127.0.0.1) and IPv6 (::1).
pub fn start_config_server(
    config: Arc<RwLock<Config>>,
    event_tx: mpsc::Sender<ConfigServerEvent>,
) -> Result<u16> {
    let port = 8766;
    PORT.store(port, Ordering::Relaxed);

    let tls_acceptor = match build_tls_acceptor() {
        Ok(acc) => Some(acc),
        Err(e) => {
            warn!("Failed to initialize TLS: {}. Serving plain HTTP only.", e);
            None
        }
    };

    let state = AppState { config, event_tx };
    let app = create_router(state);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime for config server");

        rt.block_on(async move {
            info!("Configuration interface active at: https://localhost:{}/config (and http://localhost:{}/config)", port, port);

            // Bind IPv4 and IPv6 loopback addresses
            let addrs = [
                SocketAddr::from(([127, 0, 0, 1], port)),
                SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
            ];

            let mut listeners = Vec::new();
            for addr in addrs {
                match TcpListener::bind(addr).await {
                    Ok(l) => listeners.push(l),
                    Err(e) => {
                        // IPv6 may not be enabled or bound already; log at debug
                        tracing::debug!("Could not bind listener to {}: {}", addr, e);
                    }
                }
            }

            if listeners.is_empty() {
                error!("Failed to bind any listener on port {}", port);
                return;
            }

            for listener in listeners {
                let app = app.clone();
                let tls_acceptor = tls_acceptor.clone();

                tokio::spawn(async move {
                    loop {
                        let (stream, _remote_addr) = match listener.accept().await {
                            Ok(res) => res,
                            Err(e) => {
                                tracing::debug!("Listener accept error: {}", e);
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                continue;
                            }
                        };

                        let app = app.clone();
                        let tls_acceptor = tls_acceptor.clone();

                        tokio::spawn(async move {
                            // Peek 1 byte to detect protocol:
                            // 0x16 = TLS Handshake ClientHello
                            // anything else = Plain HTTP (e.g. 'G' for GET, 'P' for POST)
                            let mut peek_buf = [0u8; 1];
                            match stream.peek(&mut peek_buf).await {
                                Ok(0) => return,
                                Err(_) => return,
                                Ok(_) => {}
                            }

                            let service = TowerToHyperService::new(app);
                            let auto = ConnBuilder::new(TokioExecutor::new());

                            if peek_buf[0] == 0x16 {
                                if let Some(acceptor) = tls_acceptor {
                                    match acceptor.accept(stream).await {
                                        Ok(tls_stream) => {
                                            let io = TokioIo::new(tls_stream);
                                            let _ = auto.serve_connection_with_upgrades(io, service).await;
                                        }
                                        Err(e) => {
                                            tracing::debug!("TLS handshake failed: {}", e);
                                        }
                                    }
                                }
                            } else {
                                // Plain HTTP connection
                                let io = TokioIo::new(stream);
                                let _ = auto.serve_connection_with_upgrades(io, service).await;
                            }
                        });
                    }
                });
            }

            // Keep the runtime alive indefinitely
            futures::future::pending::<()>().await;
        });
    });

    Ok(port)
}

fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handle_status))
        .route("/status", get(handle_status))
        .route("/config", get(handle_index))
        .route("/api/config", get(handle_get_config).post(handle_save_config))
        .route("/api/browse-folder", post(handle_browse_folder))
        .route("/api/sync-now", post(handle_sync_now))
        .route("/api/open-config-file", post(handle_open_config_file))
        .route("/api/open-config-dir", post(handle_open_config_dir))
        .route("/api/logs", get(handle_get_logs))
        .route("/api/logs/clear", post(handle_clear_logs))
        .route("/api/open-log-file", post(handle_open_log_file))
        .route("/api/autostart", get(handle_get_autostart).post(handle_set_autostart))
        .with_state(state)
}

async fn handle_status() -> Html<&'static str> {
    Html(UNIFIED_HTML)
}

#[derive(Debug, Deserialize)]
struct LogQueryParams {
    since: Option<u64>,
}

async fn handle_get_logs(
    Query(params): Query<LogQueryParams>,
) -> Json<serde_json::Value> {
    let buf = get_log_buffer();
    let rev = LOG_REVISION.load(Ordering::Relaxed);
    let total = TOTAL_LOG_COUNT.load(Ordering::Relaxed);

    let entries: Vec<LogEntry> = if let Ok(q) = buf.lock() {
        if let Some(since_id) = params.since {
            // Incremental fetch: only entries newer than since_id
            q.iter().filter(|e| e.id > since_id).cloned().collect()
        } else {
            // Initial fetch: return the latest up to 500 entries
            let skip = q.len().saturating_sub(500);
            q.iter().skip(skip).cloned().collect()
        }
    } else {
        Vec::new()
    };

    Json(serde_json::json!({
        "revision": rev,
        "total": total,
        "logs": entries
    }))
}

async fn handle_clear_logs() -> Json<serde_json::Value> {
    let buf = get_log_buffer();
    if let Ok(mut q) = buf.lock() {
        q.clear();
    }
    LOG_REVISION.fetch_add(1, Ordering::Relaxed);
    Json(serde_json::json!({ "status": "ok" }))
}

async fn handle_open_log_file() -> Json<serde_json::Value> {
    let config_dir = Config::config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let log_path = config_dir.join("cloudsync.log");
    open::that(&log_path).ok();
    Json(serde_json::json!({ "status": "ok" }))
}

async fn handle_index() -> Html<&'static str> {
    Html(UNIFIED_HTML)
}

async fn handle_get_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let current = state.config.read().await.clone();
    Json(serde_json::to_value(&current).unwrap_or_default())
}

async fn handle_save_config(
    State(state): State<AppState>,
    Json(mut new_cfg): Json<Config>,
) -> Response {
    if new_cfg.sync_interval_secs < 10 {
        new_cfg.sync_interval_secs = 10;
    }
    if new_cfg.max_parallel_downloads == 0 {
        new_cfg.max_parallel_downloads = 4;
    }
    if new_cfg.max_parallel_uploads == 0 {
        new_cfg.max_parallel_uploads = 2;
    }

    if let Err(e) = new_cfg.save() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "status": "error",
                "message": format!("Failed to save config.toml: {}", e)
            })),
        )
            .into_response();
    }

    {
        let mut w = state.config.write().await;
        *w = new_cfg.clone();
    }

    let _ = state.event_tx.send(ConfigServerEvent::ConfigUpdated(new_cfg)).await;

    Json(serde_json::json!({
        "status": "ok",
        "message": "Configuration saved and applied successfully!"
    }))
    .into_response()
}

async fn handle_browse_folder() -> Json<serde_json::Value> {
    let selected_path = pick_folder_native();
    Json(serde_json::json!({ "path": selected_path }))
}

async fn handle_sync_now(State(state): State<AppState>) -> Json<serde_json::Value> {
    let _ = state.event_tx.send(ConfigServerEvent::SyncNow).await;
    Json(serde_json::json!({
        "status": "ok",
        "message": "Sync pass initiated"
    }))
}

async fn handle_open_config_file() -> Json<serde_json::Value> {
    let path = Config::config_path();
    open::that(&path).ok();
    Json(serde_json::json!({ "status": "ok" }))
}

async fn handle_open_config_dir() -> Json<serde_json::Value> {
    if let Some(parent) = Config::config_path().parent() {
        open::that(parent).ok();
    }
    Json(serde_json::json!({ "status": "ok" }))
}

async fn handle_get_autostart() -> Json<serde_json::Value> {
    let enabled = check_autostart_native();
    Json(serde_json::json!({ "autostart": enabled }))
}

async fn handle_set_autostart(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let enable = payload.get("enable").and_then(|v| v.as_bool()).unwrap_or(false);
    let ok = set_autostart_native(enable);
    Json(serde_json::json!({ "status": if ok { "ok" } else { "error" }, "autostart": check_autostart_native() }))
}

fn check_autostart_native() -> bool {
    #[cfg(target_os = "macos")]
    {
        let script = r#"tell application "System Events" to get the name of every login item"#;
        if let Ok(out) = std::process::Command::new("osascript").arg("-e").arg(script).output() {
            let names = String::from_utf8_lossy(&out.stdout);
            return names.split(',').any(|n| n.trim().eq_ignore_ascii_case("CloudSync"));
        }
    }
    false
}

fn set_autostart_native(enable: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        let script = if enable {
            r#"tell application "System Events"
                if not (exists (login item "CloudSync")) then
                    make login item at end with properties {path:"/Applications/CloudSync.app", hidden:false, name:"CloudSync"}
                end if
            end tell"#
        } else {
            r#"tell application "System Events"
                if exists (login item "CloudSync") then
                    delete login item "CloudSync"
                end if
            end tell"#
        };
        if let Ok(status) = std::process::Command::new("osascript").arg("-e").arg(script).status() {
            return status.success();
        }
    }
    false
}

/// Pop up native macOS folder picker using osascript
fn pick_folder_native() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let script = r#"POSIX path of (choose folder with prompt "Select Local Sync Folder")"#;
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
            .ok()?;

        if output.status.success() {
            let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path_str.is_empty() {
                // Remove trailing slash if present
                let clean = path_str.trim_end_matches('/').to_string();
                return Some(clean);
            }
        }
    }
    None
}

// ── Embedded HTML / CSS / JS Configuration Interface ────────────────────────
const UNIFIED_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>CloudSync</title>
<style>
  :root {
    --bg-main: #0a0e17;
    --bg-card: #121824;
    --bg-term: #06090f;
    --card-border: #23304c;
    --border: rgba(255, 255, 255, 0.1);
    --border-accent: rgba(88, 166, 255, 0.35);
    --text-primary: #f1f5f9;
    --text-secondary: #94a3b8;
    --text-muted: #64748b;
    --accent: #3b82f6;
    --accent-hover: #2563eb;
    --accent-light: rgba(59, 130, 246, 0.15);
    --success: #10b981;
    --success-bg: rgba(16, 185, 129, 0.15);
    --warning: #f59e0b;
    --error: #ef4444;
    --error-bg: rgba(239, 68, 68, 0.15);
    --cyan: #38bdf8;
    --input-bg: #0d1424;
    --input-border: #273754;
    --font-mono: ui-monospace, "SF Mono", Menlo, Monaco, Consolas, "Liberation Mono", monospace;
    --radius: 10px;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  html, body {
    height: 100%;
    margin: 0;
    padding: 0;
    background: var(--bg-main);
    color: var(--text-primary);
    font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    -webkit-font-smoothing: antialiased;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  /* ── Header ────────────────────────────────────────────── */
  .app-header {
    height: 52px;
    background: rgba(18, 24, 36, 0.96);
    backdrop-filter: blur(14px);
    border-bottom: 1px solid var(--border);
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 18px;
    flex-shrink: 0;
    user-select: none;
    -webkit-user-select: none;
    z-index: 100;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .brand-icon {
    font-size: 20px;
    line-height: 1;
  }
  .brand-name {
    font-size: 15px;
    font-weight: 700;
    letter-spacing: -0.3px;
    color: #fff;
  }
  .status-pill {
    font-size: 11px;
    font-weight: 600;
    padding: 3px 9px;
    border-radius: 12px;
    display: inline-flex;
    align-items: center;
    gap: 5px;
  }
  .status-idle {
    background: var(--success-bg);
    color: var(--success);
    border: 1px solid rgba(16, 185, 129, 0.3);
  }
  .status-syncing {
    background: var(--accent-light);
    color: var(--accent);
    border: 1px solid var(--border-accent);
  }

  /* ── Segmented Control Tab Bar ─────────────────────────── */
  .segmented-tabs {
    display: flex;
    background: #06090f;
    padding: 3px;
    border-radius: 8px;
    border: 1px solid var(--border);
    gap: 2px;
  }
  .tab-btn {
    background: transparent;
    color: var(--text-secondary);
    border: none;
    padding: 6px 16px;
    border-radius: 6px;
    font-size: 13px;
    font-weight: 500;
    cursor: pointer;
    display: flex;
    align-items: center;
    gap: 7px;
    transition: all 0.15s ease;
  }
  .tab-btn:hover {
    color: var(--text-primary);
    background: rgba(255, 255, 255, 0.05);
  }
  .tab-btn.active {
    background: #1e293b;
    color: #fff;
    font-weight: 600;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.3);
  }

  .header-actions {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .btn-sync-quick {
    background: var(--accent);
    color: #fff;
    border: none;
    border-radius: 6px;
    padding: 6px 14px;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
    display: flex;
    align-items: center;
    gap: 6px;
    transition: background 0.15s ease;
  }
  .btn-sync-quick:hover {
    background: var(--accent-hover);
  }

  /* ── Content Area & Panels ─────────────────────────────── */
  .main-content {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    position: relative;
    overflow: hidden;
  }
  .tab-panel {
    display: none;
    flex: 1;
    min-height: 0;
    width: 100%;
    height: 100%;
  }
  .tab-panel.active {
    display: flex;
    flex-direction: column;
  }

  /* ── TAB 1: Terminal Status Styles ─────────────────────── */
  .terminal-toolbar {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    align-items: center;
    justify-content: space-between;
    padding: 9px 18px;
    background: #0d131f;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
    user-select: none;
    -webkit-user-select: none;
  }
  .filter-group {
    display: flex;
    gap: 5px;
    align-items: center;
  }
  .filter-chip {
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid var(--border);
    color: var(--text-secondary);
    border-radius: 6px;
    font-size: 11px;
    font-weight: 600;
    padding: 4px 9px;
    cursor: pointer;
    transition: all 0.15s;
  }
  .filter-chip:hover {
    background: rgba(255, 255, 255, 0.1);
    color: #fff;
  }
  .filter-chip.active {
    background: var(--accent);
    border-color: var(--accent);
    color: #fff;
  }
  .search-box {
    background: var(--input-bg);
    border: 1px solid var(--input-border);
    border-radius: 6px;
    padding: 5px 10px;
    color: var(--text-primary);
    font-size: 12px;
    width: 190px;
    outline: none;
  }
  .search-box:focus {
    border-color: var(--accent);
  }
  .terminal-actions {
    display: flex;
    gap: 7px;
    align-items: center;
  }
  .term-action-btn {
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid var(--border);
    color: var(--text-secondary);
    border-radius: 6px;
    font-size: 11px;
    font-weight: 500;
    padding: 5px 10px;
    cursor: pointer;
    transition: all 0.15s;
  }
  .term-action-btn:hover {
    background: rgba(255, 255, 255, 0.12);
    color: #fff;
  }
  .term-action-btn.active {
    background: rgba(16, 185, 129, 0.2);
    border-color: rgba(16, 185, 129, 0.4);
    color: var(--success);
  }
  .terminal-container {
    flex: 1;
    min-height: 0;
    background: var(--bg-term);
    overflow-y: auto;
    padding: 12px 18px;
    font-family: var(--font-mono);
    font-size: 12px;
    line-height: 1.6;
  }
  .log-row {
    display: flex;
    gap: 12px;
    padding: 2px 0;
    border-bottom: 1px solid rgba(255, 255, 255, 0.02);
  }
  .log-row:hover {
    background: rgba(255, 255, 255, 0.03);
  }
  .log-id {
    color: var(--text-muted);
    width: 42px;
    text-align: right;
    flex-shrink: 0;
    user-select: none;
  }
  .log-time {
    color: var(--text-muted);
    width: 75px;
    flex-shrink: 0;
  }
  .log-level {
    width: 50px;
    font-weight: 700;
    flex-shrink: 0;
  }
  .level-info { color: var(--accent); }
  .level-warn { color: var(--warning); }
  .level-error { color: var(--error); }
  .level-debug { color: #a78bfa; }
  .level-trace { color: var(--text-muted); }
  .log-msg {
    flex: 1;
    word-break: break-all;
    white-space: pre-wrap;
  }
  .highlight {
    background: rgba(234, 179, 8, 0.3);
    color: #fde047;
    border-radius: 2px;
    padding: 0 2px;
  }

  /* ── TAB 2: Preferences Styles ─────────────────────────── */
  #panel-config {
    overflow-y: auto;
    padding: 24px 20px 90px;
  }
  .config-container {
    max-width: 860px;
    margin: 0 auto;
    width: 100%;
  }
  .card {
    background: var(--bg-card);
    border: 1px solid var(--card-border);
    border-radius: var(--radius);
    padding: 20px 24px;
    margin-bottom: 20px;
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.2);
  }
  .card-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 16px;
    padding-bottom: 12px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
  }
  .card-title {
    font-size: 16px;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 8px;
    color: #fff;
  }
  .card-desc {
    font-size: 12px;
    color: var(--text-secondary);
    margin-top: 3px;
  }
  .form-row {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 16px;
    margin-bottom: 14px;
  }
  .form-group {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 14px;
  }
  .form-label {
    font-size: 12px;
    font-weight: 600;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }
  .form-input {
    background: var(--input-bg);
    border: 1px solid var(--input-border);
    border-radius: 6px;
    color: var(--text-primary);
    font-size: 13px;
    padding: 9px 12px;
    outline: none;
    font-family: inherit;
  }
  .form-input:focus {
    border-color: var(--accent);
  }
  .form-input[readonly] {
    opacity: 0.7;
    cursor: default;
  }
  .folder-item {
    background: rgba(0, 0, 0, 0.25);
    border: 1px solid rgba(255, 255, 255, 0.06);
    border-radius: 8px;
    padding: 16px;
    margin-bottom: 12px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .folder-item-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .folder-input-group {
    display: flex;
    gap: 8px;
  }
  .btn-browse {
    background: rgba(255, 255, 255, 0.08);
    border: 1px solid var(--border);
    color: var(--text-primary);
    border-radius: 6px;
    padding: 0 14px;
    font-size: 12px;
    font-weight: 500;
    cursor: pointer;
    white-space: nowrap;
    transition: all 0.15s;
  }
  .btn-browse:hover {
    background: rgba(255, 255, 255, 0.16);
  }
  .btn-remove-folder {
    background: transparent;
    border: none;
    color: var(--error);
    font-size: 12px;
    cursor: pointer;
    padding: 4px 8px;
  }
  .btn-remove-folder:hover {
    text-decoration: underline;
  }
  .btn-add-folder {
    background: rgba(59, 130, 246, 0.08);
    border: 1px dashed var(--border-accent);
    color: var(--accent);
    border-radius: 8px;
    padding: 11px;
    width: 100%;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 8px;
    margin-top: 6px;
    transition: all 0.15s;
  }
  .btn-add-folder:hover {
    background: rgba(59, 130, 246, 0.18);
  }
  .switch {
    position: relative;
    display: inline-block;
    width: 44px;
    height: 24px;
  }
  .switch input { opacity: 0; width: 0; height: 0; }
  .slider {
    position: absolute;
    cursor: pointer;
    top: 0; left: 0; right: 0; bottom: 0;
    background-color: #334155;
    transition: .2s;
    border-radius: 24px;
  }
  .slider:before {
    position: absolute;
    content: "";
    height: 18px;
    width: 18px;
    left: 3px;
    bottom: 3px;
    background-color: white;
    transition: .2s;
    border-radius: 50%;
  }
  input:checked + .slider { background-color: var(--accent); }
  input:checked + .slider:before { transform: translateX(20px); }

  .sticky-actions-bar {
    position: fixed;
    bottom: 0;
    left: 0;
    right: 0;
    height: 60px;
    background: rgba(18, 24, 36, 0.96);
    backdrop-filter: blur(14px);
    border-top: 1px solid var(--border);
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 12px;
    padding: 0 24px;
    z-index: 50;
  }
  .btn-save {
    background: var(--accent);
    color: #fff;
    border: none;
    border-radius: 6px;
    padding: 9px 22px;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s;
  }
  .btn-save:hover { background: var(--accent-hover); }
  .btn-reset {
    background: rgba(255, 255, 255, 0.08);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 9px 18px;
    font-size: 13px;
    font-weight: 500;
    cursor: pointer;
    transition: all 0.15s;
  }
  .btn-reset:hover {
    background: rgba(255, 255, 255, 0.15);
    color: #fff;
  }
  .toast {
    position: fixed;
    bottom: 72px;
    right: 24px;
    background: #1e293b;
    border: 1px solid var(--border-accent);
    color: #fff;
    border-radius: 8px;
    padding: 10px 18px;
    font-size: 13px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4);
    display: none;
    z-index: 100;
    animation: fadeIn 0.2s ease;
  }
  .toast.show { display: flex; align-items: center; gap: 8px; }
  @keyframes fadeIn {
    from { opacity: 0; transform: translateY(6px); }
    to { opacity: 1; transform: translateY(0); }
  }
</style>
</head>
<body>

<!-- Header -->
<header class="app-header">
  <div class="brand">
    <span class="brand-icon">☁️</span>
    <span class="brand-name">CloudSync</span>
    <span id="sync-status-indicator" class="status-pill status-idle">● Idle</span>
  </div>
  <div class="segmented-tabs" role="tablist">
    <button type="button" class="tab-btn active" id="tab-btn-status" onclick="switchTab('status')" role="tab">
      <span class="tab-icon">💻</span>
      <span>Terminal Status</span>
    </button>
    <button type="button" class="tab-btn" id="tab-btn-config" onclick="switchTab('config')" role="tab">
      <span class="tab-icon">⚙️</span>
      <span>Preferences</span>
    </button>
  </div>
  <div class="header-actions">
    <button type="button" class="btn-sync-quick" onclick="triggerManualSync()" id="quick-sync-btn">
      ↻ Sync Now
    </button>
  </div>
</header>

<!-- Main Container with 2 Tab Panels -->
<main class="main-content">

  <!-- ── PANEL 1: Terminal Status ──────────────────────────── -->
  <section id="panel-status" class="tab-panel active">
    <div class="terminal-toolbar">
      <div class="filter-group">
        <span style="font-size: 11px; color: var(--text-muted); font-weight: 600; text-transform: uppercase;">Filter:</span>
        <button type="button" class="filter-chip active" data-level="ALL" onclick="setLevelFilter('ALL')">ALL</button>
        <button type="button" class="filter-chip" data-level="INFO" onclick="setLevelFilter('INFO')">INFO</button>
        <button type="button" class="filter-chip" data-level="WARN" onclick="setLevelFilter('WARN')">WARN</button>
        <button type="button" class="filter-chip" data-level="ERROR" onclick="setLevelFilter('ERROR')">ERROR</button>
        <button type="button" class="filter-chip" data-level="DEBUG" onclick="setLevelFilter('DEBUG')">DEBUG</button>
        <button type="button" class="filter-chip" data-level="TRACE" onclick="setLevelFilter('TRACE')">TRACE</button>
      </div>
      <div style="display: flex; gap: 8px; align-items: center;">
        <input type="text" id="search-input" class="search-box" placeholder="Search logs..." oninput="onSearchInput()" />
        <div class="terminal-actions">
          <button type="button" id="autoscroll-btn" class="term-action-btn active" onclick="toggleAutoScroll()">Auto-scroll: ON</button>
          <button type="button" class="term-action-btn" onclick="copyLogs()">Copy</button>
          <button type="button" class="term-action-btn" onclick="clearLogs()">Clear</button>
          <button type="button" class="term-action-btn" onclick="openLogFile()">Log File</button>
        </div>
      </div>
    </div>
    <div class="terminal-container" id="term-container">
      <div id="term-body"></div>
    </div>
  </section>

  <!-- ── PANEL 2: Preferences ──────────────────────────────── -->
  <section id="panel-config" class="tab-panel">
    <div class="config-container">
      <!-- Card 1: Microsoft OneDrive Account -->
      <div class="card">
        <div class="card-header">
          <div>
            <div class="card-title"><span>☁️</span> Microsoft OneDrive Account</div>
            <div class="card-desc">OAuth2 PKCE authorization with silent background token refresh via macOS Keychain</div>
          </div>
          <span style="font-size: 12px; font-weight: 600; color: var(--success); background: var(--success-bg); padding: 3px 10px; border-radius: 12px; border: 1px solid rgba(16, 185, 129, 0.3);">● Keychain Auth Active</span>
        </div>
        <div class="form-row">
          <div class="form-group">
            <label class="form-label">Client ID (Entra Application ID)</label>
            <input type="text" id="cfg-client-id" class="form-input" spellcheck="false" />
          </div>
          <div class="form-group">
            <label class="form-label">Redirect URI</label>
            <input type="text" id="cfg-redirect-uri" class="form-input" readonly value="http://localhost:8765/callback" />
          </div>
        </div>
        <div style="font-size: 12px; color: var(--text-muted); display: flex; align-items: center; gap: 6px;">
          <span>🔒 Refresh tokens are encrypted and managed automatically in your macOS Keychain.</span>
        </div>
      </div>

      <!-- Card 2: Sync Folder Pairings -->
      <div class="card">
        <div class="card-header">
          <div>
            <div class="card-title"><span>📂</span> Sync Folders</div>
            <div class="card-desc">Configure pairs between local Mac directories and remote OneDrive folders</div>
          </div>
          <span id="folder-count-badge" style="font-size: 12px; color: var(--text-muted);">0 folders</span>
        </div>
        <div id="folders-container"></div>
        <button type="button" class="btn-add-folder" onclick="addFolder()">
          <span>+</span> Add Sync Folder
        </button>
      </div>

      <!-- Card 3: Sync Engine Settings -->
      <div class="card">
        <div class="card-header">
          <div>
            <div class="card-title"><span>⚡</span> Sync Engine Settings</div>
            <div class="card-desc">Performance, interval frequency, and log verbosity configuration</div>
          </div>
        </div>
        <div class="form-row">
          <div class="form-group">
            <label class="form-label">Full Sync Interval (Seconds)</label>
            <input type="number" id="cfg-interval" class="form-input" min="10" step="10" />
            <small style="color: var(--text-muted); font-size: 11px;">Periodic full reconciliation pass (real-time changes sync immediately)</small>
          </div>
          <div class="form-group">
            <label class="form-label">Log Level</label>
            <select id="cfg-log-level" class="form-input">
              <option value="trace">Trace (Most verbose)</option>
              <option value="debug">Debug (Detailed)</option>
              <option value="info">Info (Default / Recommended)</option>
              <option value="warn">Warn (Warnings and errors only)</option>
              <option value="error">Error (Errors only)</option>
            </select>
            <small style="color: var(--text-muted); font-size: 11px;">Controls level of messages emitted to console and cloudsync.log</small>
          </div>
        </div>
        <div class="form-row">
          <div class="form-group">
            <label class="form-label">Max Parallel Downloads</label>
            <input type="number" id="cfg-max-dl" class="form-input" min="1" max="16" />
            <small style="color: var(--text-muted); font-size: 11px;">Concurrent threads downloading files from OneDrive</small>
          </div>
          <div class="form-group">
            <label class="form-label">Max Parallel Uploads</label>
            <input type="number" id="cfg-max-ul" class="form-input" min="1" max="16" />
            <small style="color: var(--text-muted); font-size: 11px;">Concurrent threads uploading files to OneDrive</small>
          </div>
        </div>
      </div>

      <!-- Card 4: System Integration -->
      <div class="card">
        <div class="card-header">
          <div>
            <div class="card-title"><span>🖥️</span> System Integration</div>
            <div class="card-desc">Manage startup behavior and access local configuration files</div>
          </div>
        </div>
        <div style="display: flex; justify-content: space-between; align-items: center; padding: 6px 0 16px;">
          <div>
            <div style="font-weight: 600; font-size: 13px;">Launch CloudSync automatically on login</div>
            <div style="color: var(--text-muted); font-size: 12px;">Starts CloudSync silently in the menu bar when your Mac boots up</div>
          </div>
          <label class="switch">
            <input type="checkbox" id="cfg-autostart" onchange="toggleAutostart(this.checked)" />
            <span class="slider"></span>
          </label>
        </div>
        <div style="padding-top: 14px; border-top: 1px solid rgba(255, 255, 255, 0.05); display: flex; justify-content: space-between; align-items: center;">
          <div style="color: var(--text-muted); font-size: 12px;">
            Configuration folder: <code>~/Library/Application Support/cloudsync/</code>
          </div>
          <button type="button" class="btn-browse" onclick="revealConfigDir()">Reveal Config In Finder</button>
        </div>
      </div>
    </div>

    <!-- Sticky Bottom Bar -->
    <div class="sticky-actions-bar">
      <div id="save-status-msg" style="color: var(--text-muted); font-size: 13px; margin-right: auto;"></div>
      <button type="button" class="btn-reset" onclick="loadConfig()">Reset</button>
      <button type="button" class="btn-save" onclick="saveConfig()">Save Preferences</button>
    </div>
  </section>

</main>

<div id="toast" class="toast"></div>

<script>
// ── Global Tab Switching ────────────────────────────────────
function switchTab(tab) {
  const isStatus = (tab === 'status');
  document.getElementById('tab-btn-status').classList.toggle('active', isStatus);
  document.getElementById('tab-btn-config').classList.toggle('active', !isStatus);
  document.getElementById('panel-status').classList.toggle('active', isStatus);
  document.getElementById('panel-config').classList.toggle('active', !isStatus);

  try {
    history.replaceState(null, '', '#' + tab);
  } catch (e) {}

  if (isStatus && autoScroll) {
    const c = document.getElementById('term-container');
    c.scrollTop = c.scrollHeight;
  }
  if (!isStatus && !configLoaded) {
    loadConfig();
  }
}

// ── Status & Live Terminal Logic ────────────────────────────
let lastLogId = 0;
let autoScroll = true;
let currentLevelFilter = 'ALL';
let currentSearch = '';
let allLogs = [];

function toggleAutoScroll() {
  autoScroll = !autoScroll;
  const btn = document.getElementById('autoscroll-btn');
  btn.textContent = autoScroll ? 'Auto-scroll: ON' : 'Auto-scroll: OFF';
  btn.classList.toggle('active', autoScroll);
  if (autoScroll) {
    const c = document.getElementById('term-container');
    c.scrollTop = c.scrollHeight;
  }
}

function setLevelFilter(lvl) {
  currentLevelFilter = lvl;
  document.querySelectorAll('.filter-chip').forEach(c => {
    c.classList.toggle('active', c.getAttribute('data-level') === lvl);
  });
  renderFilteredLogs();
}

function onSearchInput() {
  currentSearch = document.getElementById('search-input').value.trim().toLowerCase();
  renderFilteredLogs();
}

function parseLogLine(text) {
  const m = text.match(/^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)?\s*(INFO|WARN|ERROR|DEBUG|TRACE)?\s*(.*)$/);
  if (m) {
    let time = m[1] ? m[1].substring(11, 19) : '';
    let level = m[2] || 'INFO';
    let msg = m[3] || text;
    return { time, level, msg };
  }
  return { time: '', level: 'INFO', msg: text };
}

function renderFilteredLogs() {
  const body = document.getElementById('term-body');
  body.innerHTML = '';
  const search = currentSearch;
  const filter = currentLevelFilter;

  for (let entry of allLogs) {
    const p = parseLogLine(entry.text);
    if (filter !== 'ALL' && p.level !== filter) continue;
    if (search && !entry.text.toLowerCase().includes(search)) continue;

    const row = document.createElement('div');
    row.className = 'log-row';

    let msgHtml = escapeHtml(p.msg);
    if (search) {
      const re = new RegExp('(' + escapeRegex(search) + ')', 'gi');
      msgHtml = msgHtml.replace(re, '<span class="highlight">$1</span>');
    }

    row.innerHTML = `
      <span class="log-id">${entry.id}</span>
      <span class="log-time">${p.time}</span>
      <span class="log-level level-${p.level.toLowerCase()}">${p.level}</span>
      <span class="log-msg">${msgHtml}</span>
    `;
    body.appendChild(row);
  }

  if (autoScroll) {
    const c = document.getElementById('term-container');
    c.scrollTop = c.scrollHeight;
  }
}

async function fetchLogs() {
  try {
    const res = await fetch(`/api/logs?since=${lastLogId}`);
    if (!res.ok) return;
    const data = await res.json();
    if (data.logs && data.logs.length > 0) {
      let isFirstFetch = (lastLogId === 0);
      for (let l of data.logs) {
        allLogs.push(l);
        if (l.id > lastLogId) lastLogId = l.id;
      }
      if (allLogs.length > 3000) {
        allLogs = allLogs.slice(allLogs.length - 3000);
      }
      renderFilteredLogs();

      // Check for sync indicator
      const latest = data.logs[data.logs.length - 1].text;
      const indicator = document.getElementById('sync-status-indicator');
      if (latest.includes('Starting sync') || latest.includes('Reconciliation') || latest.includes('Downloading')) {
        indicator.textContent = '● Syncing…';
        indicator.className = 'status-pill status-syncing';
      } else if (latest.includes('done') || latest.includes('Watching for changes') || latest.includes('Idle')) {
        indicator.textContent = '● Idle';
        indicator.className = 'status-pill status-idle';
      }
    }
  } catch (e) {}
}

async function clearLogs() {
  try {
    await fetch('/api/logs/clear', { method: 'POST' });
    allLogs = [];
    document.getElementById('term-body').innerHTML = '';
  } catch (e) {}
}

function copyLogs() {
  const text = allLogs.map(l => l.text).join('\n');
  navigator.clipboard.writeText(text).then(() => {
    showToast('✓ Logs copied to clipboard');
  });
}

async function openLogFile() {
  try {
    await fetch('/api/open-log-file', { method: 'POST' });
  } catch (e) {}
}

async function triggerManualSync() {
  const btn = document.getElementById('quick-sync-btn');
  btn.textContent = '↻ Syncing…';
  try {
    await fetch('/api/sync-now', { method: 'POST' });
    showToast('✓ Sync pass initiated');
  } catch (e) {}
  setTimeout(() => { btn.textContent = '↻ Sync Now'; }, 1500);
}

// ── Preferences / Config Logic ──────────────────────────────
let currentConfig = null;
let configLoaded = false;

async function loadConfig() {
  try {
    const res = await fetch('/api/config');
    if (!res.ok) return;
    currentConfig = await res.json();
    configLoaded = true;

    document.getElementById('cfg-client-id').value = currentConfig.onedrive?.client_id || '';
    document.getElementById('cfg-redirect-uri').value = currentConfig.onedrive?.redirect_uri || 'http://localhost:8765/callback';
    document.getElementById('cfg-interval').value = currentConfig.sync_interval_secs || 3600;
    document.getElementById('cfg-log-level').value = currentConfig.log_level || 'info';
    document.getElementById('cfg-max-dl').value = currentConfig.max_parallel_downloads || 4;
    document.getElementById('cfg-max-ul').value = currentConfig.max_parallel_uploads || 2;

    renderFolders();
    loadAutostart();
  } catch (e) {
    showToast('Failed to load configuration');
  }
}

function renderFolders() {
  const container = document.getElementById('folders-container');
  container.innerHTML = '';
  const folders = currentConfig.sync_folders || [];
  document.getElementById('folder-count-badge').textContent = `${folders.length} folder${folders.length === 1 ? '' : 's'}`;

  folders.forEach((f, idx) => {
    const div = document.createElement('div');
    div.className = 'folder-item';
    div.innerHTML = `
      <div class="folder-item-header">
        <div style="font-weight: 600; font-size: 13px; display: flex; align-items: center; gap: 6px;">
          <span>📁</span>
          <input type="text" value="${escapeHtml(f.subfolder || '')}"
            placeholder="Folder Name (e.g. Documents)"
            style="background: transparent; border: 1px solid transparent; color: #fff; font-weight: 600; font-size: 13px; border-radius: 4px; padding: 2px 6px;"
            onfocus="this.style.borderColor='var(--accent)'; this.style.background='var(--input-bg)'"
            onblur="this.style.borderColor='transparent'; this.style.background='transparent'"
            onchange="updateFolder(${idx}, 'subfolder', this.value)" />
        </div>
        <div style="display: flex; align-items: center; gap: 12px;">
          <label style="font-size: 12px; color: var(--text-secondary); display: flex; align-items: center; gap: 6px; cursor: pointer;">
            <input type="checkbox" ${f.enabled !== false ? 'checked' : ''} onchange="updateFolder(${idx}, 'enabled', this.checked)" />
            Enabled
          </label>
          <button type="button" class="btn-remove-folder" onclick="removeFolder(${idx})">Delete</button>
        </div>
      </div>
      <div class="form-group" style="margin-bottom: 6px;">
        <label class="form-label">Local Mac Folder</label>
        <div class="folder-input-group">
          <input type="text" class="form-input" style="flex: 1;" value="${escapeHtml(f.local_folder || '')}"
            placeholder="/Users/.../path" onchange="updateFolder(${idx}, 'local_folder', this.value)" />
          <button type="button" class="btn-browse" onclick="browseFolder(${idx})">Choose…</button>
        </div>
      </div>
      <div class="form-group" style="margin-bottom: 0;">
        <label class="form-label">Remote OneDrive Path</label>
        <input type="text" class="form-input" value="${escapeHtml(f.remote_folder || '')}"
          placeholder="/RemoteFolder" onchange="updateFolder(${idx}, 'remote_folder', this.value)" />
      </div>
    `;
    container.appendChild(div);
  });
}

function updateFolder(idx, field, value) {
  if (currentConfig && currentConfig.sync_folders && currentConfig.sync_folders[idx]) {
    currentConfig.sync_folders[idx][field] = value;
  }
}

function addFolder() {
  if (!currentConfig) return;
  if (!currentConfig.sync_folders) currentConfig.sync_folders = [];
  currentConfig.sync_folders.push({
    subfolder: "New Folder",
    local_folder: "",
    remote_folder: "/New Folder",
    enabled: true
  });
  renderFolders();
}

function removeFolder(idx) {
  if (!currentConfig || !currentConfig.sync_folders) return;
  currentConfig.sync_folders.splice(idx, 1);
  renderFolders();
}

async function browseFolder(idx) {
  try {
    const res = await fetch('/api/browse-folder', { method: 'POST' });
    const data = await res.json();
    if (data.path) {
      updateFolder(idx, 'local_folder', data.path);
      renderFolders();
    }
  } catch (e) {}
}

async function saveConfig() {
  if (!currentConfig) return;
  currentConfig.onedrive = currentConfig.onedrive || {};
  currentConfig.onedrive.client_id = document.getElementById('cfg-client-id').value.trim();
  currentConfig.onedrive.redirect_uri = document.getElementById('cfg-redirect-uri').value.trim();
  currentConfig.sync_interval_secs = parseInt(document.getElementById('cfg-interval').value, 10) || 3600;
  currentConfig.log_level = document.getElementById('cfg-log-level').value;
  currentConfig.max_parallel_downloads = parseInt(document.getElementById('cfg-max-dl').value, 10) || 4;
  currentConfig.max_parallel_uploads = parseInt(document.getElementById('cfg-max-ul').value, 10) || 2;

  const msgEl = document.getElementById('save-status-msg');
  msgEl.textContent = 'Saving…';
  try {
    const res = await fetch('/api/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(currentConfig)
    });
    if (res.ok) {
      msgEl.textContent = '✓ Saved successfully!';
      showToast('✓ Preferences saved & applied!');
      setTimeout(() => { msgEl.textContent = ''; }, 3000);
    } else {
      msgEl.textContent = '❌ Failed to save';
    }
  } catch (e) {
    msgEl.textContent = '❌ Network error';
  }
}

async function loadAutostart() {
  try {
    const res = await fetch('/api/autostart');
    if (res.ok) {
      const data = await res.json();
      document.getElementById('cfg-autostart').checked = !!data.autostart;
    }
  } catch (e) {}
}

async function toggleAutostart(enable) {
  try {
    await fetch('/api/autostart', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enable })
    });
    loadAutostart();
  } catch (e) {}
}

async function revealConfigDir() {
  try {
    await fetch('/api/open-config-dir', { method: 'POST' });
  } catch (e) {}
}

function showToast(msg) {
  const t = document.getElementById('toast');
  t.textContent = msg;
  t.classList.add('show');
  setTimeout(() => { t.classList.remove('show'); }, 2500);
}

function escapeHtml(str) {
  if (!str) return '';
  return String(str).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function escapeRegex(str) {
  return str.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

// ── Startup Initialization ──────────────────────────────────
window.addEventListener('DOMContentLoaded', () => {
  let initialTab = 'status';
  const hash = window.location.hash.replace('#', '');
  if (hash === 'config' || hash === 'preferences' || window.location.pathname.includes('/config')) {
    initialTab = 'config';
  }
  switchTab(initialTab);
  loadConfig();
  fetchLogs();
  setInterval(fetchLogs, 750);
});
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_or_generate_cert_bytes() {
        let (cert, key) = load_or_generate_cert_bytes().expect("generate cert bytes");
        assert!(!cert.is_empty());
        assert!(!key.is_empty());

        let cert_str = String::from_utf8_lossy(&cert);
        assert!(cert_str.contains("BEGIN CERTIFICATE"));

        let key_str = String::from_utf8_lossy(&key);
        assert!(key_str.contains("BEGIN PRIVATE KEY"));
    }
}

