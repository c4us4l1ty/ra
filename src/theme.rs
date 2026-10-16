//! Stylesheet installation and theme following.
//!
//! Theme following needs no code at all. GTK4 exposes a set of *named colours*
//! to CSS — `@window_bg_color`, `@accent_bg_color`, `@borders` and friends —
//! which it resolves against the active theme and re-resolves when the theme
//! changes. The built-in stylesheet is written entirely in those names, so
//! switching XFCE's GTK theme re-colours the panel for free.
//!
//! What code is needed for:
//!
//! * an explicit accent override, injected as a CSS custom property;
//! * a background opacity override, which has to be folded into a `@define-color`
//!   because GTK's `alpha()` cannot take a variable in older versions;
//! * picking up the theme name change so the accent fallback can be recomputed.

use std::rc::Rc;

use gtk4::gdk;
use gtk4::prelude::*;

use crate::config::Style;
use crate::{debug, info, warn};

const BUILTIN_CSS: &str = include_str!("../config/style.css");

/// Install the stylesheet for the whole display and keep following the theme.
///
/// Must be called after `gtk::init`.
pub fn install(style: &Style) {
    // The theme-change handlers outlive this call, so they get their own copy.
    let style: Rc<Style> = Rc::new(style.clone());
    let display = match gdk::Display::default() {
        Some(d) => d,
        None => {
            warn!("no display; cannot install stylesheet");
            return;
        }
    };

    let provider = gtk4::CssProvider::new();
    gtk4::style_context_add_provider_for_display(&display, &provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);

    rebuild(&provider, &style);

    // The user changes GTK themes often, so following is the default.
    if !style.follow_gtk_theme {
        info!("theme following disabled; colours will not track the GTK theme");
        return;
    }

    let Some(settings) = gtk4::Settings::default() else { return };
    let theme_provider = provider.clone();
    let theme_style = style.clone();
    settings.connect_gtk_theme_name_notify(move |_| {
        debug!("GTK theme changed; re-resolving panel colours");
        rebuild(&theme_provider, &theme_style);
    });
    // `gtk-accent-color` is a plain GObject property, so it needs a generic
    // notify rather than a generated one.
    let accent_provider = provider.clone();
    let accent_style = style.clone();
    settings.connect_notify_local(Some("gtk-accent-color"), move |_, _| {
        debug!("GTK accent colour changed");
        rebuild(&accent_provider, &accent_style);
    });
    if let Some(accent) = style.accent.as_deref() {
        debug!("accent override: {accent}");
    }
    info!("stylesheet installed");
}

/// Rebuild the CSS text and hand it to the provider.
fn rebuild(provider: &gtk4::CssProvider, style: &Style) {
    let mut css = String::with_capacity(BUILTIN_CSS.len() + 256);

    // Accent. `@define-color` is how a CSS variable is written in GTK; defining
    // `ra_accent` lets the rest of the sheet stay declarative.
    if let Some(accent) = style.accent.as_deref().and_then(parse_hex) {
        css.push_str(&format!("@define-color ra_accent {accent};\n"));
    } else {
        css.push_str("@define-color ra_accent @accent_bg_color;\n");
    }

    // Background opacity. Folded into a named colour rather than an `alpha()`
    // call so that `alpha(@ra_panel_bg, x)` stays valid.
    if let Some(opacity) = style.opacity {
        let o = opacity.clamp(0.0, 1.0);
        css.push_str(&format!(
            "@define-color ra_panel_bg rgba(@window_bg_color_red, @window_bg_color_green, @window_bg_color_blue, {o});\n"
        ));
    }

    css.push_str(BUILTIN_CSS);

    if let Some(path) = style.stylesheet.as_deref() {
        match std::fs::read_to_string(path) {
            Ok(user) => {
                // Appended last so the user's rules win, same specificity.
                css.push_str("\n/* --- from ");
                css.push_str(&path.display().to_string());
                css.push_str(" --- */\n");
                css.push_str(&user);
            }
            Err(e) => warn!("cannot read {}: {e}", path.display()),
        }
    }

    provider.load_from_string(&css);
}

/// Validate a colour override, normalising a bare hex string to `#rrggbb`.
///
/// Returns `None` for anything that is not a hex colour; GTK's own named
/// colours are not accepted here because this value ends up in a
/// `@define-color`, where a name would silently do nothing.
fn parse_hex(text: &str) -> Option<String> {
    let t = text.trim();
    let hex = t.strip_prefix('#').unwrap_or(t);
    if !matches!(hex.len(), 3 | 6 | 8) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{hex}"))
}