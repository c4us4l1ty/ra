//! Application lifecycle and config reload.
//!
//! Startup order matters and is deliberate:
//!
//! 1. GTK initialises, which is what makes `wlr-layer-shell` probeable;
//! 2. the config is loaded, writing a documented default if none exists;
//! 3. the stylesheet is installed, so the first frame is already themed;
//! 4. the quick-settings popup is constructed (its window is created lazily);
//! 5. panels are built and the file watcher starts.
//!
//! On reload the panels are destroyed and rebuilt rather than patched. A panel
//! is a tree of a few dozen widgets that are cheap to construct, and a full
//! rebuild is the only approach that cannot leave stale widgets, stale signal
//! handlers or stale service subscriptions behind after a config edit.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::config::{self, Config};
use crate::{info, layer, module, panel, popup, theme, warn};

const APP_ID: &str = "dev.c4us4l1ty.ra";

/// Set up and run. Returns the process exit code.
pub fn run(explicit_config: Option<PathBuf>, watch: bool) -> i32 {
    let config_path = config::watch::resolve_path(explicit_config);

    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            warn!("{e}");
            warn!("falling back to the built-in defaults");
            match Config::parse(config::DEFAULT_CONFIG) {
                Ok(c) => c,
                Err(e) => {
                    crate::error!("built-in defaults are broken: {e}");
                    return 1;
                }
            }
        }
    };

    // GTK must be initialised before anything touches widgets or before
    // `gtk_layer_is_supported()` can be probed.
    if let Err(e) = gtk4::init() {
        crate::error!("GTK failed to initialise: {e}");
        return 1;
    }

    theme::install(&config.style);

    let app = gtk4::Application::builder()
        .application_id(APP_ID)
        // The panel owns its windows explicitly and must keep running with none
        // of them mapped, so `run` is driven manually below.
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let windows: Rc<RefCell<Vec<(String, Rc<panel::PanelWindow>)>>> = Rc::new(RefCell::new(Vec::new()));
    let current = Rc::new(RefCell::new(config.clone()));

    // The popup needs a handle on the application and the resolved geometry of
    // the panel it hangs off.
    let qs = popup::install(&app, &config.quick_settings, config.general.close_popup_on_blur);
    qs.set_anchor_offset(panel::anchor_offset(&config.panels, config.quick_settings.anchor));
    qs.apply_poll_interval(config.quick_settings.poll_interval_ms);

    panel::warm_services(&config);

    let monitors = current_monitors();
    *windows.borrow_mut() = panel::build_all(&app, &config, &monitors).borrow().clone();

    if windows.borrow().is_empty() {
        crate::error!("no panel windows were created; check the monitor setting");
    }

    // Global key handling for the popup.
    install_shortcuts(&app);

    // Reload: rebuild every window from scratch. Services keep running, so this
    // costs a few milliseconds and no bus traffic.
    let rebuild = {
        let app = app.clone();
        let windows = windows.clone();
        let current = current.clone();
        let qs = qs.clone();
        Rc::new(move |config: Config| {
            theme::install(&config.style);
            panel::warm_services(&config);
            let monitors = current_monitors();
            let fresh = panel::build_all(&app, &config, &monitors);
            qs.set_anchor_offset(panel::anchor_offset(&config.panels, config.quick_settings.anchor));
            qs.apply_poll_interval(config.quick_settings.poll_interval_ms);
            // Swap only once the new windows exist, so a failed rebuild leaves
            // the old panel on screen rather than a blank desktop.
            let new_windows = std::mem::take(&mut *fresh.borrow_mut());
            let previous = std::mem::replace(&mut *windows.borrow_mut(), new_windows);
            for (_, window) in previous {
                window.destroy();
            }
            *current.borrow_mut() = config;
            info!("configuration applied");
        })
    };

    if watch && config.general.watch_config {
        let rebuild = rebuild.clone();
        let path = config_path.clone();
        let debounce = config.general.reload_debounce_ms;
        let watcher = config::watch::watch(&path, debounce, move |config| rebuild(config.clone()));
        if watcher.is_none() {
            warn!("live reload disabled; restart ra after editing {}", path.display());
        }
        // The watcher must outlive the main loop; leak it deliberately rather
        // than storing it in a scope that ends first.
        std::mem::forget(watcher);
    } else {
        info!("live reload disabled");
    }

    info!("ra ready — click any indicator for quick settings");

    // Hold the application even though every window is a panel that may be
    // hidden, so quitting means quitting rather than exiting on the last unmap.
    // The guard is deliberately leaked: dropping it releases the hold.
    std::mem::forget(app.hold());
    app.run_with_args::<&str>(&[]);
    0
}

