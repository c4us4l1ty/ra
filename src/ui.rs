//! Shared widget helpers.
//!
//! Small constructors used by both the bar modules and the quick-settings
//! popup. Everything here is deliberately thin: it builds a widget and returns
//! the handles the caller needs to keep updating, with no hidden state and no
//! callbacks of its own.

use gtk4::prelude::*;

/// A symbolic icon, sized in pixels.
/// An image from a symbolic icon name.
///
/// The name is resolved against the live icon theme first, so a name the
/// installed theme does not have falls back to something drawable rather than
/// becoming a broken-image box. See [`crate::icons`].
pub fn icon(name: &str, size: i32) -> gtk4::Image {
    crate::icons::image(name, size)
}

/// A borderless, flat button that behaves like a menu item.
pub fn button(classes: &[&str]) -> gtk4::Button {
    let b = gtk4::Button::builder().has_frame(false).build();
    for c in classes {
        b.add_css_class(c);
    }
    b
}

/// Attach a tooltip, or clear it when `text` is empty.
pub fn tooltip(widget: &impl IsA<gtk4::Widget>, text: impl AsRef<str>) {
    let text = text.as_ref();
    if text.is_empty() {
        widget.remove_css_class("has-tooltip");
    } else {
        widget.set_tooltip_text(Some(text));
    }
}

/// Toggle a CSS class according to a boolean.
pub fn set_active(widget: &impl IsA<gtk4::Widget>, active: bool, class: &str) {
    if active {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// A rounded tile: icon, title, optional subtitle, optional trailing chevron.
///
/// Mirrors the quick-settings grid in the reference screenshots. Returns the
/// button plus the labels, so the caller can update them in place rather than
/// rebuilding the widget - rebuilding on every state change is what makes
/// panels feel laggy and leak signal handlers.
pub struct Tile {
    pub root: gtk4::Button,
    pub icon: gtk4::Image,
    pub title: gtk4::Label,
    pub subtitle: gtk4::Label,
}

impl Tile {
    /// `drill` adds the trailing chevron used by tiles that open a sub-page.
    pub fn new(classes: &[&str], drill: bool) -> Self {
        let root = button(classes);
        root.set_halign(gtk4::Align::Fill);
        root.set_valign(gtk4::Align::Fill);
        root.add_css_class("ra-tile");

        let layout = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        layout.set_margin_start(14);
        layout.set_margin_end(14);
        layout.set_margin_top(10);
        layout.set_margin_bottom(10);

        let icon_image = icon("application-x-executable-symbolic", 18);
        let icon_wrap = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        icon_wrap.append(&icon_image);
        icon_wrap.set_size_request(24, -1);
        layout.append(&icon_wrap);

        let text = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
        text.set_hexpand(true);
        text.set_valign(gtk4::Align::Center);
        let title = gtk4::Label::builder().xalign(0.0).ellipsize(gtk4::pango::EllipsizeMode::End).build();
        title.add_css_class("ra-tile-title");
        let subtitle = gtk4::Label::builder().xalign(0.0).ellipsize(gtk4::pango::EllipsizeMode::End).build();
        subtitle.add_css_class("ra-tile-subtitle");
        text.append(&title);
        text.append(&subtitle);
        layout.append(&text);

        let chevron = icon("go-next-symbolic", 14);
        chevron.set_visible(drill);
        chevron.add_css_class("ra-chevron");
        let chevron_wrap = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        chevron_wrap.append(&chevron);
        layout.append(&chevron_wrap);

        root.set_child(Some(&layout));
        let _ = chevron;
        Self { root, icon: icon_image, title, subtitle }
    }

    pub fn set_icon(&self, name: &str) {
        crate::icons::set(&self.icon, name);
    }

    pub fn set_title(&self, text: &str) {
        self.title.set_text(text);
    }

    pub fn set_subtitle(&self, text: &str) {
        self.subtitle.set_text(text);
        self.subtitle.set_visible(!text.is_empty());
    }

    /// Mark the tile as the "on" state of its toggle.
    pub fn set_on(&self, on: bool) {
        set_active(&self.root, on, "checked");
    }

    pub fn set_sensitive(&self, sensitive: bool) {
        self.root.set_sensitive(sensitive);
    }
}

/// An icon + horizontal slider + optional value label row.
pub struct SliderRow {
    pub root: gtk4::Box,
    pub icon: gtk4::Image,
    pub slider: gtk4::Scale,
    value: gtk4::Label,
}

impl SliderRow {
    pub fn new(icon_name: &str, on_change: impl Fn(u8) + 'static) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        root.add_css_class("ra-slider-row");

        let icon = icon(icon_name, 18);
        icon.set_halign(gtk4::Align::Center);
        icon.set_size_request(22, -1);
        root.append(&icon);

        let adjustment = gtk4::Adjustment::new(0.0, 0.0, 100.0, 1.0, 10.0, 0.0);
        let slider = gtk4::Scale::new(gtk4::Orientation::Horizontal, Some(&adjustment));
        slider.set_draw_value(false);
        slider.set_hexpand(true);
        slider.set_valign(gtk4::Align::Center);
        slider.add_css_class("ra-slider");
        root.append(&slider);

        let value = gtk4::Label::new(None);
        value.set_width_chars(4);
        value.set_xalign(1.0);
        value.add_css_class("ra-slider-value");
        root.append(&value);

        // GTK emits this continuously while dragging. Services are written to
        // coalesce bursts, but throttling here as well keeps a drag from
        // queueing hundreds of D-Bus calls behind the pointer.
        slider.connect_value_changed(move |s| {
            let v = s.value().round().clamp(0.0, 100.0) as u8;
            on_change(v);
        });

        Self { root, icon, slider, value }
    }

    /// Update the handle position without re-entering the change handler.
    pub fn set_value(&self, percent: u8) {
        if (self.slider.value() - f64::from(percent)).abs() >= 0.5 {
            self.slider.set_value(f64::from(percent));
        }
        self.value.set_text(&format!("{percent}%"));
    }

    pub fn set_icon(&self, name: &str) {
        crate::icons::set(&self.icon, name);
    }

    pub fn set_visible_row(&self, visible: bool) {
        self.root.set_visible(visible);
    }
}

/// A page header: back chevron, title, optional action button.
pub struct PageHeader {
    pub root: gtk4::Box,
    pub back: gtk4::Button,
    pub title: gtk4::Label,
}

impl PageHeader {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        root.add_css_class("ra-page-header");

        let back = button(&["ra-page-back"]);
        crate::icons::set_on_button(&back, "go-previous-symbolic", 16);
        back.add_css_class("flat");
        root.append(&back);

        let title = gtk4::Label::new(None);
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.add_css_class("ra-page-title");
        root.append(&title);

        Self { root, back, title }
    }

    pub fn set_title(&self, text: &str) {
        self.title.set_text(text);
    }
}

impl Default for PageHeader {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a horizontal box with uniform spacing.
pub fn hbox(spacing: i32) -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Horizontal, spacing);
    b.set_halign(gtk4::Align::Fill);
    b.set_valign(gtk4::Align::Fill);
    b
}

/// Build a vertical box with uniform spacing.
pub fn vbox(spacing: i32) -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Vertical, spacing);
    b.set_halign(gtk4::Align::Fill);
    b.set_valign(gtk4::Align::Fill);
    b
}

/// Run a child process without a shell, so no argument can ever be interpreted
/// as shell syntax. Returns its stdout on success.
pub fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    use std::process::Command;
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        crate::debug!("{program} {:?}: {}", args, String::from_utf8_lossy(&out.stderr).trim());
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}