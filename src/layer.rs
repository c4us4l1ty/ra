//! wlr-layer-shell integration.
//!
//! XFCE 4.20's Wayland session is [labwc], which implements
//! `wlr-layer-shell`, so panels can be pinned to screen edges and reserve
//! space. `gtk4-layer-shell` gives us that without any dependency of our own.
//!
//! On compositors without the protocol (GNOME Shell, Mutter, X11) we fall back
//! to plain windows. The panel still renders and every control still works, it
//! just cannot be pinned to an edge - which is exactly what you want while
//! developing, and a clear error rather than a crash if it ever happens by
//! accident in production.
//!
//! [labwc]: https://labwc.github.io/integration.html

use std::cell::Cell;

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, LayerShell};

use crate::config::{Edge as CfgEdge, Panel, PopupAnchor};
use crate::{info, warn};

thread_local! {
    /// Probed exactly once. `gtk_layer_is_supported()` may block for a Wayland
    /// round trip the first time it runs, so it must not be called per window.
    static SUPPORTED: Cell<Option<bool>> = const { Cell::new(None) };
    static PROBED: Cell<bool> = const { Cell::new(false) };
}

/// Is `wlr-layer-shell` available? Cached; safe to call often.
pub fn supported() -> bool {
    if !PROBED.with(|p| p.replace(true)) {
        // wlr-layer-shell is a Wayland-only protocol. Probing it under X11 can
        // emit a warning from the C library that we do not want in the log, so
        // check the GDK backend name first ("wayland", "wayland-0", ...).
        let wayland = gdk::Display::default()
            .map(|d| d.name().to_lowercase().starts_with("wayland"))
            .unwrap_or(false);
        let ok = wayland && gtk4_layer_shell::is_supported();
        SUPPORTED.with(|s| s.set(Some(ok)));
        if !ok {
            warn!(
                "wlr-layer-shell is unavailable; panels will float as ordinary windows.\n\
                 \x20              On XFCE/Wayland this should not happen - check that the session\n\
                 \x20              is running labwc (wlr-randr will fail if it is not)."
            );
        } else {
            info!("wlr-layer-shell available (protocol version {})", gtk4_layer_shell::protocol_version());
        }
    }
    SUPPORTED.with(|s| s.get().unwrap_or(false))
}

/// Turn a GTK window into a panel surface.
///
/// Must be called before the window is realised.
pub fn attach_panel(win: &gtk4::ApplicationWindow, cfg: &Panel, monitor: Option<&gdk::Monitor>) {
    win.set_decorated(false);
    win.set_resizable(false);

    if !supported() {
        // Floating fallback, used on GNOME, Mutter and X11.
        //
        // A layer surface cannot be emulated with a normal window: it cannot be
        // pinned to an output edge and cannot reserve space. What it can do is
        // behave like a panel as closely as an ordinary toplevel allows, so
        // that the thing is actually usable while developing on a compositor
        // that does not speak the protocol:
        //
        // * be undecorated and sized from the monitor, so it reads as a bar
        //   rather than a small floating dialog.
        //
        // What an ordinary toplevel cannot do is stay above other windows or
        // hide from the taskbar: GTK4 removed `set_keep_above` and the skip
        // hints, and GDK exposes no replacement. So on this path the panel is
        // an ordinary window that the compositor is free to bury or focus.
        // That is the cost of not speaking `wlr-layer-shell`, and it is why the
        // fallback is a development convenience rather than a shipping target.
        //
        // The width comes from the monitor rather than from the content: a
        // bar that shrink-wraps to its widgets looks like a stray toolbar.
        win.set_decorated(false);
        win.set_title(Some(&format!("ra — {}", cfg.name)));
        if let Some(monitor) = monitor {
            let geo = monitor.geometry();
            match cfg.edge {
                CfgEdge::Top | CfgEdge::Bottom => win.set_default_size(geo.width(), cfg.height as i32),
                CfgEdge::Left | CfgEdge::Right => win.set_default_size(cfg.height as i32, geo.height()),
            }
        }
        return;
    }

    win.init_layer_shell();
    win.set_layer(match cfg.layer {
        crate::config::Layer::Bottom => gtk4_layer_shell::Layer::Bottom,
        crate::config::Layer::Top => gtk4_layer_shell::Layer::Top,
        crate::config::Layer::Overlay => gtk4_layer_shell::Layer::Overlay,
    });

    // Anchor the panel edge plus both perpendicular edges so the surface is
    // stretched across the whole output.
    let (anchor, cross_a, cross_b) = match cfg.edge {
        CfgEdge::Top => (Edge::Top, Edge::Left, Edge::Right),
        CfgEdge::Bottom => (Edge::Bottom, Edge::Left, Edge::Right),
        CfgEdge::Left => (Edge::Left, Edge::Top, Edge::Bottom),
        CfgEdge::Right => (Edge::Right, Edge::Top, Edge::Bottom),
    };
    win.set_anchor(anchor, true);
    win.set_anchor(cross_a, true);
    win.set_anchor(cross_b, true);

    win.set_margin(Edge::Top, cfg.margin.top);
    win.set_margin(Edge::Bottom, cfg.margin.bottom);
    win.set_margin(Edge::Left, cfg.margin.left);
    win.set_margin(Edge::Right, cfg.margin.right);

    win.set_namespace(Some(&cfg.namespace.clone().unwrap_or_else(|| format!("ra-{}", cfg.name))));
    win.set_monitor(monitor);

    // Reserve screen space. The zone covers the gap plus the panel thickness,
    // which is what keeps maximised windows from hiding behind the bar.
    let zone = if cfg.exclusive {
        let anchored_margin = match cfg.edge {
            CfgEdge::Top => cfg.margin.top,
            CfgEdge::Bottom => cfg.margin.bottom,
            CfgEdge::Left => cfg.margin.left,
            CfgEdge::Right => cfg.margin.right,
        };
        cfg.height as i32 + anchored_margin
    } else {
        0
    };
    win.set_exclusive_zone(zone);

    // A panel must never steal keyboard focus from the application the user is
    // typing in. The quick-settings popup opts back in when it opens.
    win.set_keyboard_mode(KeyboardMode::None);
}