/// Every monitor GDK currently knows about.
fn current_monitors() -> Vec<gdk::Monitor> {
    let Some(display) = gdk::Display::default() else { return Vec::new() };
    let model = display.monitors();
    let mut out = Vec::new();
    for index in 0..model.n_items() {
        if let Some(monitor) = model.item(index) {
            if let Ok(monitor) = monitor.downcast::<gdk::Monitor>() {
                out.push(monitor);
            }
        }
    }
    out
}

/// Global accelerator for the popup.
///
/// Exposed through `org.gtk.Actions` on the application's own bus name, which
/// is what `--toggle` activates. A panel never holds the keyboard grab, so a
/// key press cannot become a click locally; the key is captured by XFCE and
/// delivered as a command instead, which is the same mechanism XFCE's own
/// "Command Shortcut" uses.
fn install_shortcuts(app: &gtk4::Application) {
    let toggle = gtk4::gio::SimpleAction::new("toggle", None);
    toggle.connect_activate(|_, _| popup::toggle());
    app.add_action(&toggle);
}

/// Print a report for `--diagnose`: what is running, and which services answered.
pub fn diagnose(config_path: &Path) {
    println!("ra {}", env!("CARGO_PKG_VERSION"));
    println!("config           {}", config_path.display());

    // Initialise GTK when there is a display, so the monitor and layer-shell
    // lines are meaningful. Without a display the command still works and just
    // reports what it can from the bus.
    let has_display = gtk4::init().is_ok();

    println!(
        "gtk              {}.{}.{}",
        gtk4::major_version(),
        gtk4::minor_version(),
        gtk4::micro_version()
    );
    println!("session          {}", std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "?".into()));
    if has_display {
        println!(
            "wlr-layer-shell  {}",
            if layer::supported() { "available" } else { "UNAVAILABLE (windows will float)" }
        );
    } else {
        println!("wlr-layer-shell  unknown (no display)");
    }

    if has_display {
        let monitors = current_monitors();
        let names: Vec<String> = monitors.iter().map(crate::config::monitor_name).collect();
        println!("monitors         {}", if names.is_empty() { "none".to_string() } else { names.join(", ") });
    }

    println!("\nservices:");
    // Services are lazy singletons whose first read is asynchronous, so they
    // must be started *and then* given a main loop to complete in. This is the
    // one place that is genuinely necessary: everything else is signal-driven.
    crate::service::warm();
    pump(&glib::MainContext::default(), std::time::Duration::from_millis(1500));
    for (name, status) in crate::service::availability() {
        println!("  {name:<18}{status}");
    }

    println!("\nmodules:");
    for name in module::known() {
        println!("  {name}");
    }

    if let Ok(cfg) = Config::load(config_path) {
        println!("\nconfigured modules: {}", cfg.module_names().join(", "));
        for problem in module::validate(&cfg.module_names(), false) {
            println!("  ! {problem}");
        }
    }

    if let Ok(session) = crate::dbus::Bus::session() {
        let interesting = [
            "org.freedesktop.Notifications",
            "org.freedesktop.ScreenSaver",
            "org.wireplumber",
            "org.pipewire.PipeWire",
        ];
        let names = session.list_names();
        println!("\nsession bus:");
        for wanted in interesting {
            println!("  {wanted:<32}{}", if names.iter().any(|n| n == wanted) { "present" } else { "absent" });
        }
    }
}

/// Run the main context for `budget`, so pending asynchronous work completes.
///
/// Bounded on purpose: `--diagnose` must never hang, even if a service's D-Bus
/// call is unanswered.
fn pump(ctx: &glib::MainContext, budget: std::time::Duration) {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if !ctx.iteration(false) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}