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
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS, EventLoopWindowTargetExtMacOS};
use tao::window::{Window, WindowBuilder};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use wry::WebViewBuilder;

use cloud::onedrive::OneDriveClient;
use config::{Config, FolderPair};
use config_server::{
    get_config_server_port, start_config_server, AppLogWriter, ConfigServerEvent,
};
use state_db::StateDb;
use sync_engine::{SyncEngine, SyncSummary};
use tray::{build_tray, update_tray, TrayCommand, TrayState};
use watcher::{start_watcher, WatchEvent};

/// Events sent to the main (UI) thread.
/// Combines tray-state updates and webview-open requests.
#[derive(Debug, Clone)]
enum MainEvent {
    /// Update the tray icon / tooltip
    TrayState(TrayState),
    /// Open a native webview window for the given tab ("status" or "config")
    OpenWebview { tab: String },
}

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
    let mut event_loop = EventLoopBuilder::<MainEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory); // menu bar only, no Dock icon
    let proxy = event_loop.create_proxy();

    let folders = config.get_sync_folders();
    let local_folder_str = folders
        .first()
        .map(|f| f.local_folder.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut config = Some(config);
    let mut tray = None;

    // Keep a single webview window alive so it can be shown/hidden instantly
    let mut webview_window: Option<(Window, wry::WebView)> = None;

    event_loop.run(move |event, event_loop_wt, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            // The tray must be created inside the running loop, on the main thread
            Event::NewEvents(StartCause::Init) => {
                let (t, tray_cmd_rx) =
                    build_tray(local_folder_str.clone()).expect("failed to build tray icon");
                tray = Some(t);

                let cfg = config.take().expect("config already taken");
                let (sync_now_tx, sync_now_rx) = tokio::sync::mpsc::channel::<()>(8);
                let primary_folder = Arc::new(std::sync::RwLock::new(
                    cfg.get_sync_folders().first().map(|f| f.local_folder.clone()).unwrap_or_default()
                ));

                // Dedicated thread: executes all UI tray commands IMMEDIATELY (sub-millisecond)
                // so menu clicks (Status, Preferences, Finder) are NEVER blocked by network/sync operations
                let proxy_ui = proxy.clone();
                let primary_folder_ui = Arc::clone(&primary_folder);
                let sync_now_tx_ui = sync_now_tx.clone();
                std::thread::spawn(move || {
                    while let Ok(cmd) = tray_cmd_rx.recv() {
                        match cmd {
                            TrayCommand::OpenStatus => {
                                info!("Opening Status console in native webview");
                                proxy_ui.send_event(MainEvent::OpenWebview {
                                    tab: "status".to_string(),
                                }).ok();
                            }
                            TrayCommand::OpenConfig => {
                                info!("Opening Preferences in native webview");
                                proxy_ui.send_event(MainEvent::OpenWebview {
                                    tab: "config".to_string(),
                                }).ok();
                            }
                            TrayCommand::OpenFolder => {
                                if let Ok(folder) = primary_folder_ui.read() {
                                    if folder.as_os_str().len() > 0 {
                                        open::that(&*folder).ok();
                                    }
                                }
                            }
                            TrayCommand::RevealConfigDir => {
                                if let Some(parent) = Config::config_path().parent() {
                                    open::that(parent).ok();
                                }
                            }
                            TrayCommand::Quit => {
                                info!("Quit requested from tray.");
                                std::process::exit(0);
                            }
                            TrayCommand::SyncNow => {
                                info!("Manual sync triggered from tray.");
                                let _ = sync_now_tx_ui.blocking_send(());
                            }
                        }
                    }
                });

                let proxy = proxy.clone();
                let primary_folder_sync = Arc::clone(&primary_folder);

                // Everything async runs on a background thread
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
                    if let Err(e) = rt.block_on(run(cfg, sync_now_rx, primary_folder_sync, proxy)) {
                        error!("Fatal error: {e:#}");
                        std::process::exit(1);
                    }
                });
            }

            // Events sent from the async sync thread
            Event::UserEvent(main_event) => {
                match main_event {
                    MainEvent::TrayState(state) => {
                        if let Some(t) = &tray {
                            update_tray(t, &state).ok();
                        }
                    }
                    MainEvent::OpenWebview { tab } => {
                        let port = get_config_server_port();
                        let url = format!("http://127.0.0.1:{}/#{}", port, tab);

                        // Temporarily promote to Regular so macOS brings the window to the front
                        event_loop_wt.set_activation_policy_at_runtime(ActivationPolicy::Regular);

                        if let Some((win, wv)) = webview_window.as_ref() {
                            win.set_visible(true);
                            win.set_focus();
                            #[cfg(target_os = "macos")]
                            unsafe {
                                use cocoa::appkit::{NSApplication, NSWindow};
                                use tao::platform::macos::WindowExtMacOS;
                                let ns_win = win.ns_window() as cocoa::base::id;
                                ns_win.makeKeyAndOrderFront_(cocoa::base::nil);
                                ns_win.orderFrontRegardless();
                                cocoa::appkit::NSApp().activateIgnoringOtherApps_(cocoa::base::YES);
                            }
                            let js = format!("if (typeof switchTab === 'function') switchTab('{}');", tab);
                            wv.evaluate_script(&js).ok();
                        } else {
                            let window = WindowBuilder::new()
                                .with_title("CloudSync")
                                .with_inner_size(LogicalSize::new(980.0, 720.0))
                                .with_min_inner_size(LogicalSize::new(760.0, 500.0))
                                .build(event_loop_wt)
                                .expect("failed to create webview window");

                            let webview = WebViewBuilder::new(&window)
                                .with_url(&url)
                                .with_devtools(cfg!(debug_assertions))
                                .build()
                                .expect("failed to create webview");

                            window.set_visible(true);
                            window.set_focus();
                            #[cfg(target_os = "macos")]
                            unsafe {
                                use cocoa::appkit::{NSApplication, NSWindow};
                                use tao::platform::macos::WindowExtMacOS;
                                let ns_win = window.ns_window() as cocoa::base::id;
                                ns_win.makeKeyAndOrderFront_(cocoa::base::nil);
                                ns_win.orderFrontRegardless();
                                cocoa::appkit::NSApp().activateIgnoringOtherApps_(cocoa::base::YES);
                            }

                            webview_window = Some((window, webview));
                        }
                    }
                }
            }

            // Handle webview window close — hide instead of destroying
            Event::WindowEvent {
                event: tao::event::WindowEvent::CloseRequested,
                window_id,
                ..
            } => {
                // Hide the window instead of destroying it for instant re-open
                if let Some((win, _)) = webview_window.as_ref() {
                    if win.id() == window_id {
                        win.set_visible(false);
                        event_loop_wt.set_activation_policy_at_runtime(ActivationPolicy::Accessory);
                    }
                }
            }

            _ => {}
        }
    })
}

async fn run(
    mut config: Config,
    mut sync_now_rx: tokio::sync::mpsc::Receiver<()>,
    primary_folder_sync: Arc<std::sync::RwLock<std::path::PathBuf>>,
    proxy: EventLoopProxy<MainEvent>,
) -> Result<()> {
    // Sends a tray-state change to the main thread
    let ui = |s: TrayState| {
        proxy.send_event(MainEvent::TrayState(s)).ok();
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
                            if let Some(first) = active_folders.first() {
                                if let Ok(mut lock) = primary_folder_sync.write() {
                                    *lock = first.local_folder.clone();
                                }
                            }
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

            // Manual sync triggered from tray menu
            Some(_) = sync_now_rx.recv() => {
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