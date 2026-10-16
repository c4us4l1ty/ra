//! Panel windows.
//!
//! One `gtk::ApplicationWindow` per `[[panel]]` entry, times one per matching
//! monitor. The window is anchored to an edge by `wlr-layer-shell`, reserves
//! space when asked to, and never takes the keyboard: everything that needs
//! typing happens in the quick-settings popup, which does grab.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gdk;
use gtk4::prelude::*;

use crate::config::{Config, Edge, Panel, Slot};
use crate::layer;
use crate::module;
use crate::service;
use crate::ui;
use crate::{debug, info, warn};

/// A live panel window, kept so a config reload can destroy it.
pub struct PanelWindow {
    pub window: gtk4::ApplicationWindow,
    /// The service subscriptions belonging to the widgets in this window.
    ///
    /// Held here, next to the widgets they update, so that destroying the
    /// window unsubscribes them. Letting a module create a listener and drop it
    /// on the spot is what froze the whole bar; letting it leak instead would
    /// mean every config reload left another generation of listeners writing to
    /// widgets nobody can see.
    subs: Vec<crate::dbus::Subscription>,
}

impl PanelWindow {
    pub fn destroy(&self) {
        debug!("tearing down a panel with {} service subscription(s)", self.subs.len());
        self.window.destroy();
        // The subscriptions fall with this struct, so the listeners stop
        // exactly when the widgets they write to stop existing. A reload
        // therefore cannot leave the previous generation still repainting
        // widgets nobody can see.
    }
}

/// Build every window described by `config`.
///
/// Returns the windows keyed by the panel's `name`, so the application can
/// update them on reload. Unknown modules are reported once, with the list of
/// valid names, and skipped rather than crashing the panel.
pub fn build_all(
    app: &gtk4::Application,
    config: &Config,
    monitors: &[gdk::Monitor],
) -> Rc<RefCell<Vec<(String, Rc<PanelWindow>)>>> {
    let windows: Rc<RefCell<Vec<(String, Rc<PanelWindow>)>>> = Rc::new(RefCell::new(Vec::new()));

    module::validate(&config.module_names(), config.general.strict_modules);

    for panel in &config.panels {
        let targets = panel.monitor().resolve(monitors);
        if targets.is_empty() {
            warn!(
                "panel `{}` targets monitor `{}` but nothing matches; not creating it",
                panel.name,
                describe(&panel.monitor)
            );
            continue;
        }
        for (index, monitor) in targets.iter().enumerate() {
            let name = if targets.len() == 1 {
                panel.name.clone()
            } else {
                format!("{}@{index}", panel.name)
            };
            let window = build_one(app, panel, config, Some(monitor), &name);
            windows.borrow_mut().push((panel.name.clone(), window));
        }
    }

    info!("{} panel window(s)", windows.borrow().len());
    windows
}

fn describe(m: &crate::config::MonitorSpec) -> String {
    match m {
        crate::config::MonitorSpec::First => "first".into(),
        crate::config::MonitorSpec::All => "all".into(),
        crate::config::MonitorSpec::Named(n) => n.clone(),
    }
}

/// Build one window for one monitor.
fn build_one(
    app: &gtk4::Application,
    panel: &Panel,
    config: &Config,
    monitor: Option<&gdk::Monitor>,
    name: &str,
) -> Rc<PanelWindow> {
    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title(&format!("ra — {name}"))
        .build();
    window.add_css_class("ra-panel");
    for class in panel.css_classes() {
        window.add_css_class(&class);
    }
    window.set_widget_name(&format!("ra-panel-{}", panel.name));

    // Anchoring must be configured before the window is realised.
    layer::attach_panel(&window, panel, monitor);

    // The visible bar surface. Wrapping the contents keeps the background
    // independent of the panel's own widget, so a transparent compositor
    // region can still be requested via `margin`.
    let surface = ui::hbox(0);
    surface.add_css_class("ra-panel-surface");

    let subs = build_sections(&surface, panel, config);

    window.set_child(Some(&surface));

    // A layer surface is not a normal toplevel: it has no taskbar entry, no
    // title bar and never takes keyboard focus, which is exactly what a panel
    // should be. `attach_panel` already set `KeyboardMode::None`.

    // Panels are sized to their content along the anchored axis. Only on the
    // layer-shell path, where the surface is stretched by the compositor: in
    // the floating fallback the width has to be filled in from the monitor, or
    // the window shrinks to its content and reads as a stray toolbar rather
    // than a bar.
    if crate::layer::supported() {
        match panel.edge {
            Edge::Top | Edge::Bottom => {
                window.set_default_size(-1, panel.height as i32);
            }
            Edge::Left | Edge::Right => {
                window.set_default_size(panel.height as i32, -1);
            }
        }
    }

    // Map the window. This is unconditional on purpose.
    //
    // A layer surface is committed as soon as it is realised, so the obvious
    // reading is that `present()` is only needed for the floating fallback.
    // It is not: without it the window is built, styled and configured, and
    // never mapped. On labwc that happened to be harmless because GTK realised
    // the surface during `init_layer_shell`; on any compositor where
    // `wlr-layer-shell` is unavailable nothing maps the window at all and the
    // panel is invisible while every log line says it was created. The
    // screenshot path masked this because it presents each window itself.
    window.present();
    Rc::new(PanelWindow { window, subs })
}

