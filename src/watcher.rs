use anyhow::Result;
use notify_debouncer_mini::{new_debouncer, DebouncedEvent, DebounceEventResult};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

/// Events emitted by the file watcher
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// A file was created or modified at this path
    Changed(PathBuf),
    /// A file was deleted at this path
    Deleted(PathBuf),
}

/// Start watching the given directory for changes.
/// Returns an `mpsc::Receiver<WatchEvent>` that yields events as they occur.
/// The watcher is debounced: rapid bursts of events are collapsed into one.
pub fn start_watcher(watch_dirs: Vec<PathBuf>) -> Result<mpsc::Receiver<WatchEvent>> {
    let (tx, rx) = mpsc::channel::<WatchEvent>(256);

    std::thread::spawn(move || {
        let tx_clone = tx.clone();

        // notify-debouncer-mini v0.4: DebounceEventResult = Result<Vec<DebouncedEvent>, notify::Error>
        let (inner_tx, inner_rx) = std::sync::mpsc::channel::<DebounceEventResult>();

        let mut debouncer = new_debouncer(Duration::from_millis(500), inner_tx)
            .expect("Failed to create file watcher debouncer");

        for watch_dir in &watch_dirs {
            if watch_dir.exists() {
                if let Err(e) = debouncer
                    .watcher()
                    .watch(watch_dir, notify::RecursiveMode::Recursive)
                {
                    error!("Failed to start watching {}: {}", watch_dir.display(), e);
                } else {
                    info!("File watcher started on: {}", watch_dir.display());
                }
            }
        }

        for result in inner_rx {
            match result {
                Ok(events) => {
                    for event in events {
                        if let Some(e) = map_event(event) {
                            let _ = tx_clone.blocking_send(e);
                        }
                    }
                }
                // In notify-debouncer-mini v0.4, the error type is a single notify::Error
                Err(err) => {
                    error!("Watcher error: {:?}", err);
                }
            }
        }
    });

    Ok(rx)
}

/// Map a debounced FS event to our WatchEvent.
/// DebouncedEventKind only has `Any` / `AnyContinuous` — we infer
/// create/modify vs. delete by checking whether the path still exists.
fn map_event(event: DebouncedEvent) -> Option<WatchEvent> {
    if event.path.exists() {
        Some(WatchEvent::Changed(event.path))
    } else {
        Some(WatchEvent::Deleted(event.path))
    }
}
