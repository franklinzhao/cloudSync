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

/// Get the URL for the Status & Terminal Console
pub fn get_status_server_url() -> String {
    let port = get_config_server_port();
    format!("https://localhost:{}/status", port)
}

/// Get the preferred URL for opening the settings page.
/// Using HTTPS satisfies Safari's HTTPS-Only Mode, while our dual-mode server
/// also accepts plain HTTP without ERR_EMPTY_RESPONSE.
pub fn get_config_server_url() -> String {
    let port = get_config_server_port();
    format!("https://localhost:{}/config", port)
}

/// Get the plain HTTP URL as an alternate option
#[allow(dead_code)]
pub fn get_config_server_http_url() -> String {
    let port = get_config_server_port();
    format!("http://localhost:{}/config", port)
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
    Html(STATUS_HTML)
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
    Html(CONFIG_HTML)
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
const CONFIG_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>CloudSync Preferences</title>
<style>
  :root {
    --bg: #0b0f19;
    --card-bg: #151d30;
    --card-border: #23304c;
    --text-main: #f1f5f9;
    --text-muted: #94a3b8;
    --accent: #3b82f6;
    --accent-hover: #2563eb;
    --accent-light: rgba(59, 130, 246, 0.15);
    --success: #10b981;
    --danger: #ef4444;
    --danger-bg: rgba(239, 68, 68, 0.15);
    --input-bg: #0d1424;
    --input-border: #273754;
    --radius: 12px;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    background: var(--bg);
    color: var(--text-main);
    line-height: 1.5;
    padding-bottom: 90px;
    -webkit-font-smoothing: antialiased;
  }
  .container { max-width: 860px; margin: 0 auto; padding: 32px 20px; }
  header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 28px;
    padding-bottom: 20px;
    border-bottom: 1px solid var(--card-border);
  }
  .brand { display: flex; align-items: center; gap: 12px; }
  .brand-icon {
    font-size: 28px;
    background: var(--accent-light);
    width: 48px;
    height: 48px;
    border-radius: 12px;
    display: flex;
    align-items: center;
    justify-content: center;
    border: 1px solid rgba(59, 130, 246, 0.3);
  }
  h1 { font-size: 22px; font-weight: 600; letter-spacing: -0.02em; }
  .subtitle { font-size: 13px; color: var(--text-muted); }
  .badge {
    background: rgba(16, 185, 129, 0.15);
    color: var(--success);
    border: 1px solid rgba(16, 185, 129, 0.3);
    padding: 4px 10px;
    border-radius: 20px;
    font-size: 12px;
    font-weight: 500;
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .badge::before { content: ""; width: 7px; height: 7px; background: var(--success); border-radius: 50%; }

  .card {
    background: var(--card-bg);
    border: 1px solid var(--card-border);
    border-radius: var(--radius);
    padding: 22px;
    margin-bottom: 22px;
    box-shadow: 0 4px 20px rgba(0,0,0,0.25);
  }
  .card-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 16px;
  }
  .card-title {
    font-size: 16px;
    font-weight: 600;
    color: var(--text-main);
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .card-desc { font-size: 13px; color: var(--text-muted); margin-bottom: 16px; }

  .banner-info {
    background: rgba(59, 130, 246, 0.08);
    border: 1px solid rgba(59, 130, 246, 0.25);
    border-radius: 8px;
    padding: 12px 14px;
    font-size: 13px;
    color: #93c5fd;
    margin-bottom: 16px;
    display: flex;
    gap: 10px;
    align-items: flex-start;
  }

  .folder-item {
    background: var(--input-bg);
    border: 1px solid var(--input-border);
    border-radius: 8px;
    padding: 16px;
    margin-bottom: 12px;
    position: relative;
    transition: border-color 0.15s;
  }
  .folder-item:hover { border-color: rgba(59, 130, 246, 0.4); }
  .folder-row { display: grid; grid-template-columns: 1fr 1fr; gap: 14px; margin-bottom: 12px; }
  .folder-actions { display: flex; justify-content: space-between; align-items: center; }

  label {
    display: block;
    font-size: 12px;
    font-weight: 500;
    color: var(--text-muted);
    margin-bottom: 6px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .input-group { display: flex; gap: 8px; }
  input[type="text"], input[type="number"], select {
    width: 100%;
    padding: 10px 12px;
    background: var(--input-bg);
    border: 1px solid var(--input-border);
    border-radius: 6px;
    color: var(--text-main);
    font-size: 13px;
    outline: none;
    transition: all 0.15s;
  }
  input[type="text"]:focus, input[type="number"]:focus, select:focus {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--accent-light);
  }
  input[readonly] { background: rgba(13, 20, 36, 0.6); cursor: default; }

  .btn {
    padding: 9px 14px;
    font-size: 13px;
    font-weight: 500;
    border-radius: 6px;
    cursor: pointer;
    border: none;
    transition: all 0.15s;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    user-select: none;
  }
  .btn-primary { background: var(--accent); color: #fff; }
  .btn-primary:hover { background: var(--accent-hover); }
  .btn-secondary {
    background: #1e293b;
    color: #e2e8f0;
    border: 1px solid var(--card-border);
  }
  .btn-secondary:hover { background: #273754; color: #fff; }
  .btn-danger { background: var(--danger-bg); color: var(--danger); border: 1px solid rgba(239, 68, 68, 0.3); }
  .btn-danger:hover { background: var(--danger); color: #fff; }
  .btn-sm { padding: 6px 10px; font-size: 12px; }

  .grid-2 { display: grid; grid-template-columns: 1fr 1fr; gap: 16px; }
  .grid-3 { display: grid; grid-template-columns: 1fr 1fr 1fr; gap: 14px; }

  .range-box { display: flex; align-items: center; gap: 14px; margin-top: 4px; }
  .range-box input[type="range"] { flex: 1; accent-color: var(--accent); }
  .range-val {
    font-size: 14px;
    font-weight: 600;
    width: 38px;
    text-align: right;
    color: #93c5fd;
  }

  .switch { position: relative; display: inline-block; width: 42px; height: 24px; flex-shrink: 0; }
  .switch input { opacity: 0; width: 0; height: 0; }
  .slider { position: absolute; cursor: pointer; top: 0; left: 0; right: 0; bottom: 0; background-color: rgba(255, 255, 255, 0.15); transition: .2s; border-radius: 24px; border: 1px solid var(--border-color); }
  .slider:before { position: absolute; content: ""; height: 16px; width: 16px; left: 3px; bottom: 3px; background-color: white; transition: .2s; border-radius: 50%; }
  input:checked + .slider { background-color: #2563eb; border-color: #3b82f6; }
  input:checked + .slider:before { transform: translateX(18px); }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 10px;
    min-height: 38px;
    padding: 6px;
    background: var(--input-bg);
    border: 1px solid var(--input-border);
    border-radius: 6px;
  }
  .chip {
    background: #1e293b;
    border: 1px solid #334155;
    padding: 4px 10px;
    border-radius: 4px;
    font-size: 12px;
    font-family: monospace;
    display: flex;
    align-items: center;
    gap: 6px;
    color: #cbd5e1;
  }
  .chip-remove {
    cursor: pointer;
    color: var(--text-muted);
    font-weight: bold;
  }
  .chip-remove:hover { color: var(--danger); }

  .sticky-bar {
    position: fixed;
    bottom: 0;
    left: 0;
    right: 0;
    background: rgba(15, 23, 42, 0.92);
    backdrop-filter: blur(14px);
    border-top: 1px solid var(--card-border);
    padding: 14px 20px;
    z-index: 100;
  }
  .sticky-content {
    max-width: 860px;
    margin: 0 auto;
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .quick-links { display: flex; gap: 8px; }

  #toast {
    position: fixed;
    top: 24px;
    right: 24px;
    background: #1e293b;
    border: 1px solid var(--accent);
    color: #fff;
    padding: 12px 18px;
    border-radius: 8px;
    box-shadow: 0 10px 30px rgba(0,0,0,0.5);
    font-size: 13px;
    opacity: 0;
    transform: translateY(-10px);
    transition: all 0.25s cubic-bezier(0.16, 1, 0.3, 1);
    pointer-events: none;
    z-index: 200;
    display: flex;
    align-items: center;
    gap: 10px;
  }
  #toast.show { opacity: 1; transform: translateY(0); }
</style>
</head>
<body>

<div id="toast"></div>

<div class="container">
  <header>
    <div class="brand">
      <div class="brand-icon">☁️</div>
      <div>
        <h1>CloudSync Preferences</h1>
        <div class="subtitle">Personal OneDrive Concurrency & Bi-Directional Sync Manager</div>
      </div>
    </div>
    <div style="display: flex; gap: 10px; align-items: center;">
      <a href="/status" class="btn btn-secondary" style="text-decoration: none;">📊 Terminal Status</a>
      <div class="badge">App Active</div>
    </div>
  </header>

  <!-- Section 1: Sync Folders -->
  <div class="card">
    <div class="card-header">
      <div>
        <div class="card-title">📁 Sync Folders (Bi-Directional + Dataless Hydration)</div>
        <div class="card-desc">Configure local folders synchronized with OneDrive. Dataless/online-only files are fully downloaded.</div>
      </div>
      <button class="btn btn-secondary btn-sm" onclick="addFolderPair()">➕ Add Folder Pair</button>
    </div>

    <div style="margin-bottom: 18px; padding-bottom: 16px; border-bottom: 1px solid var(--input-border);">
      <label>Shared Parent Local Folder (reduces path duplication)</label>
      <div class="input-group">
        <input type="text" id="parent_folder" placeholder="/Users/franklinzhao/Library/CloudStorage/OneDrive-Personal" oninput="updatePreviews()">
        <button class="btn btn-secondary btn-sm" onclick="browseParentFolder()">📁 Browse Parent…</button>
      </div>
      <div style="font-size: 11px; color: var(--text-muted); margin-top: 4px;">
        When set, sync folders below only need their relative subfolder (e.g. <code>SheridanFiles/Pictures</code>).
      </div>
    </div>

    <div class="banner-info">
      <span style="font-size: 18px;">💡</span>
      <div>
        <strong>Automatic Dataless Hydration Active:</strong><br>
        If your local OneDrive folder contains dataless placeholders (APFS <code>SF_DATALESS</code>), CloudSync prevents empty uploads and automatically downloads the real files from remote.
      </div>
    </div>

    <div id="folders-list"></div>
  </div>

  <!-- Section 2: Concurrency & Performance -->
  <div class="card">
    <div class="card-title" style="margin-bottom: 8px;">⚡ Concurrency & Transfer Limits</div>
    <div class="card-desc">Fine-tune Graph API throughput while honoring personal OneDrive throttling limits.</div>

    <div class="grid-2">
      <div>
        <label>Max Parallel Downloads (Recommended: 4–8)</label>
        <div class="range-box">
          <input type="range" id="max_dl" min="1" max="16" value="4" oninput="updateRangeVal('max_dl', 'max_dl_val')">
          <span class="range-val" id="max_dl_val">4</span>
        </div>
        <div style="font-size: 11px; color: var(--text-muted); margin-top: 4px;">Downloads stream concurrently from Microsoft CDN endpoints.</div>
      </div>

      <div>
        <label>Max Parallel Uploads (Recommended: 2–3)</label>
        <div class="range-box">
          <input type="range" id="max_ul" min="1" max="8" value="2" oninput="updateRangeVal('max_ul', 'max_ul_val')">
          <span class="range-val" id="max_ul_val">2</span>
        </div>
        <div style="font-size: 11px; color: var(--text-muted); margin-top: 4px;">Uploads are chunk-buffered and rate-limit resilient.</div>
      </div>
    </div>
  </div>

  <!-- Section 3: General & Scheduling -->
  <div class="card">
    <div class="card-title" style="margin-bottom: 8px;">⏱️ Sync Schedule & Logging</div>
    <div class="card-desc">Adjust periodic reconciliation and logging verbosity.</div>

    <div class="grid-2">
      <div>
        <label>Periodic Sync Interval (seconds)</label>
        <input type="number" id="sync_interval" min="10" max="86400" step="10" value="300">
        <div style="display: flex; gap: 6px; margin-top: 6px;">
          <button class="btn btn-secondary btn-sm" onclick="setIntervalVal(60)">1m</button>
          <button class="btn btn-secondary btn-sm" onclick="setIntervalVal(180)">3m</button>
          <button class="btn btn-secondary btn-sm" onclick="setIntervalVal(300)">5m</button>
          <button class="btn btn-secondary btn-sm" onclick="setIntervalVal(900)">15m</button>
          <button class="btn btn-secondary btn-sm" onclick="setIntervalVal(3600)">1h</button>
        </div>
      </div>

      <div>
        <label>Log Verbosity Level</label>
        <select id="log_level">
          <option value="trace">trace (ultra verbose)</option>
          <option value="debug">debug (detailed diagnostic)</option>
          <option value="info" selected>info (recommended)</option>
          <option value="warn">warn (warnings & errors only)</option>
          <option value="error">error (critical errors only)</option>
        </select>
        <div style="font-size: 11px; color: var(--text-muted); margin-top: 4px;">Controls terminal and logfile output volume.</div>
      </div>
    </div>

    <div style="margin-top: 18px; padding-top: 14px; border-top: 1px solid var(--card-border); display: flex; align-items: center; justify-content: space-between;">
      <div>
        <div style="font-weight: 600; font-size: 13px; color: var(--text-main);">🚀 Open at Login</div>
        <div style="font-size: 12px; color: var(--text-muted); margin-top: 2px;">Automatically start CloudSync in your menu bar when signing in to macOS.</div>
      </div>
      <label class="switch">
        <input type="checkbox" id="autostart_toggle" onchange="toggleAutostart(this.checked)">
        <span class="slider"></span>
      </label>
    </div>
  </div>

  <!-- Section 4: File Exclusions -->
  <div class="card">
    <div class="card-title" style="margin-bottom: 8px;">🚫 Exclude File Patterns</div>
    <div class="card-desc">Glob patterns for files or folders ignored during synchronization.</div>

    <div class="chips" id="chips-container"></div>
    <div class="input-group">
      <input type="text" id="new-pattern" placeholder="e.g. *.tmp, **/.git/**, ~$*" onkeydown="if(event.key==='Enter')addPattern()">
      <button class="btn btn-secondary btn-sm" onclick="addPattern()">Add Pattern</button>
      <button class="btn btn-secondary btn-sm" onclick="resetDefaultPatterns()">Defaults</button>
    </div>
  </div>

  <!-- Section 5: OneDrive & Azure AD -->
  <div class="card">
    <div class="card-title" style="margin-bottom: 8px;">🔐 OneDrive / Azure AD Settings</div>
    <div class="card-desc">OAuth2 credentials registered in your personal Azure Portal.</div>

    <div class="grid-2">
      <div>
        <label>Azure Application (Client) ID</label>
        <input type="text" id="client_id" placeholder="e.g. 00000000-0000-0000-0000-000000000000">
      </div>
      <div>
        <label>OAuth2 Redirect URI</label>
        <input type="text" id="redirect_uri" value="http://localhost:8765/callback">
      </div>
    </div>
  </div>
</div>

<!-- Sticky Bottom Bar -->
<div class="sticky-bar">
  <div class="sticky-content">
    <div class="quick-links">
      <a href="/status" class="btn btn-secondary btn-sm" style="text-decoration: none;">📊 Terminal Status</a>
      <button class="btn btn-secondary btn-sm" onclick="syncNow()">⚡ Sync Now</button>
      <button class="btn btn-secondary btn-sm" onclick="openConfigFile()">📝 config.toml</button>
      <button class="btn btn-secondary btn-sm" onclick="openConfigDir()">📂 Open in Finder</button>
    </div>
    <button class="btn btn-primary" onclick="saveConfig()">💾 Save & Apply Changes</button>
  </div>
</div>

<script>
let currentConfig = null;
let patterns = [];

function showToast(msg, isError = false) {
  const t = document.getElementById('toast');
  t.innerHTML = (isError ? '❌ ' : '✅ ') + msg;
  t.style.borderColor = isError ? 'var(--danger)' : 'var(--accent)';
  t.classList.add('show');
  setTimeout(() => t.classList.remove('show'), 3500);
}

function updateRangeVal(sliderId, labelId) {
  document.getElementById(labelId).innerText = document.getElementById(sliderId).value;
}

function setIntervalVal(secs) {
  document.getElementById('sync_interval').value = secs;
}

async function loadConfig() {
  try {
    const res = await fetch('/api/config');
    currentConfig = await res.json();

    // Parent folder
    document.getElementById('parent_folder').value = currentConfig.local_parent_folder || "";

    // Folders
    const rawFolders = currentConfig.sync_folders && currentConfig.sync_folders.length > 0
      ? currentConfig.sync_folders
      : [{
          local_folder: currentConfig.local_folder || "",
          remote_folder: currentConfig.remote_folder || "",
          enabled: true
        }];

    renderFolders(rawFolders);

    // Concurrency
    document.getElementById('max_dl').value = currentConfig.max_parallel_downloads || 4;
    document.getElementById('max_dl_val').innerText = currentConfig.max_parallel_downloads || 4;
    document.getElementById('max_ul').value = currentConfig.max_parallel_uploads || 2;
    document.getElementById('max_ul_val').innerText = currentConfig.max_parallel_uploads || 2;

    // Interval & Log
    document.getElementById('sync_interval').value = currentConfig.sync_interval_secs || 300;
    document.getElementById('log_level').value = currentConfig.log_level || 'info';

    // Patterns
    patterns = currentConfig.exclude_patterns || [".DS_Store", "**/.git/**", "*.tmp", "~$*", "Thumbs.db"];
    renderPatterns();

    // Azure
    if (currentConfig.onedrive) {
      document.getElementById('client_id').value = currentConfig.onedrive.client_id || '';
      document.getElementById('redirect_uri').value = currentConfig.onedrive.redirect_uri || 'http://localhost:8765/callback';
    }

    loadAutostart();
  } catch (err) {
    showToast('Failed to load configuration: ' + err, true);
  }
}

async function loadAutostart() {
  try {
    const res = await fetch('/api/autostart');
    const data = await res.json();
    document.getElementById('autostart_toggle').checked = !!data.autostart;
  } catch (e) {
    console.error('Failed to load autostart status:', e);
  }
}

async function toggleAutostart(enable) {
  try {
    const res = await fetch('/api/autostart', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ enable })
    });
    const data = await res.json();
    document.getElementById('autostart_toggle').checked = !!data.autostart;
    showToast(data.autostart ? 'Auto-start at login enabled' : 'Auto-start at login disabled', false);
  } catch (e) {
    showToast('Failed to update autostart setting: ' + e, true);
  }
}

function renderFolders(folders) {
  const container = document.getElementById('folders-list');
  container.innerHTML = '';
  const parent = document.getElementById('parent_folder').value.trim();

  folders.forEach((f, idx) => {
    let subVal = f.subfolder || "";
    let localVal = f.local_folder || "";
    if (!subVal && localVal && parent && localVal.startsWith(parent)) {
      subVal = localVal.slice(parent.length).replace(/^[\\\/]+/, '');
    }
    const valToShow = subVal || localVal;

    const div = document.createElement('div');
    div.className = 'folder-item';
    div.innerHTML = `
      <div class="folder-row">
        <div>
          <label>Subfolder / Local Path</label>
          <div class="input-group">
            <input type="text" id="folder_input_${idx}" value="${escapeHtml(valToShow)}" placeholder="e.g. SheridanFiles/Pictures" oninput="updateItemPreview(${idx})">
            <button class="btn btn-secondary btn-sm" onclick="browseFolder(${idx})">📁 Browse…</button>
          </div>
          <div id="preview_${idx}" style="font-size: 11px; color: var(--text-muted); margin-top: 4px; font-family: monospace;"></div>
        </div>
        <div>
          <label>OneDrive Remote Folder Path</label>
          <input type="text" id="remote_folder_${idx}" value="${escapeHtml(f.remote_folder || '')}" placeholder="e.g. /SheridanFiles/Pictures">
        </div>
      </div>
      <div class="folder-actions">
        <label style="display:flex;align-items:center;gap:6px;cursor:pointer;text-transform:none;font-size:13px;color:var(--text-main);margin:0;">
          <input type="checkbox" id="enabled_${idx}" ${f.enabled !== false ? 'checked' : ''}>
          <span>Bi-directional sync enabled</span>
        </label>
        ${folders.length > 1 ? `<button class="btn btn-danger btn-sm" onclick="removeFolderPair(${idx})">🗑️ Remove</button>` : ''}
      </div>
    `;
    container.appendChild(div);
    updateItemPreview(idx);
  });
}

function updateItemPreview(idx) {
  const parent = document.getElementById('parent_folder').value.trim();
  const inputEl = document.getElementById(`folder_input_${idx}`);
  const previewEl = document.getElementById(`preview_${idx}`);
  if (!inputEl || !previewEl) return;

  const val = inputEl.value.trim();
  if (parent) {
    const cleanSub = val.replace(/^[\\\/]+/, '');
    const cleanParent = parent.replace(/[\\\/]+$/, '');
    const full = val.startsWith('/') ? val : `${cleanParent}/${cleanSub}`;
    previewEl.innerText = `Full: ${full}`;
  } else {
    previewEl.innerText = val ? `Full: ${val}` : '';
  }
}

function updatePreviews() {
  const items = document.querySelectorAll('.folder-item');
  items.forEach((_, idx) => updateItemPreview(idx));
}

function addFolderPair() {
  const folders = collectFolders();
  folders.push({ subfolder: '', remote_folder: '', enabled: true });
  renderFolders(folders);
}

function removeFolderPair(idx) {
  const folders = collectFolders();
  folders.splice(idx, 1);
  renderFolders(folders);
}

async function browseParentFolder() {
  try {
    showToast("Opening macOS folder picker for Parent Folder…");
    const res = await fetch('/api/browse-folder', { method: 'POST' });
    const data = await res.json();
    if (data.path) {
      document.getElementById('parent_folder').value = data.path;
      updatePreviews();
      showToast("Selected Parent: " + data.path);
    }
  } catch (err) {
    showToast("Browse folder error: " + err, true);
  }
}

async function browseFolder(idx) {
  try {
    showToast("Opening macOS folder picker…");
    const res = await fetch('/api/browse-folder', { method: 'POST' });
    const data = await res.json();
    if (data.path) {
      const parent = document.getElementById('parent_folder').value.trim();
      let selected = data.path;
      if (parent && selected.startsWith(parent)) {
        selected = selected.slice(parent.length).replace(/^[\\\/]+/, '');
      }
      document.getElementById(`folder_input_${idx}`).value = selected;
      updateItemPreview(idx);

      // Auto-fill remote if empty
      const remoteInput = document.getElementById(`remote_folder_${idx}`);
      if (!remoteInput.value.trim()) {
        remoteInput.value = '/' + selected.replace(/^[\\\/]+/, '');
      }
      showToast("Selected: " + selected);
    }
  } catch (err) {
    showToast("Browse folder error: " + err, true);
  }
}

function collectFolders() {
  const items = document.querySelectorAll('.folder-item');
  const parent = document.getElementById('parent_folder').value.trim();
  const folders = [];

  items.forEach((item, idx) => {
    const val = document.getElementById(`folder_input_${idx}`).value.trim();
    let remote = document.getElementById(`remote_folder_${idx}`).value.trim();
    const enabled = document.getElementById(`enabled_${idx}`).checked;

    if (!remote && val) {
      remote = '/' + val.replace(/^[\\\/]+/, '');
    }

    if (parent && !val.startsWith('/')) {
      folders.push({
        subfolder: val,
        remote_folder: remote,
        enabled: enabled
      });
    } else {
      folders.push({
        local_folder: val,
        remote_folder: remote,
        enabled: enabled
      });
    }
  });

  return folders;
}

function renderPatterns() {
  const container = document.getElementById('chips-container');
  container.innerHTML = '';
  patterns.forEach((pat, i) => {
    const chip = document.createElement('div');
    chip.className = 'chip';
    chip.innerHTML = `<span>${escapeHtml(pat)}</span><span class="chip-remove" onclick="removePattern(${i})">×</span>`;
    container.appendChild(chip);
  });
}

function addPattern() {
  const input = document.getElementById('new-pattern');
  const val = input.value.trim();
  if (val && !patterns.includes(val)) {
    patterns.push(val);
    renderPatterns();
    input.value = '';
  }
}

function removePattern(i) {
  patterns.splice(i, 1);
  renderPatterns();
}

function resetDefaultPatterns() {
  patterns = [".DS_Store", "**/.git/**", "**/.gitignore", "*.tmp", "*.swp", "~$*", "Thumbs.db"];
  renderPatterns();
}

async function saveConfig() {
  const parentFolder = document.getElementById('parent_folder').value.trim();
  const folders = collectFolders();
  if (folders.length === 0 || (!folders[0].subfolder && !folders[0].local_folder)) {
    showToast("Please provide at least one local sync folder path or subfolder", true);
    return;
  }

  const newConfig = {
    local_parent_folder: parentFolder || null,
    sync_folders: folders,
    sync_interval_secs: parseInt(document.getElementById('sync_interval').value, 10) || 300,
    max_parallel_downloads: parseInt(document.getElementById('max_dl').value, 10) || 4,
    max_parallel_uploads: parseInt(document.getElementById('max_ul').value, 10) || 2,
    log_level: document.getElementById('log_level').value,
    exclude_patterns: patterns,
    onedrive: {
      client_id: document.getElementById('client_id').value.trim(),
      redirect_uri: document.getElementById('redirect_uri').value.trim() || 'http://localhost:8765/callback'
    }
  };

  try {
    const res = await fetch('/api/config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(newConfig)
    });
    const result = await res.json();
    if (res.ok && result.status === 'ok') {
      showToast("Settings saved and applied successfully!");
    } else {
      showToast("Error: " + (result.message || "Failed to save"), true);
    }
  } catch (err) {
    showToast("Network error saving configuration: " + err, true);
  }
}

