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

/// What to do once the panel exists.
pub enum Mode {
    /// Hand over to the main loop.
    Run { watch: bool },
    /// Render every window to PNG and exit.
    Screenshot { dir: PathBuf },
}

/// Set up and run. Returns the process exit code.
pub fn run(explicit_config: Option<PathBuf>, mode: Mode) -> i32 {
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
    let watch = matches!(mode, Mode::Run { watch: true });

    theme::install(&config.style);

    let app = gtk4::Application::builder()
        .application_id(APP_ID)
        // Deliberately *not* NON_UNIQUE. That flag tells GApplication to skip
        // the bus entirely, which would leave the panel unaddressable and make
        // `ra --toggle` unable to find it. Claiming the name also means a
        // second panel fails loudly at startup instead of fighting the first
        // one for screen edges.
        .build();

    let windows: Rc<RefCell<Vec<(String, Rc<panel::PanelWindow>)>>> = Rc::new(RefCell::new(Vec::new()));
    let current = Rc::new(RefCell::new(config.clone()));

    // Claim the bus name before any widget exists. GTK requires `startup` -
    // emitted by `register` - before a window may be constructed, and a second
    // instance has to bail out before it builds one.
    match claim_bus(&app) {
        Claim::Primary => {}
        Claim::Remote => {
            crate::error!("another ra already owns {APP_ID}; not starting a second panel");
            // Enter the loop anyway. For a remote application this hands our
            // start-up command to the instance that is already running and then
            // unwinds cleanly, unregistering on the way out. Dropping a
            // registered GApplication without running it leaves GLib warning
            // that it did not unregister.
            app.run_with_args::<&str>(&[]);
            return 1;
        }
        Claim::Failed(e) => {
            crate::error!("cannot reach the session bus ({e})");
            return 1;
        }
    }

    // The panels are never activated by the session manager, so there is
    // nothing to do on activate; the handler exists only to silence GLib's
    // "does not implement g_application_activate" warning.
    app.connect_activate(|_| {});

    // The popup needs a handle on the application and the resolved geometry of
    // the panel it hangs off.
    let qs = popup::install(&app, &config.quick_settings, config.general.close_popup_on_blur);
    qs.set_anchor_offset(panel::anchor_offset(&config.panels, config.quick_settings.anchor));
    qs.apply_poll_interval(config.quick_settings.poll_interval_ms);

    panel::warm_services(&config);
    crate::service::notification::install(config.quick_settings.notification_daemon);

    let monitors = current_monitors();
    *windows.borrow_mut() = panel::build_all(&app, &config, &monitors).borrow().clone();

    if windows.borrow().is_empty() {
        crate::error!("no panel windows were created; check the monitor setting");
    }

    // Global key handling for the popup.
    install_shortcuts(&app);

    // Capture before the main loop starts: the windows need one real frame so
    // they are measured, and `pump` gives them that without ever blocking on a
    // compositor that may not support layer-shell at all.
    if let Mode::Screenshot { dir } = &mode {
        return capture_all(dir, &windows, &qs);
    }

    // Reload: rebuild every window from scratch. Services keep running, so this
    // costs a few milliseconds and no bus traffic.
    // One rebuild path, used by the config watcher and by monitor hotplug, so
    // both produce identical panels.
    let rebuild = {
        let app = app.clone();
        let windows = windows.clone();
        let current = current.clone();
        let qs = qs.clone();
        Rc::new(move |config: Config, reason: &str| {
            theme::install(&config.style);
            panel::warm_services(&config);
    crate::service::notification::install(config.quick_settings.notification_daemon);
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
            info!("configuration applied ({reason})");
        })
    };

    // A panel that says `monitor = "all"` has to follow the hardware. GDK
    // reports plug and unplug, and the rebuild is debounced because docking
    // emits both events in quick succession and each one rebuilds every panel.
    if let Some(display) = gdk::Display::default() {
        // GTK4 has no monitor signals. Monitor changes arrive as
        // `items-changed` on the `GListModel` behind `monitors()`, which the
        // display owns for its lifetime, so holding this subscription for the
        // process is both safe and necessary.
        let monitors_model = display.monitors();
        monitors_model.connect_items_changed({
            let rebuild = rebuild.clone();
            let current = current.clone();
            move |_, _, _, _| schedule_monitor_rebuild(rebuild.clone(), current.clone())
        });
        std::mem::forget(monitors_model);
    }

    if watch && config.general.watch_config {
        let rebuild = rebuild.clone();
        let path = config_path.clone();
        let debounce = config.general.reload_debounce_ms;
        let watcher = config::watch::watch(&path, debounce, move |config| rebuild(config.clone(), "config changed"));
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
    // The action takes an explicit string parameter rather than being
    // parameterless. That is deliberate: an empty D-Bus tuple is illegal, so a
    // parameterless action has to be encoded as some arbitrary empty container,
    // and the correct choice differs between GLib versions. A named parameter
    // is unambiguous from both ends.
    let Ok(ty) = glib::VariantTy::new("s") else { return };
    let toggle = gio::SimpleAction::new("toggle", Some(ty));
    toggle.connect_activate(|_, parameter| {
        let command = parameter.and_then(|p| p.str()).unwrap_or("toggle").to_string();
        match command.as_str() {
            "open" => {
                if let Some(q) = popup::handle() {
                    q.open();
                }
            }
            "close" => {
                if let Some(q) = popup::handle() {
                    q.hide();
                }
            }
            _ => popup::toggle(),
        }
    });
    app.add_action(&toggle);
}

/// What happened when the application tried to claim the session bus name.
enum Claim {
    /// We own the name and may build windows.
    Primary,
    /// Another instance owns it; our start-up command was forwarded to it.
    Remote,
    /// There is no usable session bus at all.
    Failed(glib::Error),
}

/// Claim the bus name and emit `startup` before any window exists.
///
/// GTK refuses to create a window until `GApplication::startup` has fired, and
/// `startup` is emitted from `register`. Doing this explicitly - rather than
/// letting `run` do it - is what lets the panels be built immediately, so they
/// are on screen before the main loop starts rather than a frame later.
///
/// `register` reporting success is *not* enough to check. When the name is
/// already taken, GApplication forwards the start-up command to the running
/// instance and marks itself remote - while still returning success. A remote
/// application has no local window list, so the first `GtkWindow` constructed
/// afterwards dereferences null inside `gtk_application_window_added` and takes
/// the process down with a bare SIGSEGV, with no message. `is_remote` is the
/// only reliable signal, and it must be checked before a widget is built.
fn claim_bus(app: &gtk4::Application) -> Claim {
    if let Err(e) = app.register(None::<&gio::Cancellable>) {
        return Claim::Failed(e);
    }
    if app.is_remote() {
        return Claim::Remote;
    }
    Claim::Primary
}

/// Render every panel, then the quick settings, as PNGs in `dir`.
///
/// The popup is opened and paged through so that what gets captured is the
/// real, populated surface: services have answered, tiles carry real labels,
/// and the sliders sit at real values. Capturing an empty shell would prove
/// nothing about whether the panel looks right.
fn capture_all(
    dir: &Path,
    windows: &Rc<RefCell<Vec<(String, Rc<panel::PanelWindow>)>>>,
    qs: &Rc<popup::QuickSettings>,
) -> i32 {
    if let Err(e) = std::fs::create_dir_all(dir) {
        crate::error!("cannot create {}: {e}", dir.display());
        return 1;
    }
    crate::screenshot::settle();

    let mut failures = 0;
    for (name, panel) in windows.borrow().iter() {
        let path = dir.join(format!("panel-{name}.png"));
        report(&panel.window, &path, &mut failures);
    }

    // The popup window is built lazily on first open, so it has to be opened
    // before it can be looked for - asking for the window first finds nothing
    // and looks like a failure to build the popup at all.
    qs.open();
    crate::screenshot::settle();
    match qs.window() {
        Some(window) => report(&window, &dir.join("popup.png"), &mut failures),
        None => {
            crate::error!("the quick settings window was never created");
            failures += 1;
        }
    }
    qs.hide();

    if failures == 0 {
        println!("wrote {} panel(s) and the quick settings to {}", windows.borrow().len(), dir.display());
        0
    } else {
        crate::error!("{failures} window(s) could not be rendered");
        1
    }
}

fn report(window: &gtk4::ApplicationWindow, path: &Path, failures: &mut i32) {
    match crate::screenshot::capture(window.as_ref(), path) {
        Ok((w, h)) => info!("rendered {} at {w}x{h}", path.display()),
        Err(e) => {
            *failures += 1;
            crate::error!("{e}");
        }
    }
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

    let unresolved = crate::icons::unresolved();
    if !unresolved.is_empty() {
        println!("\nicons: {} name(s) no installed theme provides", unresolved.len());
        for name in &unresolved {
            println!("  ! `{name}` has no fallback that exists either");
        }
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

        println!("configured tiles:  {}", cfg.quick_settings.tiles.join(", "));
        // A tile this build cannot draw is dropped with a warning at runtime,
        // so name it here rather than letting it look like it is working.
        for tile in &cfg.quick_settings.tiles {
            if !popup::known_tiles().contains(&tile.as_str()) {
                println!("  ! no tile named `{tile}` in this build");
            }
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

/// Rebuild the panels a moment after the monitor set settles.
///
/// Docking a laptop fires `monitor-added` and `monitor-removed` back to back;
/// rebuilding on each would create and destroy every panel twice for one
/// physical action.
fn schedule_monitor_rebuild(
    rebuild: Rc<dyn Fn(Config, &str)>,
    current: Rc<RefCell<Config>>,
) {
    // One timer for the whole process: a second event replaces the first
    // rather than queueing another rebuild.
    static PENDING: std::sync::Mutex<Option<glib::SourceId>> = std::sync::Mutex::new(None);
    if let Ok(mut pending) = PENDING.lock() {
        if let Some(id) = pending.take() {
            id.remove();
        }
        let config = current.borrow().clone();
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
            rebuild(config, "monitor changed");
        });
        *pending = Some(id);
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