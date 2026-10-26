use anyhow::Result;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
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
pub const MENU_CONFIG_REVEAL: &str = "config_reveal";

/// Commands emitted from the tray menu to the main loop
#[derive(Debug, Clone)]
pub enum TrayCommand {
    OpenStatus,
    SyncNow,
    Quit,
    OpenFolder,
    OpenConfig,
    RevealConfigDir,
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

    let pref_item = MenuItem::with_id(MENU_CONFIG_OPEN, "Preferences…", true, None);
    let reveal_item = MenuItem::with_id(MENU_CONFIG_REVEAL, "Reveal Config In Finder", true, None);

    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &sync_item,
        &folder_item,
        &PredefinedMenuItem::separator(),
        &pref_item,
        &reveal_item,
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
                    MENU_CONFIG_REVEAL   => Some(TrayCommand::RevealConfigDir),
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
