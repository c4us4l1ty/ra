//! Indicator modules: the buttons on a bar.
//!
//! Each one is a subscription to a service. No polling, no timers, no fetching
//! on click. Clicking any of them opens the quick-settings popup rather than
//! spawning a helper application, which is the whole point of the panel.
//!
//! Every indicator shares one shape, so the repetition here is structural
//! rather than accidental: build a button, describe how to read the current
//! `(icon, text)` from a service, and bind that to the service's stream.

use gtk4::glib;
use gtk4::prelude::*;
use serde::Deserialize;

use crate::config::ModuleSpec;
use crate::dbus::Subscription;
use crate::module::popup_button;
use crate::service;
use crate::ui;
use crate::{debug, warn};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndicatorArgs {
    /// Show the text label next to the icon.
    #[serde(default)]
    show_label: Option<bool>,
    /// Icon size in pixels.
    #[serde(default)]
    size: Option<i32>,
}

/// The three widgets an indicator owns, plus its resolved options.
struct Indicator {
    button: gtk4::Button,
    image: gtk4::Image,
    label: gtk4::Label,
    show_label: bool,
}

impl Indicator {
    fn build(spec: &ModuleSpec, tooltip: &str, show_label_default: bool) -> Self {
        let args: IndicatorArgs = spec.args_as();
        let size = args.size.unwrap_or(17);
        let show_label = args.show_label.unwrap_or(show_label_default);

        let button = popup_button(&["ra-bar-button"], tooltip);
        let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let image = ui::icon("application-x-executable-symbolic", size);
        image.add_css_class("ra-bar-icon");
        content.append(&image);
        let label = gtk4::Label::new(None);
        label.add_css_class("ra-bar-label");
        label.set_visible(show_label);
        content.append(&label);
        button.set_child(Some(&content));

        Self { button, image, label, show_label }
    }

    /// Repaint from a fresh read of the service.
    ///
    /// This runs on every state change, which is why it is also used for the
    /// very first paint: a panel that shows an empty bar until the first signal
    /// arrives looks broken on a slow machine.
    fn paint(&self, fetch: &dyn Fn() -> (&'static str, String)) {
        let (icon, text) = fetch();
        self.image.set_icon_name(Some(icon));
        self.label.set_text(&text);
        self.label.set_visible(self.show_label);
        self.button.set_tooltip_text(Some(&text));
    }

    fn finish(self) -> gtk4::Widget {
        self.button.upcast()
    }
}

/// Bind an indicator to a service stream. Returns the subscription so the
/// caller can keep it alive for as long as the widget.
fn bind<T: 'static>(
    indicator: &Indicator,
    fetch: impl Fn() -> (&'static str, String) + 'static,
    subscribe: impl FnOnce(Box<dyn Fn(&T)>) -> Subscription,
) {
    indicator.paint(&fetch);
    let button = indicator.button.clone();
    let image = indicator.image.clone();
    let label = indicator.label.clone();
    let show_label = indicator.show_label;
    let _ = subscribe(Box::new(move |_| {
        let (icon, text) = fetch();
        image.set_icon_name(Some(icon));
        label.set_text(&text);
        label.set_visible(show_label);
        button.set_tooltip_text(Some(&text));
    }));
}

// -- network -----------------------------------------------------------------

pub fn network(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let net = service::network::get();
    let indicator = Indicator::build(spec, "Network", false);
    // The fetch closure owns its own handle so it can outlive this function.
    let fetch = {
        let net = net.clone();
        move || {
            let s = net.state();
            (s.icon(), s.label())
        }
    };
    let _keep = bind(&indicator, fetch, move |handler| net.on_change(handler));
    Some(indicator.finish())
}

// -- bluetooth ---------------------------------------------------------------

pub fn bluetooth(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let bt = service::bluetooth::get();
    let indicator = Indicator::build(spec, "Bluetooth", false);
    let fetch = {
        let bt = bt.clone();
        move || {
            let s = bt.state();
            (s.icon(), s.label())
        }
    };
    let _keep = bind(&indicator, fetch, move |handler| bt.on_change(handler));
    Some(indicator.finish())
}

// -- volume ------------------------------------------------------------------

pub fn volume(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let audio = service::audio::get();
    let indicator = Indicator::build(spec, "Volume", false);
    let fetch = {
        let audio = audio.clone();
        move || {
            let s = audio.state();
            // Name the device in the tooltip: "Volume — 40%" alone is ambiguous
            // when several outputs exist.
            match s.default_sink_description() {
                Some(name) => (s.icon(), format!("{} · {}", s.label(), name)),
                None => (s.icon(), s.label()),
            }
        }
    };
    let _keep = bind(&indicator, fetch, {
        let audio = audio.clone();
        move |handler| audio.on_change(handler)
    });

    // Middle click toggles mute, the one action worth having without opening
    // anything. Left click still opens the popup via `popup_button`.
    let mute_target = audio.clone();
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(gtk4::gdk::BUTTON_MIDDLE);
    gesture.connect_released(move |_, _, _, _| {
        mute_target.toggle_mute();
    });
    indicator.button.add_controller(gesture);

    Some(indicator.finish())
}

// -- brightness --------------------------------------------------------------

pub fn brightness(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let bright = service::brightness::get();
    let indicator = Indicator::build(spec, "Brightness", false);
    let fetch = {
        let bright = bright.clone();
        move || {
            let s = bright.state();
            (s.icon(), s.label())
        }
    };
    let _keep = bind(&indicator, fetch, move |handler| bright.on_change(handler));
    Some(indicator.finish())
}

// -- battery -----------------------------------------------------------------

pub fn battery(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let power = service::power::get();
    let indicator = Indicator::build(spec, "Battery", true);
    let fetch = {
        let power = power.clone();
        move || {
            let s = power.battery();
            (s.icon(), s.label())
        }
    };
    let _keep = bind(&indicator, fetch, move |handler| power.on_battery_change(handler));
    Some(indicator.finish())
}

// -- power profile -----------------------------------------------------------

pub fn power_profile(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let power = service::power::get();
    let indicator = Indicator::build(spec, "Power mode", false);
    let fetch = {
        let power = power.clone();
        move || {
            let s = power.profile();
            (s.icon(), s.label())
        }
    };
    let _keep = bind(&indicator, fetch, {
        let power = power.clone();
        move |handler| power.on_profile_change(handler)
    });

    // Only offer to switch when there is something to switch to.
    let button = indicator.button.clone();
    power.on_profile_change(move |s| button.set_sensitive(s.profiles.len() > 1));

    Some(indicator.finish())
}

// -- aggregate ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AggregateArgs {
    /// Which indicators to show, in order.
    #[serde(default = "default_indicators")]
    indicators: Vec<String>,
    /// Show a compact `75%` label beside each icon.
    #[serde(default)]
    show_label: bool,
    /// Icon size.
    #[serde(default = "default_size")]
    size: i32,
}