/// Configure the quick-settings popup window.
///
/// `grab` selects whether the popup takes keyboard focus: `Exclusive` while
/// open so sliders and text entries work, `None` when hidden so focus always
/// returns to the previous window.
pub fn attach_popup(win: &gtk4::ApplicationWindow, anchor: PopupAnchor, grab: bool) {
    win.set_decorated(false);
    win.set_resizable(false);

    if !supported() {
        win.set_title(Some("ra — quick settings"));
        return;
    }

    win.init_layer_shell();
    win.set_layer(gtk4_layer_shell::Layer::Overlay);
    win.set_namespace(Some("ra-quick-settings"));

    // Both horizontal edges are anchored so the popup is positioned
    // horizontally, and one vertical edge fixes it vertically. The vertical
    // anchor is chosen so the popup sits under a top bar (or over a bottom
    // bar); the caller adds the real offset via `set_margin`.
    let (h, v) = match anchor {
        PopupAnchor::TopRight | PopupAnchor::BottomRight => (Edge::Right, Edge::Top),
        PopupAnchor::TopLeft | PopupAnchor::BottomLeft => (Edge::Left, Edge::Top),
        PopupAnchor::TopCenter | PopupAnchor::BottomCenter => (Edge::Top, Edge::Top),
    };
    win.set_anchor(h, true);
    win.set_anchor(Edge::Left, h != Edge::Left);
    win.set_anchor(Edge::Right, h != Edge::Right);
    win.set_anchor(v, true);
    if anchor.is_bottom() {
        win.set_anchor(Edge::Bottom, true);
        win.set_anchor(Edge::Top, false);
    }

    // An overlay must not reserve space or it would shrink the work area.
    win.set_exclusive_zone(0);
    win.set_keyboard_mode(if grab { KeyboardMode::Exclusive } else { KeyboardMode::None });
    win.set_margin(Edge::Top, 0);
    win.set_margin(Edge::Bottom, 0);
}

/// Clear the keyboard grab after the popup closes, so the user's application
/// gets focus back. Called on a short delay because unsetting `Exclusive` and
/// destroying the surface in the same frame confuses some compositors.
pub fn release_focus(win: &gtk4::ApplicationWindow) {
    if !supported() {
        return;
    }
    let weak = win.downgrade();
    glib::idle_add_local_once(move || {
        if let Some(win) = weak.upgrade() {
            win.set_keyboard_mode(KeyboardMode::None);
        }
    });
}

impl PopupAnchor {
    pub fn is_bottom(self) -> bool {
        matches!(self, PopupAnchor::BottomRight | PopupAnchor::BottomLeft | PopupAnchor::BottomCenter)
    }
}