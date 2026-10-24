use anyhow::Result;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    TrayIcon, TrayIconBuilder,
};

/// The current sync state shown in the tray icon
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayState {
    Idle,
    Syncing,
    Error(String),
}

/// Menu item IDs (used to identify which item was clicked)
pub const MENU_STATUS: &str = "status";
pub const MENU_SYNC_NOW: &str = "sync_now";
pub const MENU_QUIT: &str = "quit";
pub const MENU_OPEN_FOLDER: &str = "open_folder";
pub const MENU_CONFIG_OPEN: &str = "config_open";
pub const MENU_CONFIG_OPEN_FILE: &str = "config_open_file";
pub const MENU_CONFIG_REVEAL: &str = "config_reveal";
pub const MENU_LOG_TRACE: &str = "log_trace";
pub const MENU_LOG_DEBUG: &str = "log_debug";
pub const MENU_LOG_INFO: &str = "log_info";
pub const MENU_LOG_WARN: &str = "log_warn";
pub const MENU_LOG_ERROR: &str = "log_error";
pub const MENU_DL_2: &str = "dl_2";
pub const MENU_DL_4: &str = "dl_4";
pub const MENU_DL_8: &str = "dl_8";
pub const MENU_UL_1: &str = "ul_1";
pub const MENU_UL_2: &str = "ul_2";
pub const MENU_UL_4: &str = "ul_4";

/// Commands emitted from the tray menu to the main loop
#[derive(Debug, Clone)]
pub enum TrayCommand {
    OpenStatus,
    SyncNow,
    Quit,
    OpenFolder,
    OpenConfig,
    OpenConfigFile,
    RevealConfigDir,
    SetLogLevel(String),
    SetParallelDownloads(usize),
    SetParallelUploads(usize),
}

