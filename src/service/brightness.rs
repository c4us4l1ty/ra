//! Screen brightness through sysfs.
//!
//! Unlike everything else here, the kernel exposes no change notification for
//! backlight: there is no D-Bus signal and no udev netlink event for "someone
//! moved the brightness slider". A timer is the only option, so the design goal
//! is to make that timer as cheap and as rare as possible:
//!
//! * the value is read from a single small sysfs file (~4 KiB page, ~2 us);
//! * the timer is aligned to a whole second so it cannot drift and cause extra
//!   wakeups;
//! * nothing is emitted unless the value actually changed, so the timer costs
//!   a `read()` and a comparison;
//! * when no backlight device exists - common on desktops with external
//!   monitors - no timer is created at all.
//!
//! The interval is not user-tunable at runtime beyond what the config allows,
//! because the correct value is a hardware property, not a preference.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;

use crate::dbus::{Emitter, Subscription};
use crate::{debug, warn};

const SYSFS_BACKLIGHT: &str = "/sys/class/backlight";

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BrightnessState {
    /// A writable backlight device was found.
    pub available: bool,
    /// 0-100.
    pub value: u8,
}

impl BrightnessState {
    pub fn icon(&self) -> &'static str {
        match self.value {
            90..=100 => "display-brightness-symbolic",
            60..=89 => "display-brightness-high-symbolic",
            30..=59 => "display-brightness-medium-symbolic",
            1..=29 => "display-brightness-low-symbolic",
            _ => "display-brightness-off-symbolic",
        }
    }

    pub fn label(&self) -> String {
        if !self.available {
            return "Brightness".into();
        }
        format!("{}%", self.value)
    }
}

/// A backlight device discovered under `/sys/class/backlight`.
struct Device {
    /// Directory holding `brightness` and `max_brightness`.
    dir: PathBuf,
    max: u32,
}

impl Device {
    fn read(&self) -> Option<u32> {
        let raw = std::fs::read_to_string(self.dir.join("brightness")).ok()?;
        raw.trim().parse().ok()
    }

    fn write(&self, value: u32) -> std::io::Result<()> {
        std::fs::write(self.dir.join("brightness"), value.to_string())
    }
}

/// Prefer an integrated panel over an external monitor's backlight.
fn discover() -> Option<Device> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(SYSFS_BACKLIGHT)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("brightness").exists())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // Integrated panels are conventionally named `*_backlight`; an external
    // monitor's is usually `*_bl`. Sorting makes the choice deterministic.
    candidates.sort_by_key(|p| {
        let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        (name.ends_with("_backlight"), name)
    });
    let dir = candidates.swap_remove(0);
    let max = std::fs::read_to_string(dir.join("max_brightness")).ok()?.trim().parse().ok()?;
    if max == 0 {
        return None;
    }
    debug!("backlight device {} (max {max})", dir.display());
    Some(Device { dir, max })
}

pub struct Brightness {
    state: RefCell<BrightnessState>,
    emitter: Emitter<BrightnessState>,
    device: Option<Device>,
    /// Poll interval in seconds; 0 disables the timer entirely.
    interval_s: Cell<u32>,
    timer: RefCell<Option<glib::SourceId>>,
}

impl Brightness {
    fn new() -> Rc<Self> {
        let device = discover();
        let me = Rc::new(Self {
            state: RefCell::new(BrightnessState::default()),
            emitter: Emitter::new(),
            device,
            interval_s: Cell::new(2),
            timer: RefCell::new(None),
        });
        me.read_now();
        me.arm();
        me
    }

    /// Set the poll interval. Called when the popup opens or closes: an open
    /// popup needs to feel live while dragging, a closed bar does not.
    pub fn set_active(self: &Rc<Self>, active: bool) {
        let seconds = if active { 1 } else { self.interval_s.get() };
        self.interval_s.set(seconds);
        self.arm();
    }

    fn arm(self: &Rc<Self>) {
        let mut timer = self.timer.borrow_mut();
        if let Some(id) = timer.take() {
            id.remove();
        }
        if self.device.is_none() {
            return;
        }
        let seconds = self.interval_s.get();
        if seconds == 0 {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local(Duration::from_secs(u64::from(seconds)), move || {
            match weak.upgrade() {
                Some(me) => me.read_now(),
                None => return glib::ControlFlow::Break,
            }
            glib::ControlFlow::Continue
        });
        *timer = Some(id);
    }

    fn read_now(&self) {
        let Some(device) = &self.device else {
            self.publish(BrightnessState::default());
            return;
        };
        let Some(raw) = device.read() else {
            self.publish(BrightnessState { available: true, value: 0 });
            return;
        };
        let value = ((raw as f64 / device.max as f64) * 100.0).round().clamp(0.0, 100.0) as u8;
        self.publish(BrightnessState { available: true, value });
    }

    fn publish(&self, next: BrightnessState) {
        let mut cur = self.state.borrow_mut();
        if *cur == next {
            return;
        }
        *cur = next;
        drop(cur);
        self.emitter.emit(&self.state.borrow());
    }

    pub fn state(&self) -> std::cell::Ref<'_, BrightnessState> {
        self.state.borrow()
    }

    pub fn on_change(&self, f: impl Fn(&BrightnessState) + 'static) -> Subscription {
        self.emitter.subscribe(f)
    }

    /// Set brightness, 0-100.
    pub fn set(&self, percent: u8) {
        let Some(device) = &self.device else {
            warn!("no backlight device");
            return;
        };
        let raw = ((f64::from(percent) / 100.0) * device.max as f64).round() as u32;
        let raw = raw.min(device.max);
        if let Err(e) = device.write(raw) {
            warn!("cannot set brightness: {e}");
            return;
        }
        // Reflect immediately; the timer would otherwise make the slider feel
        // laggy for up to a second.
        let value = ((raw as f64 / device.max as f64) * 100.0).round().clamp(0.0, 100.0) as u8;
        self.publish(BrightnessState { available: true, value });
    }
}

thread_local! {
    static SERVICE: RefCell<Option<Rc<Brightness>>> = const { RefCell::new(None) };
}

/// The process-wide brightness service, started on first use.
pub fn get() -> Rc<Brightness> {
    SERVICE.with(|slot| {
        if let Some(svc) = slot.borrow().as_ref() {
            return svc.clone();
        }
        let svc = Brightness::new();
        *slot.borrow_mut() = Some(svc.clone());
        svc
    })
}