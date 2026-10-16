//! Config hot reload.
//!
//! Uses GIO's `FileMonitor`, which is part of glib and therefore free in
//! dependency terms. Reloads are debounced because editors and `cp` write in
//! bursts, and several editors replace the file (write temp, rename over it),
//! which destroys the inode the monitor was watching. We re-arm the monitor
//! after every reload and treat *any* event as a reason to look again.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::config::Config;
use crate::{debug, info, warn};

/// Watches one file and calls `on_change` with a freshly parsed config whenever
/// it changes. Parse failures are reported and the last good config is kept.
pub struct Watcher {
    _monitor: gio::FileMonitor,
}

struct State {
    /// Pending debounce source; `None` when nothing is scheduled.
    pending: RefCell<Option<glib::SourceId>>,
}

/// Start watching `path`. `on_change` runs on the main loop.
///
/// Returns `None` when the file cannot be watched; the panel then runs with the
/// configuration it loaded at startup, which is the right failure mode for a
/// panel that must not be able to take the session down.
pub fn watch<F>(path: &Path, debounce_ms: u32, on_change: F) -> Option<Watcher>
where
    F: Fn(&Config) + 'static,
{
    let state = Rc::new(State { pending: RefCell::new(None) });
    arm(path.to_path_buf(), debounce_ms, Rc::new(on_change), state).map(|m| Watcher { _monitor: m })
}

fn arm<F>(path: PathBuf, debounce_ms: u32, on_change: Rc<F>, state: Rc<State>) -> Option<gio::FileMonitor>
where
    F: Fn(&Config) + 'static,
{
    let file = gio::File::for_path(&path);
    let monitor = match file.monitor_file(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE) {
        Ok(m) => m,
        Err(e) => {
            warn!("cannot watch {}: {e}", path.display());
            return None;
        }
    };

    let state_for_event = state.clone();
    let path_for_event = path.clone();

    monitor.connect_changed(move |_, _, other_file, event| {
        // Editors that save-by-rename emit MovedIn/Renamed on the *new* inode,
        // which the monitor we hold no longer watches. Re-arm on those, and on
        // anything that hints the file was replaced.
        if matches!(
            event,
            gio::FileMonitorEvent::MovedIn
                | gio::FileMonitorEvent::MovedOut
                | gio::FileMonitorEvent::Moved
                | gio::FileMonitorEvent::Renamed
                | gio::FileMonitorEvent::Deleted
                | gio::FileMonitorEvent::Created
        ) {
            let target = other_file.and_then(|f| f.path()).unwrap_or_else(|| path_for_event.clone());
            arm(target, debounce_ms, on_change.clone(), state_for_event.clone());
        }

        // `ChangesDoneHint` is the only event guaranteed after an in-place
        // write, and it is not guaranteed at all on rename, so we schedule on
        // every event and let the debounce coalesce the burst.
        let path = path_for_event.clone();
        let on_change = on_change.clone();
        let state = state_for_event.clone();
        schedule(state, debounce_ms, move || reload(&path, on_change.clone()));
    });

    info!("watching {}", path.display());
    Some(monitor)
}

fn schedule<F>(state: Rc<State>, debounce_ms: u32, run: F)
where
    F: Fn() + 'static,
{
    let mut pending = state.pending.borrow_mut();
    if let Some(id) = pending.take() {
        id.remove();
    }
    let state = state.clone();
    let id = glib::timeout_add_local_once(std::time::Duration::from_millis(u64::from(debounce_ms)), move || {
        *state.pending.borrow_mut() = None;
        run();
    });
    *pending = Some(id);
}

fn reload<F>(path: &Path, on_change: Rc<F>)
where
    F: Fn(&Config) + 'static,
{
    match std::fs::read_to_string(path) {
        Ok(text) => match Config::parse(&text) {
            Ok(cfg) => {
                debug!("config reloaded from {}", path.display());
                on_change(&cfg);
            }
            Err(e) => {
                // Keep the previous config: a half-typed line in an editor must
                // never take the panel down.
                warn!("config error, keeping previous settings:\n{e}");
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            warn!("{} disappeared, keeping previous settings", path.display());
        }
        Err(e) => warn!("cannot read {}: {e}", path.display()),
    }
}

/// Resolve the config path: `--config <path>` if given, else `$XDG_CONFIG_HOME`.
pub fn resolve_path(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(crate::config::user_config_path)
}