fn default_indicators() -> Vec<String> {
    ["network", "volume", "battery"].iter().map(|s| s.to_string()).collect()
}

fn default_size() -> i32 {
    16
}

impl Default for AggregateArgs {
    fn default() -> Self {
        Self { indicators: default_indicators(), show_label: false, size: default_size() }
    }
}

/// Several indicators inside one button, matching the reference design where
/// the whole status cluster is a single target.
///
/// The icons are non-interactive children of one button rather than nested
/// buttons, because nested buttons are not focusable and break keyboard
/// navigation.
pub fn aggregate(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let args: AggregateArgs = spec.args_as();
    let button = popup_button(&["ra-bar-button"], "Quick settings");
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    button.set_child(Some(&row));

    let mut entries: Vec<(gtk4::Image, gtk4::Label, String)> = Vec::new();
    for name in &args.indicators {
        let fallback = match name.as_str() {
            "network" => "network-offline-symbolic",
            "bluetooth" => "bluetooth-disabled-symbolic",
            "volume" => "audio-volume-medium-symbolic",
            "brightness" => "display-brightness-symbolic",
            "battery" => "battery-missing-symbolic",
            "power-profile" => "power-plug-symbolic",
            other => {
                warn!("quick-settings aggregate: unknown indicator `{other}`");
                continue;
            }
        };
        let image = ui::icon(fallback, args.size);
        let label = gtk4::Label::new(None);
        label.add_css_class("ra-bar-label");
        label.set_visible(args.show_label);
        row.append(&image);
        row.append(&label);
        entries.push((image, label, name.clone()));
    }

    // Paint once so the bar is correct before the first signal arrives.
    repaint(&entries);

    // One subscription per service; each repaints every icon it owns, because
    // a single signal can carry changes for more than one indicator.
    // One subscription per service. Each gets its own copy of the row because
    // a single signal can carry a change for any of these indicators.
    let _keep: Vec<Subscription> = vec![
        service::network::get().on_change({
            let owned = entries.clone();
            move |_| repaint(&owned)
        }),
        service::bluetooth::get().on_change({
            let owned = entries.clone();
            move |_| repaint(&owned)
        }),
        service::audio::get().on_change({
            let owned = entries.clone();
            move |_| repaint(&owned)
        }),
        service::brightness::get().on_change({
            let owned = entries.clone();
            move |_| repaint(&owned)
        }),
        service::power::get().on_battery_change({
            let owned = entries.clone();
            move |_| repaint(&owned)
        }),
        service::power::get().on_profile_change({
            let owned = entries.clone();
            move |_| repaint(&owned)
        }),
    ];

    debug!("quick-settings aggregate with {} indicator(s)", entries.len());
    Some(button.upcast())
}

fn repaint(entries: &[(gtk4::Image, gtk4::Label, String)]) {
    for (image, label, name) in entries {
        let (icon, text) = lookup(name);
        image.set_icon_name(Some(icon));
        label.set_text(&text);
    }
}

/// Current icon and label for one aggregate indicator. Returns an owned
/// `String` so nothing has to be leaked to satisfy a `'static` return type.
fn lookup(name: &str) -> (&'static str, String) {
    match name {
        "network" => {
            let svc = service::network::get();
            let s = svc.state();
            (s.icon(), s.label())
        }
        "bluetooth" => {
            let svc = service::bluetooth::get();
            let s = svc.state();
            (s.icon(), s.label())
        }
        "volume" => {
            let svc = service::audio::get();
            let s = svc.state();
            (s.icon(), s.label())
        }
        "brightness" => {
            let svc = service::brightness::get();
            let s = svc.state();
            (s.icon(), s.label())
        }
        "battery" => {
            let svc = service::power::get();
            let s = svc.battery();
            (s.icon(), s.label())
        }
        _ => {
            let svc = service::power::get();
            let s = svc.profile();
            (s.icon(), s.label())
        }
    }
}

/// Kept so `glib` stays referenced when the file is compiled without any
/// gesture controllers (the import is used by `gtk4::GestureClick`).
const _: fn() = || {
    let _ = glib::Propagation::Proceed;
};