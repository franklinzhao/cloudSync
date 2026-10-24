mod auth;
mod cloud;
mod config;
mod config_server;
mod state_db;
mod sync_engine;
mod tray;
mod watcher;

use anyhow::Result;
use std::sync::Arc;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use cloud::onedrive::OneDriveClient;
use config::{Config, FolderPair};
use config_server::{
    get_config_server_url, get_status_server_url, start_config_server, AppLogWriter,
    ConfigServerEvent,
};
use state_db::StateDb;
use sync_engine::{SyncEngine, SyncSummary};
use tray::{build_tray, update_tray, TrayCommand, TrayState};
use watcher::{start_watcher, WatchEvent};

fn main() -> Result<()> {
    // ── Config & logging ─────────────────────────────────────────────────────
    Config::write_example_if_missing().ok();

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "❌ Could not load config: {}\n\n\
                 Please edit the config file at:\n  {}\n\
                 A template has been written there for you.",
                e,
                Config::config_path().display()
            );
            std::process::exit(1);
        }
    };

    let log_writer = AppLogWriter::new();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(&config.log_level)),
        )
        .with_writer(log_writer)
        .init();

    // ── UI event loop owns the main thread ───────────────────────────────────
    let mut event_loop = EventLoopBuilder::<TrayState>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory); // menu bar only, no Dock icon
    let proxy = event_loop.create_proxy();

    let folders = config.get_sync_folders();
    let local_folder_str = folders
        .first()
        .map(|f| f.local_folder.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut config = Some(config);
    let mut tray = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            // The tray must be created inside the running loop, on the main thread
            Event::NewEvents(StartCause::Init) => {
                let (t, tray_cmd_rx) =
                    build_tray(local_folder_str.clone()).expect("failed to build tray icon");
                tray = Some(t);

                let cfg = config.take().expect("config already taken");
                let proxy = proxy.clone();

                // Everything async runs on a background thread
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
                    if let Err(e) = rt.block_on(run(cfg, tray_cmd_rx, proxy)) {
                        error!("Fatal error: {e:#}");
                        std::process::exit(1);
                    }
                });
            }

            // Tray state changes sent from the sync thread
            Event::UserEvent(state) => {
                if let Some(t) = &tray {
                    update_tray(t, &state).ok();
                }
            }

            _ => {}
        }
    })
}

