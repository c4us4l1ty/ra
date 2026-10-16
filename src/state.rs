//! Persisted runtime state.
//!
//! Toggles that represent a *mode* rather than a property of some daemon -
//! Do Not Disturb, night light, dark mode - have nowhere to live: no D-Bus
//! service on XFCE stores them, and they must survive a logout. They go in a
//! single small TOML file under `$XDG_STATE_HOME/ra/state.toml`.
//!
//! This is deliberately not a database and not a settings library. It is
//! read once at startup and written only when a toggle actually changes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{debug, warn};

pub struct State {
    path: PathBuf,
    values: RefCell<BTreeMap<String, String>>,
}

use std::cell::RefCell;

impl State {
    pub fn load() -> Self {
        let path = state_path();
        let values = match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<BTreeMap<String, String>>(&text) {
                Ok(map) => map,
                Err(e) => {
                    warn!("{} is malformed ({e}); starting from defaults", path.display());
                    BTreeMap::new()
                }
            },
            Err(_) => BTreeMap::new(),
        };
        Self { path, values: RefCell::new(values) }
    }

    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        match self.values.borrow().get(key) {
            Some(v) => v == "true",
            None => default,
        }
    }

    pub fn set_bool(&self, key: &str, value: bool) {
        if self.get_bool(key, !value) == value {
            return;
        }
        self.values.borrow_mut().insert(key.to_string(), value.to_string());
        self.flush();
    }

    fn flush(&self) {
        let text: String = self
            .values
            .borrow()
            .iter()
            .map(|(k, v)| format!("{k} = \"{}\"\n", v.replace('"', "\\\"")))
            .collect();
        if let Some(dir) = self.path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                warn!("cannot create {}", dir.display());
                return;
            }
        }
        // Write-then-rename so a crash mid-write cannot leave a truncated file
        // that would then be rejected as malformed on the next start.
        let tmp = self.path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &self.path)) {
            warn!("cannot write {}: {e}", self.path.display());
        } else {
            debug!("state saved to {}", self.path.display());
        }
    }
}

fn state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
            home.join(".local/state")
        });
    base.join("ra").join("state.toml")
}

// Well-known keys.
pub const KEY_DND: &str = "do-not-disturb";
pub const KEY_NIGHT_LIGHT: &str = "night-light";
pub const KEY_DARK: &str = "dark-style";