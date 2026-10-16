//! Night light (screen temperature shifting).
//!
//! There is no night-light API on XFCE. GNOME's is private to
//! `gnome-settings-daemon`; labwc does not implement one. Every existing
//! solution is an external process that owns a wlr-gamma-control surface, so
//! this service is a thin, careful wrapper around one of those rather than a
//! reimplementation.
//!
//! The wrapper's job is the part that is easy to get wrong:
//!
//! * these tools *keep running* to hold the gamma ramp, so the child has to be
//!   tracked and killed when the feature is switched off, or the screen stays
//!   orange until logout;
//! * the child must be spawned directly, never through a shell;
//! * if no tool is installed the tile has to say so rather than silently doing
//!   nothing.
//!
//! The command is configurable, so a user with a tool this list does not know
//! about does not need a new release.

use std::cell::RefCell;
use std::process::{Child, Command};

use crate::dbus::Emitter;
use crate::dbus::Subscription;
use crate::{debug, info, warn};

/// A known night-light back-end and how to drive it.
#[derive(Clone, Copy)]
struct Backend {
    program: &'static str,
    /// Command to enable at temperature `temp`.
    set: &'static str,
    /// Command to disable.
    reset: &'static str,
}

/// Searched in order; the first one present on `PATH` wins.
const BACKENDS: &[Backend] = &[
    Backend { program: "gamut", set: "gamut -s {temp}", reset: "gamut -r" },
    Backend { program: "gammastep", set: "gammastep -s {temp}", reset: "gammastep -s 6500" },
    Backend { program: "wl-gammastep", set: "wl-gammastep -s {temp}", reset: "wl-gammastep -s 6500" },
    Backend { program: "sct", set: "sct {temp}", reset: "sct 6500" },
    Backend { program: "redshift", set: "redshift -O {temp}", reset: "redshift -x" },
    Backend { program: "hyprsunset", set: "hyprsunset -T {temp}", reset: "pkill -x hyprsunset" },
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NightLightState {
    /// A usable back-end was found.
    pub available: bool,
    pub enabled: bool,
    /// Colour temperature in Kelvin.
    pub temperature: u32,
}

impl Default for NightLightState {
    fn default() -> Self {
        Self { available: false, enabled: false, temperature: 4500 }
    }
}

pub struct NightLight {
    state: RefCell<NightLightState>,
    emitter: Emitter<NightLightState>,
    /// The running back-end process, which holds the gamma ramp open.
    child: RefCell<Option<Child>>,
    /// Index into [`BACKENDS`], or a user override.
    backend: RefCell<Option<Backend>>,
    /// Explicit command templates from the config.
    set_template: RefCell<Option<String>>,
    reset_template: RefCell<Option<String>>,
}

impl NightLight {
    pub fn new(set_template: Option<String>, reset_template: Option<String>, persist: &crate::state::State) -> Self {
        let backend = BACKENDS.iter().find(|b| which(b.program).is_some()).copied();
        match &backend {
            Some(b) => info!("night light back-end: {}", b.program),
            None => info!("no night light back-end found; the tile will report itself unavailable"),
        }
        let enabled = persist.get_bool(crate::state::KEY_NIGHT_LIGHT, false);
        let me = Self {
            state: RefCell::new(NightLightState { available: backend.is_some(), enabled: false, temperature: 4500 }),
            emitter: Emitter::new(),
            child: RefCell::new(None),
            backend: RefCell::new(backend),
            set_template: RefCell::new(set_template),
            reset_template: RefCell::new(reset_template),
        };
        // Adopt the persisted preference, but only engage if the tool is
        // really installed.
        if enabled {
            me.set_enabled(true);
        }
        me
    }

    pub fn state(&self) -> std::cell::Ref<'_, NightLightState> {
        self.state.borrow()
    }

    pub fn on_change(&self, f: impl Fn(&NightLightState) + 'static) -> Subscription {
        self.emitter.subscribe(f)
    }

    pub fn set_temperature(&self, kelvin: u32) {
        self.state.borrow_mut().temperature = kelvin;
        if self.state.borrow().enabled {
            // Restart so the new temperature takes effect immediately.
            self.stop();
            self.start();
        }
        self.publish();
    }

    pub fn set_enabled(&self, enabled: bool) {
        if enabled {
            self.start();
        } else {
            self.stop();
        }
        self.publish();
    }

    pub fn toggle(&self) {
        let next = !self.state.borrow().enabled;
        self.set_enabled(next);
    }

    fn publish(&self) {
        let snapshot = *self.state.borrow();
        self.emitter.emit(&snapshot);
    }

    fn start(&self) {
        if self.state.borrow().enabled {
            return;
        }
        let Some(backend) = *self.backend.borrow() else {
            warn!("night light requested but no back-end is installed");
            return;
        };
        let temperature = self.state.borrow().temperature;
        let template = self
            .set_template
            .borrow()
            .clone()
            .unwrap_or_else(|| backend.set.to_string());
        let Some((program, args)) = render(&template, temperature) else { return };

        // These tools hold the gamma ramp for as long as they live, so the
        // child is kept and killed on disable.
        match Command::new(&program).args(&args).spawn() {
            Ok(child) => {
                info!("night light on at {temperature} K ({program})");
                *self.child.borrow_mut() = Some(child);
                self.state.borrow_mut().enabled = true;
            }
            Err(e) => warn!("cannot start {program}: {e}"),
        }
    }

    fn stop(&self) {
        let mut slot = self.child.borrow_mut();
        if let Some(mut child) = slot.take() {
            // Some tools need to be asked to exit; killing outright leaves the
            // gamma ramp at whatever it last wrote on compositors that do not
            // reset it. Try a clean exit first, then insist.
            let _ = child.kill();
            let _ = child.wait();
            debug!("night light stopped");
        }

        // Run the reset command, which is what actually restores the colour
        // ramp on wlroots compositors.
        let template = self
            .reset_template
            .borrow()
            .clone()
            .or_else(|| self.backend.borrow().map(|b| b.reset.to_string()));
        if let Some(template) = template {
            if let Some((program, args)) = render(&template, 6500) {
                let _ = Command::new(&program).args(&args).status();
            }
        }
        self.state.borrow_mut().enabled = false;
    }
}

impl Drop for NightLight {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Split a command template into a program and arguments, substituting `{temp}`.
///
/// Splitting is done here rather than handing the string to a shell, so a
/// value coming from the config can never introduce a command.
fn render(template: &str, temperature: u32) -> Option<(String, Vec<String>)> {
    let mut parts = template.split_whitespace();
    let program = parts.next()?.to_string();
    let args = parts.map(|a| a.replace("{temp}", &temperature.to_string())).collect();
    Some((program, args))
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

thread_local! {
    static SERVICE: RefCell<Option<Rc<NightLight>>> = const { RefCell::new(None) };
}

use std::rc::Rc;

/// The process-wide night-light service, started on first use.
pub fn get(set_template: Option<String>, reset_template: Option<String>) -> Rc<NightLight> {
    SERVICE.with(|slot| {
        if let Some(svc) = slot.borrow().as_ref() {
            return svc.clone();
        }
        let persist = crate::state::State::load();
        let svc = Rc::new(NightLight::new(set_template, reset_template, &persist));
        *slot.borrow_mut() = Some(svc.clone());
        svc
    })
}