async fn run(
    mut config: Config,
    tray_cmd_rx: std::sync::mpsc::Receiver<TrayCommand>,
    proxy: EventLoopProxy<TrayState>,
) -> Result<()> {
    // Sends a state change to the main thread, which updates the tray icon
    let ui = |s: TrayState| {
        proxy.send_event(s).ok();
    };

    info!("CloudSync starting…");

    let mut active_folders = config.get_sync_folders();
    if active_folders.is_empty() {
        warn!("No sync folders configured! Open Preferences (Config) to set up sync folders.");
    } else {
        for f in &active_folders {
            info!("Sync folder: {} ⇄ {}", f.local_folder.display(), f.remote_folder);
            std::fs::create_dir_all(&f.local_folder).ok();
        }
    }

    // ── Start Web Configuration Server ────────────────────────────────────────
    let (config_server_tx, mut config_server_rx) =
        tokio::sync::mpsc::channel::<ConfigServerEvent>(16);
    let config_arc = Arc::new(RwLock::new(config.clone()));
    start_config_server(Arc::clone(&config_arc), config_server_tx)?;

    // ── Shared HTTP client (used by auth + OneDrive client) ──────────────────
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    // ── Authentication ───────────────────────────────────────────────────────
    let access_token = auth::get_access_token(&config.onedrive, &http).await?;

    // Default remote root for base client
    let default_remote_root = active_folders
        .first()
        .map(|f| f.remote_folder.clone())
        .unwrap_or_else(|| "/".to_string());

    // ── Cloud provider ───────────────────────────────────────────────────────
    let provider = Arc::new(Mutex::new(OneDriveClient::new(
        access_token,
        default_remote_root,
        Some(config.onedrive.clone()),
    )));

    // ── File watcher ─────────────────────────────────────────────────────────
    let watch_dirs: Vec<_> = active_folders.iter().map(|f| f.local_folder.clone()).collect();
    let mut watch_rx = start_watcher(watch_dirs)?;

    // ── Tray command bridge (std → async) ────────────────────────────────────
    let (async_tray_tx, mut async_tray_rx) = tokio::sync::mpsc::channel::<TrayCommand>(16);
    std::thread::spawn(move || {
        while let Ok(cmd) = tray_cmd_rx.recv() {
            let _ = async_tray_tx.blocking_send(cmd);
        }
    });

    // ── Periodic sync timer ──────────────────────────────────────────────────
    let mut sync_interval_secs = config.sync_interval_secs;
    let (periodic_tx, mut periodic_rx) = tokio::sync::mpsc::channel::<()>(4);
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(sync_interval_secs));
        ticker.tick().await; // skip first immediate tick — handled by initial sync below
        loop {
            ticker.tick().await;
            let _ = periodic_tx.send(()).await;
        }
    });

    // ── Helper: refresh access token, update provider ─────────────────────────
    let refresh_token = {
        let http = http.clone();
        let cfg = config.onedrive.clone();
        let provider = Arc::clone(&provider);
        move || {
            let http = http.clone();
            let cfg = cfg.clone();
            let provider = Arc::clone(&provider);
            async move {
                match auth::get_access_token(&cfg, &http).await {
                    Ok(token) => {
                        provider.lock().await.set_token(token).await;
                    }
                    Err(e) => warn!("Token refresh failed: {}", e),
                }
            }
        }
    };

    // ── Initial sync ─────────────────────────────────────────────────────────
    ui(TrayState::Syncing);
    {
        refresh_token().await;
        match reconcile_all(&active_folders, &provider, &config).await {
            Ok(s) => info!(
                "Initial sync done — ↑{} ↓{} ✓{} ✗{}",
                s.uploaded, s.downloaded, s.skipped, s.errors
            ),
            Err(e) => error!("Initial sync error: {}", e),
        }
    }
    ui(TrayState::Idle);

    // ── Main event loop ──────────────────────────────────────────────────────
    info!("CloudSync running. Watching for changes… (interval: {}s)", sync_interval_secs);

    loop {
        tokio::select! {
            // FS event from watcher
            Some(event) = watch_rx.recv() => {
                match event {
                    WatchEvent::Changed(path) | WatchEvent::Deleted(path) => {
                        if let Some(folder) = active_folders.iter().find(|f| f.enabled && path.starts_with(&f.local_folder)) {
                            info!("FS event: {}", path.display());
                            ui(TrayState::Syncing);
                            refresh_token().await;

                            let guard = provider.lock().await;
                            let folder_provider = guard.with_remote_root(folder.remote_folder.clone());
                            let db_path = StateDb::path_for_folder(&folder.local_folder);

                            match StateDb::open(&db_path) {
                                Ok(db) => {
                                    let engine = SyncEngine::new(
                                        &db,
                                        &folder_provider,
                                        &folder.local_folder,
                                        &config.exclude_patterns,
                                        config.max_parallel_downloads,
                                        config.max_parallel_uploads,
                                    );
                                    if let Err(e) = engine.sync_path(&path).await {
                                        error!("Sync error for {}: {}", path.display(), e);
                                        ui(TrayState::Error(e.to_string()));
                                        tokio::time::sleep(Duration::from_secs(3)).await;
                                    }
                                }
                                Err(e) => error!("Failed to open state DB for {}: {}", folder.local_folder.display(), e),
                            }

                            ui(TrayState::Idle);
                        }
                    }
                }
            }

            // Periodic full reconcile
            Some(_) = periodic_rx.recv() => {
                info!("Periodic sync triggered.");
                ui(TrayState::Syncing);
                refresh_token().await;
                match reconcile_all(&active_folders, &provider, &config).await {
                    Ok(s) => info!(
                        "Periodic sync done — ↑{} ↓{} ✓{} ✗{}",
                        s.uploaded, s.downloaded, s.skipped, s.errors
                    ),
                    Err(e) => error!("Periodic sync error: {}", e),
                }
                ui(TrayState::Idle);
            }

            // Config server events (from Web UI)
            Some(server_event) = config_server_rx.recv() => {
                match server_event {
                    ConfigServerEvent::SyncNow => {
                        info!("Manual sync triggered from Web UI.");
                        ui(TrayState::Syncing);
                        refresh_token().await;
                        match reconcile_all(&active_folders, &provider, &config).await {
                            Ok(s) => info!(
                                "Manual sync done — ↑{} ↓{} ✓{} ✗{}",
                                s.uploaded, s.downloaded, s.skipped, s.errors
                            ),
                            Err(e) => error!("Manual sync error: {}", e),
                        }
                        ui(TrayState::Idle);
                    }
                    ConfigServerEvent::ConfigUpdated(new_cfg) => {
                        info!("Configuration updated from Web UI. Applying live changes…");
                        let new_folders = new_cfg.get_sync_folders();
                        if new_folders != active_folders {
                            active_folders = new_folders;
                            let new_watch_dirs = active_folders.iter().map(|f| f.local_folder.clone()).collect();
                            if let Ok(new_rx) = start_watcher(new_watch_dirs) {
                                watch_rx = new_rx;
                            }
                        }
                        if new_cfg.sync_interval_secs != sync_interval_secs {
                            sync_interval_secs = new_cfg.sync_interval_secs;
                            info!("Periodic sync interval updated to: {}s", sync_interval_secs);
                        }
                        config = new_cfg;

                        // Trigger sync pass with new settings
                        ui(TrayState::Syncing);
                        refresh_token().await;
                        let _ = reconcile_all(&active_folders, &provider, &config).await;
                        ui(TrayState::Idle);
                    }
                }
            }

            // Tray menu command
            Some(cmd) = async_tray_rx.recv() => {
                match cmd {
                    TrayCommand::SyncNow => {
                        info!("Manual sync triggered from tray.");
                        ui(TrayState::Syncing);
                        refresh_token().await;
                        match reconcile_all(&active_folders, &provider, &config).await {
                            Ok(s) => info!(
                                "Manual sync done — ↑{} ↓{} ✓{} ✗{}",
                                s.uploaded, s.downloaded, s.skipped, s.errors
                            ),
                            Err(e) => error!("Manual sync error: {}", e),
                        }
                        ui(TrayState::Idle);
                    }
                    TrayCommand::OpenStatus => {
                        let url = get_status_server_url();
                        info!("Opening Status console: {}", url);
                        open::that(&url).ok();
                    }
                    TrayCommand::OpenFolder => {
                        if let Some(first) = active_folders.first() {
                            open::that(&first.local_folder).ok();
                        }
                    }
                    TrayCommand::OpenConfig => {
                        let url = get_config_server_url();
                        info!("Opening settings page: {}", url);
                        open::that(&url).ok();
                    }
                    TrayCommand::OpenConfigFile => {
                        let path = Config::config_path();
                        info!("Opening config file: {}", path.display());
                        open::that(&path).ok();
                    }
                    TrayCommand::RevealConfigDir => {
                        if let Some(parent) = Config::config_path().parent() {
                            open::that(parent).ok();
                        }
                    }
                    TrayCommand::SetLogLevel(lvl) => {
                        info!("Setting log level to: {}", lvl);
                        config.log_level = lvl;
                        let _ = config.save();
                        let mut w = config_arc.write().await;
                        *w = config.clone();
                    }
                    TrayCommand::SetParallelDownloads(dl) => {
                        info!("Setting max parallel downloads to: {}", dl);
                        config.max_parallel_downloads = dl;
                        let _ = config.save();
                        let mut w = config_arc.write().await;
                        *w = config.clone();
                    }
                    TrayCommand::SetParallelUploads(ul) => {
                        info!("Setting max parallel uploads to: {}", ul);
                        config.max_parallel_uploads = ul;
                        let _ = config.save();
                        let mut w = config_arc.write().await;
                        *w = config.clone();
                    }
                    TrayCommand::Quit => {
                        info!("Quit requested from tray.");
                        std::process::exit(0);
                    }
                }
            }
        }
    }
}

