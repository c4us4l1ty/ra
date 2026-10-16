//! Passive bar modules: text, separators, spacers and user commands.

use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;
use serde::Deserialize;

use crate::config::ModuleSpec;
use crate::ui;
use crate::{debug, warn};

// -- label -------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelArgs {
    /// The text. `{}` is replaced by the module's live value.
    text: Option<String>,
}

pub fn label(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let args: LabelArgs = spec.args_as();
    let text = args.text.unwrap_or_else(|| spec.module.clone());
    let label = gtk4::Label::new(Some(&text));
    label.add_css_class("ra-bar-label");
    Some(label.upcast())
}

// -- separator ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeparatorArgs {
    /// Thickness in pixels.
    #[serde(default)]
    size: Option<u32>,
}

impl Default for SeparatorArgs {
    fn default() -> Self {
        Self { size: None }
    }
}

pub fn separator(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let args: SeparatorArgs = spec.args_as();
    let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    box_.add_css_class("ra-separator");
    if let Some(size) = args.size {
        box_.set_size_request(size as i32, 0);
    }
    Some(box_.upcast())
}

// -- spacer ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpacerArgs {
    /// Fixed width, or `expand = true` to absorb the free space in a section.
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    expand: Option<bool>,
}

impl Default for SpacerArgs {
    fn default() -> Self {
        Self { width: None, expand: None }
    }
}

pub fn spacer(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let args: SpacerArgs = spec.args_as();
    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    if args.expand.unwrap_or(true) {
        box_.set_hexpand(true);
    }
    if let Some(width) = args.width {
        box_.set_size_request(width as i32, -1);
    }
    Some(box_.upcast())
}

// -- launcher ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherArgs {
    /// Icon shown on the button.
    #[serde(default = "default_launcher_icon")]
    icon: String,
    /// Optional fixed label next to the icon.
    #[serde(default)]
    label: Option<String>,
}

fn default_launcher_icon() -> String {
    "view-app-grid-symbolic".to_string()
}

impl Default for LauncherArgs {
    fn default() -> Self {
        Self { icon: default_launcher_icon(), label: None }
    }
}

/// The Activities button: opens the application grid.
///
/// The grid itself lives inside the quick-settings surface rather than in its
/// own window, because a new toplevel cannot take keyboard focus while the
/// layer-shell popup holds the grab.
pub fn launcher(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let args: LauncherArgs = spec.args_as();
    let button = ui::button(&["ra-bar-button"]);

    let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let icon = ui::icon(&args.icon, 17);
    icon.add_css_class("ra-bar-icon");
    content.append(&icon);
    if let Some(label) = &args.label {
        let text = gtk4::Label::new(Some(label));
        text.add_css_class("ra-bar-label");
        content.append(&text);
    }
    button.set_child(Some(&content));
    ui::tooltip(&button, "Applications");

    button.connect_clicked(|_| {
        if let Some(popup) = crate::popup::handle() {
            popup.open_apps();
        }
    });

    // The pressed state follows the popup, however it was dismissed.
    let popup = crate::popup::handle();
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

    Some(button.upcast())
}

// -- custom ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomArgs {
    /// Program to run. Never passed through a shell.
    command: String,
    /// Arguments, fixed.
    #[serde(default)]
    args: Vec<String>,
    /// How often to re-run, in seconds. 0 means "only once".
    #[serde(default = "thirty")]
    interval: u32,
    /// Shown in a tooltip.
    #[serde(default)]
    tooltip: Option<String>,
}

fn thirty() -> u32 {
    30
}

impl Default for CustomArgs {
    fn default() -> Self {
        Self { command: String::new(), args: Vec::new(), interval: thirty(), tooltip: None }
    }
}

/// Run a user command and show its first line of output.
///
/// The command is executed directly, never through a shell, so a value coming
/// from the config cannot introduce a command. This is the same trust model as
/// a `waybar` custom module or a `polybar` script: whoever can write your
/// `config.toml` can already run code as you.
pub fn custom(spec: &ModuleSpec) -> Option<gtk4::Widget> {
    let args: CustomArgs = spec.args_as();
    if args.command.trim().is_empty() {
        warn!("custom module has no command");
        return None;
    }

    let button = ui::button(&["ra-bar-button"]);
    let label = gtk4::Label::new(Some("…"));
    label.add_css_class("ra-bar-label");
    button.set_child(Some(&label));
    if let Some(tip) = &args.tooltip {
        ui::tooltip(&button, tip);
    }

    let program = args.command.clone();
    let argv: Vec<String> = args.args.clone();
    let interval = args.interval;

    let run = move || {
        let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
        let out = ui::run_capture(&program, &borrowed);
        let text = match out {
            Some(raw) => raw.lines().next().unwrap_or("").trim().to_string(),
            None => {
                warn!("custom module: `{program}` produced nothing");
                String::new()
            }
        };
        label.set_text(&text);
        debug!("custom `{program}` -> {text:?}");
    };

    // Immediate first run, then on a timer. A custom module is the only place
    // in the panel that deliberately polls, because it is explicitly asking to
    // be told the value of something.
    run();
    if interval > 0 {
        glib::timeout_add_local(Duration::from_secs(u64::from(interval)), move || {
            run();
            glib::ControlFlow::Continue
        });
    }

    Some(button.upcast())
}

// -- dark mode helper --------------------------------------------------------

/// `gtk-application-prefer-dark-theme` lives in `settings.ini`, which is also
/// what xfce4-settings writes. Editing the same key rather than setting an
/// environment variable means the change survives into every other GTK
/// application and is visible in the XFCE Appearance panel.
pub fn set_prefer_dark(dark: bool) {
    let path = settings_ini_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        "[Settings]\ngtk-theme-name=Adwaita\n".to_string()
    });

    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
    let key = "gtk-application-prefer-dark-theme";
    let value = if dark { "1" } else { "0" };

    let mut replaced = false;
    for line in lines.iter_mut() {
        if line.starts_with(key) {
            *line = format!("{key}={value}");
            replaced = true;
        }
    }
    if !replaced {
        if !lines.iter().any(|l| l.trim() == "[Settings]") {
            lines.insert(0, "[Settings]".to_string());
        }
        lines.push(format!("{key}={value}"));
    }

    let mut text = lines.join("\n");
    text.push('\n');
    match std::fs::write(&path, text) {
        Ok(()) => {
            debug!("dark preference set to {dark} in {}", path.display());
            // Live GTK apps pick this up through the settings file; nothing
            // else is needed.
            if let Some(settings) = gtk4::Settings::default() {
                settings.set_gtk_application_prefer_dark_theme(dark);
            }
        }
        Err(e) => warn!("cannot write {}: {e}", path.display()),
    }
}

fn settings_ini_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(|| std::path::PathBuf::from("/"));
            home.join(".config")
        });
    base.join("gtk-4.0").join("settings.ini")
}

