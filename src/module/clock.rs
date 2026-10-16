//! The clock.
//!
//! The only unavoidable timer in the panel, and deliberately cheap:
//!
//! * each tick reschedules itself against the wall clock rather than counting
//!   intervals, so it cannot drift and never fires twice for one second;
//! * the label is only re-laid-out when the formatted string actually changes,
//!   so an `HH:MM` clock does two relayouts a minute rather than sixty;
//! * `glib::DateTime` does the formatting, because GLib is already linked in
//!   and a date library for one widget would be a pointless dependency.
//!
//! A tick is a `format()` plus a string compare: about a microsecond, once a
//! second.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use serde::Deserialize;

use crate::config::ModuleSpec;
use crate::ui;
use crate::{debug, warn};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClockArgs {
    /// strftime format.
    #[serde(default = "default_format")]
    format: String,
    /// Force a 1 s tick. Defaults to true when the format includes seconds.
    #[serde(default)]
    seconds: Option<bool>,
    /// Tooltip format, usually the full date.
    #[serde(default = "default_tooltip")]
    tooltip: String,
    /// Left click runs this program.
    #[serde(default)]
    on_click: Option<Command>,
    /// Middle click runs this program.
    #[serde(default)]
    on_middle_click: Option<Command>,
}

fn default_format() -> String {
    "%a %d %b  %H:%M".to_string()
}

fn default_tooltip() -> String {
    "%A, %d %B %Y".to_string()
}

/// A program to run on click. Never passed through a shell.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

impl Command {
    fn run(&self) {
        let argv: Vec<&std::ffi::OsStr> = std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(std::ffi::OsStr::new)
            .collect();
        match gtk4::gio::Subprocess::newv(&argv, gtk4::gio::SubprocessFlags::NONE) {
            Ok(_) => debug!("ran {}", self.program),
            Err(e) => warn!("{}: {e}", self.program),
        }
    }
}

impl Default for ClockArgs {
    fn default() -> Self {
        Self {
            format: default_format(),
            seconds: None,
            tooltip: default_tooltip(),
            on_click: None,
            on_middle_click: None,
        }
    }
}

pub fn build(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let args: ClockArgs = spec.args_as();

    let button = ui::button(&["ra-bar-button"]);
    let label = gtk4::Label::new(None);
    label.add_css_class("ra-bar-label");
    button.set_child(Some(&label));

    if let Some(cmd) = args.on_click.clone() {
        button.connect_clicked(move |_| cmd.run());
    }
    if let Some(cmd) = args.on_middle_click.clone() {
        // A plain `clicked` handler cannot see the mouse button, so middle
        // click needs its own controller.
        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gtk4::gdk::BUTTON_MIDDLE);
        gesture.connect_released(move |_, _, _, _| cmd.run());
        button.add_controller(gesture);
    }

    // Repaint only when the rendered string changes.
    let last = RefCell::new(String::new());
    let format = args.format.clone();
    let tooltip_format = args.tooltip.clone();
    let tip_button = button.clone();
    let tick: Rc<RefCell<dyn Fn()>> = Rc::new(RefCell::new(move || {
        let Ok(now) = glib::DateTime::now_local() else { return };
        let Ok(text) = now.format(&format) else { return };
        let text = text.to_string();
        if *last.borrow() != text {
            label.set_text(&text);
            *last.borrow_mut() = text;
        }
        if let Ok(tip) = now.format(&tooltip_format) {
            tip_button.set_tooltip_text(Some(&tip));
        }
    }));

    // Paint immediately: waiting for the first tick would leave the clock
    // blank for up to a minute after login.
    tick.borrow_mut()();

    let with_seconds = args.seconds.unwrap_or_else(|| args.format.contains("%S"));
    schedule(tick, with_seconds);

    Some(button.upcast())
}

/// Re-arm a self-correcting tick.
///
/// Each callback re-arms itself with a delay computed from the *current* wall
/// clock, so the clock cannot drift away from the minute boundary the way a
/// plain repeating timeout eventually would.
fn schedule(tick: Rc<RefCell<dyn Fn()>>, with_seconds: bool) {
    let for_tick = tick.clone();
    glib::timeout_add_local(next_delay(with_seconds), move || {
        for_tick.borrow_mut()();
        schedule(for_tick.clone(), with_seconds);
        // This source is single-shot; the next one is the repeating timer.
        glib::ControlFlow::Break
    });
}

/// How long to wait before the next tick.
fn next_delay(with_seconds: bool) -> Duration {
    let Some(now) = glib::DateTime::now_local().ok() else {
        return Duration::from_secs(60);
    };
    let seconds = now.second() as i64;
    let micros = now.microsecond() as u64 / 1000;

    if with_seconds {
        // Aim just after the next second boundary.
        let elapsed = seconds as u64 * 1000 + micros;
        Duration::from_millis(1000u64.saturating_sub(elapsed).max(50))
    } else {
        // Aim just after the next minute boundary.
        let to_boundary = 60u64.saturating_sub(seconds as u64);
        Duration::from_millis((to_boundary * 1000 + 300).saturating_sub(micros).max(200))
    }
}