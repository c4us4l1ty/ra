//! Render the panel and the quick settings to PNG files.
//!
//! This exists because the normal way to look at a layer-shell panel is to run
//! it on a compositor that implements layer-shell - so "the panel is invisible,
//! let me take a screenshot" and "I want a screenshot" both fail on the
//! machines most likely to want one. Going through GSK's CPU renderer instead
//! needs no compositor cooperation, no D-Bus screenshot permission, and no
//! second tool.
//!
//! It is also how the panel gets reviewed at all before it is installed: a
//! picture catches a tile that rendered at zero height or a label clipped by
//! two pixels, which reading the log never will.

use std::path::Path;

use gtk4::glib;
use gtk4::prelude::*;

/// Render `window`'s contents to `path` as a PNG.
///
/// The window is mapped first so GTK performs a real measure/allocate pass.
/// A widget that has never been allocated has no size, and snapshotting it
/// would quietly produce an empty image rather than an error - which is the
/// failure this function most needs to avoid.
pub fn capture(window: &gtk4::Window, path: &Path) -> Result<(i32, i32), String> {
    window.present();
    // Let the first frame land: mapping is asynchronous, and snapshotting
    // before allocation has run yields nothing at all.
    pump(std::time::Duration::from_millis(150));

    let Some(child) = window.child() else {
        return Err(format!("{path:?}: the window has no contents"));
    };

    // Ask the widget where it landed rather than reading `allocation()`, which
    // GTK deprecated in 4.12 in favour of this. `compute_bounds` walks the
    // transform chain, so it is right even inside a scroller or a viewport -
    // both of which the quick settings puts its stack inside.
    let bounds = child
        .compute_bounds(window)
        .ok_or_else(|| format!("{path:?}: the contents have no bounds"))?;
    let width = (bounds.width() + 0.5) as i32;
    let height = (bounds.height() + 0.5) as i32;
    if width <= 0 || height <= 0 {
        return Err(format!(
            "{path:?}: contents measured {width}x{height}; nothing to render"
        ));
    }

    let snapshot = gtk4::Snapshot::new();
    window.snapshot_child(&child, &snapshot);
    let Some(node) = snapshot.to_node() else {
        return Err(format!("{path:?}: the widget tree produced no render node"));
    };

    // A Cairo renderer is GSK's software path: no GPU, no display, no
    // compositor involvement.
    let renderer = gsk4::CairoRenderer::new().upcast::<gsk4::Renderer>();
    renderer
        .realize(None)
        .map_err(|e| format!("{path:?}: cannot create a software renderer: {e}"))?;

    let texture = renderer.render_texture(&node, None);
    texture
        .save_to_png(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;

    Ok((width, height))
}

/// Run the main context for `budget`, so pending work completes.
fn pump(budget: std::time::Duration) {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if !glib::MainContext::default().iteration(false) {
            // Nothing is ready. Block briefly rather than spinning a core.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Settle after a rebuild, so a capture sees the finished layout rather than
/// the first, pre-resize pass.
///
/// The wait is generous on purpose. Services answer over D-Bus in several round
/// trips, so a capture taken the instant the panel exists shows every tile in
/// its "not loaded yet" state - Wi-Fi with no SSID, battery missing entirely.
/// Waiting for the subscriptions to deliver makes the picture show what a user
/// would actually see a second after logging in.
pub fn settle() {
    pump(std::time::Duration::from_millis(1200));
}