/// Lay out `left`, `center` and `right`.
///
/// The centre section is what makes a clock sit in the middle of a wide bar
/// rather than next to the left-hand widgets, so it is given the free space
/// while the outer sections shrink-wrap their contents.
fn build_sections(surface: &gtk4::Box, panel: &Panel, config: &Config) -> Vec<crate::dbus::Subscription> {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    row.add_css_class("ra-panel-row");
    if panel.edge == Edge::Left || panel.edge == Edge::Right {
        row.set_orientation(gtk4::Orientation::Vertical);
    }
    surface.append(&row);

    let left = build_section(panel, &config.include, &panel.left, Align::Start);
    let center = build_section(panel, &config.include, &panel.center, Align::Center);
    let right = build_section(panel, &config.include, &panel.right, Align::End);

    let mut subs = Vec::new();
    if let Some((left, part)) = left {
        row.append(&left);
        subs.extend(part);
    }
    if let Some((center, part)) = center {
        // The centre section absorbs the free space, which is what pins it to
        // the middle of the bar.
        center.set_hexpand(true);
        row.append(&center);
        subs.extend(part);
    }
    if let Some((right, part)) = right {
        row.append(&right);
        subs.extend(part);
    }
    subs
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Start,
    Center,
    End,
}

/// A section box, and the subscriptions of the modules inside it.
type Section = (gtk4::Box, Vec<crate::dbus::Subscription>);

fn build_section(
    panel: &Panel,
    includes: &std::collections::BTreeMap<String, Vec<Slot>>,
    slots: &[Slot],
    align: Align,
) -> Option<Section> {
    let slots = Config::expand_slots(slots, includes);
    if slots.is_empty() {
        return None;
    }
    let horizontal = panel.edge == Edge::Top || panel.edge == Edge::Bottom;
    let section = gtk4::Box::new(
        if horizontal { gtk4::Orientation::Horizontal } else { gtk4::Orientation::Vertical },
        panel.spacing.unwrap_or(0) as i32,
    );
    section.add_css_class("ra-section");
    section.add_css_class(match align {
        Align::Start => "ra-section-left",
        Align::Center => "ra-section-center",
        Align::End => "ra-section-right",
    });
    if horizontal {
        section.set_valign(gtk4::Align::Center);
        section.set_halign(match align {
            Align::Start => gtk4::Align::Start,
            Align::Center => gtk4::Align::Center,
            Align::End => gtk4::Align::End,
        });
    } else {
        section.set_halign(gtk4::Align::Center);
        section.set_valign(match align {
            Align::Start => gtk4::Align::Start,
            Align::Center => gtk4::Align::Center,
            Align::End => gtk4::Align::End,
        });
    }

    let mut subs = Vec::new();
    for slot in slots {
        match slot {
            Slot::Module(reference) => {
                let spec = reference.spec();
                if let Some(built) = module::build(&spec.module, &spec) {
                    section.append(&built.widget);
                    subs.extend(built.subs);
                }
            }
            Slot::Group(group) => {
                let container = gtk4::Box::new(
                    if horizontal { gtk4::Orientation::Horizontal } else { gtk4::Orientation::Vertical },
                    0,
                );
                container.set_widget_name(&group.name);
                container.add_css_class("ra-group");
                if let Some(class) = &group.class {
                    for c in class.split_whitespace() {
                        container.add_css_class(c);
                    }
                }
                if let Some(tip) = &group.tooltip {
                    ui::tooltip(&container, tip);
                }
                let inner: Vec<crate::config::ModuleRef> = group.modules.clone();
                for entry in inner {
                    let spec = entry.spec();
                    if let Some(built) = module::build(&spec.module, &spec) {
                        container.append(&built.widget);
                        subs.extend(built.subs);
                    }
                }
                // An empty group would render as a zero-width gap; drop it.
                if container.first_child().is_some() {
                    section.append(&container);
                }
            }
        }
    }

    Some((section, subs))
}

/// Distance from the screen edge to where the popup should start, i.e. the
/// thickness of the panel it hangs off.
pub fn anchor_offset(panels: &[Panel], anchor: crate::config::PopupAnchor) -> i32 {
    let want_bottom = anchor.is_bottom();
    panels
        .iter()
        .find(|p| if want_bottom { p.edge == Edge::Bottom } else { p.edge == Edge::Top })
        .map(|p| {
            let margin = if want_bottom { p.margin.bottom } else { p.margin.top };
            p.height as i32 + margin
        })
        .unwrap_or(0)
}

/// Start the services a config actually references, so a panel that mentions
/// nothing does not open bus connections it will never use.
pub fn warm_services(config: &Config) {
    let mut started: Vec<&str> = Vec::new();
    for name in config.module_names() {
        let root = name.split([':', '.']).next().unwrap_or(&name).to_string();
        match root.as_str() {
            "network" => {
                service::network::get();
                started.push("network");
            }
            "bluetooth" => {
                service::bluetooth::get();
                started.push("bluetooth");
            }
            "volume" => {
                service::audio::get();
                started.push("audio");
            }
            "brightness" => {
                service::brightness::get();
                started.push("brightness");
            }
            "battery" | "power-profile" => {
                service::power::get();
                started.push("power");
            }
            _ => {}
        }
    }
    if !started.is_empty() {
        debug!("services started: {}", started.join(", "));
    }
}