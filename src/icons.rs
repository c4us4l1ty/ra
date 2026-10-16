//! Icon names, resolved against the theme that is actually installed.
//!
//! Symbolic icon names are not a stable API. Adwaita ships
//! `battery-good-symbolic` but not `battery-medium-symbolic`; it ships no
//! `power-plug-symbolic` at all; it ships exactly one
//! `display-brightness-symbolic` where most distributions ship four; and it
//! spells dark mode `weather-clear-night-symbolic` where the name ra wants,
//! `weather-night-symbolic`, does not exist. Across a stock Fedora install, 15
//! of the 62 names this panel asks for are absent.
//!
//! GTK's answer to a name it cannot find is not an error. It draws a broken
//! image placeholder, silently, at icon size, in the middle of the bar or the
//! popup. So every icon ra asks for is a *preference list*: the first name the
//! live theme actually has wins, and the rest are progressively blunter
//! approximations of the same idea.
//!
//! Resolution happens at build time rather than being cached, because the user
//! can switch GTK themes at any moment and a cache would keep serving names
//! from the theme they just left. A lookup is a hash probe in the theme's own
//! table, and it only happens when a service reports a change.
//!
//! Names that resolve to nothing are recorded and listed by `ra --diagnose`,
//! so a missing icon is a line of output rather than a mystery box.

use std::cell::RefCell;

use gtk4::prelude::*;

use crate::debug;

/// Where an icon falls back to when nothing better is available.
///
/// Every theme ships something from the freedesktop standard icon set, so this
/// is the floor: a generic image is wrong, but it is *recognisably* wrong and
/// not a broken-image box.
const LAST_RESORT: &str = "image-missing";

/// Preference lists, keyed by the name ra asks for.
///
/// Each list is best-first and must end at a name that at least one common
/// theme has. The `LAST_RESORT` is appended automatically, so a list never has
/// to name it.
const CHAINS: &[(&str, &[&str])] = &[
    // Adwaita has the charging variants for `full`/`good`/`low`/`empty` and a
    // missing `medium`, so the mid-range steps down rather than breaking.
    // (Its `battery-level-NN-symbolic` family is complete, but reading a
    // battery from 0-100 in tens is a different idea from four named states,
    // and mixing the two looks wrong next to every other panel.)
    ("battery-medium-symbolic", &["battery-good-symbolic", "battery-symbolic"]),
    ("battery-not-charging-symbolic", &["battery-good-symbolic", "battery-symbolic"]),
    // Adwaita has no `battery-saver`; its power-profile icons are the honest
    // equivalent.
    ("battery-saver-symbolic", &["power-profile-power-saver-symbolic", "battery-symbolic"]),
    ("speedometer-symbolic", &["power-profile-performance-symbolic", "utilities-system-monitor-symbolic"]),
    // No power icons at all in Adwaita.
    ("power-plug-symbolic", &["power-profile-balanced-symbolic", "emblem-system-symbolic"]),
    ("power-plug-off-symbolic", &["battery-empty-symbolic", "emblem-system-symbolic"]),
    // One brightness icon ships; every level collapses onto it.
    ("display-brightness-low-symbolic", &["display-brightness-symbolic", "video-display-symbolic"]),
    ("display-brightness-medium-symbolic", &["display-brightness-symbolic", "video-display-symbolic"]),
    ("display-brightness-high-symbolic", &["display-brightness-symbolic", "video-display-symbolic"]),
    ("display-brightness-off-symbolic", &["video-display-symbolic"]),
    // Adwaita spells it `weather-clear-night`.
    ("weather-night-symbolic", &["weather-clear-night-symbolic", "night-light-symbolic"]),
    ("weather-clear-day-symbolic", &["weather-clear-symbolic", "daytime-sunrise-symbolic"]),
    // Adwaita says `offline`, not `off`.
    ("network-wireless-off-symbolic", &["network-wireless-offline-symbolic", "network-wireless-symbolic"]),
    // Adwaita has no `emblem-ok`; the checked state is what this is marking.
    ("emblem-ok-symbolic", &["object-select-symbolic", "emblem-default-symbolic"]),
    // Adwaita says `lock-screen`.
    ("system-lock-symbolic", &["system-lock-screen-symbolic", "auth-sim-locked-symbolic"]),
];

thread_local! {
    /// Names that fell all the way through their chain, for `--diagnose`.
    static UNRESOLVED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Resolve `preferred` to a name the current icon theme actually has.
///
/// Always returns something drawable.
pub fn resolve(preferred: &str) -> String {
    if theme_has(preferred) {
        return preferred.to_string();
    }
    // Walk the preference list for this name.
    if let Some(chain) = CHAINS.iter().find(|(name, _)| *name == preferred) {
        for candidate in chain.1 {
            if theme_has(candidate) {
                debug!("icon `{preferred}` is absent; using `{candidate}`");
                return (*candidate).to_string();
            }
        }
    }
    UNRESOLVED.with(|missing| {
        let mut missing = missing.borrow_mut();
        if !missing.iter().any(|n| n == preferred) {
            missing.push(preferred.to_string());
        }
    });
    LAST_RESORT.to_string()
}

/// Build an image from a symbolic icon name, resolved against the theme.
pub fn image(preferred: &str, size: i32) -> gtk4::Image {
    gtk4::Image::builder().icon_name(resolve(preferred)).pixel_size(size).build()
}

/// Point an existing image at a name, resolving it first.
///
/// `Image::set_icon_name` is the obvious thing to call and the wrong one: it
/// takes the name verbatim, so an absent icon becomes a broken-image box with
/// no log line and no way to tell afterwards. Everything in ra goes through
/// here instead.
pub fn set(image: &gtk4::Image, preferred: &str) {
    image.set_icon_name(Some(&resolve(preferred)));
}

/// Give a button an icon, resolving the name first.
///
/// `Button::set_icon_name` has no resolving sibling, so replace the image
/// outright. The button is expected to be empty.
pub fn set_on_button(button: &gtk4::Button, preferred: &str, size: i32) {
    let image = image(preferred, size);
    button.set_child(Some(&image));
}

/// Every name this session could not resolve, sorted, for `--diagnose`.
pub fn unresolved() -> Vec<String> {
    UNRESOLVED.with(|missing| {
        let mut names = missing.borrow().clone();
        names.sort();
        names.dedup();
        names
    })
}

fn theme_has(name: &str) -> bool {
    let Some(display) = gtk4::gdk::Display::default() else { return false };
    let theme = gtk4::IconTheme::for_display(&display);
    theme.has_icon(name)
}
