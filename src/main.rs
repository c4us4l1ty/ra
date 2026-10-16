//! ra — a modular panel for XFCE/Wayland.
//!
//! A drop-in replacement for the XFCE panel: a top bar, an optional bottom
//! dock, and one Android-style quick-settings popup that holds every control.
//! Written in Rust against GTK4 and wlr-layer-shell, with no async runtime, no
//! database and no HTTP client.

mod app;
mod config;
mod dbus;
mod future;
mod layer;
mod logging;
mod module;
mod panel;
mod popup;
mod service;
mod state;
mod theme;
mod ui;

use std::path::PathBuf;

use gtk4::glib::prelude::ToVariant;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
ra — a modular panel for XFCE/Wayland

USAGE
    ra [OPTIONS]

OPTIONS
    -c, --config <PATH>   Use this configuration file
    -w, --no-watch        Do not reload the config when it changes
    -t, --toggle          Toggle the quick settings, then exit
        --diagnose        Print what ra can see, then exit
    -v, --version         Print the version, then exit
    -h, --help            Print this help, then exit

ENVIRONMENT
    RA_LOG        error | warn | info | debug | trace   (default: warn)
    RA_AUDIO_POLL_MS   Fallback audio poll interval in ms; 0 disables it
";

fn main() -> ExitCode {
    logging::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path: Option<PathBuf> = None;
    let mut watch = true;
    let mut diagnose = false;
    let mut toggle = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            "-v" | "--version" => {
                println!("ra {VERSION}");
                return ExitCode::SUCCESS;
            }
            "--diagnose" => diagnose = true,
            "-t" | "--toggle" => toggle = true,
            "-w" | "--no-watch" => watch = false,
            "-c" | "--config" => {
                i += 1;
                match args.get(i) {
                    Some(path) => config_path = Some(PathBuf::from(path)),
                    None => {
                        eprintln!("--config needs a path");
                        return ExitCode::FAILURE;
                    }
                }
            }
            other => {
                eprintln!("unknown option `{other}`\n\n{HELP}");
                return ExitCode::FAILURE;
            }
        }
        i += 1;
    }

    // `--diagnose` must work even without a display, because the most common
    // reason to run it is "my panel is not showing up".
    if diagnose {
        let path = config::watch::resolve_path(config_path);
        app::diagnose(&path);
        return ExitCode::SUCCESS;
    }

    if toggle {
        return toggle_quick_settings();
    }

    let code = app::run(config_path, watch);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Open or close the popup in an already-running instance.
///
/// A panel holds no keyboard grab, so a key press cannot be turned into a
/// click locally. The way a shell-level shortcut reaches a panel is a command,
/// which is exactly what XFCE's own "Command Shortcut" setting runs. This
/// activates the `toggle` action the running panel exports through GTK's
/// standard `org.gtk.Actions` interface, which works no matter which panel is
/// on screen.
fn toggle_quick_settings() -> ExitCode {
    let Ok(bus) = dbus::Bus::session() else {
        eprintln!("cannot reach the session bus");
        return ExitCode::FAILURE;
    };

    if !bus.name_has_owner(APP_ID) {
        eprintln!("ra is not running");
        return ExitCode::FAILURE;
    }

    let body = gtk4::glib::Variant::tuple_from_iter([
        &"toggle".to_variant(),
        &gtk4::glib::Variant::array_from_iter::<gtk4::glib::Variant>([]),
        &gtk4::glib::Variant::array_from_iter::<gtk4::glib::Variant>([]),
    ]);
    match bus.call_blocking(APP_ID, "/dev/c4us4l1ty/ra", "org.gtk.Actions", "Activate", Some(&body)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("could not toggle quick settings: {e}");
            ExitCode::FAILURE
        }
    }
}

const APP_ID: &str = "dev.c4us4l1ty.ra";