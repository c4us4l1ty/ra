//! Bar modules.
//!
//! A module turns one `[[panel]]` slot into a widget. Adding one is a single
//! `match` arm in [`build`] plus a `known()` entry, both compile-time checked,
//! which keeps the surface small and makes a typo in the config a build error
//! rather than a silent empty box.
//!
//! Modules come in two flavours:
//!
//! * **indicator** - subscribes to a service and repaints itself when that
//!   service changes. Never polls.
//! * **passive** - a label, a separator, a clock. Cheap by construction.
//!
//! Anything that needs the user to type (`custom` with an argument) gets its
//! input from the quick-settings surface rather than from the bar, because a
//! bar window never holds the keyboard grab.

use gtk4::prelude::*;

use crate::config::ModuleSpec;
use crate::popup;
use crate::ui;

pub mod basic;
pub mod clock;
pub mod indicators;

/// Build one module. `None` means "unknown module name".
pub fn build(name: &str, spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let widget = match name {
        // Passive.
        "label" => basic::label(spec),
        "separator" => basic::separator(spec),
        "spacer" => basic::spacer(spec),
        "clock" => clock::build(spec),
        "custom" => basic::custom(spec),
        "launcher" => basic::launcher(spec),

        // Indicators.
        "network" => indicators::network(spec),
        "bluetooth" => indicators::bluetooth(spec),
        "volume" => indicators::volume(spec),
        "brightness" => indicators::brightness(spec),
        "battery" => indicators::battery(spec),
        "power-profile" => indicators::power_profile(spec),

        // Aggregates.
        "quick-settings" => indicators::aggregate(spec),

        _ => return None,
    }?;
    Some(wrap(widget, spec))
}

/// Apply the slot's CSS classes.
fn wrap(widget: gtk4::Widget, spec: &ModuleSpec) -> gtk4::Widget {
    if let Some(class) = &spec.class {
        for c in class.split_whitespace() {
            widget.add_css_class(c);
        }
    }
    widget
}

/// Every module name this build understands, sorted, for `ra --diagnose`.
pub fn known() -> &'static [&'static str] {
    &[
        "battery",
        "bluetooth",
        "brightness",
        "clock",
        "custom",
        "label",
        "launcher",
        "network",
        "power-profile",
        "quick-settings",
        "separator",
        "spacer",
        "volume",
    ]
}

/// Report config references that do not resolve, so a typo is loud.
pub fn validate(names: &[String], strict: bool) -> Vec<String> {
    let mut problems = Vec::new();
    for name in names {
        // `custom:<id>` is a user-defined module handled by `basic::custom`.
        if name.starts_with("custom") || known().contains(&name.as_str()) {
            continue;
        }
        let message = format!("unknown module `{name}` (known: {})", known().join(", "));
        if strict {
            crate::error!("{message}");
        } else {
            crate::warn!("{message}");
        }
        problems.push(message);
    }
    problems
}

/// A flat button that toggles the quick-settings popup.
///
/// Several indicators want the same behaviour, and one shared implementation
/// means the pressed state stays correct when the popup is dismissed by a click
/// outside rather than by the button that opened it.
pub fn popup_button(classes: &[&str], tooltip: &str) -> gtk4::Button {
    let button = ui::button(classes);
    ui::tooltip(&button, tooltip);
    button.add_css_class("ra-bar-button");

    button.connect_clicked(|_| popup::toggle());

    // Keep the "active" look in step with the popup, including when it closes
    // because focus was lost somewhere else.
    let popup = popup::handle();
    button.connect_map(move |b| {
        let Some(popup) = popup.as_ref() else { return };
        let weak = b.downgrade();
        popup.connect_visible(move |visible| {
            if let Some(b) = weak.upgrade() {
                if visible {
                    b.add_css_class("active");
                } else {
                    b.remove_css_class("active");
                }
            }
        });
    });
    button
}