/// Build the system tray icon and menu. Returns:
/// - The `TrayIcon` handle (must be kept alive for the duration of the app)
/// - A receiver for `TrayCommand` events
pub fn build_tray(
    _local_folder: String,
) -> Result<(TrayIcon, std::sync::mpsc::Receiver<TrayCommand>)> {
    let (tx, rx) = std::sync::mpsc::channel::<TrayCommand>();

    // Load the idle icon
    let icon = load_icon(TrayState::Idle)?;

    let menu = Menu::new();
    let status_item = MenuItem::with_id(MENU_STATUS, "Status", true, None);
    let sync_item = MenuItem::with_id(MENU_SYNC_NOW, "Sync Now", true, None);
    let folder_item = MenuItem::with_id(MENU_OPEN_FOLDER, "Open Sync Folder", true, None);
    let quit_item = MenuItem::with_id(MENU_QUIT, "Quit CloudSync", true, None);

    // ── Config submenu ────────────────────────────────────────────────────────
    let config_menu = Submenu::with_id("config", "Config", true);
    let open_config_item = MenuItem::with_id(MENU_CONFIG_OPEN, "Open Settings…", true, None);
    let open_file_item = MenuItem::with_id(MENU_CONFIG_OPEN_FILE, "Open config.toml", true, None);
    let reveal_item = MenuItem::with_id(MENU_CONFIG_REVEAL, "Reveal in Finder", true, None);

    let log_menu = Submenu::with_id("log_level", "Log Level", true);
    let log_trace = MenuItem::with_id(MENU_LOG_TRACE, "Trace", true, None);
    let log_debug = MenuItem::with_id(MENU_LOG_DEBUG, "Debug", true, None);
    let log_info = MenuItem::with_id(MENU_LOG_INFO, "Info", true, None);
    let log_warn = MenuItem::with_id(MENU_LOG_WARN, "Warn", true, None);
    let log_error = MenuItem::with_id(MENU_LOG_ERROR, "Error", true, None);
    log_menu.append_items(&[&log_trace, &log_debug, &log_info, &log_warn, &log_error])?;

    let dl_menu = Submenu::with_id("max_dl", "Max Downloads", true);
    let dl_2 = MenuItem::with_id(MENU_DL_2, "2 Downloads", true, None);
    let dl_4 = MenuItem::with_id(MENU_DL_4, "4 Downloads", true, None);
    let dl_8 = MenuItem::with_id(MENU_DL_8, "8 Downloads", true, None);
    dl_menu.append_items(&[&dl_2, &dl_4, &dl_8])?;

    let ul_menu = Submenu::with_id("max_ul", "Max Uploads", true);
    let ul_1 = MenuItem::with_id(MENU_UL_1, "1 Upload", true, None);
    let ul_2 = MenuItem::with_id(MENU_UL_2, "2 Uploads", true, None);
    let ul_4 = MenuItem::with_id(MENU_UL_4, "4 Uploads", true, None);
    ul_menu.append_items(&[&ul_1, &ul_2, &ul_4])?;

    config_menu.append_items(&[
        &open_config_item,
        &PredefinedMenuItem::separator(),
        &log_menu,
        &dl_menu,
        &ul_menu,
        &PredefinedMenuItem::separator(),
        &open_file_item,
        &reveal_item,
    ])?;

    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &sync_item,
        &folder_item,
        &config_menu,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("CloudSync — Idle")
        .with_icon(icon)
        .with_icon_as_template(true)
        .build()?;

    // Spawn a thread to forward menu events into our command channel
    let tx_clone = tx.clone();
    std::thread::spawn(move || {
        let event_rx = MenuEvent::receiver();
        loop {
            if let Ok(event) = event_rx.recv() {
                let cmd = match event.id().0.as_str() {
                    MENU_STATUS          => Some(TrayCommand::OpenStatus),
                    MENU_SYNC_NOW        => Some(TrayCommand::SyncNow),
                    MENU_QUIT            => Some(TrayCommand::Quit),
                    MENU_OPEN_FOLDER     => Some(TrayCommand::OpenFolder),
                    MENU_CONFIG_OPEN     => Some(TrayCommand::OpenConfig),
                    MENU_CONFIG_OPEN_FILE=> Some(TrayCommand::OpenConfigFile),
                    MENU_CONFIG_REVEAL   => Some(TrayCommand::RevealConfigDir),
                    MENU_LOG_TRACE       => Some(TrayCommand::SetLogLevel("trace".to_string())),
                    MENU_LOG_DEBUG       => Some(TrayCommand::SetLogLevel("debug".to_string())),
                    MENU_LOG_INFO        => Some(TrayCommand::SetLogLevel("info".to_string())),
                    MENU_LOG_WARN        => Some(TrayCommand::SetLogLevel("warn".to_string())),
                    MENU_LOG_ERROR       => Some(TrayCommand::SetLogLevel("error".to_string())),
                    MENU_DL_2            => Some(TrayCommand::SetParallelDownloads(2)),
                    MENU_DL_4            => Some(TrayCommand::SetParallelDownloads(4)),
                    MENU_DL_8            => Some(TrayCommand::SetParallelDownloads(8)),
                    MENU_UL_1            => Some(TrayCommand::SetParallelUploads(1)),
                    MENU_UL_2            => Some(TrayCommand::SetParallelUploads(2)),
                    MENU_UL_4            => Some(TrayCommand::SetParallelUploads(4)),
                    _                    => None,
                };
                if let Some(c) = cmd {
                    let _ = tx_clone.send(c);
                }
            }
        }
    });

    Ok((tray, rx))
}

/// Update the tray icon and tooltip to reflect the current state
pub fn update_tray(tray: &TrayIcon, state: &TrayState) -> Result<()> {
    let icon = load_icon(state.clone())?;
    tray.set_icon(Some(icon))?;

    let tooltip = match state {
        TrayState::Idle          => "CloudSync — Idle".to_string(),
        TrayState::Syncing       => "CloudSync — Syncing…".to_string(),
        TrayState::Error(msg)    => format!("CloudSync — Error: {}", msg),
    };
    tray.set_tooltip(Some(&tooltip))?;

    Ok(())
}

/// Load the appropriate tray icon for the given state.
/// Falls back to a 16×16 solid colored image if asset files aren't bundled.
fn load_icon(state: TrayState) -> Result<tray_icon::Icon> {
    // Try to load PNG assets embedded at compile time
    let png_bytes: &[u8] = match state {
        TrayState::Idle    => include_bytes!("../assets/icon_idle.png"),
        TrayState::Syncing => include_bytes!("../assets/icon_syncing.png"),
        TrayState::Error(_)=> include_bytes!("../assets/icon_error.png"),
    };

    let img = image::load_from_memory(png_bytes)?.into_rgba8();
    let (w, h) = (img.width(), img.height());
    let icon = tray_icon::Icon::from_rgba(img.into_raw(), w, h)?;
    Ok(icon)
}