/// Helper: Reconcile all configured active folder pairs
async fn reconcile_all(
    folders: &[FolderPair],
    provider: &Mutex<OneDriveClient>,
    config: &Config,
) -> Result<SyncSummary> {
    let mut total = SyncSummary::default();
    let guard = provider.lock().await;

    for folder in folders {
        if !folder.enabled {
            continue;
        }

        if let Err(e) = std::fs::create_dir_all(&folder.local_folder) {
            warn!(
                "Failed to create local dir {}: {}",
                folder.local_folder.display(),
                e
            );
            continue;
        }

        let folder_provider = guard.with_remote_root(folder.remote_folder.clone());
        let db_path = StateDb::path_for_folder(&folder.local_folder);
        let db = match StateDb::open(&db_path) {
            Ok(d) => d,
            Err(e) => {
                error!(
                    "Failed to open DB for {}: {}",
                    folder.local_folder.display(),
                    e
                );
                total.errors += 1;
                continue;
            }
        };

        let engine = SyncEngine::new(
            &db,
            &folder_provider,
            &folder.local_folder,
            &config.exclude_patterns,
            config.max_parallel_downloads,
            config.max_parallel_uploads,
        );

        match engine.reconcile().await {
            Ok(s) => {
                total.uploaded += s.uploaded;
                total.downloaded += s.downloaded;
                total.skipped += s.skipped;
                total.errors += s.errors;
            }
            Err(e) => {
                error!(
                    "Reconcile error for {}: {}",
                    folder.local_folder.display(),
                    e
                );
                total.errors += 1;
            }
        }
    }

    Ok(total)
}