async function syncNow() {
  try {
    await fetch('/api/sync-now', { method: 'POST' });
    showToast("Sync triggered!");
  } catch (err) {
    showToast("Sync trigger failed: " + err, true);
  }
}

async function openConfigFile() {
  await fetch('/api/open-config-file', { method: 'POST' });
}

async function openConfigDir() {
  await fetch('/api/open-config-dir', { method: 'POST' });
}

function escapeHtml(str) {
  return str.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

window.onload = loadConfig;
</script>
</body>
</html>
"#;

const STATUS_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>CloudSync Console — Status & Live Terminal</title>
<style>
  :root {
    --bg-main: #0a0e17;
    --bg-card: #121824;
    --bg-term: #06090f;
    --border: rgba(255, 255, 255, 0.1);
    --border-accent: rgba(88, 166, 255, 0.35);
    --text-primary: #e6edf3;
    --text-secondary: #8b949e;
    --text-muted: #484f58;
    --accent: #58a6ff;
    --success: #3fb950;
    --warning: #d29922;
    --error: #f85149;
    --cyan: #39c5bb;
    --font-mono: ui-monospace, "SF Mono", Menlo, Monaco, Consolas, "Liberation Mono", monospace;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    background: var(--bg-main);
    color: var(--text-primary);
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
    min-height: 100vh;
    display: flex;
    flex-direction: column;
  }
  .app-header {
    background: rgba(18, 24, 36, 0.95);
    backdrop-filter: blur(14px);
    border-bottom: 1px solid var(--border);
    padding: 12px 24px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    position: sticky;
    top: 0;
    z-index: 100;
  }
  .app-title {
    display: flex;
    align-items: center;
    gap: 14px;
  }
  .traffic-lights {
    display: flex;
    gap: 7px;
  }
  .dot {
    width: 12px;
    height: 12px;
    border-radius: 50%;
  }
  .dot-red { background: #ff5f56; }
  .dot-yellow { background: #ffbd2e; }
  .dot-green { background: #27c93f; }
  .title-text {
    font-size: 15px;
    font-weight: 700;
    letter-spacing: -0.3px;
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .status-badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 3px 10px;
    border-radius: 12px;
    font-size: 11px;
    font-weight: 600;
    background: rgba(63, 185, 80, 0.15);
    color: var(--success);
    border: 1px solid rgba(63, 185, 80, 0.3);
  }
  .status-badge.syncing {
    background: rgba(88, 166, 255, 0.15);
    color: var(--accent);
    border-color: rgba(88, 166, 255, 0.3);
  }
  .status-pulse {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: currentColor;
    box-shadow: 0 0 8px currentColor;
    animation: pulse 1.5s infinite;
  }
  @keyframes pulse {
    0%, 100% { opacity: 1; transform: scale(1); }
    50% { opacity: 0.4; transform: scale(0.85); }
  }
  .header-actions {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 6px 14px;
    border-radius: 6px;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
    text-decoration: none;
    border: none;
    transition: all 0.15s ease;
  }
  .btn-primary {
    background: #238636;
    color: white;
  }
  .btn-primary:hover { background: #2ea043; }
  .btn-secondary {
    background: rgba(255, 255, 255, 0.08);
    color: var(--text-primary);
    border: 1px solid var(--border);
  }
  .btn-secondary:hover {
    background: rgba(255, 255, 255, 0.14);
    border-color: rgba(255, 255, 255, 0.2);
  }
  .main-content {
    flex: 1;
    display: flex;
    flex-direction: column;
    padding: 16px 24px 24px;
    max-width: 1400px;
    margin: 0 auto;
    width: 100%;
    gap: 14px;
  }
  .stats-row {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
    gap: 12px;
  }
  .stat-card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 12px 16px;
  }
  .stat-label {
    font-size: 11px;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 4px;
  }
  .stat-val {
    font-size: 16px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .terminal-frame {
    flex: 1;
    display: flex;
    flex-direction: column;
    background: var(--bg-term);
    border: 1px solid var(--border);
    border-radius: 10px;
    overflow: hidden;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.6);
    min-height: 520px;
  }
  .terminal-bar {
    background: #111723;
    border-bottom: 1px solid var(--border);
    padding: 8px 14px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
  }
  .filter-controls {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .search-box {
    background: rgba(0, 0, 0, 0.45);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 5px 10px;
    color: var(--text-primary);
    font-size: 12px;
    width: 240px;
  }
  .search-box:focus {
    outline: none;
    border-color: var(--accent);
  }
  .level-tag {
    background: transparent;
    border: 1px solid var(--border);
    color: var(--text-secondary);
    padding: 3px 9px;
    border-radius: 4px;
    font-size: 11px;
    cursor: pointer;
  }
  .level-tag.active {
    background: rgba(88, 166, 255, 0.2);
    color: var(--accent);
    border-color: var(--accent);
  }
  .terminal-tools {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .scroll-label {
    font-size: 12px;
    color: var(--text-secondary);
    display: flex;
    align-items: center;
    gap: 5px;
    cursor: pointer;
  }
  .terminal-body {
    flex: 1;
    padding: 14px 18px;
    overflow-y: auto;
    font-family: var(--font-mono);
    font-size: 12.5px;
    line-height: 1.6;
    max-height: calc(100vh - 280px);
  }
  .term-line {
    display: flex;
    gap: 12px;
    white-space: pre-wrap;
    word-break: break-all;
  }
  .term-line:hover {
    background: rgba(255, 255, 255, 0.03);
  }
  .line-num {
    color: var(--text-muted);
    min-width: 48px;
    text-align: right;
    user-select: none;
    font-size: 11px;
    line-height: 1.8;
  }
  .line-text {
    flex: 1;
  }
  .highlight-info { color: #58a6ff; font-weight: 600; }
  .highlight-warn { color: var(--warning); font-weight: 600; }
  .highlight-error { color: var(--error); font-weight: bold; }
  .highlight-upload { color: var(--cyan); font-weight: 600; }
  .highlight-done { color: var(--success); font-weight: 600; }
  .highlight-folder { color: #f0f6fc; }
  .highlight-time { color: #6e7681; }
  .term-cursor {
    display: inline-block;
    width: 8px;
    height: 15px;
    background: var(--accent);
    vertical-align: middle;
    margin-left: 4px;
    animation: blink 1s infinite;
  }
  @keyframes blink {
    0%, 49% { opacity: 1; }
    50%, 100% { opacity: 0; }
  }
  #toast {
    position: fixed;
    top: 20px;
    right: 20px;
    background: #1c2433;
    border: 1px solid var(--accent);
    color: #fff;
    padding: 10px 18px;
    border-radius: 6px;
    font-size: 13px;
    opacity: 0;
    transform: translateY(-8px);
    transition: all 0.2s;
    pointer-events: none;
    z-index: 200;
  }
  #toast.show { opacity: 1; transform: translateY(0); }
</style>
</head>
<body>

<div id="toast"></div>

<header class="app-header">
  <div class="app-title">
    <div class="traffic-lights">
      <div class="dot dot-red"></div>
      <div class="dot dot-yellow"></div>
      <div class="dot dot-green"></div>
    </div>
    <div class="title-text">
      <span>☁️ CloudSync Terminal Console</span>
      <span class="status-badge" id="app-badge"><span class="status-pulse"></span><span id="badge-text">Idle</span></span>
    </div>
  </div>
  <div class="header-actions">
    <button class="btn btn-primary" onclick="syncNow()">⚡ Sync Now</button>
    <a href="/config" class="btn btn-secondary">⚙️ Preferences (Config)</a>
  </div>
</header>

<div class="main-content">
  <div class="stats-row">
    <div class="stat-card">
      <div class="stat-label">Sync Folders</div>
      <div class="stat-val" id="stat-folders">Loading…</div>
    </div>
    <div class="stat-card">
      <div class="stat-label">Interval</div>
      <div class="stat-val" id="stat-interval">300s</div>
    </div>
    <div class="stat-card">
      <div class="stat-label">Terminal Stream</div>
      <div class="stat-val" id="stat-lines">0 lines</div>
    </div>
    <div class="stat-card">
      <div class="stat-label">Log File</div>
      <div class="stat-val" style="font-size: 13px; font-weight: normal; color: var(--accent); cursor: pointer;" onclick="openLogFile()">📄 cloudsync.log</div>
    </div>
  </div>

  <div class="terminal-frame">
    <div class="terminal-bar">
      <div class="filter-controls">
        <input type="text" class="search-box" id="search-filter" placeholder="🔍 Filter terminal logs..." oninput="renderFilteredLogs()">
        <button class="level-tag active" data-lvl="all" onclick="setLevelFilter('all')">All</button>
        <button class="level-tag" data-lvl="info" onclick="setLevelFilter('info')">INFO</button>
        <button class="level-tag" data-lvl="upload" onclick="setLevelFilter('upload')">Uploads</button>
        <button class="level-tag" data-lvl="warn" onclick="setLevelFilter('warn')">WARN</button>
        <button class="level-tag" data-lvl="error" onclick="setLevelFilter('error')">ERROR</button>
      </div>
      <div class="terminal-tools">
        <label class="scroll-label">
          <input type="checkbox" id="auto-scroll" checked> Auto-scroll
        </label>
        <button class="btn btn-secondary btn-sm" onclick="copyLogs()">📋 Copy</button>
        <button class="btn btn-secondary btn-sm" onclick="clearLogs()">🗑️ Clear</button>
      </div>
    </div>

    <div class="terminal-body" id="term-body">
      <div id="term-lines"></div>
      <div style="margin-top: 6px;"><span class="term-cursor"></span></div>
    </div>
  </div>
</div>

<script>
let displayedLogs = [];
let activeLevel = 'all';
let lastReceivedId = 0;
let isFetchingLogs = false;
let totalLinesCount = 0;

function showToast(msg) {
  const t = document.getElementById('toast');
  t.innerText = msg;
  t.classList.add('show');
  setTimeout(() => t.classList.remove('show'), 2500);
}

function setLevelFilter(lvl) {
  activeLevel = lvl;
  document.querySelectorAll('.level-tag').forEach(b => {
    b.classList.toggle('active', b.getAttribute('data-lvl') === lvl);
  });
  renderAllLogs();
}

function formatLogLine(raw) {
  // Strip ANSI color escape codes (e.g. \u001b[2m)
  let clean = raw.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '').replace(/\\u001b\[[0-9;]*[a-zA-Z]/g, '');
  let text = clean.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  
  // Highlighting
  text = text.replace(/^(\d{4}-\d{2}-\d{2}T[\d:\.]+Z)/, '<span class="highlight-time">$1</span>');
  text = text.replace(/\bINFO\b/, '<span class="highlight-info">INFO</span>');
  text = text.replace(/\bWARN\b/, '<span class="highlight-warn">WARN</span>');
  text = text.replace(/\bERROR\b/, '<span class="highlight-error">ERROR</span>');
  text = text.replace(/\bDEBUG\b/, '<span class="highlight-time">DEBUG</span>');
  text = text.replace(/(Uploading: [^\s]+)/, '<span class="highlight-upload">$1</span>');
  text = text.replace(/(Downloading: [^\s]+)/, '<span class="highlight-folder">$1</span>');
  text = text.replace(/(Reconciliation done [^$]+)/, '<span class="highlight-done">$1</span>');
  text = text.replace(/(Manual sync done [^$]+)/, '<span class="highlight-done">$1</span>');
  text = text.replace(/(Sync folder: [^$]+)/, '<span class="highlight-folder">$1</span>');
  return text;
}

function matchesFilter(entry, query) {
  const text = entry.text || '';
  if (activeLevel === 'info' && !text.includes('INFO')) return false;
  if (activeLevel === 'upload' && !text.includes('Uploading:') && !text.includes('Downloading:')) return false;
  if (activeLevel === 'warn' && !text.includes('WARN')) return false;
  if (activeLevel === 'error' && !text.includes('ERROR')) return false;
  if (query && !text.toLowerCase().includes(query)) return false;
  return true;
}

function renderAllLogs() {
  const container = document.getElementById('term-lines');
  const query = document.getElementById('search-filter').value.toLowerCase().trim();
  const autoScroll = document.getElementById('auto-scroll').checked;

  const filtered = displayedLogs.filter(e => matchesFilter(e, query));
  const slice = filtered.slice(Math.max(0, filtered.length - 1000));

  container.innerHTML = slice.map(entry => {
    return `<div class="term-line"><span class="line-num">${entry.id}</span><span class="line-text">${formatLogLine(entry.text)}</span></div>`;
  }).join('');

  updateSyncStateBadge();

  if (autoScroll) {
    const body = document.getElementById('term-body');
    body.scrollTop = body.scrollHeight;
  }
}

function appendNewLogs(newEntries) {
  const container = document.getElementById('term-lines');
  const autoScroll = document.getElementById('auto-scroll').checked;
  const query = document.getElementById('search-filter').value.toLowerCase().trim();

  for (const entry of newEntries) {
    displayedLogs.push(entry);

    if (matchesFilter(entry, query)) {
      const row = document.createElement('div');
      row.className = 'term-line';
      row.innerHTML = `<span class="line-num">${entry.id}</span><span class="line-text">${formatLogLine(entry.text)}</span>`;
      container.appendChild(row);
    }
  }

  // Keep DOM element count bounded to last 1000 items for buttery smooth performance
  while (container.childNodes.length > 1000) {
    container.removeChild(container.firstChild);
  }
  if (displayedLogs.length > 2500) {
    displayedLogs = displayedLogs.slice(displayedLogs.length - 2500);
  }

  updateSyncStateBadge();

  if (autoScroll) {
    const body = document.getElementById('term-body');
    body.scrollTop = body.scrollHeight;
  }
}

function updateSyncStateBadge() {
  let isSyncing = false;
  for (let i = displayedLogs.length - 1; i >= Math.max(0, displayedLogs.length - 25); i--) {
    const text = displayedLogs[i].text || '';
    if (text.includes('Reconciliation done') || text.includes('Manual sync done')) {
      isSyncing = false;
      break;
    }
    if (text.includes('Starting sync') || text.includes('Uploading:') || text.includes('Downloading:') || text.includes('queueing download')) {
      isSyncing = true;
      break;
    }
  }

  const badge = document.getElementById('app-badge');
  const badgeText = document.getElementById('badge-text');
  if (isSyncing) {
    badge.className = 'status-badge syncing';
    badgeText.innerText = 'Syncing…';
  } else {
    badge.className = 'status-badge';
    badgeText.innerText = 'Idle (Watching)';
  }
}

async function fetchLogs() {
  if (isFetchingLogs) return;
  isFetchingLogs = true;
  try {
    const url = lastReceivedId > 0 ? `/api/logs?since=${lastReceivedId}` : '/api/logs';
    const res = await fetch(url);
    if (res.ok) {
      const data = await res.json();
      if (data.total !== undefined) {
        totalLinesCount = data.total;
        document.getElementById('stat-lines').innerText = totalLinesCount.toLocaleString() + ' total lines';
      }

      const newEntries = data.logs || [];
      if (newEntries.length > 0) {
        // Track the highest received id
        lastReceivedId = Math.max(lastReceivedId, newEntries[newEntries.length - 1].id);

        if (displayedLogs.length === 0) {
          displayedLogs = newEntries;
          renderAllLogs();
        } else {
          appendNewLogs(newEntries);
        }
      }
    }
  } catch (err) {
    console.warn('Fetch logs error:', err);
  } finally {
    isFetchingLogs = false;
  }
}

async function fetchConfigSummary() {
  try {
    const res = await fetch('/api/config');
    if (res.ok) {
      const cfg = await res.json();
      const count = (cfg.sync_folders || []).length;
      document.getElementById('stat-folders').innerText = count + ' folder' + (count === 1 ? '' : 's');
      document.getElementById('stat-interval').innerText = (cfg.sync_interval_secs || 300) + 's';
    }
  } catch (err) {}
}

async function syncNow() {
  try {
    await fetch('/api/sync-now', { method: 'POST' });
    showToast('⚡ Sync pass initiated');
    fetchLogs();
  } catch (err) {
    showToast('❌ Sync trigger failed: ' + err);
  }
}

async function openLogFile() {
  try {
    await fetch('/api/open-log-file', { method: 'POST' });
    showToast('📄 Opened cloudsync.log');
  } catch (err) {}
}

async function clearLogs() {
  await fetch('/api/logs/clear', { method: 'POST' });
  displayedLogs = [];
  lastReceivedId = 0;
  document.getElementById('term-lines').innerHTML = '';
  document.getElementById('stat-lines').innerText = '0 lines';
  showToast('Cleared terminal output');
}

function copyLogs() {
  const text = displayedLogs.map(e => `[${e.id}] ${e.text}`).join('\n');
  navigator.clipboard.writeText(text);
  showToast('📋 Copied ' + displayedLogs.length + ' lines to clipboard');
}

window.onload = () => {
  fetchConfigSummary();
  fetchLogs();
  setInterval(fetchLogs, 800);
};
</script>
</body>
</html>
"#;

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

